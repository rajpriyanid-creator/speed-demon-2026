use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[inline(always)]
unsafe fn find_comma_swar(p: *const u8, mut pos: usize, end: usize) -> usize {
    const COMMA_MASK: u64 = 0x2C2C2C2C_2C2C2C2C;
    const LO_MAGIC: u64 = 0x01010101_01010101;
    const HI_MAGIC: u64 = 0x80808080_80808080;

    while pos + 8 <= end {
        let val = std::ptr::read_unaligned(p.add(pos) as *const u64);
        let x = val ^ COMMA_MASK;
        let zeros = x.wrapping_sub(LO_MAGIC) & !x & HI_MAGIC;
        if zeros != 0 {
            return pos + (zeros.trailing_zeros() as usize >> 3);
        }
        pos += 8;
    }
    while pos < end && *p.add(pos) != b',' {
        pos += 1;
    }
    pos
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

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};

#[inline(always)]
unsafe fn crc32_hardware(crc: u32, p: *const u8, len: usize) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        let mut i = 0;
        let mut c = crc as u64;
        while i + 8 <= len {
            let val = std::ptr::read_unaligned(p.add(i) as *const u64);
            c = _mm_crc32_u64(c, val);
            i += 8;
        }
        while i < len {
            c = _mm_crc32_u8(c as u32, *p.add(i)) as u64;
            i += 1;
        }
        c as u32
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut c = crc;
        for i in 0..len {
            c = (c ^ (*p.add(i) as u32)).wrapping_mul(0x01000193);
        }
        c
    }
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
fn write_cents(cents: i64, out: &mut Vec<u8>) {
    if cents < 0 {
        out.push(b'-');
    }
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let rem = (abs % 100) as u8;

    let mut buf = [0u8; 24];
    let mut i = 0;
    let mut d = dollars;
    if d == 0 {
        buf[0] = b'0';
        i = 1;
    } else {
        while d > 0 {
            buf[i] = b'0' + (d % 10) as u8;
            d /= 10;
            i += 1;
        }
    }
    while i > 0 {
        i -= 1;
        out.push(buf[i]);
    }
    out.push(b'.');
    out.push(b'0' + (rem / 10));
    out.push(b'0' + (rem % 10));
}

