use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const NUM_HOURS: usize = 72;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[inline(always)]
fn parse_hour_id(line: &[u8]) -> usize {
    let day = (line[8] - b'0') as usize * 10 + (line[9] - b'0') as usize;
    let hr = (line[11] - b'0') as usize * 10 + (line[12] - b'0') as usize;
    let day_offset = match day {
        30 => 0,
        31 => 24,
        1 => 48,
        _ => 72,
    };
    day_offset + hr - 18
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

// Fast hash for endpoint
#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h = (h ^ (x as u64)).wrapping_mul(0x100000001b3);
    }
    h
}

struct EndpointCatalog {
    endpoints: Vec<Box<[u8]>>,
    table: Vec<Option<(u64, u16)>>, // (hash, ep_id)
    mask: usize,
}

impl EndpointCatalog {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        Self {
            endpoints: Vec::with_capacity(2048),
            table: vec![None; size],
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, ep: &[u8]) -> u16 {
        let h = hash_bytes(ep);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.table[idx] {
                Some((slot_h, id)) => {
                    if slot_h == h && &self.endpoints[id as usize][..] == ep {
                        return id;
                    }
                    idx = (idx + 1) & self.mask;
                }
                None => {
                    let id = self.endpoints.len() as u16;
                    self.endpoints.push(Box::from(ep));
                    self.table[idx] = Some((h, id));
                    return id;
                }
            }
        }
    }

    #[inline(always)]
    fn get(&self, ep: &[u8]) -> u16 {
        let h = hash_bytes(ep);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.table[idx] {
                Some((slot_h, id)) => {
                    if slot_h == h && &self.endpoints[id as usize][..] == ep {
                        return id;
                    }
                    idx = (idx + 1) & self.mask;
                }
                None => unreachable!(),
            }
        }
    }
}

