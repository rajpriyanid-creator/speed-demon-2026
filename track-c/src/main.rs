use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_mm_crc32_u64;

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
    let mut temp = [0u8; 20];
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
unsafe fn write_amount_fast(mut val: i64, buf: *mut u8) -> usize {
    let mut len = 0;
    if val < 0 {
        *buf = b'-';
        len = 1;
        val = -val;
    }
    let dollars = val / 100;
    let cents = val % 100;
    let w = write_u64_fast(dollars as u64, buf.add(len));
    len += w;
    *buf.add(len) = b'.';
    len += 1;
    let rem = (cents as usize) * 2;
    *buf.add(len) = DIGIT_PAIRS[rem];
    *buf.add(len + 1) = DIGIT_PAIRS[rem + 1];
    len + 2
}

fn pdqsort_u32(v: &mut [u32]) {
    v.sort_unstable();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
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
    let reg_bytes = match std::fs::read(&regions_path) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut unique_countries: Vec<Vec<u8>> = Vec::with_capacity(300);
    unique_countries.push(b"_UNMATCHED".to_vec());

    let mut raw_regions = Vec::with_capacity(3000);
    let mut r_pos = 0;
    while r_pos < reg_bytes.len() && reg_bytes[r_pos] != b'\n' { r_pos += 1; }
    if r_pos < reg_bytes.len() { r_pos += 1; }

    while r_pos < reg_bytes.len() {
        if reg_bytes[r_pos] == b'\r' || reg_bytes[r_pos] == b'\n' {
            r_pos += 1;
            continue;
        }
        let mut id = 0u32;
        while r_pos < reg_bytes.len() && reg_bytes[r_pos] >= b'0' && reg_bytes[r_pos] <= b'9' {
            id = id * 10 + (reg_bytes[r_pos] - b'0') as u32;
            r_pos += 1;
        }
        if r_pos < reg_bytes.len() && reg_bytes[r_pos] == b',' { r_pos += 1; }
        let c_start = r_pos;
        while r_pos < reg_bytes.len() && reg_bytes[r_pos] != b'\r' && reg_bytes[r_pos] != b'\n' {
            r_pos += 1;
        }
        let c_name = reg_bytes[c_start..r_pos].to_vec();
        if !unique_countries.iter().any(|x| x == &c_name) {
            unique_countries.push(c_name.clone());
        }
        raw_regions.push((id, c_name));
        while r_pos < reg_bytes.len() && (reg_bytes[r_pos] == b'\r' || reg_bytes[r_pos] == b'\n') {
            r_pos += 1;
        }
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

    // SINGLE PASS: Each thread parses scalars and collects (c_id << 32) | name_crc
    struct ThreadParseData {
        aggs: Vec<CountryScalarAgg>,
        crcs: Vec<u64>,
    }

    let mut thread_data: Vec<ThreadParseData> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let reg_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let est_records = (end - start) / 60 + 1024;
                let mut local_crcs: Vec<u64> = Vec::with_capacity(est_records);

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

                        // 2. Parse & Unescape Name + Calculate CRC32
                        let mut name_crc = 0u64;
                        let mut name_len = 0usize;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            loop {
                                let b = *p.add(pos);
                                if b == b'"' {
                                    if *p.add(pos + 1) == b'"' {
                                        name_crc = _mm_crc32_u64(name_crc, b'"' as u64);
                                        name_len += 1;
                                        pos += 2;
                                    } else {
                                        pos += 1;
                                        break;
                                    }
                                } else {
                                    name_crc = _mm_crc32_u64(name_crc, b as u64);
                                    name_len += 1;
                                    pos += 1;
                                }
                            }
                        } else {
                            while *p.add(pos) != b',' {
                                name_crc = _mm_crc32_u64(name_crc, *p.add(pos) as u64);
                                name_len += 1;
                                pos += 1;
                            }
                        }
                        name_crc = _mm_crc32_u64(name_crc, name_len as u64);
                        pos += 1; // skip comma

                        // 3. Parse & Unescape Note
                        let mut note_len = 0usize;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            loop {
                                let b = *p.add(pos);
                                if b == b'"' {
                                    if *p.add(pos + 1) == b'"' {
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
                        } else {
                            while *p.add(pos) != b',' {
                                note_len += 1;
                                pos += 1;
                            }
                        }
                        pos += 1; // skip comma

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

                        local_crcs.push(((c_id as u64) << 32) | (name_crc as u32 as u64));
                    }
                }

                ThreadParseData { aggs: local_aggs, crcs: local_crcs }
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });


    // Munmap records.csv IMMEDIATELY! Drops 230MB from address space
    unsafe {
        munmap(ptr, len);
    }

    // Merge scalar aggregates
    let mut final_aggs = vec![CountryScalarAgg::default(); num_countries];
    for r in &thread_data {
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

    // Prefix sums for country offsets
    let mut country_offsets = vec![0u32; num_countries + 1];
    let mut t_write_pos: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_countries]).collect();
    let mut total_records = 0u32;

    for c in 0..num_countries {
        country_offsets[c] = total_records;
        let mut cur = total_records;
        for t in 0..num_threads {
            t_write_pos[t][c] = cur;
            cur += thread_data[t].aggs[c].count as u32;
        }
        total_records = cur;
    }
    country_offsets[num_countries] = total_records;

    // Allocate flat all_crcs: exactly 3.7 million u32 = 14.8 MB!
    let mut all_crcs = vec![0u32; total_records as usize];
    let raw_crc_ptr = all_crcs.as_mut_ptr() as usize;

    // Fast in-memory scatter across 12 threads
    std::thread::scope(|s| {
        for t in 0..num_threads {
            let crc_list = std::mem::take(&mut thread_data[t].crcs);
            let mut write_pos = std::mem::take(&mut t_write_pos[t]);

            s.spawn(move || {
                let raw_crc = raw_crc_ptr as *mut u32;
                for packed in crc_list {
                    let c = (packed >> 32) as usize;
                    let crc = packed as u32;
                    unsafe {
                        let target = *write_pos.get_unchecked(c);
                        *raw_crc.add(target as usize) = crc;
                        *write_pos.get_unchecked_mut(c) += 1;
                    }
                }
            });
        }
    });

    drop(thread_data);
    drop(t_write_pos);


    // Parallel in-place sort & unique count per country across 12 threads
    let mut unique_names_per_country = vec![0u64; num_countries];
    let countries_per_thread = (num_countries + num_threads - 1) / num_threads;
    let raw_crc_ref = &all_crcs;
    let c_offsets_ref = &country_offsets;
    let out_u_ptr = unique_names_per_country.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for t in 0..num_threads {
            let c_start = t * countries_per_thread;
            let c_end = (c_start + countries_per_thread).min(num_countries);
            if c_start >= c_end { continue; }

            s.spawn(move || {
                let out_u = out_u_ptr as *mut u64;
                let crc_slice = unsafe {
                    std::slice::from_raw_parts_mut(raw_crc_ref.as_ptr() as *mut u32, raw_crc_ref.len())
                };

                for c in c_start..c_end {
                    let s_idx = c_offsets_ref[c] as usize;
                    let e_idx = c_offsets_ref[c + 1] as usize;
                    if s_idx >= e_idx {
                        unsafe { *out_u.add(c) = 0; }
                        continue;
                    }

                    let slice = &mut crc_slice[s_idx..e_idx];
                    pdqsort_u32(slice);

                    let mut unique = 1u64;
                    let mut prev = slice[0];
                    for &val in &slice[1..] {
                        if val != prev {
                            unique += 1;
                            prev = val;
                        }
                    }
                    unsafe { *out_u.add(c) = unique; }
                }
            });
        }
    });


    // Drop all_crcs BEFORE formatting output!
    drop(all_crcs);

    // Format output
    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        let mut out_buf = Vec::with_capacity(num_countries * 128);
        let mut num_buf = [0u8; 64];

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

            let w = unsafe { write_amount_fast(agg.amount_sum, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_amount_fast(agg.amount_min, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_amount_fast(agg.amount_max, num_buf.as_mut_ptr()) };
            out_buf.extend_from_slice(&num_buf[..w]);
            out_buf.push(b' ');

            let w = unsafe { write_u64_fast(unique_names_per_country[c], num_buf.as_mut_ptr()) };
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
