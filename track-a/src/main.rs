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

struct LocalCatalog<'a> {
    endpoints: Vec<&'a [u8]>,
    table: Vec<(u64, u16)>,
    mask: usize,
}

impl<'a> LocalCatalog<'a> {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        Self {
            endpoints: Vec::with_capacity(2048),
            table: vec![(0, 0); size],
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn get_or_insert_with_hash(&mut self, ep: &'a [u8], h: u64) -> u16 {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == h {
                return slot.1;
            }
            if slot.0 == 0 {
                let id = self.endpoints.len() as u16;
                self.endpoints.push(ep);
                unsafe {
                    *self.table.get_unchecked_mut(idx) = (h, id);
                }
                return id;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

struct ThreadOutput<'a> {
    catalog: LocalCatalog<'a>,
    records: Vec<u64>,
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

    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(10);
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
                let cptr = chunk.as_ptr();

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

                    let ep_id = catalog.get_or_insert_with_hash(&chunk[ep_start..ep_end], h_combined);

                    let is_error = unsafe { *cptr.add(ep_end + 1) } == b'5';
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

                    let local_group_id = (hour_id * MAX_LOCAL_EPS + ep_id as usize) as u64;
                    let packed = local_group_id | ((is_error as u64) << 20) | ((latency as u64) << 32);
                    records.push(packed);

                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;
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
            let mut curr = 0;
            let mut h: u64 = 0xcbf29ce484222325;
            let ep_ptr = ep.as_ptr();
            let ep_len = ep.len();
            while curr + 8 <= ep_len {
                let w = unsafe { (ep_ptr.add(curr) as *const u64).read_unaligned() };
                h = (h ^ w).wrapping_mul(0x100000001b3);
                curr += 8;
            }
            if curr < ep_len {
                let rem = ep_len - curr;
                let mut tail = 0u64;
                for k in 0..rem {
                    tail |= (unsafe { *ep_ptr.add(curr + k) } as u64) << (k * 8);
                }
                h = (h ^ tail).wrapping_mul(0x100000001b3);
            }
            let h_combined = h ^ ((ep_len as u64).rotate_left(56));

            let global_id = global_catalog.get_or_insert_with_hash(ep, h_combined);
            remap.push(global_id);
        }
        thread_ep_remap.push(remap);
    }

    let num_endpoints = global_catalog.endpoints.len();

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
        for (th_idx, (mut th, mut dest_offsets)) in thread_outputs.into_iter().zip(thread_dest_offsets.into_iter()).enumerate() {
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
                th.records.clear();
                th.records.shrink_to_fit();
            });
        }
    });
    drop(thread_ep_remap);

    // Parallel percentiles computation using O(N) select_nth_unstable & fast formatting
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

                        let p50_idx = nearest_rank(count, 50);
                        let p95_idx = nearest_rank(count, 95);
                        let p99_idx = nearest_rank(count, 99);

                        let (p50, p95, p99) = if count <= 64 {
                            lats_slice.sort_unstable();
                            (lats_slice[p50_idx], lats_slice[p95_idx], lats_slice[p99_idx])
                        } else {
                            lats_slice.select_nth_unstable(p95_idx);
                            let p95 = lats_slice[p95_idx];
                            lats_slice[..p95_idx].select_nth_unstable(p50_idx);
                            let p50 = lats_slice[p50_idx];
                            lats_slice[p95_idx + 1..].select_nth_unstable(p99_idx - (p95_idx + 1));
                            let p99 = lats_slice[p99_idx];
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
