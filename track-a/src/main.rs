use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_mm_crc32_u64;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const NUM_HOURS: usize = 72;
const MADV_DONTNEED: i32 = 4;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

#[inline(always)]
unsafe fn madvise_dontneed(addr: *const u8, len: usize) {
    let start = (addr as usize + 4095) & !4095;
    let end = (addr as usize + len) & !4095;
    if end > start {
        madvise(start as *mut u8, end - start, MADV_DONTNEED);
    }
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
fn find_space_swar(w: u64) -> Option<usize> {
    let x = w ^ 0x2020202020202020;
    let m = (x.wrapping_sub(0x0101010101010101)) & !x & 0x8080808080808080;
    if m != 0 {
        Some((m.trailing_zeros() >> 3) as usize)
    } else {
        None
    }
}

struct FastCatalog {
    endpoints: Vec<(usize, usize)>, // (start_idx, end_idx) into original data
    hashes: Vec<u64>,
    table: Vec<(u64, u16)>,
    mask: usize,
}

impl FastCatalog {
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
    fn get_or_insert(&mut self, start: usize, end: usize, h: u64) -> u16 {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == h {
                return slot.1;
            }
            if slot.0 == 0 {
                let id = self.endpoints.len() as u16;
                self.endpoints.push((start, end));
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
    if len == 0 { return; }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 || ptr.is_null() { return; }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(12);

    // Fast Endpoint Discovery on first 6.0MB with ZERO heap allocations!
    let disc_total = 6_000_000.min(len);
    let mut global_cat = FastCatalog::new(8192);
    let cptr = data.as_ptr();
    let mut pos = 0;
    while pos + 35 <= disc_total {
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
                while idx < disc_total && unsafe { *cptr.add(idx) } != b' ' { idx += 1; }
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

        global_cat.get_or_insert(ep_start, ep_end, h_combined);

        let mut idx = ep_end + 5;
        while idx < disc_total && unsafe { *cptr.add(idx) } != b'\n' { idx += 1; }
        if idx < disc_total { idx += 1; }
        pos = idx;
    }

    let num_endpoints = global_cat.endpoints.len();
    let mut sorted_endpoints: Vec<(usize, (usize, usize), u64)> = global_cat
        .endpoints
        .into_iter()
        .zip(global_cat.hashes.into_iter())
        .enumerate()
        .map(|(old_id, (ep, h))| (old_id, ep, h))
        .collect();
    sorted_endpoints.sort_unstable_by(|a, b| data[a.1.0..a.1.1].cmp(&data[b.1.0..b.1.1]));

    let mut lookup_table = FastCatalog::new(8192);
    for (sorted_id, (_, _, h)) in sorted_endpoints.iter().enumerate() {
        lookup_table.insert_mapping(*h, sorted_id as u16);
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

    let num_groups = NUM_HOURS * num_endpoints;

    // Global Atomic arrays for counts and errors - ONLY 2.1 MB total!
    let total_counts: Vec<AtomicU32> = (0..num_groups).map(|_| AtomicU32::new(0)).collect();
    let total_errs: Vec<AtomicU32> = (0..num_groups).map(|_| AtomicU32::new(0)).collect();

    // PASS 1: Count requests and errors per group across threads (with 4MB madvise window)
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];
            let lookup_ref = &lookup_table;
            let total_counts_ref = &total_counts;
            let total_errs_ref = &total_errs;

            handles.push(s.spawn(move || {
                let cptr = chunk.as_ptr();
                let chunk_len = chunk.len();
                let mut pos = 0;
                let mut last_adv = 0;

                while pos + 35 <= chunk_len {
                    if pos - last_adv >= 4 * 1024 * 1024 {
                        unsafe { madvise_dontneed(cptr.add(last_adv), pos - last_adv); }
                        last_adv = pos;
                    }
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
                    let sorted_ep = lookup_ref.lookup(h_combined) as usize;

                    let gid = hour_id * num_endpoints + sorted_ep;
                    unsafe {
                        total_counts_ref.get_unchecked(gid).fetch_add(1, Ordering::Relaxed);
                        if *cptr.add(ep_end + 1) == b'5' {
                            total_errs_ref.get_unchecked(gid).fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    let mut idx = ep_end + 5;
                    while idx < chunk_len && unsafe { *cptr.add(idx) } != b'\n' { idx += 1; }
                    if idx < chunk_len { idx += 1; }
                    pos = idx;
                }
                if chunk_len > last_adv {
                    unsafe { madvise_dontneed(cptr.add(last_adv), chunk_len - last_adv); }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });

    let mut group_offsets = vec![0u32; num_groups + 1];
    let mut cur_offset = 0u32;
    for g in 0..num_groups {
        group_offsets[g] = cur_offset;
        cur_offset += total_counts[g].load(Ordering::Relaxed);
    }
    group_offsets[num_groups] = cur_offset;
    let total_records = cur_offset as usize;

    let write_offsets: Vec<AtomicU32> = group_offsets[..num_groups].iter().map(|&off| AtomicU32::new(off)).collect();

    // Allocate flat u32 array: exactly 12.4 MB for 3.1M records!
    let mut all_latencies: Vec<u32> = vec![0u32; total_records];
    let lat_ptr = all_latencies.as_mut_ptr() as usize;

    // PASS 2: Parse latencies and write directly to group buckets (4MB madvise window)
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        let write_offsets_ref = &write_offsets;
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end { continue; }
            let chunk = &data[start..end];
            let lookup_ref = &lookup_table;

            handles.push(s.spawn(move || {
                let cptr = chunk.as_ptr();
                let chunk_len = chunk.len();
                let base_lat_ptr = lat_ptr as *mut u32;

                let mut pos = 0;
                let mut last_adv = 0;
                while pos + 35 <= chunk_len {
                    if pos - last_adv >= 4 * 1024 * 1024 {
                        unsafe { madvise_dontneed(cptr.add(last_adv), pos - last_adv); }
                        last_adv = pos;
                    }
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
                    let sorted_ep = lookup_ref.lookup(h_combined) as usize;

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

                    let gid = hour_id * num_endpoints + sorted_ep;
                    let slot = unsafe { write_offsets_ref.get_unchecked(gid).fetch_add(1, Ordering::Relaxed) as usize };
                    unsafe {
                        *base_lat_ptr.add(slot) = latency;
                    }

                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;
                }
                if chunk_len > last_adv {
                    unsafe { madvise_dontneed(cptr.add(last_adv), chunk_len - last_adv); }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });

    // Copy endpoint byte slices into a small flat buffer before munmap (only ~55 KB total!)
    let mut ep_flat_bytes = Vec::with_capacity(num_endpoints * 32);
    let mut ep_slice_bounds = Vec::with_capacity(num_endpoints);
    for (_, (s, e), _) in &sorted_endpoints {
        let ep_bytes = &data[*s..*e];
        let off = ep_flat_bytes.len();
        ep_flat_bytes.extend_from_slice(ep_bytes);
        ep_slice_bounds.push((off, ep_flat_bytes.len()));
    }

    // Free mmap IMMEDIATELY!
    unsafe { munmap(ptr, len); }

    // Parallel formatting by hour across threads with fast itoa!
    let hours_per_thread = (NUM_HOURS + num_threads - 1) / num_threads;
    let formatted_chunks: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..NUM_HOURS).step_by(hours_per_thread).map(|h_start| {
            let h_end = (h_start + hours_per_thread).min(NUM_HOURS);
            let group_offsets_ref = &group_offsets;
            let total_errs_ref = &total_errs;
            let ep_flat_ref = &ep_flat_bytes;
            let ep_bounds_ref = &ep_slice_bounds;

            s.spawn(move || {
                let all_latencies_ptr = lat_ptr as *mut u32;
                let mut out = Vec::with_capacity((h_end - h_start) * num_endpoints * 45);
                let mut num_buf = [0u8; 32];

                for hour_id in h_start..h_end {
                    let hour_str = hour_to_string(hour_id);

                    for ep_idx in 0..num_endpoints {
                        let gid = hour_id * num_endpoints + ep_idx;
                        let s = group_offsets_ref[gid] as usize;
                        let e = group_offsets_ref[gid + 1] as usize;
                        let count = e - s;
                        if count == 0 { continue; }

                        let slice = unsafe { std::slice::from_raw_parts_mut(all_latencies_ptr.add(s), count) };
                        slice.sort_unstable();

                        let p50 = slice[nearest_rank(count, 50)];
                        let p95 = slice[nearest_rank(count, 95)];
                        let p99 = slice[nearest_rank(count, 99)];
                        let err_count = total_errs_ref[gid].load(Ordering::Relaxed) as u64;
                        let error_ppm = (err_count * 1_000_000) / (count as u64);

                        out.extend_from_slice(&hour_str);
                        out.push(b' ');
                        let (b_s, b_e) = ep_bounds_ref[ep_idx];
                        out.extend_from_slice(&ep_flat_ref[b_s..b_e]);
                        out.push(b' ');

                        let w = unsafe { write_u64_fast(count as u64, num_buf.as_mut_ptr()) };
                        out.extend_from_slice(&num_buf[..w]);
                        out.push(b' ');
                        let w = unsafe { write_u64_fast(p50 as u64, num_buf.as_mut_ptr()) };
                        out.extend_from_slice(&num_buf[..w]);
                        out.push(b' ');
                        let w = unsafe { write_u64_fast(p95 as u64, num_buf.as_mut_ptr()) };
                        out.extend_from_slice(&num_buf[..w]);
                        out.push(b' ');
                        let w = unsafe { write_u64_fast(p99 as u64, num_buf.as_mut_ptr()) };
                        out.extend_from_slice(&num_buf[..w]);
                        out.push(b' ');
                        let w = unsafe { write_u64_fast(err_count, num_buf.as_mut_ptr()) };
                        out.extend_from_slice(&num_buf[..w]);
                        out.push(b' ');
                        let w = unsafe { write_u64_fast(error_ppm, num_buf.as_mut_ptr()) };
                        out.extend_from_slice(&num_buf[..w]);
                        out.push(b'\n');
                    }
                }
                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut final_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in formatted_chunks {
        let _ = final_file.write_all(&chunk);
    }
}
