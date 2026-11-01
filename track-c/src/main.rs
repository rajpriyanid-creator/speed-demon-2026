use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

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
fn find_quote_swar(w: u64) -> Option<usize> {
    let x = w ^ 0x22222222_22222222;
    let m = (x.wrapping_sub(0x01010101_01010101)) & !x & 0x80808080_80808080;
    if m != 0 { Some((m.trailing_zeros() >> 3) as usize) } else { None }
}

#[inline(always)]
fn find_comma_swar(w: u64) -> Option<usize> {
    let x = w ^ 0x2C2C2C2C_2C2C2C2C;
    let m = (x.wrapping_sub(0x01010101_01010101)) & !x & 0x80808080_80808080;
    if m != 0 { Some((m.trailing_zeros() >> 3) as usize) } else { None }
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
    let mut len = 0;
    let abs_c = if c < 0 {
        *buf = b'-';
        len = 1;
        (-c) as u64
    } else {
        c as u64
    };
    let dollars = abs_c / 100;
    let cents = (abs_c % 100) as usize;
    len += write_u64_fast(dollars, buf.add(len));
    *buf.add(len) = b'.';
    *buf.add(len + 1) = DIGIT_PAIRS[cents * 2];
    *buf.add(len + 2) = DIGIT_PAIRS[cents * 2 + 1];
    len + 3
}

