use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const NUM_HOURS: usize = 72;
const MAX_LOCAL_EPS: usize = 2048;

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
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h: u32 = 0x811c9dc5;
    for &x in b {
        h = (h ^ (x as u32)).wrapping_mul(0x01000193);
    }
    ((b.len() as u64) << 32) | (h as u64)
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

    #[inline(always)]
    fn get_id_with_hash(&self, h: u64) -> u16 {
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
    fn set_mapped_id(&mut self, h: u64, new_id: u16) {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == h {
                unsafe {
                    self.table.get_unchecked_mut(idx).1 = new_id;
                }
                return;
            }
            idx = (idx + 1) & self.mask;
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

    // =========================================================================
    // PASS 1: Parse lines, discover local endpoints, count (hour, local_ep)
    // =========================================================================
    struct Pass1Result<'a> {
        catalog: LocalCatalog<'a>,
        counts: Vec<u32>,
        errors: Vec<u32>,
    }

    let pass1_results: Vec<Pass1Result> = thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end {
                continue;
            }
            let chunk = &data[start..end];

            handles.push(s.spawn(move || {
                let mut catalog = LocalCatalog::new(4096);
                let mut counts = vec![0u32; NUM_HOURS * MAX_LOCAL_EPS];
                let mut errors = vec![0u32; NUM_HOURS * MAX_LOCAL_EPS];

                let mut pos = 0;
                let chunk_len = chunk.len();
                let cptr = chunk.as_ptr();

                while pos + 35 <= chunk_len {
                    let day = unsafe { (*cptr.add(pos + 8) - b'0') as usize * 10 + (*cptr.add(pos + 9) - b'0') as usize };
                    let hr = unsafe { (*cptr.add(pos + 11) - b'0') as usize * 10 + (*cptr.add(pos + 12) - b'0') as usize };
                    let day_offset = match day {
                        30 => 0,
                        31 => 24,
                        1 => 48,
                        _ => 72,
                    };
                    let hour_id = day_offset + hr - 18;

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

                    let mut h: u32 = 0x811c9dc5;
                    let mut ep_end = ep_start;
                    while ep_end < chunk_len {
                        let b = unsafe { *cptr.add(ep_end) };
                        if b == b' ' { break; }
                        h = (h ^ (b as u32)).wrapping_mul(0x01000193);
                        ep_end += 1;
                    }
                    if ep_end >= chunk_len {
                        break;
                    }
                    let ep_len = (ep_end - ep_start) as u64;
                    let h_combined = (ep_len << 32) | (h as u64);

                    let ep_id = catalog.get_or_insert_with_hash(&chunk[ep_start..ep_end], h_combined) as usize;
                    let is_error = unsafe { *cptr.add(ep_end + 1) } == b'5';

                    let cell = hour_id * MAX_LOCAL_EPS + ep_id;
                    counts[cell] += 1;
                    if is_error {
                        errors[cell] += 1;
                    }

                    // Seek past latency to newline
                    let mut idx = ep_end + 5;
                    while idx < chunk_len && unsafe { *cptr.add(idx) } >= b'0' {
                        idx += 1;
                    }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;
                }

                Pass1Result { catalog, counts, errors }
            }));
        }

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // =========================================================================
    // UNIFY ENDPOINTS & COMPUTE OFFSETS
    // =========================================================================
    let mut global_catalog = LocalCatalog::new(4096);
    let mut thread_ep_remap: Vec<Vec<u16>> = Vec::with_capacity(pass1_results.len());

    for res in &pass1_results {
        let mut remap = Vec::with_capacity(res.catalog.endpoints.len());
        for &ep in &res.catalog.endpoints {
            let global_id = global_catalog.get_or_insert_with_hash(ep, hash_bytes(ep));
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

    let thread_local_to_sorted: Vec<Vec<u16>> = thread_ep_remap
        .iter()
        .map(|remap| {
            remap.iter().map(|&g_id| global_to_sorted[g_id as usize] as u16).collect()
        })
        .collect();

    let total_groups = NUM_HOURS * num_endpoints;
    let num_worker_threads = pass1_results.len();

    let mut group_counts = vec![0u32; total_groups];
    let mut group_errors = vec![0u32; total_groups];
    let mut thread_dest_offsets: Vec<Vec<u32>> = (0..num_worker_threads).map(|_| vec![0u32; total_groups]).collect();
    let mut group_offsets = vec![0usize; total_groups + 1];

    let mut thread_canonical_counts: Vec<Vec<u32>> = (0..num_worker_threads).map(|_| vec![0u32; total_groups]).collect();
    for (t, res) in pass1_results.iter().enumerate() {
        let map = &thread_local_to_sorted[t];
        for hr in 0..NUM_HOURS {
            for (local_ep, &sorted_ep) in map.iter().enumerate() {
                let cell = hr * MAX_LOCAL_EPS + local_ep;
                let c = res.counts[cell];
                if c > 0 {
                    let g_id = hr * num_endpoints + sorted_ep as usize;
                    thread_canonical_counts[t][g_id] = c;
                    group_errors[g_id] += res.errors[cell];
                }
            }
        }
    }

    let mut cur_total = 0usize;
    for g in 0..total_groups {
        group_offsets[g] = cur_total;
        let mut g_cnt = 0u32;
        let mut cur_slot = cur_total as u32;
        for t in 0..num_worker_threads {
            thread_dest_offsets[t][g] = cur_slot;
            let c = thread_canonical_counts[t][g];
            cur_slot += c;
            g_cnt += c;
        }
        group_counts[g] = g_cnt;
        cur_total = cur_slot as usize;
    }
    group_offsets[total_groups] = cur_total;
    drop(thread_canonical_counts);

    // Overwrite local catalogs with sorted_ep directly for O(1) Pass 2 lookups
    let mut catalogs: Vec<LocalCatalog> = Vec::with_capacity(pass1_results.len());
    for (t, mut res) in pass1_results.into_iter().enumerate() {
        let map = &thread_local_to_sorted[t];
        for (local_ep, &sorted_ep) in map.iter().enumerate() {
            let ep = res.catalog.endpoints[local_ep];
            let h = hash_bytes(ep);
            res.catalog.set_mapped_id(h, sorted_ep);
        }
        catalogs.push(res.catalog);
    }
    drop(thread_local_to_sorted);

    let mut flat_latencies = vec![0u32; cur_total];
    let flat_ptr = flat_latencies.as_mut_ptr() as usize;

    // =========================================================================
    // PASS 2: Parse raw lines second time, scatter directly to flat_latencies!
    // =========================================================================
    thread::scope(|s| {
        for (i, (catalog, mut dest_offsets)) in catalogs.into_iter().zip(thread_dest_offsets.into_iter()).enumerate() {
            let start = boundaries[i];
            let end = boundaries[i + 1];
            if start >= end {
                continue;
            }
            let chunk = &data[start..end];

            s.spawn(move || {
                let raw_ptr = flat_ptr as *mut u32;
                let mut pos = 0;
                let chunk_len = chunk.len();
                let cptr = chunk.as_ptr();

                while pos + 35 <= chunk_len {
                    let day = unsafe { (*cptr.add(pos + 8) - b'0') as usize * 10 + (*cptr.add(pos + 9) - b'0') as usize };
                    let hr = unsafe { (*cptr.add(pos + 11) - b'0') as usize * 10 + (*cptr.add(pos + 12) - b'0') as usize };
                    let day_offset = match day {
                        30 => 0,
                        31 => 24,
                        1 => 48,
                        _ => 72,
                    };
                    let hour_id = day_offset + hr - 18;

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

                    let mut h: u32 = 0x811c9dc5;
                    let mut ep_end = ep_start;
                    while ep_end < chunk_len {
                        let b = unsafe { *cptr.add(ep_end) };
                        if b == b' ' { break; }
                        h = (h ^ (b as u32)).wrapping_mul(0x01000193);
                        ep_end += 1;
                    }
                    if ep_end >= chunk_len {
                        break;
                    }
                    let ep_len = (ep_end - ep_start) as u64;
                    let h_combined = (ep_len << 32) | (h as u64);

                    let sorted_ep = catalog.get_id_with_hash(h_combined) as usize;
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

                    let slot = dest_offsets[g_id] as usize;
                    dest_offsets[g_id] += 1;
                    unsafe {
                        *raw_ptr.add(slot) = latency;
                    }

                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\r' { idx += 1; }
                    if idx < chunk_len && unsafe { *cptr.add(idx) } == b'\n' { idx += 1; }
                    pos = idx;
                }
            });
        }
    });

    // =========================================================================
    // PASS 3: Sort group latencies & format output
    // =========================================================================
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

    let _ = fs::create_dir_all("/out");
    let mut out_file = File::create("/out/summary.txt").unwrap();
    for chunk in formatted_chunks {
        out_file.write_all(&chunk).unwrap();
    }
    out_file.flush().unwrap();

    unsafe { munmap(ptr, len); }
}
