use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const NUM_HOURS: usize = 72;
const MAX_LOCAL_EPS: usize = 4096;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[inline(always)]
fn parse_hour_id(line: &[u8]) -> usize {
    if line.len() < 13 {
        return 0;
    }
    let p = line.as_ptr();
    let day = unsafe { (*p.add(8) - b'0') as usize * 10 + (*p.add(9) - b'0') as usize };
    let hr = unsafe { (*p.add(11) - b'0') as usize * 10 + (*p.add(12) - b'0') as usize };
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

#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h = (h ^ (x as u64)).wrapping_mul(0x100000001b3);
    }
    h
}

struct LocalCatalog<'a> {
    endpoints: Vec<&'a [u8]>,
    table: Vec<Option<(u64, u16)>>,
    mask: usize,
}

impl<'a> LocalCatalog<'a> {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        Self {
            endpoints: Vec::with_capacity(2048),
            table: vec![None; size],
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, ep: &'a [u8]) -> u16 {
        let h = hash_bytes(ep);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.table[idx] {
                Some((slot_h, id)) => {
                    if slot_h == h && self.endpoints[id as usize] == ep {
                        return id;
                    }
                    idx = (idx + 1) & self.mask;
                }
                None => {
                    let id = self.endpoints.len() as u16;
                    self.endpoints.push(ep);
                    self.table[idx] = Some((h, id));
                    return id;
                }
            }
        }
    }
}

