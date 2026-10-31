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
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
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

#[inline(always)]
unsafe fn hash_endpoint(cptr: *const u8, ep_start: usize) -> (usize, u64) {
    let mut curr = ep_start;
    let mut crc = 0u64;
    loop {
        let w = (cptr.add(curr) as *const u64).read_unaligned();
        if let Some(off) = find_space_swar(w) {
            let rem_mask = if off == 8 { u64::MAX } else { (1u64 << (off * 8)) - 1 };
            crc = _mm_crc32_u64(crc, w & rem_mask);
            curr += off;
            break;
        } else {
            crc = _mm_crc32_u64(crc, w);
            curr += 8;
        }
    }
    let ep_len = (curr - ep_start) as u64;
    (curr, _mm_crc32_u64(crc, ep_len))
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
            if slot.0 == h {
                return slot.1;
            }
            if slot.0 == 0 {
                let id = self.endpoints.len() as u16;
                self.endpoints.push(ep);
                self.hashes.push(h);
                unsafe {
                    *self.table.get_unchecked_mut(idx) = (h, id);
                }
                return id;
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
                return slot.1;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    fn insert_mapping(&mut self, h: u64, sorted_id: u16) {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == 0 || slot.0 == h {
                unsafe {
                    *self.table.get_unchecked_mut(idx) = (h, sorted_id);
                }
                return;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

fn main() {
    unsafe {
        mallopt(-8, 2);
    }

    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() {
            data_dir = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--out" && i + 1 < args.len() {
            out_dir = args[i + 1].clone();
            i += 2;
        } else {
            i += 1;
        }
    }

    let logs_path = format!("{}/requests.log", data_dir);
    let file = match File::open(&logs_path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let len = match file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    if len == 0 {
        return;
    }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(8);
    let mut boundaries = Vec::with_capacity(num_threads + 1);
    boundaries.push(0);
    for i in 1..num_threads {
        let mut pos = (len * i) / num_threads;
        while pos < len && data[pos] != b'\n' { pos += 1; }
        if pos < len { pos += 1; }
        boundaries.push(pos.min(len));
    }
    boundaries.push(len);

    // Micro-scan endpoint discovery on first 5.8MB
    let mut discovery_len = 5_800_000.min(len);
    while discovery_len < len && data[discovery_len] != b'\n' { discovery_len += 1; }
    if discovery_len < len { discovery_len += 1; }

    let mut global_cat = FastCatalog::new(8192);
    {
        let cptr = data.as_ptr();
        let chunk_len = discovery_len;
        let mut pos = 0;

        while pos + 35 <= chunk_len {
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

            let (ep_end, h_combined) = unsafe { hash_endpoint(cptr, ep_start) };
            global_cat.get_or_insert(&data[ep_start..ep_end], h_combined);

            let mut idx = ep_end + 5;
            while idx < chunk_len && unsafe { *cptr.add(idx) } != b'\n' { idx += 1; }
            if idx < chunk_len { idx += 1; }
            pos = idx;
        }
    }

    let num_endpoints = global_cat.endpoints.len();
    let mut sorted_endpoints: Vec<(usize, Vec<u8>, u64)> = global_cat
        .endpoints
        .iter()
        .zip(global_cat.hashes.iter())
        .enumerate()
        .map(|(old_id, (&ep, &h))| (old_id, ep.to_vec(), h))
        .collect();
    sorted_endpoints.sort_unstable_by(|a, b| a.1.cmp(&b.1));

    let mut lookup_table = FastCatalog::new(8192);
    for (sorted_id, &(_, _, h)) in sorted_endpoints.iter().enumerate() {
        lookup_table.insert_mapping(h, sorted_id as u16);
    }

    let num_buckets = NUM_HOURS * num_endpoints;

    // ONE-PASS: Parse records directly, stream-madvise, collect thread records (24MB total)
    type ThreadResult = (Vec<u64>, Vec<u32>, Vec<u32>); // (packed_records, counts, errors)
    let thread_results: Vec<ThreadResult> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];
            let lookup_ref = &lookup_table;

            handles.push(s.spawn(move || {
                let mut local_records = Vec::with_capacity(400_000);
                let mut local_counts = vec![0u32; num_buckets];
                let mut local_errors = vec![0u32; num_buckets];
                let cptr = chunk.as_ptr();
                let chunk_len = chunk.len();
                let mut pos = 0;
                let mut last_madv_pos = 0;

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

                    let (ep_end, h_combined) = unsafe { hash_endpoint(cptr, ep_start) };
                    let sorted_ep = lookup_ref.lookup(h_combined) as usize;
                    let g_id = hour_id * num_endpoints + sorted_ep;

                    let is_error = if unsafe { *cptr.add(ep_end + 1) } == b'5' { 1u32 } else { 0u32 };
                    local_errors[g_id] += is_error;
                    local_counts[g_id] += 1;

                    let mut idx = ep_end + 5;
                    let mut latency: u32 = 0;
                    while idx < chunk_len {
                        let b = unsafe { *cptr.add(idx) };
                        if b >= b'0' && b <= b'9' {
                            latency = latency * 10 + (b - b'0') as u32;
                            idx += 1;
                        } else {
                            break;
                        }
                    }

                    local_records.push(((g_id as u64) << 32) | (latency as u64));

                    while idx < chunk_len && unsafe { *cptr.add(idx) } != b'\n' { idx += 1; }
                    if idx < chunk_len { idx += 1; }
                    pos = idx;

                    if pos - last_madv_pos >= 2 * 1024 * 1024 {
                        let m_len = (pos - last_madv_pos) & !4095;
                        unsafe {
                            madvise(cptr.add(last_madv_pos) as *mut _, m_len, 4);
                        }
                        last_madv_pos += m_len;
                    }
                }

                unsafe {
                    if chunk_len > last_madv_pos {
                        madvise(cptr.add(last_madv_pos) as *mut _, chunk_len - last_madv_pos, 4);
                    }
                }

                (local_records, local_counts, local_errors)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe { munmap(ptr, len); }

    // Consolidate bucket offsets, total counts, and total errors
    let mut bucket_offsets = vec![0u32; num_buckets + 1];
    let mut total_errors = vec![0u32; num_buckets];
    let mut thread_write_ptrs: Vec<Vec<u32>> = (0..num_threads)
        .map(|_| vec![0u32; num_buckets])
        .collect();

    let mut total_records = 0u32;
    for g_id in 0..num_buckets {
        bucket_offsets[g_id] = total_records;
        let mut cur = total_records;
        for t in 0..num_threads {
            thread_write_ptrs[t][g_id] = cur;
            let cnt = thread_results[t].1[g_id];
            cur += cnt;
            total_errors[g_id] += thread_results[t].2[g_id];
        }
        total_records = cur;
    }
    bucket_offsets[num_buckets] = total_records;

    // Scatter into contiguous all_latencies (12.4 MB)
    let mut all_latencies = vec![0u32; total_records as usize];
    let raw_lats_ptr = all_latencies.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for (t, res) in thread_results.into_iter().enumerate() {
            let mut write_ptrs = std::mem::take(&mut thread_write_ptrs[t]);
            s.spawn(move || {
                let raw_lats = raw_lats_ptr as *mut u32;
                for &rec in &res.0 {
                    let g_id = (rec >> 32) as usize;
                    let latency = rec as u32;
                    let dest = write_ptrs[g_id];
                    write_ptrs[g_id] += 1;
                    unsafe { *raw_lats.add(dest as usize) = latency; }
                }
            });
        }
    });

    // Parallel formatting & percentiles by hour
    let num_format_threads = num_threads.min(NUM_HOURS);
    let chunk_hours = (NUM_HOURS + num_format_threads - 1) / num_format_threads;

    let hour_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..NUM_HOURS).step_by(chunk_hours).map(|h_start| {
            let h_end = (h_start + chunk_hours).min(NUM_HOURS);
            let sorted_endpoints_ref = &sorted_endpoints;
            let bucket_offsets_ref = &bucket_offsets;
            let total_errors_ref = &total_errors;
            let all_latencies_ref = &all_latencies;

            s.spawn(move || {
                let mut out = Vec::with_capacity((h_end - h_start) * num_endpoints * 128);

                for h in h_start..h_end {
                    let ts_bytes = hour_to_string(h);

                    for (ep_idx, item) in sorted_endpoints_ref.iter().enumerate() {
                        let g_id = h * num_endpoints + ep_idx;
                        let count = (bucket_offsets_ref[g_id + 1] - bucket_offsets_ref[g_id]) as u64;
                        if count == 0 {
                            continue;
                        }

                        let errors = total_errors_ref[g_id] as u64;
                        let start_idx = bucket_offsets_ref[g_id] as usize;
                        let end_idx = bucket_offsets_ref[g_id + 1] as usize;
                        let slice = unsafe {
                            let ptr = all_latencies_ref.as_ptr().add(start_idx) as *mut u32;
                            std::slice::from_raw_parts_mut(ptr, end_idx - start_idx)
                        };
                        slice.sort_unstable();

                        let n = count as usize;
                        let p50 = slice[nearest_rank(n, 50)];
                        let p95 = slice[nearest_rank(n, 95)];
                        let p99 = slice[nearest_rank(n, 99)];

                        let error_ppm = (errors * 1_000_000) / count;

                        out.extend_from_slice(&ts_bytes);
                        out.push(b' ');
                        out.extend_from_slice(&item.1);
                        out.push(b' ');
                        write_u64_into(count, &mut out);
                        out.push(b' ');
                        write_u64_into(p50 as u64, &mut out);
                        out.push(b' ');
                        write_u64_into(p95 as u64, &mut out);
                        out.push(b' ');
                        write_u64_into(p99 as u64, &mut out);
                        out.push(b' ');
                        write_u64_into(errors, &mut out);
                        out.push(b' ');
                        write_u64_into(error_ppm, &mut out);
                        out.push(b'\n');
                    }
                }
                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let _ = fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(format!("{}/summary.txt", out_dir)) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in &hour_outputs {
        let _ = out_file.write_all(chunk);
    }
}
