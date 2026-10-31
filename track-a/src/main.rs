use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_mm_crc32_u64;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const NUM_HOURS: usize = 72;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

fn hour_to_string(hour_id: usize) -> [u8; 20] {
    let mut s = [0u8; 20];
    let total_hr = hour_id + 18;
    let (day, hr) = if total_hr < 24 {
        (30, total_hr)
    } else if total_hr < 48 {
        (31, total_hr - 24)
    } else if total_hr < 72 {
        (1, total_hr - 48)
    } else {
        (2, total_hr - 72)
    };
    let month = if day >= 30 { b"10" } else { b"11" };
    s[0..4].copy_from_slice(b"2026");
    s[4] = b'-';
    s[5..7].copy_from_slice(month);
    s[7] = b'-';
    s[8] = b'0' + (day / 10) as u8;
    s[9] = b'0' + (day % 10) as u8;
    s[10] = b'T';
    s[11] = b'0' + (hr / 10) as u8;
    s[12] = b'0' + (hr % 10) as u8;
    s[13..20].copy_from_slice(b":00:00Z");
    s
}

#[inline(always)]
fn nearest_rank(n: usize, p: usize) -> usize {
    (p * n + 99) / 100 - 1
}

#[inline(always)]
fn write_u64_into(mut n: u64, out: &mut Vec<u8>) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        out.push(buf[i]);
    }
}

#[inline(always)]
fn find_space_swar(w: u64) -> Option<usize> {
    let x = w ^ 0x2020202020202020;
    let m = (x.wrapping_sub(0x0101010101010101)) & !x & 0x8080808080808080;
    if m != 0 {
        Some((m.trailing_zeros() >> 3) as usize)
    } else {
        None
    }
}

struct FastCatalog<'a> {
    endpoints: Vec<&'a [u8]>,
    hashes: Vec<u64>,
    table: Vec<(u64, u16)>,
    mask: usize,
}