#[inline(always)]
fn write_u64(mut n: u64, out: &mut Vec<u8>) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 24];
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

    // 1. Fast load regions.csv via mmap (zero heap allocation)
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
    let r_ptr = unsafe {
        mmap(std::ptr::null_mut(), r_len, PROT_READ, MAP_SHARED, regions_file.as_raw_fd(), 0)
    };
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
            if rid < 1_000_000 {
                region_to_country[rid] = code_map[code as usize];
            }
        }
    }
    unsafe { munmap(r_ptr, r_len); }

    // 2. Mmap records.csv
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
        let mut pos = target;
        while pos + 1 < len && !(data[pos] == b'\r' && data[pos + 1] == b'\n') {
            pos += 1;
        }
        if pos + 1 < len { pos += 2; }
        boundaries[k] = pos;
    }
    boundaries[num_threads] = len;

    // 3. Fast parallel records parse with SWAR quote & delimiter jumping and hardware CRC32
    type ThreadResult = (Vec<CountryScalarAgg>, Vec<u64>, Vec<(u32, u32)>);
    let thread_outputs: Vec<ThreadResult> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let region_to_country_ref = &region_to_country;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
                let est_records = (end - start) / 54 + 4096;
                let mut local_names: Vec<u64> = Vec::with_capacity(est_records);
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

                        // 1. id
                        let mut id: u64 = 0;
                        while *p.add(pos) != b',' {
                            id = id * 10 + (*p.add(pos) - b'0') as u64;
                            pos += 1;
                        }
                        pos += 1;

                        // 2. name: hardware CRC32
                        let name_crc: u32;
                        let name_len: u32;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut curr = pos;
                            let mut escaped_quotes = 0u32;
                            let mut crc: u32 = 0;
                            loop {
                                if curr + 8 <= end {
                                    let w = (p.add(curr) as *const u64).read_unaligned();
                                    if let Some(off) = find_quote_swar(w) {
                                        let q = curr + off;
                                        if *p.add(q + 1) == b'"' {
                                            crc = crc32_hardware(crc, p.add(curr), q - curr + 1);
                                            escaped_quotes += 1;
                                            curr = q + 2;
                                        } else {
                                            crc = crc32_hardware(crc, p.add(curr), q - curr);
                                            name_len = (q - n_start) as u32 - escaped_quotes;
                                            pos = q + 1;
                                            break;
                                        }
                                    } else {
                                        crc = crc32_hardware(crc, p.add(curr), 8);
                                        curr += 8;
                                    }
                                } else {
                                    while *p.add(curr) != b'"' {
                                        crc = _mm_crc32_u8(crc, *p.add(curr));
                                        curr += 1;
                                    }
                                    if *p.add(curr + 1) == b'"' {
                                        crc = _mm_crc32_u8(crc, b'"');
                                        escaped_quotes += 1;
                                        curr += 2;
                                    } else {
                                        name_len = (curr - n_start) as u32 - escaped_quotes;
                                        pos = curr + 1;
                                        break;
                                    }
                                }
                            }
                            if *p.add(pos) == b',' { pos += 1; }
                            name_crc = crc;
                        } else {
                            let n_start = pos;
                            let mut crc: u32 = 0;
                            while *p.add(pos) != b',' {
                                crc = _mm_crc32_u8(crc, *p.add(pos));
                                pos += 1;
                            }
                            name_len = (pos - n_start) as u32;
                            pos += 1;
                            name_crc = crc;
                        }

                        // 3. note (SWAR quote skip)
                        let note_len: u64;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            let n_start = pos;
                            let mut curr = pos;
                            let mut escaped_quotes = 0u64;
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
                            pos = find_comma_swar(p, pos, end);
                            note_len = (pos - n_start) as u64;
                            pos += 1;
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
                        pos += 1;

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

                        let packed = ((c_id as u64) << 48) | ((name_len as u64) << 32) | (name_crc as u64);
                        local_names.push(packed);
                    }
                }

                // In-place sort (only ~300k u64 = 2.4MB, takes 2ms!)
                local_names.sort_unstable();
                local_names.dedup();

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

    unsafe { munmap(ptr, len); }

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

    // Parallel k-way merge of sorted 48-bit CRC+length tags per country
    let country_chunk = (num_countries + num_threads - 1) / num_threads;
    let distinct_names: Vec<usize> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_countries).step_by(country_chunk).map(|start| {
            let end = (start + country_chunk).min(num_countries);
            let thread_outputs_ref = &thread_outputs;

            s.spawn(move || {
                let mut local_distincts = Vec::with_capacity(end - start);
                let mut cursors = [0usize; 16];
                let mut ends = [0usize; 16];

                for c_id in start..end {
                    let mut has_any = false;
                    for t in 0..num_threads {
                        let (s_off, e_off) = thread_outputs_ref[t].2[c_id];
                        cursors[t] = s_off as usize;
                        ends[t] = e_off as usize;
                        if s_off < e_off { has_any = true; }
                    }
                    if !has_any {
                        local_distincts.push(0);
                        continue;
                    }

                    let mut distinct = 0usize;
                    let mut last_val = u64::MAX;
                    loop {
                        let mut min_val = u64::MAX;
                        let mut min_t = num_threads;
                        for t in 0..num_threads {
                            let cur = cursors[t];
                            if cur < ends[t] {
                                let val = unsafe { *thread_outputs_ref[t].1.get_unchecked(cur) } & 0x0000_FFFF_FFFF_FFFF;
                                if val < min_val {
                                    min_val = val;
                                    min_t = t;
                                }
                            }
                        }
                        if min_t == num_threads { break; }
                        cursors[min_t] += 1;
                        if min_val != last_val {
                            distinct += 1;
                            last_val = min_val;
                        }
                    }
                    local_distincts.push(distinct);
                }
                local_distincts
            })
        }).collect();

        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });

    let mut out_buf = Vec::with_capacity(64 * 1024);
    for (c_id, country_bytes) in country_names.iter().enumerate() {
        let agg = &country_aggs[c_id];
        if agg.count == 0 { continue; }
        out_buf.extend_from_slice(country_bytes);
        out_buf.push(b' ');
        write_u64(agg.count, &mut out_buf);
        out_buf.push(b' ');
        write_u64(agg.id_sum, &mut out_buf);
        out_buf.push(b' ');
        write_cents(agg.amount_sum, &mut out_buf);
        out_buf.push(b' ');
        write_cents(agg.amount_min, &mut out_buf);
        out_buf.push(b' ');
        write_cents(agg.amount_max, &mut out_buf);
        out_buf.push(b' ');
        write_u64(distinct_names[c_id] as u64, &mut out_buf);
        out_buf.push(b' ');
        write_u64(agg.name_bytes, &mut out_buf);
        out_buf.push(b' ');
        write_u64(agg.note_bytes, &mut out_buf);
        out_buf.push(b'\n');
    }

    fs::create_dir_all(&out_dir).unwrap();
    let out_file_path = format!("{}/aggregates.txt", out_dir);
    let mut out_file = File::create(&out_file_path).unwrap();
    out_file.write_all(&out_buf).unwrap();
}