fn main() {
    let input_path = "/data/requests.log";
    let file = match File::open(input_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };
    let len = meta.len() as usize;
    if len == 0 {
        let _ = fs::create_dir_all("/out");
        let _ = File::create("/out/summary.txt");
        return;
    }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(12);
    let mut boundaries = Vec::with_capacity(num_threads + 1);
    boundaries.push(0);
    for i in 1..num_threads {
        let mut pos = (len * i) / num_threads;
        while pos < len && data[pos] != b'\n' {
            pos += 1;
        }
        if pos < len {
            pos += 1;
        }
        boundaries.push(pos.min(len));
    }
    boundaries.push(len);

    // Pass 1: Build endpoint catalog in single pass (fast)
    let mut catalog = EndpointCatalog::new(4096);
    {
        let mut pos = 0;
        while pos < len {
            let line_start = pos;
            while pos < len && data[pos] != b'\n' {
                pos += 1;
            }
            let line = &data[line_start..pos];
            if pos < len { pos += 1; }
            if line.len() < 35 { continue; }

            let mut idx = 28;
            while idx < line.len() && line[idx] != b' ' { idx += 1; }
            idx += 1;
            let ep_start = idx;
            while idx < line.len() && line[idx] != b' ' { idx += 1; }
            catalog.get_or_insert(&line[ep_start..idx]);
        }
    }

    let num_endpoints = catalog.endpoints.len();

    // Map each endpoint_id to sorted_rank
    let mut sorted_endpoints: Vec<(usize, &[u8])> = catalog.endpoints.iter().enumerate().map(|(id, b)| (id, &b[..])).collect();
    sorted_endpoints.sort_unstable_by(|a, b| a.1.cmp(b.1));
    let mut old_to_sorted = vec![0usize; num_endpoints];
    for (rank, &(old_id, _)) in sorted_endpoints.iter().enumerate() {
        old_to_sorted[old_id] = rank;
    }

    // Parallel Pass 2: Parse and collect into thread-local flat record buffers
    // Each record is packed: group_id (u32), latency (u32), is_error (bool)
    struct ThreadResult {
        records: Vec<(u32, u32, bool)>,
    }

    let mut thread_results: Vec<ThreadResult> = Vec::with_capacity(num_threads);

    thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end {
                continue;
            }
            let chunk = &data[start..end];
            let cat = &catalog;
            let remap = &old_to_sorted;

            handles.push(s.spawn(move || {
                let est_lines = (end - start) / 75 + 1024;
                let mut records = Vec::with_capacity(est_lines);
                let mut pos = 0;
                let chunk_len = chunk.len();

                while pos < chunk_len {
                    let line_start = pos;
                    while pos < chunk_len && chunk[pos] != b'\n' {
                        pos += 1;
                    }
                    let line = &chunk[line_start..pos];
                    if pos < chunk_len { pos += 1; }
                    if line.len() < 35 { continue; }

                    let hour_id = parse_hour_id(line);

                    let mut idx = 28;
                    while idx < line.len() && line[idx] != b' ' { idx += 1; }
                    idx += 1;

                    let ep_start = idx;
                    while idx < line.len() && line[idx] != b' ' { idx += 1; }
                    let ep = &line[ep_start..idx];
                    idx += 1;

                    let ep_id = cat.get(ep);
                    let sorted_ep_id = remap[ep_id as usize];

                    let is_5xx = idx < line.len() && line[idx] == b'5';
                    while idx < line.len() && line[idx] != b' ' { idx += 1; }
                    idx += 1;

                    let mut latency: u32 = 0;
                    while idx < line.len() {
                        let b = line[idx];
                        if b >= b'0' && b <= b'9' {
                            latency = latency * 10 + (b - b'0') as u32;
                        }
                        idx += 1;
                    }

                    let group_id = (hour_id * num_endpoints + sorted_ep_id) as u32;
                    records.push((group_id, latency, is_5xx));
                }
                ThreadResult { records }
            }));
        }

        for h in handles {
            thread_results.push(h.join().unwrap());
        }
    });

    let total_groups = NUM_HOURS * num_endpoints;
    let mut group_counts = vec![0u32; total_groups];
    let mut group_errors = vec![0u32; total_groups];

    for tr in &thread_results {
        for &(gid, _, err) in &tr.records {
            group_counts[gid as usize] += 1;
            if err {
                group_errors[gid as usize] += 1;
            }
        }
    }

    // Prefix sums to find offsets in flat latency array
    let mut group_offsets = vec![0usize; total_groups + 1];
    let mut total_records = 0usize;
    for i in 0..total_groups {
        group_offsets[i] = total_records;
        total_records += group_counts[i] as usize;
    }
    group_offsets[total_groups] = total_records;

    // Fill flat latencies array
    let mut latencies = vec![0u32; total_records];
    let mut write_cursors = group_offsets.clone();

    for tr in thread_results {
        for (gid, lat, _) in tr.records {
            let idx = write_cursors[gid as usize];
            latencies[idx] = lat;
            write_cursors[gid as usize] += 1;
        }
    }

    // Output buffer
    let mut out_buf = Vec::with_capacity(8 * 1024 * 1024);

    for hour_id in 0..NUM_HOURS {
        let hour_str = hour_to_string(hour_id);
        for sorted_ep_id in 0..num_endpoints {
            let gid = hour_id * num_endpoints + sorted_ep_id;
            let count = group_counts[gid] as usize;
            if count == 0 {
                continue;
            }

            let start = group_offsets[gid];
            let end = group_offsets[gid + 1];
            let group_lats = &mut latencies[start..end];

            let k50 = nearest_rank(count, 50);
            let k95 = nearest_rank(count, 95);
            let k99 = nearest_rank(count, 99);

            let (_, p99_val, _) = group_lats.select_nth_unstable(k99);
            let p99 = *p99_val;

            let (_, p95_val, _) = group_lats[..=k99].select_nth_unstable(k95);
            let p95 = *p95_val;

            let (_, p50_val, _) = group_lats[..=k95].select_nth_unstable(k50);
            let p50 = *p50_val;

            let errs = group_errors[gid];
            let ppm = (errs as u64 * 1_000_000) / (count as u64);
            let ep_bytes = sorted_endpoints[sorted_ep_id].1;

            out_buf.extend_from_slice(&hour_str);
            out_buf.push(b' ');
            out_buf.extend_from_slice(ep_bytes);
            out_buf.push(b' ');
            write!(out_buf, "{} {} {} {} {} {}\n", count, p50, p95, p99, errs, ppm).unwrap();
        }
    }

    let _ = fs::create_dir_all("/out");
    if let Ok(mut out_file) = File::create("/out/summary.txt") {
        let _ = out_file.write_all(&out_buf);
    }

    unsafe {
        munmap(ptr, len);
    }
}
