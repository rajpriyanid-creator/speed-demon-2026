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

    // 1. Parse regions.csv
    let reg_bytes = match fs::read(&regions_path) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut raw_regions: Vec<(u32, Vec<u8>)> = Vec::with_capacity(5000);
    let mut unique_countries: Vec<Vec<u8>> = Vec::with_capacity(300);

    let mut p = 0;
    let rlen = reg_bytes.len();
    if p + 3 <= rlen && &reg_bytes[p..p + 3] == b"\xEF\xBB\xBF" {
        p += 3;
    }
    while p < rlen && reg_bytes[p] != b'\n' { p += 1; }
    if p < rlen { p += 1; }

    while p < rlen {
        let b = reg_bytes[p];
        if b == b'\r' || b == b'\n' { p += 1; continue; }
        let mut reg_id = 0u32;
        while p < rlen && reg_bytes[p] != b',' {
            reg_id = reg_id * 10 + (reg_bytes[p] - b'0') as u32;
            p += 1;
        }
        p += 1;
        let c_start = p;
        while p < rlen && reg_bytes[p] != b'\r' && reg_bytes[p] != b'\n' { p += 1; }
        let c_name = &reg_bytes[c_start..p];
        if p < rlen && reg_bytes[p] == b'\r' { p += 1; }
        if p < rlen && reg_bytes[p] == b'\n' { p += 1; }

        if !unique_countries.iter().any(|c| c.as_slice() == c_name) {
            unique_countries.push(c_name.to_vec());
        }
        raw_regions.push((reg_id, c_name.to_vec()));
    }

    unique_countries.sort();
    let unmatched_name = b"_UNMATCHED".to_vec();
    if !unique_countries.iter().any(|c| c == &unmatched_name) {
        unique_countries.push(unmatched_name);
    }
    unique_countries.sort();

    let num_countries = unique_countries.len();
    let country_names = unique_countries;
    let unmatched_id = country_names.binary_search_by(|c| c.as_slice().cmp(b"_UNMATCHED")).unwrap() as u16;

    let max_reg_id = raw_regions.iter().map(|&(id, _)| id).max().unwrap_or(0) as usize;
    let mut region_to_country = vec![unmatched_id; max_reg_id + 1];
    for (reg_id, c_name) in raw_regions {
        let c_id = country_names.binary_search(&c_name).unwrap() as u16;
        region_to_country[reg_id as usize] = c_id;
    }

    // 2. Mmap records.csv
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

    // PASS 1: Accumulate scalar aggregates & count records per country. ZERO Vec<u64>!
    struct Pass1Result {
        aggs: Vec<CountryScalarAgg>,
    }

    let pass1_results: Vec<Pass1Result> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let reg_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let p = data.as_ptr();
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

                        // 2. Parse Name length
                        let name_len: u32;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut escaped_quotes: u32 = 0;
                            loop {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_quote_swar(w) {
                                    let q = pos + off;
                                    if *p.add(q + 1) == b'"' {
                                        escaped_quotes += 1;
                                        pos = q + 2;
                                    } else {
                                        name_len = (q - n_start) as u32 - escaped_quotes;
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
                            while *p.add(pos) != b',' { pos += 1; }
                            name_len = (pos - n_start) as u32;
                            pos += 1;
                        }

                        // 3. Parse Note length
                        let note_len: u32;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut escaped_quotes: u32 = 0;
                            loop {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_quote_swar(w) {
                                    let q = pos + off;
                                    if *p.add(q + 1) == b'"' {
                                        escaped_quotes += 1;
                                        pos = q + 2;
                                    } else {
                                        note_len = (q - n_start) as u32 - escaped_quotes;
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
                            while *p.add(pos) != b',' { pos += 1; }
                            note_len = (pos - n_start) as u32;
                            pos += 1;
                        }

                        // 4. Parse Region ID
                        let mut reg_id = 0usize;
                        while *p.add(pos) != b',' {
                            reg_id = reg_id * 10 + (*p.add(pos) - b'0') as usize;
                            pos += 1;
                        }
                        pos += 1;

                        // 5. Parse Amount in Cents
                        let is_neg = if *p.add(pos) == b'-' { pos += 1; true } else { false };
                        let mut dollars = 0i64;
                        while *p.add(pos) != b'.' {
                            dollars = dollars * 10 + (*p.add(pos) - b'0') as i64;
                            pos += 1;
                        }
                        pos += 1;
                        let d0 = (*p.add(pos) - b'0') as i64;
                        let d1 = (*p.add(pos + 1) - b'0') as i64;
                        let mut amount = dollars * 100 + d0 * 10 + d1;
                        if is_neg { amount = -amount; }
                        pos += 2;

                        while pos < len && *p.add(pos) != b'\n' { pos += 1; }
                        if pos < len { pos += 1; }

                        let c_id = if reg_id < reg_ref.len() { *reg_ref.get_unchecked(reg_id) as usize } else { unmatched_id as usize };

                        let agg = local_aggs.get_unchecked_mut(c_id);
                        agg.count += 1;
                        agg.id_sum += rec_id;
                        agg.amount_sum += amount;
                        if amount < agg.amount_min { agg.amount_min = amount; }
                        if amount > agg.amount_max { agg.amount_max = amount; }
                        agg.name_bytes += name_len as u64;
                        agg.note_bytes += note_len as u64;
                    }
                }

                Pass1Result { aggs: local_aggs }
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge scalar aggregates
    let mut final_aggs = vec![CountryScalarAgg::default(); num_countries];
    for r in &pass1_results {
        for c in 0..num_countries {
            let src = &r.aggs[c];
            let dst = &mut final_aggs[c];
            dst.count += src.count;
            dst.id_sum += src.id_sum;
            dst.amount_sum += src.amount_sum;
            if src.amount_min < dst.amount_min { dst.amount_min = src.amount_min; }
            if src.amount_max > dst.amount_max { dst.amount_max = src.amount_max; }
            dst.name_bytes += src.name_bytes;
            dst.note_bytes += src.note_bytes;
        }
    }

    // Prefix sums for country offsets and per-thread write bases
    let mut country_offsets = vec![0u32; num_countries + 1];
    let mut t_write_pos: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_countries]).collect();
    let mut total_records = 0u32;

    for c in 0..num_countries {
        country_offsets[c] = total_records;
        let mut cur = total_records;
        for t in 0..num_threads {
            t_write_pos[t][c] = cur;
            cur += pass1_results[t].aggs[c].count as u32;
        }
        total_records = cur;
    }
    country_offsets[num_countries] = total_records;

    drop(pass1_results);

    // Allocate flat all_crcs: exactly 3.7 million u32 = 14.8 MB!
    let mut all_crcs = vec![0u32; total_records as usize];
    let raw_crc_ptr = all_crcs.as_mut_ptr() as usize;

    // PASS 2: Parse name CRC and write DIRECTLY into all_crcs!
    std::thread::scope(|s| {
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let reg_ref = &region_to_country;
            let mut write_pos = std::mem::take(&mut t_write_pos[k]);

            s.spawn(move || {
                let out_ptr = raw_crc_ptr as *mut u32;
                let p = data.as_ptr();
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

                        // Skip Record ID
                        while *p.add(pos) != b',' { pos += 1; }
                        pos += 1;

                        // Parse Name & compute CRC
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

                        // Skip Note
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            loop {
                                let w = (p.add(pos) as *const u64).read_unaligned();
                                if let Some(off) = find_quote_swar(w) {
                                    let q = pos + off;
                                    if *p.add(q + 1) == b'"' {
                                        pos = q + 2;
                                    } else {
                                        pos = q + 1;
                                        break;
                                    }
                                } else {
                                    pos += 8;
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                        } else {
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
                            pos += 1;
                        }

                        // Parse Region ID
                        let mut reg_id = 0usize;
                        while *p.add(pos) != b',' {
                            reg_id = reg_id * 10 + (*p.add(pos) - b'0') as usize;
                            pos += 1;
                        }
                        pos += 1;

                        // Skip Amount & Timestamp to newline
                        while pos < len && *p.add(pos) != b'\n' { pos += 1; }
                        if pos < len { pos += 1; }

                        let c_id = if reg_id < reg_ref.len() { *reg_ref.get_unchecked(reg_id) as usize } else { unmatched_id as usize };

                        let target = *write_pos.get_unchecked(c_id);
                        *out_ptr.add(target as usize) = name_crc;
                        *write_pos.get_unchecked_mut(c_id) += 1;
                    }
                }
            });
        }
    });

    // Unmap records.csv IMMEDIATELY
    unsafe {
        munmap(ptr, len);
    }

    // Parallel in-place sort and dedup of each country's CRCs
    let mut unique_names = vec![0u64; num_countries];
    let raw_uniq_ptr = unique_names.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        let chunk_size = (num_countries + num_threads - 1) / num_threads;
        let crcs_raw = raw_crc_ptr;
        let offsets_ref = &country_offsets;

        for t in 0..num_threads {
            let c_start = t * chunk_size;
            let c_end = (c_start + chunk_size).min(num_countries);
            if c_start >= c_end { continue; }

            s.spawn(move || {
                let crc_ptr = crcs_raw as *mut u32;
                let uniq_ptr = raw_uniq_ptr as *mut u64;

                for c in c_start..c_end {
                    let s_off = offsets_ref[c] as usize;
                    let e_off = offsets_ref[c + 1] as usize;
                    let count = e_off - s_off;
                    if count == 0 {
                        continue;
                    }

                    let sl = unsafe { std::slice::from_raw_parts_mut(crc_ptr.add(s_off), count) };
                    sl.sort_unstable();

                    let mut unique = 1u64;
                    let mut prev = sl[0];
                    for &crc in &sl[1..] {
                        if crc != prev {
                            unique += 1;
                            prev = crc;
                        }
                    }
                    unsafe {
                        *uniq_ptr.add(c) = unique;
                    }
                }
            });
        }
    });

    // Format output
    let _ = fs::create_dir_all(&out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        let mut out_buf = Vec::with_capacity(num_countries * 128);
        let mut num_buf = [0u8; 32];

        for c in 0..num_countries {
            let agg = &final_aggs[c];
            if agg.count == 0 { continue; }

            out_buf.extend_from_slice(&country_names[c]);
            out_buf.push(b' ');

            let w = unsafe { write_u64_fast(agg.count, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_u64_fast(agg.id_sum, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_cents_fast(agg.amount_sum, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_cents_fast(agg.amount_min, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_cents_fast(agg.amount_max, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_u64_fast(unique_names[c], num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_u64_fast(agg.name_bytes, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_u64_fast(agg.note_bytes, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b'\n');
        }

        let _ = out_file.write_all(&out_buf);
    }
}
