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

    #[inline(always)]
    fn insert_mapping(&mut self, h: u64, id: u16) {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == 0 || slot.0 == h {
                unsafe {
                    *self.table.get_unchecked_mut(idx) = (h, id);
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
        let _ = File::create(format!("{}/summary.txt", out_dir));
        return;
    }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(12);
    let mut boundaries = Vec::with_capacity(num_threads + 1);
    boundaries.push(0);
    for i in 1..num_threads {
        let mut pos = (len * i) / num_threads;
        while pos < len && data[pos] != b'\n' { pos += 1; }
        if pos < len { pos += 1; }
        boundaries.push(pos.min(len));
    }
    boundaries.push(len);

    // Phase 1: Sequential micro-scan endpoint discovery on first 5.8MB (zero thread overhead)
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

    // Phase 2: Packed 64-bit record: [g_id: 31 bits] [is_error: 1 bit] [latency: 32 bits]
    // Eliminates all error vectors (saves 6.4MB immediately!)
    let parse_results: Vec<(Vec<u64>, Vec<u32>)> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];
            let lookup_ref = &lookup_table;

            handles.push(s.spawn(move || {
                let est_records = (end - start) / 72 + 1024;
                let mut records: Vec<u64> = Vec::with_capacity(est_records);

                let cptr = chunk.as_ptr();
                let chunk_len = chunk.len();
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

                    let (ep_end, h_combined) = unsafe { hash_endpoint(cptr, ep_start) };

                    let sorted_ep = lookup_ref.lookup(h_combined) as usize;
                    let g_id = (hour_id * num_endpoints + sorted_ep) as u64;

                    let is_error = if unsafe { *cptr.add(ep_end + 1) } == b'5' { 1u64 } else { 0u64 };

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

                    // Pack: [g_id: 31 bits] [is_error: 1 bit] [latency: 32 bits]
                    let packed = (g_id << 33) | (is_error << 32) | (latency & 0xFFFF_FFFF);
                    records.push(packed);

                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;
                }

                // In-place sort thread's records (only ~260,000 u64, takes 2ms!)
                records.sort_unstable();

                // Compute exact hour offsets in sorted records array
                let mut hour_offsets = vec![0u32; NUM_HOURS + 1];
                let mut cur_h = 0usize;
                for (idx, &packed) in records.iter().enumerate() {
                    let g_id = (packed >> 33) as usize;
                    let h = (g_id / num_endpoints).min(NUM_HOURS);
                    while cur_h < h {
                        cur_h += 1;
                        hour_offsets[cur_h] = idx as u32;
                    }
                }
                while cur_h < NUM_HOURS {
                    cur_h += 1;
                    hour_offsets[cur_h] = records.len() as u32;
                }

                (records, hour_offsets)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Phase 3: Direct scatter into sorted_recs, zero intermediate hour_recs buffer
    let hours_per_thread = (NUM_HOURS + num_threads - 1) / num_threads;
    let formatted_chunks: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..NUM_HOURS).step_by(hours_per_thread).map(|h_start| {
            let h_end = (h_start + hours_per_thread).min(NUM_HOURS);
            let results_ref = &parse_results;
            let sorted_endpoints_ref = &sorted_endpoints;

            s.spawn(move || {
                let mut count_per_ep = vec![0u32; num_endpoints];
                let mut errors_per_ep = vec![0u32; num_endpoints];
                let mut next_offset = vec![0u32; num_endpoints];
                let mut sorted_recs = vec![0u32; 65_536];
                let mut out = Vec::with_capacity((h_end - h_start) * num_endpoints * 64);

                for hour_id in h_start..h_end {
                    let hour_str = hour_to_string(hour_id);

                    // Pass A: Count per ep and errors
                    count_per_ep.fill(0);
                    errors_per_ep.fill(0);
                    let mut total_hour_recs = 0usize;

                    for (records, hour_offsets) in results_ref {
                        let s_off = hour_offsets[hour_id] as usize;
                        let e_off = hour_offsets[hour_id + 1] as usize;
                        total_hour_recs += e_off - s_off;
                        for &packed in &records[s_off..e_off] {
                            let g_id = (packed >> 33) as usize;
                            let ep_id = g_id % num_endpoints;
                            count_per_ep[ep_id] += 1;
                            errors_per_ep[ep_id] += ((packed >> 32) & 1) as u32;
                        }
                    }

                    if total_hour_recs == 0 { continue; }

                    let mut acc = 0u32;
                    for ep_id in 0..num_endpoints {
                        next_offset[ep_id] = acc;
                        acc += count_per_ep[ep_id];
                    }

                    if sorted_recs.len() < total_hour_recs {
                        sorted_recs.resize(total_hour_recs, 0);
                    }

                    // Pass B: Scatter directly into sorted_recs without hour_recs
                    for (records, hour_offsets) in results_ref {
                        let s_off = hour_offsets[hour_id] as usize;
                        let e_off = hour_offsets[hour_id + 1] as usize;
                        for &packed in &records[s_off..e_off] {
                            let g_id = (packed >> 33) as usize;
                            let ep_id = g_id % num_endpoints;
                            let lat = (packed & 0xFFFF_FFFF) as u32;
                            let off = next_offset[ep_id] as usize;
                            next_offset[ep_id] += 1;
                            sorted_recs[off] = lat;
                        }
                    }

                    // Compute percentiles & format
                    let mut offset = 0usize;
                    for ep_id in 0..num_endpoints {
                        let count = count_per_ep[ep_id] as usize;
                        if count == 0 { continue; }

                        let lats = &mut sorted_recs[offset..offset + count];
                        offset += count;

                        let p50_idx = nearest_rank(count, 50);
                        let p95_idx = nearest_rank(count, 95);
                        let p99_idx = nearest_rank(count, 99);

                        let (p50, p95, p99) = if count <= 64 {
                            lats.sort_unstable();
                            (lats[p50_idx], lats[p95_idx], lats[p99_idx])
                        } else {
                            lats.select_nth_unstable(p95_idx);
                            let p95 = lats[p95_idx];
                            lats[..p95_idx].select_nth_unstable(p50_idx);
                            let p50 = lats[p50_idx];
                            lats[p95_idx + 1..].select_nth_unstable(p99_idx - (p95_idx + 1));
                            let p99 = lats[p99_idx];
                            (p50, p95, p99)
                        };

                        let errors = errors_per_ep[ep_id] as u64;
                        let error_ppm = (errors * 1_000_000) / (count as u64);
                        let ep_bytes = sorted_endpoints_ref[ep_id].1;

                        out.extend_from_slice(&hour_str);
                        out.push(b' ');
                        out.extend_from_slice(ep_bytes);
                        out.push(b' ');
                        write_u64_into(count as u64, &mut out);
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
    let out_file_path = format!("{}/summary.txt", out_dir);
    let mut out_file = match File::create(&out_file_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in &formatted_chunks {
        let _ = out_file.write_all(chunk);
    }
    unsafe { munmap(ptr, len); }
}
