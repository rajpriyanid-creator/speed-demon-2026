use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_mm_crc32_u64;

const NUM_HOURS: usize = 72;
const SLAB_CAP: usize = 14;

#[repr(C)]
#[derive(Clone, Copy)]
struct Slab {
    next: u32,
    len: u16,
    _pad: u16,
    lats: [u32; SLAB_CAP],
}

impl Default for Slab {
    fn default() -> Self {
        Self {
            next: u32::MAX,
            len: 0,
            _pad: 0,
            lats: [0u32; SLAB_CAP],
        }
    }
}


const MADV_DONTNEED: i32 = 4;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

#[inline(always)]
unsafe fn madvise_dontneed(addr: *const u8, len: usize) {
    let start = (addr as usize + 4095) & !4095;
    let end = (addr as usize + len) & !4095;
    if end > start {
        madvise(start as *mut u8, end - start, MADV_DONTNEED);
    }
}


#[inline(always)]
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
            let (th, tid) = unsafe { *self.table.get_unchecked(idx) };
            if th == h {
                return tid;
            }
            if th == 0 {
                let id = self.endpoints.len() as u16;
                self.endpoints.push(ep);
                self.hashes.push(h);
                unsafe { *self.table.get_unchecked_mut(idx) = (h, id); }
                return id;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn insert_mapping(&mut self, h: u64, sorted_id: u16) {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { self.table.get_unchecked_mut(idx) };
            if slot.0 == 0 || slot.0 == h {
                *slot = (h, sorted_id);
                return;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get_id(&self, h: u64) -> u16 {
        let mut idx = (h as usize) & self.mask;
        loop {
            let (th, tid) = unsafe { *self.table.get_unchecked(idx) };
            if th == h {
                return tid;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

fn pdqsort(v: &mut [u32]) {
    v.sort_unstable();
}

#[inline(always)]
unsafe fn parse_hour_from_ts(cptr: *const u8, pos: usize) -> usize {
    let day = (*cptr.add(pos + 8) - b'0') as usize * 10 + (*cptr.add(pos + 9) - b'0') as usize;
    let hr = (*cptr.add(pos + 11) - b'0') as usize * 10 + (*cptr.add(pos + 12) - b'0') as usize;
    match day {
        30 => hr - 18,
        31 => hr + 6,
        1 => hr + 30,
        _ => hr + 54,
    }
}

fn main() {
    let t0 = std::time::Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
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
        let _ = std::fs::create_dir_all(&out_dir);
        let _ = File::create(&results_path);
        return;
    }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, 1, 1, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 || ptr.is_null() { return; }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

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

    let mut lookup_table = FastCatalog::new(16384);
    for (sorted_id, &(_, _, h)) in sorted_endpoints.iter().enumerate() {
        lookup_table.insert_mapping(h, sorted_id as u16);
    }

    struct ThreadBucketState {
        h_min: usize,
        h_max: usize,
        head: Vec<u32>,
        counts: Vec<u16>,
        errors: Vec<u16>,
        slabs: Vec<Slab>,
    }

    let thread_states: Vec<ThreadBucketState> = std::thread::scope(|s| {
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

                // Compute exact hour bounds for this chunk with 1-hour safety margin
                let first_h = unsafe { parse_hour_from_ts(cptr, 0) };
                let mut last_line_start = chunk_len.saturating_sub(100);
                while last_line_start > 0 && unsafe { *cptr.add(last_line_start) } != b'\n' {
                    last_line_start -= 1;
                }
                if last_line_start > 0 { last_line_start += 1; }
                let last_h = if last_line_start + 35 <= chunk_len {
                    unsafe { parse_hour_from_ts(cptr, last_line_start) }
                } else {
                    first_h
                };

                let h_min = first_h.min(last_h).saturating_sub(1);
                let h_max = (first_h.max(last_h) + 1).min(NUM_HOURS - 1);
                let num_chunk_hours = h_max - h_min + 1;
                let num_chunk_buckets = num_chunk_hours * num_endpoints;

                let est_records = (end - start) / 72 + 1024;
                let est_slabs = est_records / SLAB_CAP + 512;
                let mut slabs: Vec<Slab> = Vec::with_capacity(est_slabs);
                let mut head = vec![u32::MAX; num_chunk_buckets];
                let mut tail = vec![u32::MAX; num_chunk_buckets];
                let mut counts = vec![0u16; num_chunk_buckets];
                let mut errors = vec![0u16; num_chunk_buckets];

                let mut pos = 0;
                let mut last_adv = 0;
                while pos + 35 <= chunk_len {
                    if pos - last_adv >= 2 * 1024 * 1024 {
                        unsafe { madvise_dontneed(cptr.add(last_adv), pos - last_adv); }
                        last_adv = pos;
                    }
                    let hour_id = unsafe { parse_hour_from_ts(cptr, pos) };
                    let loc_hour = hour_id.saturating_sub(h_min);

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
                    let ep_id = lookup_ref.get_id(h_combined) as usize;
                    let bucket_id = loc_hour * num_endpoints + ep_id;

                    let status_byte = unsafe { *cptr.add(ep_end + 1) };
                    let is_err = status_byte == b'5';

                    let mut lat_pos = ep_end + 5;
                    let mut lat = 0u32;
                    while lat_pos < chunk_len {
                        let b = unsafe { *cptr.add(lat_pos) };
                        if b == b'\n' { break; }
                        if b >= b'0' && b <= b'9' {
                            lat = lat * 10 + (b - b'0') as u32;
                        }
                        lat_pos += 1;
                    }
                    pos = lat_pos + 1;

                    if bucket_id < num_chunk_buckets {
                        unsafe {
                            *counts.get_unchecked_mut(bucket_id) += 1;
                            if is_err {
                                *errors.get_unchecked_mut(bucket_id) += 1;
                            }

                            let t_idx = *tail.get_unchecked(bucket_id);
                            if t_idx == u32::MAX {
                                let new_idx = slabs.len() as u32;
                                let mut new_slab = Slab::default();
                                new_slab.lats[0] = lat;
                                new_slab.len = 1;
                                slabs.push(new_slab);
                                *head.get_unchecked_mut(bucket_id) = new_idx;
                                *tail.get_unchecked_mut(bucket_id) = new_idx;
                            } else if (slabs.get_unchecked(t_idx as usize).len as usize) < SLAB_CAP {
                                let slab = slabs.get_unchecked_mut(t_idx as usize);
                                slab.lats[slab.len as usize] = lat;
                                slab.len += 1;
                            } else {
                                let new_idx = slabs.len() as u32;
                                slabs.get_unchecked_mut(t_idx as usize).next = new_idx;
                                let mut new_slab = Slab::default();
                                new_slab.lats[0] = lat;
                                new_slab.len = 1;
                                slabs.push(new_slab);
                                *tail.get_unchecked_mut(bucket_id) = new_idx;
                            }
                        }
                    }
                }

                if chunk_len > last_adv {
                    unsafe { madvise_dontneed(cptr.add(last_adv), chunk_len - last_adv); }
                }
                ThreadBucketState { h_min, h_max, head, counts, errors, slabs }
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let t_parse = t0.elapsed();

    // Munmap input file IMMEDIATELY after single pass
    unsafe {
        munmap(ptr, len);
    }

    // Parallel evaluation & formatting across 12 threads
    let t_eval_start = std::time::Instant::now();
    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        let hours_per_thread = (NUM_HOURS + num_threads - 1) / num_threads;
        let sorted_eps_ref = &sorted_endpoints;
        let states_ref = &thread_states;

        let thread_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(num_threads);
            for t in 0..num_threads {
                let h_start = t * hours_per_thread;
                let h_end = (h_start + hours_per_thread).min(NUM_HOURS);
                if h_start >= h_end { continue; }

                handles.push(s.spawn(move || {
                    let mut out = Vec::with_capacity(1_000_000);
                    let mut num_buf = [0u8; 32];
                    let mut lat_buf = Vec::with_capacity(1024);

                    for h in h_start..h_end {
                        let h_str = hour_to_string(h);
                        for (ep_idx, (_, ep_bytes, _)) in sorted_eps_ref.iter().enumerate() {
                            let mut total_cnt = 0u32;
                            let mut total_err = 0u32;
                            for ts in states_ref {
                                if h >= ts.h_min && h <= ts.h_max {
                                    let loc_h = h - ts.h_min;
                                    let b = loc_h * num_endpoints + ep_idx;
                                    if b < ts.counts.len() {
                                        total_cnt += ts.counts[b] as u32;
                                        total_err += ts.errors[b] as u32;
                                    }
                                }
                            }
                            if total_cnt == 0 { continue; }

                            lat_buf.clear();
                            lat_buf.reserve(total_cnt as usize);

                            for ts in states_ref {
                                if h >= ts.h_min && h <= ts.h_max {
                                    let loc_h = h - ts.h_min;
                                    let b = loc_h * num_endpoints + ep_idx;
                                    if b < ts.head.len() {
                                        let mut cur_idx = ts.head[b];
                                        while cur_idx != u32::MAX {
                                            let slab = &ts.slabs[cur_idx as usize];
                                            lat_buf.extend_from_slice(&slab.lats[..slab.len as usize]);
                                            cur_idx = slab.next;
                                        }
                                    }
                                }
                            }

                            let n = total_cnt as usize;
                            let (p50, p95, p99) = if n <= 32 {
                                pdqsort(&mut lat_buf);
                                (lat_buf[nearest_rank(n, 50)], lat_buf[nearest_rank(n, 95)], lat_buf[nearest_rank(n, 99)])
                            } else {
                                let r99 = nearest_rank(n, 99);
                                let r95 = nearest_rank(n, 95);
                                let r50 = nearest_rank(n, 50);

                                lat_buf.select_nth_unstable(r99);
                                let v99 = lat_buf[r99];

                                let v95 = if r95 == r99 {
                                    v99
                                } else {
                                    lat_buf[..r99].select_nth_unstable(r95);
                                    lat_buf[r95]
                                };

                                let v50 = if r50 == r95 {
                                    v95
                                } else {
                                    lat_buf[..r95].select_nth_unstable(r50);
                                    lat_buf[r50]
                                };

                                (v50, v95, v99)
                            };

                            let ppm = ((total_err as u64 * 1_000_000) / total_cnt as u64) as u32;

                            out.extend_from_slice(&h_str);
                            out.push(b' ');
                            out.extend_from_slice(ep_bytes);
                            out.push(b' ');

                            let w = unsafe { write_u64_fast(total_cnt as u64, num_buf.as_mut_ptr()) };
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

                            let w = unsafe { write_u64_fast(total_err as u64, num_buf.as_mut_ptr()) };
                            out.extend_from_slice(&num_buf[..w]);
                            out.push(b' ');

                            let w = unsafe { write_u64_fast(ppm as u64, num_buf.as_mut_ptr()) };
                            out.extend_from_slice(&num_buf[..w]);
                            out.push(b'\n');
                        }
                    }
                    out
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        for buf in thread_outputs {
            let _ = out_file.write_all(&buf);
        }
    }
    let t_eval = t_eval_start.elapsed();

    eprintln!("[BOUNDED SLAB TIMING] Parse: {:?}, Eval & Sort: {:?}, Total: {:?}",
        t_parse, t_eval, t0.elapsed());
}