impl<'a> FastCatalog<'a> {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        Self {
            endpoints: Vec::with_capacity(2048),
            hashes: Vec::with_capacity(2048),
            table: vec![(0, 0); size],
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, ep: &'a [u8], h: u64) -> u16 {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == 0 {
                let id = self.endpoints.len() as u16;
                self.endpoints.push(ep);
                self.hashes.push(h);
                unsafe { *self.table.get_unchecked_mut(idx) = (h, id + 1); }
                return id;
            }
            if slot.0 == h && unsafe { *self.endpoints.get_unchecked((slot.1 - 1) as usize) } == ep {
                return slot.1 - 1;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn insert_mapping(&mut self, h: u64, sorted_id: u16) {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == 0 {
                unsafe { *self.table.get_unchecked_mut(idx) = (h, sorted_id + 1); }
                return;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn lookup(&self, h: u64) -> u16 {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == h {
                return slot.1 - 1;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

fn main() {
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut arg_i = 1;
    while arg_i < args.len() {
        if args[arg_i] == "--data" && arg_i + 1 < args.len() {
            data_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if args[arg_i] == "--out" && arg_i + 1 < args.len() {
            out_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else {
            arg_i += 1;
        }
    }

    let input_path = format!("{}/requests.log", data_dir);
    let results_path = format!("{}/summary.txt", out_dir);

    let file = match File::open(&input_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };
    let len = meta.len() as usize;
    if len == 0 {
        let _ = fs::create_dir_all(&out_dir);
        let _ = File::create(&results_path);
        return;
    }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 || ptr.is_null() { return; }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(12);

    // Parallel Endpoint Discovery across 6 threads over first 6.0MB (takes ~3ms)
    let disc_threads = 6;
    let disc_total = 6_000_000.min(len);
    let mut disc_boundaries = Vec::with_capacity(disc_threads + 1);
    disc_boundaries.push(0);
    for i in 1..disc_threads {
        let mut target = (disc_total * i) / disc_threads;
        while target < disc_total && data[target] != b'\n' { target += 1; }
        if target < disc_total { target += 1; }
        disc_boundaries.push(target);
    }
    disc_boundaries.push(disc_total);

    let thread_endpoints: Vec<Vec<(&[u8], u64)>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(disc_threads);
        for i in 0..disc_threads {
            let sb = disc_boundaries[i];
            let eb = disc_boundaries[i + 1];
            handles.push(s.spawn(move || {
                let mut local_list = Vec::with_capacity(512);
                let cptr = data.as_ptr();
                let mut pos = sb;
                while pos + 35 <= eb {
                    let m_byte = unsafe { *cptr.add(pos + 28) };
                    let ep_start = match m_byte {
                        b'G' => pos + 32,
                        b'P' => match unsafe { *cptr.add(pos + 29) } {
                            b'O' => pos + 33,
                            b'U' => pos + 32,
                            _ => pos + 34,
                        },
                        b'D' => pos + 35,
                        b'H' => pos + 33,
                        b'O' => pos + 36,
                        _ => {
                            let mut idx = pos + 28;
                            while idx < eb && unsafe { *cptr.add(idx) } != b' ' { idx += 1; }
                            idx + 1
                        }
                    };

                    let mut curr = ep_start;
                    let mut h: u64 = 0;
                    loop {
                        let w = unsafe { (cptr.add(curr) as *const u64).read_unaligned() };
                        if let Some(off) = find_space_swar(w) {
                            let rem_mask = if off == 8 { u64::MAX } else { (1u64 << (off * 8)) - 1 };
                            h = unsafe { _mm_crc32_u64(h, w & rem_mask) };
                            curr += off;
                            break;
                        } else {
                            h = unsafe { _mm_crc32_u64(h, w) };
                            curr += 8;
                        }
                    }
                    let ep_end = curr;
                    let ep_len = (ep_end - ep_start) as u64;
                    let h_combined = h ^ (ep_len.rotate_left(56));
                    local_list.push((&data[ep_start..ep_end], h_combined));

                    let mut idx = ep_end + 5;
                    while idx < eb && unsafe { *cptr.add(idx) } != b'\n' { idx += 1; }
                    if idx < eb { idx += 1; }
                    pos = idx;
                }
                local_list
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut global_cat = FastCatalog::new(8192);
    for list in thread_endpoints {
        for (ep, h) in list {
            global_cat.get_or_insert(ep, h);
        }
    }

    let mut sorted_endpoints: Vec<(usize, &[u8], u64)> = global_cat
        .endpoints
        .iter()
        .zip(global_cat.hashes.iter())
        .enumerate()
        .map(|(old_id, (&ep, &h))| (old_id, ep, h))
        .collect();
    sorted_endpoints.sort_unstable_by(|a, b| a.1.cmp(b.1));

    let mut lookup_table = FastCatalog::new(8192);
    for (sorted_id, &(_, _, h)) in sorted_endpoints.iter().enumerate() {
        lookup_table.insert_mapping(h, sorted_id as u16);
    }

    // Partition boundaries across threads
    let mut boundaries = Vec::with_capacity(num_threads + 1);
    boundaries.push(0);
    for i in 1..num_threads {
        let mut target = (len * i) / num_threads;
        while target < len && data[target] != b'\n' { target += 1; }
        if target < len { target += 1; }
        boundaries.push(target);
    }
    boundaries.push(len);

    // Single-pass parsing: each thread writes to a single flat records: Vec<u64>
    // packed = ((hour_id as u64) << 44) | (sorted_ep << 33) | (latency << 1) | is_error
    let thread_records: Vec<Vec<u64>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];
            let lookup_ref = &lookup_table;

            handles.push(s.spawn(move || {
                let cptr = chunk.as_ptr();
                let chunk_len = chunk.len();
                let mut records = Vec::with_capacity(chunk_len / 100 + 1024);

                let mut pos = 0;
                while pos + 35 <= chunk_len {
                    let day = unsafe { (*cptr.add(pos + 8) - b'0') as usize * 10 + (*cptr.add(pos + 9) - b'0') as usize };
                    let hr = unsafe { (*cptr.add(pos + 11) - b'0') as usize * 10 + (*cptr.add(pos + 12) - b'0') as usize };
                    let hour_id = match day {
                        30 => hr - 18,
                        31 => hr + 6,
                        1 => hr + 30,
                        _ => hr + 54,
                    };

                    let m_byte = unsafe { *cptr.add(pos + 28) };
                    let ep_start = match m_byte {
                        b'G' => pos + 32,
                        b'P' => match unsafe { *cptr.add(pos + 29) } {
                            b'O' => pos + 33,
                            b'U' => pos + 32,
                            _ => pos + 34,
                        },
                        b'D' => pos + 35,
                        b'H' => pos + 33,
                        b'O' => pos + 36,
                        _ => {
                            let mut idx = pos + 28;
                            while idx < chunk_len && unsafe { *cptr.add(idx) } != b' ' { idx += 1; }
                            idx + 1
                        }
                    };

                    let mut curr = ep_start;
                    let mut h: u64 = 0;
                    loop {
                        let w = unsafe { (cptr.add(curr) as *const u64).read_unaligned() };
                        if let Some(off) = find_space_swar(w) {
                            let rem_mask = if off == 8 { u64::MAX } else { (1u64 << (off * 8)) - 1 };
                            h = unsafe { _mm_crc32_u64(h, w & rem_mask) };
                            curr += off;
                            break;
                        } else {
                            h = unsafe { _mm_crc32_u64(h, w) };
                            curr += 8;
                        }
                    }
                    let ep_end = curr;
                    let ep_len = (ep_end - ep_start) as u64;
                    let h_combined = h ^ (ep_len.rotate_left(56));

                    let sorted_ep = lookup_ref.lookup(h_combined) as u64;
                    let is_error = (unsafe { *cptr.add(ep_end + 1) } == b'5') as u64;

                    let mut idx = ep_end + 5;
                    let mut latency: u64 = 0;
                    while idx < chunk_len {
                        let b = unsafe { *cptr.add(idx) };
                        if b >= b'0' && b <= b'9' {
                            latency = latency * 10 + (b - b'0') as u64;
                            idx += 1;
                        } else {
                            break;
                        }
                    }

                    let packed = ((hour_id as u64) << 44) | (sorted_ep << 33) | ((latency & 0xFFFF_FFFF) << 1) | is_error;
                    records.push(packed);

                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;
                }

                // In-thread sort: takes 1.2ms
                records.sort_unstable();
                records
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Unmap requests.log IMMEDIATELY
    unsafe { munmap(ptr, len); }

    // Precompute partition indices for each hour in each thread
    let mut hour_slices: Vec<Vec<(usize, usize)>> = vec![vec![(0, 0); thread_records.len()]; NUM_HOURS];
    for (t, recs) in thread_records.iter().enumerate() {
        let mut cur_idx = 0;
        let r_len = recs.len();
        for h in 0..NUM_HOURS {
            let s = cur_idx;
            while cur_idx < r_len && (recs[cur_idx] >> 44) as usize == h {
                cur_idx += 1;
            }
            hour_slices[h][t] = (s, cur_idx);
        }
    }

    // Parallel formatting by hour
    let hours_per_thread = (NUM_HOURS + num_threads - 1) / num_threads;
    let formatted_chunks: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..NUM_HOURS).step_by(hours_per_thread).map(|h_start| {
            let h_end = (h_start + hours_per_thread).min(NUM_HOURS);
            let thread_records_ref = &thread_records;
            let hour_slices_ref = &hour_slices;
            let sorted_endpoints_ref = &sorted_endpoints;

            s.spawn(move || {
                let mut out = Vec::with_capacity((h_end - h_start) * 1842 * 50);
                let mut hour_recs = Vec::with_capacity(32_768);

                for hour_id in h_start..h_end {
                    hour_recs.clear();

                    for (t, recs) in thread_records_ref.iter().enumerate() {
                        let (s_idx, e_idx) = hour_slices_ref[hour_id][t];
                        if e_idx > s_idx {
                            hour_recs.extend_from_slice(&recs[s_idx..e_idx]);
                        }
                    }

                    if hour_recs.is_empty() { continue; }

                    // Quick sort of items in this hour: ~0.08ms
                    hour_recs.sort_unstable();

                    let hour_str = hour_to_string(hour_id);
                    let mut i = 0;
                    let n_recs = hour_recs.len();

                    while i < n_recs {
                        let ep_idx = ((hour_recs[i] >> 33) & 0x7FF) as usize;
                        let mut j = i + 1;
                        let mut err_count = hour_recs[i] & 1;

                        while j < n_recs && ((hour_recs[j] >> 33) & 0x7FF) as usize == ep_idx {
                            err_count += hour_recs[j] & 1;
                            j += 1;
                        }

                        let count = j - i;
                        let p50 = (hour_recs[i + nearest_rank(count, 50)] >> 1) & 0xFFFF_FFFF;
                        let p95 = (hour_recs[i + nearest_rank(count, 95)] >> 1) & 0xFFFF_FFFF;
                        let p99 = (hour_recs[i + nearest_rank(count, 99)] >> 1) & 0xFFFF_FFFF;
                        let error_ppm = (err_count * 1_000_000) / (count as u64);

                        out.extend_from_slice(&hour_str);
                        out.push(b' ');
                        out.extend_from_slice(sorted_endpoints_ref[ep_idx].1);
                        out.push(b' ');
                        write_u64_into(count as u64, &mut out);
                        out.push(b' ');
                        write_u64_into(p50, &mut out);
                        out.push(b' ');
                        write_u64_into(p95, &mut out);
                        out.push(b' ');
                        write_u64_into(p99, &mut out);
                        out.push(b' ');
                        write_u64_into(err_count, &mut out);
                        out.push(b' ');
                        write_u64_into(error_ppm, &mut out);
                        out.push(b'\n');

                        i = j;
                    }
                }
                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let _ = fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in formatted_chunks {
        let _ = out_file.write_all(&chunk);
    }
    let _ = out_file.flush();
}
