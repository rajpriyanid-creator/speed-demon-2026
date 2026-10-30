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
    let num_worker_threads = thread_outputs.len();

    // Parallel Pass A: Count records and errors per global group
    let (thread_counts, thread_errors): (Vec<Vec<u32>>, Vec<Vec<u32>>) = thread::scope(|s| {
        let handles: Vec<_> = thread_outputs
            .iter()
            .enumerate()
            .map(|(th_idx, th)| {
                let remap = &thread_ep_remap[th_idx];
                let global_to_sorted_ref = &global_to_sorted;

                s.spawn(move || {
                    let mut local_counts = vec![0u32; total_groups];
                    let mut local_errors = vec![0u32; total_groups];
                    for &packed in &th.records {
                        let local_group_id = (packed & 0xFFFFF) as usize;
                        let hour_id = local_group_id / MAX_LOCAL_EPS;
                        let local_ep = local_group_id % MAX_LOCAL_EPS;
                        let is_error = ((packed >> 20) & 1) != 0;
                        let global_ep = remap[local_ep] as usize;
                        let sorted_ep = global_to_sorted_ref[global_ep];
                        let g_id = hour_id * num_endpoints + sorted_ep;
                        local_counts[g_id] += 1;
                        if is_error {
                            local_errors[g_id] += 1;
                        }
                    }
                    (local_counts, local_errors)
                })
            })
            .collect();

        let mut tc = Vec::with_capacity(handles.len());
        let mut te = Vec::with_capacity(handles.len());
        for h in handles {
            let (c, e) = h.join().unwrap();
            tc.push(c);
            te.push(e);
        }
        (tc, te)
    });

    // Compute prefix sums and thread write bases
    let mut group_counts = vec![0u32; total_groups];
    let mut group_errors = vec![0u32; total_groups];
    let mut thread_dest_offsets: Vec<Vec<u32>> = (0..num_worker_threads).map(|_| vec![0u32; total_groups]).collect();
    let mut group_offsets = vec![0usize; total_groups + 1];

    let mut cur_total = 0usize;
    for g in 0..total_groups {
        group_offsets[g] = cur_total;
        let mut g_cnt = 0u32;
        let mut g_err = 0u32;
        let mut cur_slot = cur_total as u32;
        for t in 0..num_worker_threads {
            thread_dest_offsets[t][g] = cur_slot;
            let c = thread_counts[t][g];
            cur_slot += c;
            g_cnt += c;
            g_err += thread_errors[t][g];
        }
        group_counts[g] = g_cnt;
        group_errors[g] = g_err;
        cur_total = cur_slot as usize;
    }
    group_offsets[total_groups] = cur_total;
    drop(thread_counts);
    drop(thread_errors);

    let mut flat_latencies = vec![0u32; cur_total];
    let flat_ptr = flat_latencies.as_mut_ptr() as usize;

    // Parallel Pass B: Scatter latencies into contiguous group slices
    thread::scope(|s| {
        for (th_idx, (th, mut dest_offsets)) in thread_outputs.into_iter().zip(thread_dest_offsets.into_iter()).enumerate() {
            let remap = &thread_ep_remap[th_idx];
            let global_to_sorted_ref = &global_to_sorted;

            s.spawn(move || {
                let raw_ptr = flat_ptr as *mut u32;
                for &packed in &th.records {
                    let local_group_id = (packed & 0xFFFFF) as usize;
                    let hour_id = local_group_id / MAX_LOCAL_EPS;
                    let local_ep = local_group_id % MAX_LOCAL_EPS;
                    let latency = (packed >> 32) as u32;
                    let global_ep = remap[local_ep] as usize;
                    let sorted_ep = global_to_sorted_ref[global_ep];
                    let g_id = hour_id * num_endpoints + sorted_ep;
                    let slot = dest_offsets[g_id] as usize;
                    dest_offsets[g_id] += 1;
                    unsafe {
                        *raw_ptr.add(slot) = latency;
                    }
                }
            });
        }
    });
    drop(thread_ep_remap);

    // Parallel percentiles computation & output formatting
    let hours_per_chunk = (NUM_HOURS + num_threads - 1) / num_threads;
    let formatted_chunks: Vec<Vec<u8>> = thread::scope(|s| {
        let handles: Vec<_> = (0..NUM_HOURS).step_by(hours_per_chunk).map(|h_start| {
            let h_end = (h_start + hours_per_chunk).min(NUM_HOURS);
            let group_counts_ref = &group_counts;
            let group_errors_ref = &group_errors;
            let group_offsets_ref = &group_offsets;
            let sorted_endpoints_ref = &sorted_endpoints;
            let flat_latencies_ref = &flat_latencies;

            s.spawn(move || {
                let mut out = Vec::with_capacity((h_end - h_start) * num_endpoints * 48);
                for hour_id in h_start..h_end {
                    let hour_str = hour_to_string(hour_id);
                    for sorted_ep in 0..num_endpoints {
                        let g_id = hour_id * num_endpoints + sorted_ep;
                        let count = group_counts_ref[g_id] as usize;
                        if count == 0 {
                            continue;
                        }
                        let start = group_offsets_ref[g_id];
                        let end = group_offsets_ref[g_id + 1];
                        let lats_ptr = flat_latencies_ref.as_ptr() as *mut u32;
                        let lats_slice = unsafe { std::slice::from_raw_parts_mut(lats_ptr.add(start), end - start) };
                        lats_slice.sort_unstable();

                        let p50_idx = nearest_rank(count, 50);
                        let p95_idx = nearest_rank(count, 95);
                        let p99_idx = nearest_rank(count, 99);

                        let p50 = lats_slice[p50_idx];
                        let p95 = lats_slice[p95_idx];
                        let p99 = lats_slice[p99_idx];

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

    // Write formatted chunks sequentially
    let _ = fs::create_dir_all("/out");
    let mut out_file = File::create("/out/summary.txt").unwrap();
    for chunk in formatted_chunks {
        out_file.write_all(&chunk).unwrap();
    }
    out_file.flush().unwrap();

    unsafe { munmap(ptr, len); }
}
