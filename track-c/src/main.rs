use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8, _mm_crc32_u16};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

#[inline(always)]
fn find_quote_swar(w: u64) -> Option<usize> {
    let x = w ^ 0x22222222_22222222;
    let m = (x.wrapping_sub(0x01010101_01010101)) & !x & 0x80808080_80808080;
    if m != 0 {
        Some((m.trailing_zeros() >> 3) as usize)
    } else {
        None
    }
}

#[inline(always)]
unsafe fn crc32_hardware(crc: u32, p: *const u8, len: usize) -> u32 {
    let mut c = crc as u64;
    let mut i = 0;
    while i + 8 <= len {
        let val = std::ptr::read_unaligned(p.add(i) as *const u64);
        c = _mm_crc32_u64(c, val);
        i += 8;
    }
    let mut c32 = c as u32;
    while i < len {
        c32 = _mm_crc32_u8(c32, *p.add(i));
        i += 1;
    }
    c32
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

#[inline(always)]
fn write_u64_fast(mut n: u64, out: &mut Vec<u8>) {
    if n == 0 { out.push(b'0'); return; }
    let mut buf = [0u8; 24];
    let mut i = 0;
    while n > 0 { buf[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
    while i > 0 { i -= 1; out.push(buf[i]); }
}

#[inline(always)]
fn write_cents_fast(c: i64, out: &mut Vec<u8>) {
    if c < 0 { out.push(b'-'); }
    let abs_c = c.abs() as u64;
    let dollars = abs_c / 100;
    let cents = abs_c % 100;
    write_u64_fast(dollars, out);
    out.push(b'.');
    out.push(b'0' + (cents / 10) as u8);
    out.push(b'0' + (cents % 10) as u8);
}

fn main() {
    unsafe { mallopt(-8, 2); }

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

    let regions_path = format!("{}/regions.csv", data_dir);
    let regions_file = match File::open(&regions_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let r_len = match regions_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    if r_len == 0 { return; }
    let r_ptr = unsafe { mmap(std::ptr::null_mut(), r_len, 1, 1, regions_file.as_raw_fd(), 0) };
    if r_ptr == usize::MAX as *mut u8 { return; }
    let regions_data = unsafe { std::slice::from_raw_parts(r_ptr, r_len) };

    let mut seen = [false; 17576];
    let mut pos = 0;
    while pos < regions_data.len() && regions_data[pos] != b'\n' { pos += 1; }
    if pos < regions_data.len() { pos += 1; }

    let body_start = pos;
    while pos < regions_data.len() {
        while pos < regions_data.len() && regions_data[pos] != b',' { pos += 1; }
        if pos >= regions_data.len() { break; }
        pos += 1;
        let b0 = regions_data[pos];
        let b1 = regions_data[pos + 1];
        let b2 = regions_data[pos + 2];
        pos += 3;
        while pos < regions_data.len() && regions_data[pos] != b'\n' { pos += 1; }
        if pos < regions_data.len() { pos += 1; }

        if b0 >= b'A' && b0 <= b'Z' && b1 >= b'A' && b1 <= b'Z' && b2 >= b'A' && b2 <= b'Z' {
            let code = ((b0 - b'A') as u16) * 676 + ((b1 - b'A') as u16) * 26 + ((b2 - b'A') as u16);
            seen[code as usize] = true;
        }
    }

    let mut country_names: Vec<Vec<u8>> = Vec::with_capacity(300);
    let mut code_map = [u8::MAX; 17576];
    for idx in 0..17576 {
        if seen[idx] {
            let c0 = b'A' + (idx / 676) as u8;
            let c1 = b'A' + ((idx / 26) % 26) as u8;
            let c2 = b'A' + (idx % 26) as u8;
            let c_id = country_names.len() as u8;
            country_names.push(vec![c0, c1, c2]);
            code_map[idx] = c_id;
        }
    }
    let unmatched_id = country_names.len() as u8;
    country_names.push(b"_UNMATCHED".to_vec());
    let num_countries = country_names.len();

    let mut region_to_country: Vec<u8> = vec![unmatched_id; 1_000_000];
    pos = body_start;
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
        pos += 1;
        let b0 = regions_data[pos];
        let b1 = regions_data[pos + 1];
        let b2 = regions_data[pos + 2];
        pos += 3;
        while pos < regions_data.len() && regions_data[pos] != b'\n' { pos += 1; }
        if pos < regions_data.len() { pos += 1; }

        if b0 >= b'A' && b0 <= b'Z' && b1 >= b'A' && b1 <= b'Z' && b2 >= b'A' && b2 <= b'Z' {
            let code = ((b0 - b'A') as u16) * 676 + ((b1 - b'A') as u16) * 26 + ((b2 - b'A') as u16);
            let c_id = code_map[code as usize];
            if rid < region_to_country.len() {
                region_to_country[rid] = c_id;
            }
        }
    }
    unsafe { munmap(r_ptr, r_len); }

    let rec_path = format!("{}/records.csv", data_dir);
    let file = match File::open(&rec_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = match file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    if len == 0 { return; }
    let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, file.as_raw_fd(), 0) };
    if ptr == usize::MAX as *mut u8 { return; }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let mut header_end = 0;
    if header_end + 3 <= len && &data[header_end..header_end + 3] == b"\xEF\xBB\xBF" {
        header_end += 3;
    }
    while header_end < len && data[header_end] != b'\n' { header_end += 1; }
    if header_end < len { header_end += 1; }

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(12);
    let mut boundaries = vec![header_end; num_threads + 1];
    let chunk_target = (len - header_end) / num_threads;

    for k in 1..num_threads {
        let target = header_end + k * chunk_target;
        let mut p = target;
        while p + 1 < len && !(data[p] == b'\r' && data[p + 1] == b'\n') {
            p += 1;
        }
        if p + 1 < len { p += 2; }
        boundaries[k] = p;
    }
    boundaries[num_threads] = len;

    // Phase 1: Parse records, accumulate scalar aggs, and bucket name CRCs per country
    type ThreadResult = (Vec<CountryScalarAgg>, Vec<Vec<u32>>);
    let thread_results: Vec<ThreadResult> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let region_to_country_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let mut country_crcs: Vec<Vec<u32>> = (0..num_countries).map(|_| Vec::with_capacity(1200)).collect();

                let p = data.as_ptr();
                let mut pos = start;
                let mut last_madv_pos = start;

                while pos < end {
                    unsafe {
                        if pos + 3 <= len && *p.add(pos) == 0xEF && *p.add(pos + 1) == 0xBB && *p.add(pos + 2) == 0xBF {
                            pos += 3;
                        }
                        if pos >= len { break; }
                        let b_lead = *p.add(pos);
                        if b_lead == b'\r' || b_lead == b'\n' {
                            pos += 1;
                            continue;
                        }

                        let mut id: u64 = 0;
                        while *p.add(pos) != b',' {
                            id = id * 10 + (*p.add(pos) - b'0') as u64;
                            pos += 1;
                        }
                        pos += 1;

                        let name_crc: u32;
                        let name_len: u32;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut crc: u32 = 0;
                            let mut escaped_quotes: u32 = 0;
                            loop {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_quote_swar(w) {
                                    crc = crc32_hardware(crc, p.add(pos), off);
                                    let q = pos + off;
                                    if *p.add(q + 1) == b'"' {
                                        crc = _mm_crc32_u8(crc, b'"');
                                        escaped_quotes += 1;
                                        pos = q + 2;
                                    } else {
                                        name_len = (q - n_start) as u32 - escaped_quotes;
                                        pos = q + 1;
                                        break;
                                    }
                                } else {
                                    crc = crc32_hardware(crc, p.add(pos), 8);
                                    pos += 8;
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                            name_crc = _mm_crc32_u16(crc, name_len as u16);
                        } else {
                            let n_start = pos;
                            let mut crc: u32 = 0;
                            while *p.add(pos) != b',' {
                                crc = _mm_crc32_u8(crc, *p.add(pos));
                                pos += 1;
                            }
                            name_len = (pos - n_start) as u32;
                            pos += 1;
                            name_crc = _mm_crc32_u16(crc, name_len as u16);
                        }

                        let note_len: u64;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut escaped_quotes: u64 = 0;
                            let mut curr = pos;
                            loop {
                                if curr + 8 <= end {
                                    let w = (p.add(curr) as *const u64).read_unaligned();
                                    if let Some(off) = find_quote_swar(w) {
                                        let q = curr + off;
                                        if *p.add(q + 1) == b'"' {
                                            escaped_quotes += 1;
                                            curr = q + 2;
                                        } else {
                                            note_len = (q - n_start) as u64 - escaped_quotes;
                                            pos = q + 1;
                                            break;
                                        }
                                    } else {
                                        curr += 8;
                                    }
                                } else {
                                    while *p.add(curr) != b'"' { curr += 1; }
                                    if *p.add(curr + 1) == b'"' {
                                        escaped_quotes += 1;
                                        curr += 2;
                                    } else {
                                        note_len = (curr - n_start) as u64 - escaped_quotes;
                                        pos = curr + 1;
                                        break;
                                    }
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                        } else {
                            let n_start = pos;
                            while *p.add(pos) != b',' { pos += 1; }
                            note_len = (pos - n_start) as u64;
                            pos += 1;
                        }

                        let mut region_id: usize = 0;
                        let mut has_region = false;
                        while *p.add(pos) != b',' {
                            has_region = true;
                            region_id = region_id * 10 + (*p.add(pos) - b'0') as usize;
                            pos += 1;
                        }
                        pos += 1;

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
                        if is_neg { cents = -cents; }
                        if *p.add(pos) == b'\r' { pos += 1; }
                        if pos < len && *p.add(pos) == b'\n' { pos += 1; }

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

                        country_crcs[c_id as usize].push(name_crc);

                        if pos - last_madv_pos >= 2 * 1024 * 1024 {
                            let madv_len = (pos - last_madv_pos) & !4095;
                            madvise(p.add(last_madv_pos) as *mut _, madv_len, 4);
                            last_madv_pos += madv_len;
                        }
                    }
                }

                unsafe {
                    if end > last_madv_pos {
                        madvise(p.add(last_madv_pos) as *mut _, end - last_madv_pos, 4);
                    }
                }

                (local_aggs, country_crcs)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe { munmap(ptr, len); }

    // Parallel Country Consolidation & Distinct Count
    let country_chunk = (num_countries + num_threads - 1) / num_threads;
    let formatted_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_countries).step_by(country_chunk).map(|c_start| {
            let c_end = (c_start + country_chunk).min(num_countries);
            let thread_results_ref = &thread_results;
            let country_names_ref = &country_names;

            s.spawn(move || {
                let mut out = Vec::with_capacity((c_end - c_start) * 128);
                let mut scratch = Vec::with_capacity(32_768);

                for c_id in c_start..c_end {
                    let mut count = 0u64;
                    let mut id_sum = 0u64;
                    let mut amount_sum = 0i64;
                    let mut amount_min = i64::MAX;
                    let mut amount_max = i64::MIN;
                    let mut name_bytes = 0u64;
                    let mut note_bytes = 0u64;

                    scratch.clear();

                    for t in 0..num_threads {
                        let agg = &thread_results_ref[t].0[c_id];
                        if agg.count > 0 {
                            count += agg.count;
                            id_sum += agg.id_sum;
                            amount_sum += agg.amount_sum;
                            amount_min = amount_min.min(agg.amount_min);
                            amount_max = amount_max.max(agg.amount_max);
                            name_bytes += agg.name_bytes;
                            note_bytes += agg.note_bytes;
                            scratch.extend_from_slice(&thread_results_ref[t].1[c_id]);
                        }
                    }

                    if count == 0 { continue; }

                    scratch.sort_unstable();
                    let mut distinct = 0usize;
                    let mut last = u32::MAX;
                    for &crc in &scratch {
                        if crc != last {
                            distinct += 1;
                            last = crc;
                        }
                    }

                    // Format line
                    let c_name = &country_names_ref[c_id];
                    out.extend_from_slice(c_name);
                    out.push(b' ');
                    write_u64_fast(count, &mut out);
                    out.push(b' ');
                    write_u64_fast(id_sum, &mut out);
                    out.push(b' ');
                    write_cents_fast(amount_sum, &mut out);
                    out.push(b' ');
                    write_cents_fast(amount_min, &mut out);
                    out.push(b' ');
                    write_cents_fast(amount_max, &mut out);
                    out.push(b' ');
                    write_u64_fast(distinct as u64, &mut out);
                    out.push(b' ');
                    write_u64_fast(name_bytes, &mut out);
                    out.push(b' ');
                    write_u64_fast(note_bytes, &mut out);
                    out.push(b'\n');
                }
                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(format!("{}/aggregates.txt", out_dir)) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in &formatted_outputs {
        let _ = out_file.write_all(chunk);
    }
}