struct ThreadOutput<'a> {
    catalog: LocalCatalog<'a>,
    records: Vec<u64>,
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

    let mut thread_outputs: Vec<ThreadOutput> = Vec::with_capacity(num_threads);

    thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end {
                continue;
            }
            let chunk = &data[start..end];

            handles.push(s.spawn(move || {
                let est_records = (end - start) / 75 + 1024;
                let mut catalog = LocalCatalog::new(4096);
                let mut records = Vec::with_capacity(est_records);

                let mut pos = 0;
                let chunk_len = chunk.len();
                while pos < chunk_len {
                    let line_start = pos;
                    while pos < chunk_len && chunk[pos] != b'\n' {
                        pos += 1;
                    }
                    let line = &chunk[line_start..pos];
                    if pos < chunk_len {
                        pos += 1;
                    }
                    if line.len() < 35 {
                        continue;
                    }

                    let hour_id = parse_hour_id(line);
                    let lptr = line.as_ptr();
                    let llen = line.len();
                    let mut idx = 28;
                    while idx < llen && unsafe { *lptr.add(idx) } != b' ' {
                        idx += 1;
                    }
                    if idx >= llen {
                        continue;
                    }
                    idx += 1; // skip method space
                    let ep_start = idx;
                    while idx < llen && unsafe { *lptr.add(idx) } != b' ' {
                        idx += 1;
                    }
                    if idx >= llen {
                        continue;
                    }
                    let ep_slice = &line[ep_start..idx];
                    let ep_id = catalog.get_or_insert(ep_slice);
                    idx += 1; // skip space

                    let is_error = unsafe { *lptr.add(idx) } == b'5';
                    idx += 4; // skip status and space

                    let mut latency: u32 = 0;
                    while idx < llen {
                        let b = unsafe { *lptr.add(idx) };
                        if b >= b'0' && b <= b'9' {
                            latency = latency * 10 + (b - b'0') as u32;
                            idx += 1;
                        } else {
                            break;
                        }
                    }

                    let local_group_id = (hour_id * MAX_LOCAL_EPS + ep_id as usize) as u64;
                    let packed = local_group_id | ((is_error as u64) << 20) | ((latency as u64) << 32);
                    records.push(packed);
                }

                ThreadOutput { catalog, records }
            }));
        }

        for h in handles {
            thread_outputs.push(h.join().unwrap());
        }
    });

    // 1. Unify all endpoints across threads
    let mut global_catalog = LocalCatalog::new(4096);
    let mut thread_ep_remap: Vec<Vec<u16>> = Vec::with_capacity(thread_outputs.len());

    for th in &thread_outputs {
        let mut remap = Vec::with_capacity(th.catalog.endpoints.len());
        for &ep in &th.catalog.endpoints {
            let global_id = global_catalog.get_or_insert(ep);
            remap.push(global_id);
        }
        thread_ep_remap.push(remap);
    }

    let num_endpoints = global_catalog.endpoints.len();

    // Sort endpoints alphabetically to get canonical ranks
    let mut sorted_endpoints: Vec<(usize, &[u8])> = global_catalog
        .endpoints
        .iter()
        .enumerate()
        .map(|(id, &b)| (id, b))
        .collect();
    sorted_endpoints.sort_unstable_by(|a, b| a.1.cmp(b.1));

    let mut global_to_sorted = vec![0usize; num_endpoints];
    for (sorted_rank, &(old_id, _)) in sorted_endpoints.iter().enumerate() {
        global_to_sorted[old_id] = sorted_rank;
    }

    let total_groups = NUM_HOURS * num_endpoints;
    let mut group_counts = vec![0u32; total_groups];
    let mut group_errors = vec![0u32; total_groups];

    // Pass A: Count records per global group
    for (th_idx, th) in thread_outputs.iter().enumerate() {
        let remap = &thread_ep_remap[th_idx];
        for &packed in &th.records {
            let local_group_id = (packed & 0xFFFFF) as usize;
            let hour_id = local_group_id / MAX_LOCAL_EPS;
            let local_ep = local_group_id % MAX_LOCAL_EPS;
            let is_error = ((packed >> 20) & 1) != 0;
            let global_ep = remap[local_ep] as usize;
            let sorted_ep = global_to_sorted[global_ep];
            let g_id = hour_id * num_endpoints + sorted_ep;
            group_counts[g_id] += 1;
            if is_error {
                group_errors[g_id] += 1;
            }
        }
    }

    // Prefix sums for flat latency storage
    let mut group_offsets = vec![0usize; total_groups + 1];
    let mut total_records = 0usize;
    for i in 0..total_groups {
        group_offsets[i] = total_records;
        total_records += group_counts[i] as usize;
    }
    group_offsets[total_groups] = total_records;

    let mut flat_latencies = vec![0u32; total_records];
    let mut write_cursors = group_offsets.clone();

    // Pass B: Scatter latencies into contiguous group slices
    for (th_idx, th) in thread_outputs.iter().enumerate() {
        let remap = &thread_ep_remap[th_idx];
        for &packed in &th.records {
            let local_group_id = (packed & 0xFFFFF) as usize;
            let hour_id = local_group_id / MAX_LOCAL_EPS;
            let local_ep = local_group_id % MAX_LOCAL_EPS;
            let latency = (packed >> 32) as u32;
            let global_ep = remap[local_ep] as usize;
            let sorted_ep = global_to_sorted[global_ep];
            let g_id = hour_id * num_endpoints + sorted_ep;
            let slot = write_cursors[g_id];
            write_cursors[g_id] += 1;
            flat_latencies[slot] = latency;
        }
    }

    drop(write_cursors);
    drop(thread_outputs);

    // Format output
    let _ = fs::create_dir_all("/out");
    let out_file = match File::create("/out/summary.txt") {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut writer = std::io::BufWriter::with_capacity(128 * 1024, out_file);

    for hour_id in 0..NUM_HOURS {
        let hour_str = hour_to_string(hour_id);
        for sorted_ep in 0..num_endpoints {
            let g_id = hour_id * num_endpoints + sorted_ep;
            let count = group_counts[g_id] as usize;
            if count == 0 {
                continue;
            }

            let start = group_offsets[g_id];
            let end = group_offsets[g_id + 1];
            let lats = &mut flat_latencies[start..end];

            let p50_idx = nearest_rank(count, 50);
            let p95_idx = nearest_rank(count, 95);
            let p99_idx = nearest_rank(count, 99);

            let p50 = *lats.select_nth_unstable(p50_idx).1;
            let p95 = *lats.select_nth_unstable(p95_idx).1;
            let p99 = *lats.select_nth_unstable(p99_idx).1;

            let errors = group_errors[g_id] as u64;
            let error_ppm = (errors * 1_000_000) / (count as u64);
            let ep_bytes = sorted_endpoints[sorted_ep].1;

            let _ = writer.write_all(&hour_str);
            let _ = writer.write_all(b" ");
            let _ = writer.write_all(ep_bytes);
            let _ = writeln!(writer, " {} {} {} {} {} {}", count, p50, p95, p99, errors, error_ppm);
        }
    }

    let _ = writer.flush();
    unsafe { munmap(ptr, len); }
}
