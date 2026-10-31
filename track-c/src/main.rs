use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[derive(Clone, Copy)]
struct CountryScalarAgg {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: u64,
    note_bytes: u64,
}

impl Default for CountryScalarAgg {
    fn default() -> Self {
        Self {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
        }
    }
}

fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.abs();
    format!("{}{}.{:02}", sign, abs / 100, abs % 100)
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

    // 1. Fast single-pass load regions.csv and catalog countries
    let regions_path = format!("{}/regions.csv", data_dir);
    let regions_data = match fs::read(&regions_path) {
        Ok(b) => b,
        Err(_) => return,
    };

    let mut seen = [false; 17576];
    let mut region_to_code: Vec<u16> = vec![u16::MAX; 1_000_000];

    let mut pos = 0;
    while pos < regions_data.len() && regions_data[pos] != b'\n' {
        pos += 1;
    }
    if pos < regions_data.len() { pos += 1; }

    while pos < regions_data.len() {
        let mut rid = 0usize;
        while pos < regions_data.len() && regions_data[pos] != b',' {
            let b = regions_data[pos];
            if b >= b'0' && b <= b'9' {
                rid = rid * 10 + (b - b'0') as usize;
            }
            pos += 1;
        }
        if pos >= regions_data.len() { break; }
        pos += 1; // skip comma
        let b0 = regions_data[pos];
        let b1 = regions_data[pos + 1];
        let b2 = regions_data[pos + 2];
        pos += 3;
        while pos < regions_data.len() && regions_data[pos] != b'\n' {
            pos += 1;
        }
        if pos < regions_data.len() { pos += 1; }

        if b0 >= b'A' && b0 <= b'Z' && b1 >= b'A' && b1 <= b'Z' && b2 >= b'A' && b2 <= b'Z' {
            let code = ((b0 - b'A') as u16) * 676 + ((b1 - b'A') as u16) * 26 + ((b2 - b'A') as u16);
            seen[code as usize] = true;
            if rid < 1_000_000 {
                region_to_code[rid] = code;
            }
        }
    }

    let mut country_names: Vec<Vec<u8>> = Vec::with_capacity(300);
    let mut code_map = [u16::MAX; 17576];
    for idx in 0..17576 {
        if seen[idx] {
            let c0 = b'A' + (idx / 676) as u8;
            let c1 = b'A' + ((idx / 26) % 26) as u8;
            let c2 = b'A' + (idx % 26) as u8;
            let c_id = country_names.len() as u16;
            country_names.push(vec![c0, c1, c2]);
            code_map[idx] = c_id;
        }
    }
    let unmatched_id = country_names.len() as u16;
    country_names.push(b"_UNMATCHED".to_vec());
    let num_countries = country_names.len();

    let mut region_to_country: Vec<u16> = vec![unmatched_id; 1_000_000];
    for rid in 0..1_000_000 {
        let code = region_to_code[rid];
        if code != u16::MAX {
            region_to_country[rid] = code_map[code as usize];
        }
    }

    // 2. Memory-map records.csv
    let records_path = format!("{}/records.csv", data_dir);
    let file = match File::open(&records_path) {
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
        let _ = File::create(format!("{}/aggregates.txt", out_dir));
        return;
    }

    let ptr = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, file.as_raw_fd(), 0)
    };
    if ptr == usize::MAX as *mut u8 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let mut header_end = 0;
    if header_end + 3 <= len && &data[header_end..header_end + 3] == b"\xEF\xBB\xBF" {
        header_end += 3;
    }
    while header_end < len && data[header_end] != b'\n' {
        header_end += 1;
    }
    if header_end < len {
        header_end += 1;
    }

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(12);
    let mut boundaries = vec![header_end; num_threads + 1];
    let chunk_target = (len - header_end) / num_threads;

    for k in 1..num_threads {
        let target = header_end + k * chunk_target;
        let mut pos = target;
        while pos + 1 < len && !(data[pos] == b'\r' && data[pos + 1] == b'\n') {
            pos += 1;
        }
        if pos + 1 < len {
            pos += 2;
        }
        boundaries[k] = pos;
    }
    boundaries[num_threads] = len;

    // 3. Fast parallel records parse with zero-allocation flat buffer per thread
    let thread_outputs: Vec<(Vec<CountryScalarAgg>, Vec<u64>, Vec<(u32, u32)>)> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let region_to_country_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let mut local_names: Vec<u64> = Vec::with_capacity(350_000);
                let p = data.as_ptr();
                let mut pos = start;

                while pos < end {
                    unsafe {
                        if pos + 3 <= len && *p.add(pos) == 0xEF && *p.add(pos + 1) == 0xBB && *p.add(pos + 2) == 0xBF {
                            pos += 3;
                        }
                        if pos >= len {
                            break;
                        }
                        let b_lead = *p.add(pos);
                        if b_lead == b'\r' || b_lead == b'\n' {
                            pos += 1;
                            continue;
                        }

                        // 1. id
                        let mut id: u64 = 0;
                        while *p.add(pos) != b',' {
                            id = id * 10 + (*p.add(pos) - b'0') as u64;
                            pos += 1;
                        }
                        pos += 1; // skip comma

                        // 2. name (32-bit FNV-1a with length folding)
                        let mut n_hash: u32 = 0x811c9dc5;
                        let mut name_len: u32 = 0;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            loop {
                                let b = *p.add(pos);
                                if b == b'"' {
                                    if *p.add(pos + 1) == b'"' {
                                        n_hash = (n_hash ^ (b'"' as u32)).wrapping_mul(0x01000193);
                                        name_len += 1;
                                        pos += 2;
                                    } else {
                                        pos += 1;
                                        break;
                                    }
                                } else {
                                    n_hash = (n_hash ^ (b as u32)).wrapping_mul(0x01000193);
                                    name_len += 1;
                                    pos += 1;
                                }
                            }
                            if *p.add(pos) == b',' {
                                pos += 1;
                            }
                        } else {
                            while *p.add(pos) != b',' {
                                let b = *p.add(pos);
                                n_hash = (n_hash ^ (b as u32)).wrapping_mul(0x01000193);
                                name_len += 1;
                                pos += 1;
                            }
                            pos += 1; // skip comma
                        }
                        let n_hash_combined = ((name_len as u64) << 32) | (n_hash as u64);

                        // 3. note (fast skip & length computation)
                        let note_len: u64;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut escaped_quotes = 0u64;
                            loop {
                                let b = *p.add(pos);
                                if b == b'"' {
                                    if *p.add(pos + 1) == b'"' {
                                        escaped_quotes += 1;
                                        pos += 2;
                                    } else {
                                        note_len = (pos - n_start) as u64 - escaped_quotes;
                                        pos += 1;
                                        break;
                                    }
                                } else {
                                    pos += 1;
                                }
                            }
                            if *p.add(pos) == b',' {
                                pos += 1;
                            }
                        } else {
                            let n_start = pos;
                            while *p.add(pos) != b',' {
                                pos += 1;
                            }
                            note_len = (pos - n_start) as u64;
                            pos += 1; // skip comma
                        }

                        // 4. region_id
                        let mut has_region = false;
                        let mut region_id: usize = 0;
                        while *p.add(pos) != b',' {
                            let b = *p.add(pos);
                            if b >= b'0' && b <= b'9' {
                                has_region = true;
                                region_id = region_id * 10 + (b - b'0') as usize;
                            }
                            pos += 1;
                        }
                        pos += 1; // skip comma

                        // 5. amount
                        let mut is_neg = false;
                        if *p.add(pos) == b'-' {
                            is_neg = true;
                            pos += 1;
                        }
                        let mut cents: i64 = 0;
                        while *p.add(pos) != b'\r' && *p.add(pos) != b'\n' {
                            let b = *p.add(pos);
                            if b >= b'0' && b <= b'9' {
                                cents = cents * 10 + (b - b'0') as i64;
                            }
                            pos += 1;
                        }
                        if is_neg {
                            cents = -cents;
                        }
                        if *p.add(pos) == b'\r' {
                            pos += 1;
                        }
                        if pos < len && *p.add(pos) == b'\n' {
                            pos += 1;
                        }

                        let c_id = if has_region && region_id < region_to_country_ref.len() {
                            region_to_country_ref[region_id]
                        } else {
                            unmatched_id
                        };

                        let agg = &mut local_aggs[c_id as usize];
                        agg.count += 1;
                        agg.id_sum += id;
                        agg.amount_sum += cents;
                        agg.amount_min = agg.amount_min.min(cents);
                        agg.amount_max = agg.amount_max.max(cents);
                        agg.name_bytes += name_len as u64;
                        agg.note_bytes += note_len;

                        let packed = ((c_id as u64) << 48) | (n_hash_combined & 0x0000_FFFF_FFFF_FFFF);
                        local_names.push(packed);
                    }
                }
                local_names.sort_unstable();
                local_names.dedup();

                // Compute binary slice boundaries per country
                let mut offsets = vec![(0u32, 0u32); num_countries];
                let mut cur_idx = 0;
                for c_id in 0..num_countries {
                    let target_prefix = ((c_id as u64) + 1) << 48;
                    let next_idx = local_names[cur_idx..].partition_point(|&x| x < target_prefix) + cur_idx;
                    offsets[c_id] = (cur_idx as u32, next_idx as u32);
                    cur_idx = next_idx;
                }

                (local_aggs, local_names, offsets)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe {
        munmap(ptr, len);
    }

    // Merge scalar aggregates
    let mut country_aggs = vec![CountryScalarAgg::default(); num_countries];
    for (thread_agg, _, _) in &thread_outputs {
        for c_id in 0..num_countries {
            let t = &thread_agg[c_id];
            if t.count == 0 { continue; }
            let g = &mut country_aggs[c_id];
            g.count += t.count;
            g.id_sum += t.id_sum;
            g.amount_sum += t.amount_sum;
            g.amount_min = g.amount_min.min(t.amount_min);
            g.amount_max = g.amount_max.max(t.amount_max);
            g.name_bytes += t.name_bytes;
            g.note_bytes += t.note_bytes;
        }
    }

    // Parallel deduplication of name hashes per country
    let country_chunk = (num_countries + num_threads - 1) / num_threads;
    let distinct_names: Vec<usize> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_countries).step_by(country_chunk).map(|start| {
            let end = (start + country_chunk).min(num_countries);
            let thread_outputs_ref = &thread_outputs;

            s.spawn(move || {
                let mut local_distincts = Vec::with_capacity(end - start);
                let mut merged = Vec::with_capacity(30_000);
                for c_id in start..end {
                    merged.clear();
                    for t in 0..num_threads {
                        let (s_off, e_off) = thread_outputs_ref[t].2[c_id];
                        let slice = &thread_outputs_ref[t].1[s_off as usize..e_off as usize];
                        merged.extend_from_slice(slice);
                    }
                    if merged.is_empty() {
                        local_distincts.push(0);
                        continue;
                    }
                    merged.sort_unstable();
                    let distinct = 1 + merged.windows(2).filter(|w| w[0] != w[1]).count();
                    local_distincts.push(distinct);
                }
                local_distincts
            })
        }).collect();

        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });

    // Format output
    let mut out_buf = Vec::with_capacity(64 * 1024);
    for (c_id, country_bytes) in country_names.iter().enumerate() {
        let agg = &country_aggs[c_id];
        if agg.count == 0 {
            continue;
        }
        let country_str = std::str::from_utf8(country_bytes).unwrap();
        let amt_sum = format_cents(agg.amount_sum);
        let amt_min = format_cents(agg.amount_min);
        let amt_max = format_cents(agg.amount_max);
        let distinct = distinct_names[c_id];

        writeln!(
            out_buf,
            "{} {} {} {} {} {} {} {} {}",
            country_str,
            agg.count,
            agg.id_sum,
            amt_sum,
            amt_min,
            amt_max,
            distinct,
            agg.name_bytes,
            agg.note_bytes
        ).unwrap();
    }

    fs::create_dir_all(&out_dir).unwrap();
    let out_file_path = format!("{}/aggregates.txt", out_dir);
    let mut out_file = File::create(&out_file_path).unwrap();
    out_file.write_all(&out_buf).unwrap();
}
