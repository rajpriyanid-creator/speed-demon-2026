use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[derive(Default)]
struct GroupData {
    count: u32,
    errors: u32,
    latencies: Vec<u32>,
}

#[inline(always)]
fn nearest_rank(n: usize, p: usize) -> usize {
    (p * n + 99) / 100 - 1
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

    let mut thread_maps: Vec<BTreeMap<([u8; 20], Box<[u8]>), GroupData>> = Vec::with_capacity(num_threads);

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
                let mut local_map: BTreeMap<([u8; 20], Box<[u8]>), GroupData> = BTreeMap::new();
                let mut pos = 0;
                let chunk_len = chunk.len();

                while pos < chunk_len {
                    // Line format: ts SP method SP endpoint SP status SP latency LF
                    let line_start = pos;
                    while pos < chunk_len && chunk[pos] != b'\n' {
                        pos += 1;
                    }
                    let line = &chunk[line_start..pos];
                    if pos < chunk_len {
                        pos += 1; // skip \n
                    }
                    if line.len() < 35 {
                        continue;
                    }

                    // Timestamp: first 13 bytes e.g. "2026-10-30T18"
                    let mut hour = [0u8; 20];
                    hour[0..13].copy_from_slice(&line[0..13]);
                    hour[13..20].copy_from_slice(b":00:00Z");

                    // Skip method
                    let mut idx = 28; // after "ts SP"
                    while idx < line.len() && line[idx] != b' ' {
                        idx += 1;
                    }
                    idx += 1; // skip SP after method

                    // Endpoint
                    let ep_start = idx;
                    while idx < line.len() && line[idx] != b' ' {
                        idx += 1;
                    }
                    let endpoint = &line[ep_start..idx];
                    idx += 1; // skip SP after endpoint

                    // Status
                    let status_start = idx;
                    while idx < line.len() && line[idx] != b' ' {
                        idx += 1;
                    }
                    let is_5xx = idx > status_start && line[status_start] == b'5';
                    idx += 1; // skip SP after status

                    // Latency
                    let mut latency: u32 = 0;
                    while idx < line.len() {
                        let b = line[idx];
                        if b >= b'0' && b <= b'9' {
                            latency = latency * 10 + (b - b'0') as u32;
                        }
                        idx += 1;
                    }

                    let key = (hour, Box::from(endpoint));
                    let entry = local_map.entry(key).or_default();
                    entry.count += 1;
                    if is_5xx {
                        entry.errors += 1;
                    }
                    entry.latencies.push(latency);
                }
                local_map
            }));
        }

        for h in handles {
            thread_maps.push(h.join().unwrap());
        }
    });

    // Merge into global map
    let mut global_map: BTreeMap<([u8; 20], Box<[u8]>), GroupData> = BTreeMap::new();
    for local in thread_maps {
        for (key, val) in local {
            let entry = global_map.entry(key).or_default();
            entry.count += val.count;
            entry.errors += val.errors;
            entry.latencies.extend(val.latencies);
        }
    }

    // Prepare output
    let mut out_buf = Vec::with_capacity(8 * 1024 * 1024);
    for ((hour, endpoint), mut group) in global_map {
        let n = group.latencies.len();
        if n == 0 {
            continue;
        }

        let k50 = nearest_rank(n, 50);
        let k95 = nearest_rank(n, 95);
        let k99 = nearest_rank(n, 99);

        // Partition for k99
        let (_, p99_val, _) = group.latencies.select_nth_unstable(k99);
        let p99 = *p99_val;

        let (_, p95_val, _) = group.latencies[..=k99].select_nth_unstable(k95);
        let p95 = *p95_val;

        let (_, p50_val, _) = group.latencies[..=k95].select_nth_unstable(k50);
        let p50 = *p50_val;

        let ppm = (group.errors as u64 * 1_000_000) / (group.count as u64);

        // Format line: hour endpoint count p50 p95 p99 errors ppm\n
        out_buf.extend_from_slice(&hour);
        out_buf.push(b' ');
        out_buf.extend_from_slice(&endpoint);
        out_buf.push(b' ');
        write!(out_buf, "{} {} {} {} {} {}\n", group.count, p50, p95, p99, group.errors, ppm).unwrap();
    }

    let _ = fs::create_dir_all("/out");
    if let Ok(mut out_file) = File::create("/out/summary.txt") {
        let _ = out_file.write_all(&out_buf);
    }

    unsafe {
        munmap(ptr, len);
    }
}
