use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::time::Instant;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8, _mm_crc32_u16};

const MADV_DONTNEED: i32 = 4;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
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
fn find_record_boundary(data: &[u8], mut p: usize) -> usize {
    let len = data.len();
    while p + 8 < len {
        if data[p] == b'\n' {
            let prev = if p > 0 && data[p - 1] == b'\r' { p - 1 } else { p };
            if prev >= 3 && data[prev - 3] == b'.' && data[prev - 2].is_ascii_digit() && data[prev - 1].is_ascii_digit() {
                let mut next = p + 1;
                while next < len && data[next].is_ascii_digit() { next += 1; }
                if next < len && data[next] == b',' && next > p + 1 {
                    return p + 1;
                }
            }
        }
        p += 1;
    }
    len
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
    let mut temp = [0u8; 24];
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
unsafe fn write_cents_fast(c: i64, buf: *mut u8) -> usize {
    let mut p = 0;
    if c < 0 {
        *buf.add(p) = b'-';
        p += 1;
    }
    let abs_c = c.abs() as u64;
    let dollars = abs_c / 100;
    let cents = (abs_c % 100) as usize * 2;
    p += write_u64_fast(dollars, buf.add(p));
    *buf.add(p) = b'.';
    p += 1;
    *buf.add(p) = DIGIT_PAIRS[cents];
    *buf.add(p + 1) = DIGIT_PAIRS[cents + 1];
    p + 2
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
fn find_comma_swar(w: u64) -> Option<usize> {
    let x = w ^ 0x2C2C2C2C_2C2C2C2C;
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

fn main() {
    let t0 = Instant::now();
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
        } else if !args[arg_i].starts_with("--") {
            if data_dir == "/data" { data_dir = args[arg_i].clone(); }
            else if out_dir == "/out" { out_dir = args[arg_i].clone(); }
            arg_i += 1;
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

    let num_threads = 12;
    // Fast parallel boundary finding
    let chunk_target = (len - header_end) / num_threads;
    let mut boundaries = vec![header_end; num_threads + 1];
    boundaries[num_threads] = len;
    for k in 1..num_threads {
        boundaries[k] = find_record_boundary(data, header_end + k * chunk_target);
    }

    struct ThreadResult {
        aggs: Vec<CountryScalarAgg>,
        records: Vec<u64>, // packed ((c_id as u64) << 32) | (name_crc as u64)
    }

    let thread_results: Vec<ThreadResult> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let region_to_country_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let est_cap = (end - start) / 48 + 4096;
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let mut records = Vec::with_capacity(est_cap);

                let p = data.as_ptr();
                let mut pos = start;
                let mut last_adv = start;

                while pos < end {
                    if pos - last_adv >= 2 * 1024 * 1024 {
                        unsafe { madvise_dontneed(p.add(last_adv), pos - last_adv); }
                        last_adv = pos;
                    }
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

                        let (name_crc, name_len): (u32, u32) = if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut crc: u32 = 0;
                            let mut escaped_quotes: u32 = 0;
                            let n_len: u32;
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
                                        n_len = (q - n_start) as u32 - escaped_quotes;
                                        pos = q + 1;
                                        break;
                                    }
                                } else {
                                    crc = crc32_hardware(crc, p.add(pos), 8);
                                    pos += 8;
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                            (_mm_crc32_u16(crc, n_len as u16), n_len)
                        } else {
                            let n_start = pos;
                            let mut crc: u32 = 0;
                            while pos + 8 <= end {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_comma_swar(w) {
                                    crc = crc32_hardware(crc, p.add(pos), off);
                                    pos += off;
                                    break;
                                } else {
                                    crc = crc32_hardware(crc, p.add(pos), 8);
                                    pos += 8;
                                }
                            }
                            while pos < end && *p.add(pos) != b',' {
                                crc = _mm_crc32_u8(crc, *p.add(pos));
                                pos += 1;
                            }
                            let n_len = (pos - n_start) as u32;
                            pos += 1;
                            (_mm_crc32_u16(crc, n_len as u16), n_len)
                        };

                        let note_len: u64;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let mut escaped_quotes: u64 = 0;
                            let n_start = pos;
                            loop {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_quote_swar(w) {
                                    let q = pos + off;
                                    if *p.add(q + 1) == b'"' {
                                        escaped_quotes += 1;
                                        pos = q + 2;
                                    } else {
                                        note_len = (q - n_start) as u64 - escaped_quotes;
                                        pos = q + 1;
                                        break;
                                    }
                                } else {
                                    pos += 8;
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                        } else {
                            let n_start = pos;
                            while pos + 8 <= end {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_comma_swar(w) {
                                    pos += off;
                                    break;
                                } else {
                                    pos += 8;
                                }
                            }
                            while pos < end && *p.add(pos) != b',' { pos += 1; }
                            note_len = (pos - n_start) as u64;
                            pos += 1;
                        }

                        let mut region_id = 0usize;
                        let mut has_region = false;
                        while *p.add(pos) != b',' {
                            let b = *p.add(pos);
                            if b >= b'0' && b <= b'9' {
                                region_id = region_id * 10 + (b - b'0') as usize;
                                has_region = true;
                            }
                            pos += 1;
                        }
                        pos += 1;

                        let is_neg = *p.add(pos) == b'-';
                        if is_neg { pos += 1; }
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

                        records.push(((c_id as u64) << 32) | (name_crc as u64));
                    }
                }

                if end > last_adv {
                    unsafe { madvise_dontneed(p.add(last_adv), end - last_adv); }
                }

                ThreadResult { aggs: local_aggs, records }
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe { munmap(ptr, len); }

    // Merge scalar aggregates and compute total count per country
    let mut final_aggs = vec![CountryScalarAgg::default(); num_countries];
    let mut country_offsets = vec![0u32; num_countries + 1];
    for t in 0..num_threads {
        for c in 0..num_countries {
            let src = &thread_results[t].aggs[c];
            let dst = &mut final_aggs[c];
            dst.count += src.count;
            dst.id_sum += src.id_sum;
            dst.amount_sum += src.amount_sum;
            dst.amount_min = dst.amount_min.min(src.amount_min);
            dst.amount_max = dst.amount_max.max(src.amount_max);
            dst.name_bytes += src.name_bytes;
            dst.note_bytes += src.note_bytes;
            country_offsets[c + 1] += src.count as u32;
        }
    }

    // Prefix sum to compute write offsets for each country
    for c in 0..num_countries {
        country_offsets[c + 1] += country_offsets[c];
    }

    let total_records = country_offsets[num_countries] as usize;
    let mut all_crcs = vec![0u32; total_records];
    let all_crcs_ptr = all_crcs.as_mut_ptr() as usize;

    // Parallel scatter of CRCs into contiguous country slices
    // Each thread gets write cursor per country
    std::thread::scope(|s| {
        // Precompute write offsets for each (thread, country)
        let mut thread_cursors: Vec<Vec<u32>> = Vec::with_capacity(num_threads);
        let mut cur_offsets = country_offsets[..num_countries].to_vec();
        for t in 0..num_threads {
            thread_cursors.push(cur_offsets.clone());
            for c in 0..num_countries {
                cur_offsets[c] += thread_results[t].aggs[c].count as u32;
            }
        }

        for t in 0..num_threads {
            let mut cursors = thread_cursors[t].clone();
            let records_ref = &thread_results[t].records;
            s.spawn(move || {
                let dst_base = all_crcs_ptr as *mut u32;
                for &rec in records_ref {
                    let c_id = (rec >> 32) as usize;
                    let crc = rec as u32;
                    let dst_idx = cursors[c_id];
                    cursors[c_id] += 1;
                    unsafe { *dst_base.add(dst_idx as usize) = crc; }
                }
            });
        }
    });

    drop(thread_results);

    // Parallel sort of CRCs and distinct count formatting
    let country_chunk = (num_countries + num_threads - 1) / num_threads;
    let all_crcs_slice = &mut all_crcs[..];
    let all_crcs_ref = all_crcs_slice.as_mut_ptr() as usize;

    let formatted_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_countries).step_by(country_chunk).map(|c_start| {
            let c_end = (c_start + country_chunk).min(num_countries);
            let offsets_ref = &country_offsets;
            let aggs_ref = &final_aggs;
            let names_ref = &country_names;

            s.spawn(move || {
                let mut out = Vec::with_capacity((c_end - c_start) * 128);
                let mut num_buf = [0u8; 32];
                let base_ptr = all_crcs_ref as *mut u32;

                for c_id in c_start..c_end {
                    let agg = &aggs_ref[c_id];
                    if agg.count == 0 { continue; }

                    let s_idx = offsets_ref[c_id] as usize;
                    let e_idx = offsets_ref[c_id + 1] as usize;
                    let slice = unsafe { std::slice::from_raw_parts_mut(base_ptr.add(s_idx), e_idx - s_idx) };
                    slice.sort_unstable();

                    let mut distinct = 0u64;
                    let mut last = u32::MAX;
                    for &crc in slice.iter() {
                        if crc != last {
                            distinct += 1;
                            last = crc;
                        }
                    }

                    out.extend_from_slice(&names_ref[c_id]);
                    out.push(b' ');

                    let w = unsafe { write_u64_fast(agg.count, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');

                    let w = unsafe { write_u64_fast(agg.id_sum, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');

                    let w = unsafe { write_cents_fast(agg.amount_sum, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');

                    let w = unsafe { write_cents_fast(agg.amount_min, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');

                    let w = unsafe { write_cents_fast(agg.amount_max, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');

                    let w = unsafe { write_u64_fast(distinct, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');

                    let w = unsafe { write_u64_fast(agg.name_bytes, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');

                    let w = unsafe { write_u64_fast(agg.note_bytes, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b'\n');
                }

                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let results_path = format!("{}/aggregates.txt", out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        for part in &formatted_outputs {
            let _ = out_file.write_all(part);
        }
    }

    println!("Track C Completed in {:.3}s", t0.elapsed().as_secs_f64());
}
