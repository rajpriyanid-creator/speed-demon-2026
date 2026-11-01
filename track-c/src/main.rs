use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8, _mm_crc32_u16};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
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
    *buf.add(p + 1) = DIGIT_PAIRS[cents];
    *buf.add(p + 2) = DIGIT_PAIRS[cents + 1];
    p + 3
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
    let records_path = format!("{}/records.csv", data_dir);
    let results_path = format!("{}/aggregates.txt", out_dir);

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

    let mut seen = [false; 17576];
    let mut pos = 0;
    while pos < r_len && regions_data[pos] != b'\n' { pos += 1; }
    if pos < r_len { pos += 1; }

    let body_start = pos;
    while pos < r_len {
        while pos < r_len && regions_data[pos] != b',' { pos += 1; }
        if pos >= r_len { break; }
        pos += 1;
        let b0 = regions_data[pos];
        let b1 = regions_data[pos + 1];
        let b2 = regions_data[pos + 2];
        pos += 3;
        while pos < r_len && regions_data[pos] != b'\n' { pos += 1; }
        if pos < r_len { pos += 1; }

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
    while pos < r_len {
        let mut rid = 0usize;
        while pos < r_len && regions_data[pos] != b',' {
            let b = regions_data[pos];
            if b >= b'0' && b <= b'9' {
                rid = rid * 10 + (b - b'0') as usize;
            }
            pos += 1;
        }
        if pos >= r_len { break; }
        pos += 1;
        let b0 = regions_data[pos];
        let b1 = regions_data[pos + 1];
        let b2 = regions_data[pos + 2];
        pos += 3;
        while pos < r_len && regions_data[pos] != b'\n' { pos += 1; }
        if pos < r_len { pos += 1; }

        if b0 >= b'A' && b0 <= b'Z' && b1 >= b'A' && b1 <= b'Z' && b2 >= b'A' && b2 <= b'Z' {
            let code = ((b0 - b'A') as u16) * 676 + ((b1 - b'A') as u16) * 26 + ((b2 - b'A') as u16);
            let c_id = code_map[code as usize];
            if rid < region_to_country.len() {
                region_to_country[rid] = c_id;
            }
        }
    }
    unsafe { munmap(r_ptr, r_len); }

    let file = match File::open(&records_path) {
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
    if header_end + 3 <= len && &data[header_end..header_end + 3] == b"\xEF\xBB\xBF" {
        header_end += 3;
    }
    while header_end < len && data[header_end] != b'\n' { header_end += 1; }
    if header_end < len { header_end += 1; }

    let num_threads = 12;
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

    // PASS 1: Count records per country (tiny 960 bytes per thread)
    let thread_counts: Vec<Vec<u32>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let reg_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut counts = vec![0u32; num_countries];
                let p = data.as_ptr();
                let mut pos = start;

                while pos < end {
                    unsafe {
                        if pos + 3 <= len && *p.add(pos) == 0xEF && *p.add(pos + 1) == 0xBB && *p.add(pos + 2) == 0xBF { pos += 3; }
                        if pos >= len { break; }
                        let b_lead = *p.add(pos);
                        if b_lead == b'\r' || b_lead == b'\n' { pos += 1; continue; }

                        // Skip ID
                        while *p.add(pos) != b',' { pos += 1; }
                        pos += 1;

                        // Skip Name
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            loop {
                                if *p.add(pos) == b'"' {
                                    if *p.add(pos + 1) == b'"' { pos += 2; }
                                    else { pos += 1; break; }
                                } else { pos += 1; }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                        } else {
                            while *p.add(pos) != b',' { pos += 1; }
                            pos += 1;
                        }

                        // Skip Note
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            loop {
                                if *p.add(pos) == b'"' {
                                    if *p.add(pos + 1) == b'"' { pos += 2; }
                                    else { pos += 1; break; }
                                } else { pos += 1; }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                        } else {
                            while *p.add(pos) != b',' { pos += 1; }
                            pos += 1;
                        }

                        // Read Region ID
                        let mut reg_id = 0usize;
                        while *p.add(pos) != b',' {
                            reg_id = reg_id * 10 + (*p.add(pos) - b'0') as usize;
                            pos += 1;
                        }
                        let c_id = if reg_id < reg_ref.len() { *reg_ref.get_unchecked(reg_id) as usize } else { unmatched_id as usize };
                        counts[c_id] += 1;

                        // Skip rest of line
                        while pos < len && *p.add(pos) != b'\n' { pos += 1; }
                        if pos < len { pos += 1; }
                    }
                }
                counts
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Compute prefix sums
    let mut country_offsets = vec![0u32; num_countries + 1];
    let mut thread_write_ptrs: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_countries]).collect();
    let mut total_records = 0u32;
    for c in 0..num_countries {
        country_offsets[c] = total_records;
        let mut cur = total_records;
        for t in 0..num_threads {
            thread_write_ptrs[t][c] = cur;
            cur += thread_counts[t][c];
        }
        total_records = cur;
    }
    country_offsets[num_countries] = total_records;

    // Allocate flat all_crcs: exactly 14.8 MB!
    let mut all_crcs = vec![0u32; total_records as usize];
    let raw_crcs_ptr = all_crcs.as_mut_ptr() as usize;

    // PASS 2: Parse scalars & write name_crc directly into contiguous slices
    let thread_aggs: Vec<Vec<CountryScalarAgg>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let reg_ref = &region_to_country;
            let mut write_ptrs = thread_write_ptrs[k].clone();

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let p = data.as_ptr();
                let crc_ptr = raw_crcs_ptr as *mut u32;
                let mut pos = start;

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

                        // 1. Parse Record ID
                        let mut rec_id = 0u64;
                        while *p.add(pos) != b',' {
                            rec_id = rec_id * 10 + (*p.add(pos) - b'0') as u64;
                            pos += 1;
                        }
                        pos += 1;

                        // 2. Parse Name & compute CRC
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
                            while *p.add(pos) != b',' {
                                crc = _mm_crc32_u8(crc, *p.add(pos));
                                pos += 1;
                            }
                            name_len = (pos - n_start) as u32;
                            pos += 1;
                            name_crc = _mm_crc32_u16(crc, name_len as u16);
                        }

                        // 3. Parse Note
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
                                    let b = *p.add(curr);
                                    if b == b'"' {
                                        if *p.add(curr + 1) == b'"' {
                                            escaped_quotes += 1;
                                            curr += 2;
                                        } else {
                                            note_len = (curr - n_start) as u64 - escaped_quotes;
                                            pos = curr + 1;
                                            break;
                                        }
                                    } else {
                                        curr += 1;
                                    }
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                        } else {
                            let nt_start = pos;
                            while pos + 8 <= end {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_comma_swar(w) {
                                    pos += off;
                                    break;
                                } else {
                                    pos += 8;
                                }
                            }
                            while *p.add(pos) != b',' { pos += 1; }
                            note_len = (pos - nt_start) as u64;
                            pos += 1;
                        }

                        // 4. Parse Region ID
                        let mut reg_id = 0usize;
                        while *p.add(pos) != b',' {
                            reg_id = reg_id * 10 + (*p.add(pos) - b'0') as usize;
                            pos += 1;
                        }
                        pos += 1;

                        let c_id = if reg_id < reg_ref.len() {
                            *reg_ref.get_unchecked(reg_id) as usize
                        } else {
                            unmatched_id as usize
                        };

                        // 5. Parse Amount (cents)
                        let is_neg = if *p.add(pos) == b'-' { pos += 1; true } else { false };
                        let mut cents = 0i64;
                        loop {
                            let b = *p.add(pos);
                            if b == b'.' {
                                let d0 = *p.add(pos + 1) - b'0';
                                let d1 = *p.add(pos + 2) - b'0';
                                cents = cents * 100 + (d0 as i64 * 10 + d1 as i64);
                                pos += 3;
                                break;
                            }
                            cents = cents * 10 + (b - b'0') as i64;
                            pos += 1;
                        }
                        if is_neg { cents = -cents; }
                        while pos < len && *p.add(pos) != b'\n' { pos += 1; }
                        if pos < len { pos += 1; }

                        let agg = local_aggs.get_unchecked_mut(c_id);
                        agg.count += 1;
                        agg.id_sum += rec_id;
                        agg.amount_sum += cents;
                        if cents < agg.amount_min { agg.amount_min = cents; }
                        if cents > agg.amount_max { agg.amount_max = cents; }
                        agg.name_bytes += name_len as u64;
                        agg.note_bytes += note_len as u64;

                        let dst = write_ptrs[c_id];
                        write_ptrs[c_id] += 1;
                        *crc_ptr.add(dst as usize) = name_crc;
                    }
                }
                local_aggs
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge scalar aggregates
    let mut global_aggs = vec![CountryScalarAgg::default(); num_countries];
    for aggs in &thread_aggs {
        for c in 0..num_countries {
            let a = &aggs[c];
            if a.count > 0 {
                let g = &mut global_aggs[c];
                g.count += a.count;
                g.id_sum += a.id_sum;
                g.amount_sum += a.amount_sum;
                if a.amount_min < g.amount_min { g.amount_min = a.amount_min; }
                if a.amount_max > g.amount_max { g.amount_max = a.amount_max; }
                g.name_bytes += a.name_bytes;
                g.note_bytes += a.note_bytes;
            }
        }
    }

    // In-place parallel sort and unique count per country
    let raw_crcs_ptr = all_crcs.as_mut_ptr() as usize;
    let country_offsets_ref = &country_offsets;
    let mut unique_names = vec![0usize; num_countries];
    let raw_unique_ptr = unique_names.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for t in 0..num_threads {
            let start_c = (num_countries * t) / num_threads;
            let end_c = (num_countries * (t + 1)) / num_threads;

            s.spawn(move || {
                let crc_ptr = raw_crcs_ptr as *mut u32;
                let uniq_ptr = raw_unique_ptr as *mut usize;

                for c in start_c..end_c {
                    let st = country_offsets_ref[c] as usize;
                    let en = country_offsets_ref[c + 1] as usize;
                    let count = en - st;
                    if count == 0 {
                        unsafe { *uniq_ptr.add(c) = 0; }
                        continue;
                    }
                    let slice = unsafe { std::slice::from_raw_parts_mut(crc_ptr.add(st), count) };
                    slice.sort_unstable();

                    let mut u = 1;
                    for i in 1..count {
                        if slice[i] != slice[i - 1] {
                            u += 1;
                        }
                    }
                    unsafe { *uniq_ptr.add(c) = u; }
                }
            });
        }
    });

    // Formatting output
    let mut sorted_country_indices: Vec<usize> = (0..num_countries).collect();
    sorted_country_indices.sort_unstable_by(|&a, &b| country_names[a].cmp(&country_names[b]));

    let _ = fs::create_dir_all(&out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        let mut line_buf = [0u8; 512];
        for c in sorted_country_indices {
            let agg = &global_aggs[c];
            if agg.count == 0 { continue; }

            let cname = &country_names[c];
            let _ = out_file.write_all(cname);
            let _ = out_file.write_all(b" ");

            let mut p = 0;
            unsafe {
                let buf_ptr = line_buf.as_mut_ptr();
                p += write_u64_fast(agg.count, buf_ptr.add(p));
                *buf_ptr.add(p) = b' '; p += 1;
                p += write_u64_fast(agg.id_sum, buf_ptr.add(p));
                *buf_ptr.add(p) = b' '; p += 1;
                p += write_cents_fast(agg.amount_sum, buf_ptr.add(p));
                *buf_ptr.add(p) = b' '; p += 1;
                p += write_cents_fast(agg.amount_min, buf_ptr.add(p));
                *buf_ptr.add(p) = b' '; p += 1;
                p += write_cents_fast(agg.amount_max, buf_ptr.add(p));
                *buf_ptr.add(p) = b' '; p += 1;
                p += write_u64_fast(unique_names[c] as u64, buf_ptr.add(p));
                *buf_ptr.add(p) = b' '; p += 1;
                p += write_u64_fast(agg.name_bytes, buf_ptr.add(p));
                *buf_ptr.add(p) = b' '; p += 1;
                p += write_u64_fast(agg.note_bytes, buf_ptr.add(p));
                *buf_ptr.add(p) = b'\n'; p += 1;
            }
            let _ = out_file.write_all(&line_buf[..p]);
        }
    }
}
