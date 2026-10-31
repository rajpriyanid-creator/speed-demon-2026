use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

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

    // Phase 1: Micro-scan endpoint discovery (parallel)
    let thread_catalogs = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];

            handles.push(s.spawn(move || {
                let mut cat = FastCatalog::new(4096);
                let cptr = chunk.as_ptr();
                let chunk_len = chunk.len();
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

                    let mut curr = ep_start;
                    let mut h: u64 = 0xcbf29ce484222325;
                    loop {
                        let w = unsafe { (cptr.add(curr) as *const u64).read_unaligned() };
                        if let Some(off) = find_space_swar(w) {
                            let rem_mask = if off == 8 { u64::MAX } else { (1u64 << (off * 8)) - 1 };
                            h = (h ^ (w & rem_mask)).wrapping_mul(0x100000001b3);
                            curr += off;
                            break;
                        } else {
                            h = (h ^ w).wrapping_mul(0x100000001b3);
                            curr += 8;
                        }
                    }
                    let ep_end = curr;
                    let ep_len = (ep_end - ep_start) as u64;
                    let h_combined = h ^ (ep_len.rotate_left(56));
                    cat.get_or_insert(&chunk[ep_start..ep_end], h_combined);

                    // Skip to next line
                    let mut idx = ep_end + 5;
                    while idx < chunk_len && unsafe { *cptr.add(idx) } != b'\n' { idx += 1; }
                    if idx < chunk_len { idx += 1; }
                    pos = idx;
                }
                cat
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    });

    // Unify endpoints
    let mut global_cat = FastCatalog::new(4096);
    for cat in &thread_catalogs {
        for (&ep, &h) in cat.endpoints.iter().zip(cat.hashes.iter()) {
            global_cat.get_or_insert(ep, h);
        }
    }
    drop(thread_catalogs);

    let num_endpoints = global_cat.endpoints.len();
    let mut sorted_endpoints: Vec<(usize, &[u8], u64)> = global_cat
        .endpoints
        .iter()
        .zip(global_cat.hashes.iter())
        .enumerate()
        .map(|(old_id, (&ep, &h))| (old_id, ep, h))
        .collect();
    sorted_endpoints.sort_unstable_by(|a, b| a.1.cmp(b.1));

    // Global direct lookup table: hash -> sorted_ep_id
    let mut lookup_table = FastCatalog::new(4096);
    for (sorted_id, &(_, _, h)) in sorted_endpoints.iter().enumerate() {
        lookup_table.insert_mapping(h, sorted_id as u16);
    }

    // Phase 2: Single-pass parse directly into packed u64 records
    let total_groups = NUM_HOURS * num_endpoints;

    let thread_records: Vec<(Vec<u64>, Vec<u32>)> = std::thread::scope(|s| {
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
                let mut errors = vec![0u32; total_groups];

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

                    let mut curr = ep_start;
                    let mut h: u64 = 0xcbf29ce484222325;
                    loop {
                        let w = unsafe { (cptr.add(curr) as *const u64).read_unaligned() };
                        if let Some(off) = find_space_swar(w) {
                            let rem_mask = if off == 8 { u64::MAX } else { (1u64 << (off * 8)) - 1 };
                            h = (h ^ (w & rem_mask)).wrapping_mul(0x100000001b3);
                            curr += off;
                            break;
                        } else {
                            h = (h ^ w).wrapping_mul(0x100000001b3);
                            curr += 8;
                        }
                    }
                    let ep_end = curr;
                    let ep_len = (ep_end - ep_start) as u64;
                    let h_combined = h ^ (ep_len.rotate_left(56));

                    let sorted_ep = lookup_ref.lookup(h_combined) as usize;
                    let g_id = hour_id * num_endpoints + sorted_ep;

                    let is_error = unsafe { *cptr.add(ep_end + 1) } == b'5';
                    if is_error {
                        unsafe { *errors.get_unchecked_mut(g_id) += 1; }
                    }

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

                    let packed = ((g_id as u64) << 32) | (latency as u64);
                    records.push(packed);

                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;
                }

                records.sort_unstable();
                (records, errors)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    });

    // Merge group errors
    let mut group_errors = vec![0u32; total_groups];
    for (_, errs) in &thread_records {
        for g in 0..total_groups {
            group_errors[g] += errs[g];
        }
    }

    // Merge group slices and format
    let hours_per_chunk = (NUM_HOURS + num_threads - 1) / num_threads;
    let formatted_chunks: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..NUM_HOURS).step_by(hours_per_chunk).map(|h_start| {
            let h_end = (h_start + hours_per_chunk).min(NUM_HOURS);
            let thread_records_ref = &thread_records;
            let group_errors_ref = &group_errors;
            let sorted_endpoints_ref = &sorted_endpoints;

            s.spawn(move || {
                let mut out = Vec::with_capacity((h_end - h_start) * num_endpoints * 48);
                let mut group_lats = Vec::with_capacity(1024);

                for hour_id in h_start..h_end {
                    let hour_str = hour_to_string(hour_id);
                    for sorted_ep in 0..num_endpoints {
                        let g_id = hour_id * num_endpoints + sorted_ep;
                        let g_prefix = (g_id as u64) << 32;
                        let g_next_prefix = ((g_id as u64) + 1) << 32;

                        group_lats.clear();
                        for (records, _) in thread_records_ref {
                            let s_idx = records.partition_point(|&x| x < g_prefix);
                            let e_idx = records[s_idx..].partition_point(|&x| x < g_next_prefix) + s_idx;
                            for &packed in &records[s_idx..e_idx] {
                                group_lats.push((packed & 0xFFFF_FFFF) as u32);
                            }
                        }

                        let count = group_lats.len();
                        if count == 0 { continue; }

                        let p50_idx = nearest_rank(count, 50);
                        let p95_idx = nearest_rank(count, 95);
                        let p99_idx = nearest_rank(count, 99);

                        let (p50, p95, p99) = if count <= 64 {
                            group_lats.sort_unstable();
                            (group_lats[p50_idx], group_lats[p95_idx], group_lats[p99_idx])
                        } else {
                            group_lats.select_nth_unstable(p95_idx);
                            let p95 = group_lats[p95_idx];
                            group_lats[..p95_idx].select_nth_unstable(p50_idx);
                            let p50 = group_lats[p50_idx];
                            group_lats[p95_idx + 1..].select_nth_unstable(p99_idx - (p95_idx + 1));
                            let p99 = group_lats[p99_idx];
                            (p50, p95, p99)
                        };

                        let errors = group_errors_ref[g_id] as u64;
                        let error_ppm = (errors * 1_000_000) / (count as u64);
                        let ep_bytes = sorted_endpoints_ref[sorted_ep].1;

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
    let mut out_file = File::create(&out_file_path).unwrap();
    for chunk in formatted_chunks {
        out_file.write_all(&chunk).unwrap();
    }
    out_file.flush().unwrap();
    unsafe { munmap(ptr, len); }
}
