use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[derive(Default, Clone, Copy)]
struct CountryScalarAgg {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: Option<i64>,
    amount_max: Option<i64>,
    name_bytes: u64,
    note_bytes: u64,
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

    // SINGLE PASS: Parse records.csv and collect scalar aggs + name hashes per country
    let thread_outputs: Vec<(Vec<CountryScalarAgg>, Vec<Vec<u64>>)> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let region_to_country_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let mut local_names: Vec<Vec<u64>> = (0..num_countries).map(|_| Vec::new()).collect();
                let mut pos = start;

                while pos < end {
                    if pos + 3 <= len && &data[pos..pos + 3] == b"\xEF\xBB\xBF" {
                        pos += 3;
                    }
                    if pos >= len {
                        break;
                    }
                    if data[pos] == b'\r' || data[pos] == b'\n' {
                        pos += 1;
                        continue;
                    }

                    // 1. id
                    let mut id: u64 = 0;
                    while pos < len && data[pos] != b',' {
                        id = id * 10 + (data[pos] - b'0') as u64;
                        pos += 1;
                    }
                    if pos < len && data[pos] == b',' {
                        pos += 1;
                    }

                    // 2. name
                    let mut n_hash: u64 = 0xcbf29ce484222325;
                    let mut name_len: u64 = 0;
                    if pos < len && data[pos] == b'"' {
                        pos += 1;
                        while pos < len {
                            let b = data[pos];
                            if b == b'"' {
                                if pos + 1 < len && data[pos + 1] == b'"' {
                                    n_hash = (n_hash ^ (b'"' as u64)).wrapping_mul(0x100000001b3);
                                    name_len += 1;
                                    pos += 2;
                                } else {
                                    pos += 1;
                                    break;
                                }
                            } else {
                                n_hash = (n_hash ^ (b as u64)).wrapping_mul(0x100000001b3);
                                name_len += 1;
                                pos += 1;
                            }
                        }
                        if pos < len && data[pos] == b',' {
                            pos += 1;
                        }
                    } else {
                        while pos < len && data[pos] != b',' {
                            let b = data[pos];
                            n_hash = (n_hash ^ (b as u64)).wrapping_mul(0x100000001b3);
                            name_len += 1;
                            pos += 1;
                        }
                        if pos < len && data[pos] == b',' {
                            pos += 1;
                        }
                    }
                    let n_hash = n_hash ^ (name_len << 48);

                    // 3. note
                    let mut note_len: u64 = 0;
                    if pos < len && data[pos] == b'"' {
                        pos += 1;
                        while pos < len {
                            let b = data[pos];
                            if b == b'"' {
                                if pos + 1 < len && data[pos + 1] == b'"' {
                                    note_len += 1;
                                    pos += 2;
                                } else {
                                    pos += 1;
                                    break;
                                }
                            } else {
                                note_len += 1;
                                pos += 1;
                            }
                        }
                        if pos < len && data[pos] == b',' {
                            pos += 1;
                        }
                    } else {
                        while pos < len && data[pos] != b',' {
                            note_len += 1;
                            pos += 1;
                        }
                        if pos < len && data[pos] == b',' {
                            pos += 1;
                        }
                    }

                    // 4. region_id
                    let mut has_region = false;
                    let mut region_id: usize = 0;
                    while pos < len && data[pos] != b',' {
                        if data[pos] >= b'0' && data[pos] <= b'9' {
                            has_region = true;
                            region_id = region_id * 10 + (data[pos] - b'0') as usize;
                        }
                        pos += 1;
                    }
                    if pos < len && data[pos] == b',' {
                        pos += 1;
                    }

                    // 5. amount (-?[0-9]+\.[0-9]{2})
                    let mut is_neg = false;
                    if pos < len && data[pos] == b'-' {
                        is_neg = true;
                        pos += 1;
                    }
                    let mut cents: i64 = 0;
                    while pos < len && data[pos] != b'\r' && data[pos] != b'\n' {
                        let b = data[pos];
                        if b >= b'0' && b <= b'9' {
                            cents = cents * 10 + (b - b'0') as i64;
                        }
                        pos += 1;
                    }
                    if is_neg {
                        cents = -cents;
                    }
                    if pos < len && data[pos] == b'\r' {
                        pos += 1;
                    }
                    if pos < len && data[pos] == b'\n' {
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
                    agg.amount_min = Some(match agg.amount_min {
                        Some(cur) => cur.min(cents),
                        None => cents,
                    });
                    agg.amount_max = Some(match agg.amount_max {
                        Some(cur) => cur.max(cents),
                        None => cents,
                    });
                    agg.name_bytes += name_len;
                    agg.note_bytes += note_len;

                    local_names[c_id as usize].push(n_hash);
                }
                (local_aggs, local_names)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe {
        munmap(ptr, len);
    }

    // Merge scalar aggregates
    let mut country_aggs = vec![CountryScalarAgg::default(); num_countries];
    for (thread_agg, _) in &thread_outputs {
        for c_id in 0..num_countries {
            let t = &thread_agg[c_id];
            if t.count == 0 { continue; }
            let g = &mut country_aggs[c_id];
            g.count += t.count;
            g.id_sum += t.id_sum;
            g.amount_sum += t.amount_sum;
            g.amount_min = Some(match g.amount_min {
                Some(cur) => match t.amount_min {
                    Some(t_min) => cur.min(t_min),
                    None => cur,
                },
                None => t.amount_min.unwrap_or(0),
            });
            g.amount_max = Some(match g.amount_max {
                Some(cur) => match t.amount_max {
                    Some(t_max) => cur.max(t_max),
                    None => cur,
                },
                None => t.amount_max.unwrap_or(0),
            });
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
                for c_id in start..end {
                    let mut total_len = 0;
                    for t in 0..num_threads {
                        total_len += thread_outputs_ref[t].1[c_id].len();
                    }
                    if total_len == 0 {
                        local_distincts.push(0);
                        continue;
                    }
                    let mut merged = Vec::with_capacity(total_len);
                    for t in 0..num_threads {
                        merged.extend_from_slice(&thread_outputs_ref[t].1[c_id]);
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
        let amt_min = format_cents(agg.amount_min.unwrap_or(0));
        let amt_max = format_cents(agg.amount_max.unwrap_or(0));
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