fn main() {
    unsafe { mallopt(-8, 2); }

    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut positional = Vec::new();
    let mut arg_i = 1;
    while arg_i < args.len() {
        if args[arg_i] == "--data" && arg_i + 1 < args.len() {
            data_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if args[arg_i] == "--out" && arg_i + 1 < args.len() {
            out_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if !args[arg_i].starts_with("--") {
            positional.push(args[arg_i].clone());
            arg_i += 1;
        } else {
            arg_i += 1;
        }
    }
    if positional.len() >= 1 && data_dir == "/data" { data_dir = positional[0].clone(); }
    if positional.len() >= 2 && out_dir == "/out" { out_dir = positional[1].clone(); }

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
    if r_ptr == usize::MAX as *mut u8 || r_ptr.is_null() { return; }
    let regions_data = unsafe { std::slice::from_raw_parts(r_ptr, r_len) };

    let mut body_start = 0;
    while body_start < r_len && regions_data[body_start] != b'\n' { body_start += 1; }
    if body_start < r_len { body_start += 1; }

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(12);

    let reg_chunk = (r_len - body_start) / num_threads;
    let mut reg_bounds = vec![body_start; num_threads + 1];
    for k in 1..num_threads {
        let mut p = body_start + k * reg_chunk;
        while p < r_len && regions_data[p] != b'\n' { p += 1; }
        if p < r_len { p += 1; }
        reg_bounds[k] = p;
    }
    reg_bounds[num_threads] = r_len;

    let mut seen = [false; 17576];
    let mut p = body_start;
    while p < r_len {
        while p < r_len && regions_data[p] != b',' { p += 1; }
        if p >= r_len { break; }
        p += 1;
        let b0 = regions_data[p];
        let b1 = regions_data[p + 1];
        let b2 = regions_data[p + 2];
        p += 3;
        while p < r_len && regions_data[p] != b'\n' { p += 1; }
        if p < r_len { p += 1; }
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
    let reg_to_country_ptr = region_to_country.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for k in 0..num_threads {
            let start = reg_bounds[k];
            let end = reg_bounds[k + 1];
            let code_map_ref = &code_map;
            s.spawn(move || {
                let target_ptr = reg_to_country_ptr as *mut u8;
                let mut pos = start;
                let rdata = regions_data;
                while pos < end {
                    let mut rid = 0usize;
                    while pos < end && rdata[pos] != b',' {
                        let b = rdata[pos];
                        if b >= b'0' && b <= b'9' { rid = rid * 10 + (b - b'0') as usize; }
                        pos += 1;
                    }
                    if pos >= end { break; }
                    pos += 1;
                    let b0 = rdata[pos];
                    let b1 = rdata[pos + 1];
                    let b2 = rdata[pos + 2];
                    pos += 3;
                    while pos < end && rdata[pos] != b'\n' { pos += 1; }
                    if pos < end { pos += 1; }
                    if b0 >= b'A' && b0 <= b'Z' && b1 >= b'A' && b1 <= b'Z' && b2 >= b'A' && b2 <= b'Z' {
                        let code = ((b0 - b'A') as u16) * 676 + ((b1 - b'A') as u16) * 26 + ((b2 - b'A') as u16);
                        let c_id = code_map_ref[code as usize];
                        if rid < 1_000_000 {
                            unsafe { *target_ptr.add(rid) = c_id; }
                        }
                    }
                }
            });
        }
    });

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
    if ptr == usize::MAX as *mut u8 || ptr.is_null() { return; }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let mut header_end = 0;
    if header_end + 3 <= len && &data[header_end..header_end + 3] == b"\xEF\xBB\xBF" { header_end += 3; }
    while header_end < len && data[header_end] != b'\n' { header_end += 1; }
    if header_end < len { header_end += 1; }

    let mut boundaries = vec![header_end; num_threads + 1];
    let chunk_target = (len - header_end) / num_threads;
    for k in 1..num_threads {
        let target = header_end + k * chunk_target;
        let mut p = target;
        while p + 1 < len && !(data[p] == b'\r' && data[p + 1] == b'\n') { p += 1; }
        if p + 1 < len { p += 2; }
        boundaries[k] = p;
    }
    boundaries[num_threads] = len;

    type ThreadResult = (Vec<CountryScalarAgg>, Vec<u32>, [u32; 256]);
    let thread_results: Vec<ThreadResult> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let reg_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let est_records = (end - start) / 50 + 2048;
                let mut flat_country = Vec::with_capacity(est_records);
                let mut flat_crc = Vec::with_capacity(est_records);
                let mut c_counts = [0u32; 256];

                let p = data.as_ptr();
                let mut pos = start;
                let mut last_adv = start;

                while pos < end {
                    if pos - last_adv >= 128 * 1024 {
                        unsafe { madvise_dontneed(p.add(last_adv), pos - last_adv); }
                        last_adv = pos;
                    }
                    unsafe {
                        if pos + 3 <= len && *p.add(pos) == 0xEF && *p.add(pos + 1) == 0xBB && *p.add(pos + 2) == 0xBF { pos += 3; }
                        if pos >= len { break; }
                        let b_lead = *p.add(pos);
                        if b_lead == b'\r' || b_lead == b'\n' { pos += 1; continue; }
                        let mut id: u64 = 0;
                        while *p.add(pos) != b',' { id = id * 10 + (*p.add(pos) - b'0') as u64; pos += 1; }
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
                            name_len = (pos - n_start) as u32;
                            pos += 1;
                            name_crc = _mm_crc32_u16(crc, name_len as u16);
                        }

                        let mut note_len: u64 = 0;
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
                                    } else { curr += 8; }
                                } else {
                                    while curr < end {
                                        if *p.add(curr) == b'"' {
                                            if curr + 1 < end && *p.add(curr + 1) == b'"' { escaped_quotes += 1; curr += 2; }
                                            else { note_len = (curr - n_start) as u64 - escaped_quotes; pos = curr + 1; break; }
                                        } else { curr += 1; }
                                    }
                                    break;
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                        } else {
                            let n_start = pos;
                            while pos + 8 <= end {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_comma_swar(w) { pos += off; break; }
                                else { pos += 8; }
                            }
                            while pos < end && *p.add(pos) != b',' { pos += 1; }
                            note_len = (pos - n_start) as u64;
                            pos += 1;
                        }

                        let mut region_id = 0usize;
                        let mut has_region = false;
                        while *p.add(pos) != b',' {
                            let b = *p.add(pos);
                            if b >= b'0' && b <= b'9' { region_id = region_id * 10 + (b - b'0') as usize; has_region = true; }
                            pos += 1;
                        }
                        pos += 1;

                        let is_neg = *p.add(pos) == b'-';
                        if is_neg { pos += 1; }
                        let mut cents: i64 = 0;
                        while *p.add(pos) != b'\r' && *p.add(pos) != b'\n' {
                            let b = *p.add(pos);
                            if b >= b'0' && b <= b'9' { cents = cents * 10 + (b - b'0') as i64; }
                            pos += 1;
                        }
                        if is_neg { cents = -cents; }
                        if *p.add(pos) == b'\r' { pos += 1; }
                        if pos < len && *p.add(pos) == b'\n' { pos += 1; }

                        let c_id = if has_region && region_id < reg_ref.len() { reg_ref[region_id] } else { unmatched_id };
                        let agg = &mut local_aggs[c_id as usize];
                        agg.count += 1;
                        agg.id_sum += id;
                        agg.amount_sum += cents;
                        agg.amount_min = agg.amount_min.min(cents);
                        agg.amount_max = agg.amount_max.max(cents);
                        agg.name_bytes += name_len as u64;
                        agg.note_bytes += note_len;

                        flat_country.push(c_id);
                        flat_crc.push(name_crc);
                        c_counts[c_id as usize] += 1;
                    }
                }

                if end > last_adv {
                    unsafe { madvise_dontneed(p.add(last_adv), end - last_adv); }
                }

                let total_hits = flat_crc.len();
                let mut c_offsets = [0u32; 256];
                let mut sum = 0u32;
                for i in 0..256 { c_offsets[i] = sum; sum += c_counts[i]; }
                let mut sorted_crcs = vec![0u32; total_hits];
                let mut write_pos = c_offsets;
                for i in 0..total_hits {
                    let c = flat_country[i] as usize;
                    let slot = write_pos[c] as usize;
                    write_pos[c] += 1;
                    sorted_crcs[slot] = flat_crc[i];
                }

                (local_aggs, sorted_crcs, c_offsets)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe { munmap(ptr, len); }

    let country_chunk = (num_countries + num_threads - 1) / num_threads;
    let formatted_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_countries).step_by(country_chunk).map(|c_start| {
            let c_end = (c_start + country_chunk).min(num_countries);
            let thread_results_ref = &thread_results;
            let country_names_ref = &country_names;

            s.spawn(move || {
                let mut out = Vec::with_capacity((c_end - c_start) * 128);
                let mut scratch: Vec<u32> = Vec::with_capacity(32_768);
                let mut num_buf = [0u8; 32];

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

                            let s_off = thread_results_ref[t].2[c_id] as usize;
                            let e_off = if c_id + 1 < 256 {
                                thread_results_ref[t].2[c_id + 1] as usize
                            } else {
                                thread_results_ref[t].1.len()
                            };
                            if e_off > s_off {
                                scratch.extend_from_slice(&thread_results_ref[t].1[s_off..e_off]);
                            }
                        }
                    }

                    if count == 0 { continue; }

                    scratch.sort_unstable();
                    let mut distinct_names = 0u64;
                    let mut last_crc = u32::MAX;
                    for &crc in &scratch {
                        if crc != last_crc {
                            distinct_names += 1;
                            last_crc = crc;
                        }
                    }

                    let c_name = &country_names_ref[c_id];
                    out.extend_from_slice(c_name);
                    out.push(b' ');
                    let w = unsafe { write_u64_fast(count, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_u64_fast(id_sum, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_cents_fast(amount_sum, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_cents_fast(amount_min, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_cents_fast(amount_max, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_u64_fast(distinct_names, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_u64_fast(name_bytes, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_u64_fast(note_bytes, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b'\n');
                }
                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let results_path = format!("{}/aggregates.txt", out_dir);
    let mut out_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in formatted_outputs {
        let _ = out_file.write_all(&chunk);
    }
}
