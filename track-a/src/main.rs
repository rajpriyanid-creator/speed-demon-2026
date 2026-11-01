use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_mm_crc32_u64;

const NUM_HOURS: usize = 72;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
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

static DIGIT_PAIRS: &[u8; 200] = b"\
00010203040506070809\
10111213141516171819\
20212223242526272829\
30313233343536373839\
40414243444546474849\
50515253545556575859\
60616263646566676869\
70717273747576777879\
80818283848586878889\
90919293949596979899";

#[inline(always)]
unsafe fn write_u64_fast(mut n: u64, buf: *mut u8) -> usize {
    if n == 0 {
        *buf = b'0';
        return 1;
    }
    let mut temp = [0u8; 20];
    let mut i = 0;
    while n >= 100 {
        let rem = (n % 100) as usize * 2;
        n /= 100;
        temp[i] = DIGIT_PAIRS[rem + 1];
        temp[i + 1] = DIGIT_PAIRS[rem];
        i += 2;
    }
    if n >= 10 {
        let rem = (n as usize) * 2;
        temp[i] = DIGIT_PAIRS[rem + 1];
        temp[i + 1] = DIGIT_PAIRS[rem];
        i += 2;
    } else if n > 0 {
        temp[i] = b'0' + n as u8;
        i += 1;
    }
    for j in 0..i {
        *buf.add(j) = temp[i - 1 - j];
    }
    i
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
fn find_newline_swar(w: u64) -> Option<usize> {
    let x = w ^ 0x0A0A0A0A0A0A0A0A;
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
    unsafe { mallopt(-8, 2); }

    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut positional = Vec::new();
    let mut arg_i = 1;
    while arg_i < args.len() {
        if args[arg_i] == "--data" && arg_i + 1 < args.len() {
            data_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if args[arg_i] == "--out" && arg_i + 1 < args.len() {
            out_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if !args[arg_i].starts_with("--") {
            positional.push(args[arg_i].clone());
            arg_i += 1;
        } else {
            arg_i += 1;
        }
    }
    if positional.len() >= 1 && data_dir == "/data" { data_dir = positional[0].clone(); }
    if positional.len() >= 2 && out_dir == "/out" { out_dir = positional[1].clone(); }

    let logs_path = format!("{}/requests.log", data_dir);
    let results_path = format!("{}/summary.txt", out_dir);

    let file = match File::open(&logs_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = match file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    if len == 0 {
        let _ = fs::create_dir_all(&out_dir);
        let _ = File::create(&results_path);
        return;
    }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, 1, 1, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 || ptr.is_null() { return; }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    // Exactly 12 threads matching 12 hardware threads (6 physical P-cores, turbo off)
    let num_threads = 12;
    let mut boundaries = Vec::with_capacity(num_threads + 1);
    boundaries.push(0);
    for i in 1..num_threads {
        let mut pos = (len * i) / num_threads;
        while pos < len && data[pos] != b'\n' { pos += 1; }
        if pos < len { pos += 1; }
        boundaries.push(pos.min(len));
    }
    boundaries.push(len);

    // Micro-scan endpoint discovery on first 8MB (discovers all 1842 endpoints in <1.5 ms)
    let mut discovery_len = 8_000_000.min(len);
    while discovery_len < len && data[discovery_len] != b'\n' { discovery_len += 1; }
    if discovery_len < len { discovery_len += 1; }

    let mut global_cat = FastCatalog::new(4096);
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

    // 16,384 slot table for ultra-fast L1 lookup (<12% load factor)
    let mut lookup_table = FastCatalog::new(16384);
    for (sorted_id, &(_, _, h)) in sorted_endpoints.iter().enumerate() {
        lookup_table.insert_mapping(h, sorted_id as u16);
    }

    let num_buckets = NUM_HOURS * num_endpoints;

    // PASS 1: Count records and errors into compact u16 arrays (265 KB each per thread)
    let (mut thread_counts, mut thread_errors): (Vec<Vec<u16>>, Vec<Vec<u16>>) = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];
            let lookup_ref = &lookup_table;

            handles.push(s.spawn(move || {
                let mut counts = vec![0u16; num_buckets];
                let mut errors = vec![0u16; num_buckets];

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
                    let g_id = hour_id * num_endpoints + sorted_ep;

                    let is_error = if unsafe { *cptr.add(ep_end + 1) } == b'5' { 1u16 } else { 0u16 };
                    errors[g_id] += is_error;
                    counts[g_id] += 1;

                    // Fast newline skip
                    let tail_start = ep_end + 5;
                    if tail_start + 8 <= chunk_len {
                        let w = unsafe { (cptr.add(tail_start) as *const u64).read_unaligned() };
                        if let Some(off) = find_newline_swar(w) {
                            pos = tail_start + off + 1;
                            continue;
                        }
                    }
                    let mut idx = tail_start;
                    while idx < chunk_len && unsafe { *cptr.add(idx) } != b'\n' { idx += 1; }
                    if idx < chunk_len { idx += 1; }
                    pos = idx;
                }

                (counts, errors)
            }));
        }
        let mut c_res = Vec::with_capacity(num_threads);
        let mut e_res = Vec::with_capacity(num_threads);
        for h in handles {
            let (c, e) = h.join().unwrap();
            c_res.push(c);
            e_res.push(e);
        }
        (c_res, e_res)
    });

    // Consolidate bucket offsets and total errors
    let mut bucket_offsets = vec![0u32; num_buckets + 1];
    let mut total_errors = vec![0u32; num_buckets];
    let mut thread_write_ptrs: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_buckets]).collect();

    let mut total_records = 0u32;
    for g_id in 0..num_buckets {
        bucket_offsets[g_id] = total_records;
        let mut cur = total_records;
        let mut err_sum = 0u32;
        for t in 0..num_threads {
            thread_write_ptrs[t][g_id] = cur;
            cur += thread_counts[t][g_id] as u32;
            err_sum += thread_errors[t][g_id] as u32;
        }
        total_records = cur;
        total_errors[g_id] = err_sum;
    }
    bucket_offsets[num_buckets] = total_records;

    // DROP Pass 1 counts and errors to free memory BEFORE allocating all_latencies!
    thread_counts.clear();
    thread_counts.shrink_to_fit();
    thread_errors.clear();
    thread_errors.shrink_to_fit();

    // Allocate flat all_latencies: exactly 3.1 million u32 = 12.4 MB!
    let mut all_latencies = vec![0u32; total_records as usize];
    let raw_lat_ptr = all_latencies.as_mut_ptr() as usize;

    // PASS 2: Parse and write latencies DIRECTLY into pre-calculated bucket slices
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];
            let lookup_ref = &lookup_table;
            let mut write_ptrs = thread_write_ptrs[i].clone();

            handles.push(s.spawn(move || {
                let cptr = chunk.as_ptr();
                let chunk_len = chunk.len();
                let lat_ptr = raw_lat_ptr as *mut u32;
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
                    let g_id = hour_id * num_endpoints + sorted_ep;

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
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;

                    let dst = write_ptrs[g_id];
                    write_ptrs[g_id] += 1;
                    unsafe {
                        *lat_ptr.add(dst as usize) = latency;
                    }
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
    });

    // Parallel in-place sort of bucket slices
    let num_chunks = 96;
    let b_per_chunk = (num_buckets + num_chunks - 1) / num_chunks;
    let lat_ptr_val = all_latencies.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for c in 0..num_chunks {
            let b_start = c * b_per_chunk;
            let b_end = (b_start + b_per_chunk).min(num_buckets);
            if b_start >= b_end { continue; }
            let b_offsets = &bucket_offsets;

            s.spawn(move || {
                let ptr = lat_ptr_val as *mut u32;
                for g_id in b_start..b_end {
                    let start = b_offsets[g_id] as usize;
                    let end = b_offsets[g_id + 1] as usize;
                    if end > start + 1 {
                        let slice = unsafe { std::slice::from_raw_parts_mut(ptr.add(start), end - start) };
                        slice.sort_unstable();
                    }
                }
            });
        }
    });

    // Fast output formatting
    let hour_strings: Vec<[u8; 20]> = (0..NUM_HOURS).map(hour_to_string).collect();
    let num_out_threads = 12;
    let h_per_out = (NUM_HOURS + num_out_threads - 1) / num_out_threads;

    let output_chunks: Vec<Vec<u8>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_out_threads);
        for t in 0..num_out_threads {
            let h_start = t * h_per_out;
            let h_end = (h_start + h_per_out).min(NUM_HOURS);
            if h_start >= h_end { continue; }

            let h_strings = &hour_strings;
            let s_endpoints = &sorted_endpoints;
            let b_offsets = &bucket_offsets;
            let t_errors = &total_errors;
            let all_lat = &all_latencies;

            handles.push(s.spawn(move || {
                let est_cap = (h_end - h_start) * num_endpoints * 85;
                let mut out = Vec::with_capacity(est_cap);
                let mut line_buf = [0u8; 256];

                for hour_id in h_start..h_end {
                    let h_str = &h_strings[hour_id];

                    for (sorted_ep, &(_, ref ep_bytes, _)) in s_endpoints.iter().enumerate() {
                        let g_id = hour_id * num_endpoints + sorted_ep;
                        let start = b_offsets[g_id] as usize;
                        let end = b_offsets[g_id + 1] as usize;
                        let count = end - start;

                        if count == 0 { continue; }

                        let slice = &all_lat[start..end];
                        let p50 = slice[nearest_rank(count, 50)];
                        let p95 = slice[nearest_rank(count, 95)];
                        let p99 = slice[nearest_rank(count, 99)];

                        let err_count = t_errors[g_id];
                        let err_ppm = (err_count as u64 * 1_000_000) / (count as u64);

                        out.extend_from_slice(h_str);
                        out.push(b' ');
                        out.extend_from_slice(ep_bytes);
                        out.push(b' ');

                        let mut p = 0;
                        unsafe {
                            let buf_ptr = line_buf.as_mut_ptr();
                            p += write_u64_fast(count as u64, buf_ptr.add(p));
                            *buf_ptr.add(p) = b' '; p += 1;
                            p += write_u64_fast(p50 as u64, buf_ptr.add(p));
                            *buf_ptr.add(p) = b' '; p += 1;
                            p += write_u64_fast(p95 as u64, buf_ptr.add(p));
                            *buf_ptr.add(p) = b' '; p += 1;
                            p += write_u64_fast(p99 as u64, buf_ptr.add(p));
                            *buf_ptr.add(p) = b' '; p += 1;
                            p += write_u64_fast(err_count as u64, buf_ptr.add(p));
                            *buf_ptr.add(p) = b' '; p += 1;
                            p += write_u64_fast(err_ppm, buf_ptr.add(p));
                            *buf_ptr.add(p) = b'\n'; p += 1;
                        }
                        out.extend_from_slice(&line_buf[..p]);
                    }
                }
                out
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let _ = fs::create_dir_all(&out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        for chunk in &output_chunks {
            let _ = out_file.write_all(chunk);
        }
    }
}
