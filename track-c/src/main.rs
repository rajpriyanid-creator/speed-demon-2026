use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::_mm_crc32_u8;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
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

// Concurrent Lock-Free Name Interner:
// Up to 131,072 names.
// Open addressing table with 262,144 slots.
// Slot stores atomic hash (u64) and name_id (u32).
const INTERNER_CAP: usize = 262_144;
const INTERNER_MASK: usize = INTERNER_CAP - 1;

struct AtomicNameSlot {
    hash: AtomicU64,
    id: AtomicU32,
}

struct GlobalNameInterner {
    table: Box<[AtomicNameSlot]>,
    next_id: AtomicU32,
}

impl GlobalNameInterner {
    fn new() -> Self {
        let mut slots = Vec::with_capacity(INTERNER_CAP);
        for _ in 0..INTERNER_CAP {
            slots.push(AtomicNameSlot {
                hash: AtomicU64::new(0),
                id: AtomicU32::new(0),
            });
        }
        Self {
            table: slots.into_boxed_slice(),
            next_id: AtomicU32::new(1), // 0 is reserved for empty name
        }
    }

    #[inline(always)]
    fn get_or_insert(&self, h: u64) -> u32 {
        if h == 0 { return 0; }
        let h_key = h;
        let mut idx = (h_key as usize) & INTERNER_MASK;
        loop {
            let slot = unsafe { self.table.get_unchecked(idx) };
            let existing = slot.hash.load(Ordering::Acquire);
            if existing == h_key {
                loop {
                    let id = slot.id.load(Ordering::Acquire);
                    if id != 0 { return id; }
                    std::hint::spin_loop();
                }
            }
            if existing == 0 {
                match slot.hash.compare_exchange(0, h_key, Ordering::SeqCst, Ordering::Acquire) {
                    Ok(_) => {
                        let new_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                        slot.id.store(new_id, Ordering::Release);
                        return new_id;
                    }
                    Err(actual) => {
                        if actual == h_key {
                            loop {
                                let id = slot.id.load(Ordering::Acquire);
                                if id != 0 { return id; }
                                std::hint::spin_loop();
                            }
                        }
                    }
                }
            }
            idx = (idx + 1) & INTERNER_MASK;
        }
    }
}

// Country Bitsets: 241 countries * 2048 u64 words = 131,072 bits per country.
// 241 * 2048 * 8 bytes = 3.94 MB!
const BITSET_WORDS: usize = 2048;

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
    let reg_file = match File::open(&regions_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reg_len = match reg_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let reg_ptr = unsafe { mmap(std::ptr::null_mut(), reg_len, 1, 1, reg_file.as_raw_fd(), 0) };
    if reg_ptr == usize::MAX as *mut u8 || reg_ptr.is_null() { return; }
    let reg_data = unsafe { std::slice::from_raw_parts(reg_ptr, reg_len) };

    let mut country_map = std::collections::BTreeMap::new();
    let mut country_names: Vec<[u8; 3]> = Vec::with_capacity(256);
    let mut region_to_country = vec![255u8; 1_000_000];

    let mut pos = 0;
    while pos < reg_len && reg_data[pos] != b'\n' { pos += 1; }
    if pos < reg_len { pos += 1; }

    while pos < reg_len {
        let mut reg_id = 0usize;
        while pos < reg_len && reg_data[pos] != b',' {
            reg_id = reg_id * 10 + (reg_data[pos] - b'0') as usize;
            pos += 1;
        }
        pos += 1;
        if pos + 3 <= reg_len {
            let c_bytes: [u8; 3] = [reg_data[pos], reg_data[pos + 1], reg_data[pos + 2]];
            let c_id = *country_map.entry(c_bytes).or_insert_with(|| {
                let id = country_names.len() as u8;
                country_names.push(c_bytes);
                id
            });
            if reg_id < region_to_country.len() {
                region_to_country[reg_id] = c_id;
            }
            pos += 3;
            while pos < reg_len && reg_data[pos] != b'\n' { pos += 1; }
            if pos < reg_len { pos += 1; }
        }
    }
    unsafe { munmap(reg_ptr, reg_len); }

    let unmatched_id = country_names.len() as u8;
    let num_countries = (unmatched_id + 1) as usize;

    // 2. Map records.csv
    let file = match File::open(&records_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = match file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, file.as_raw_fd(), 0) };
    if ptr == usize::MAX as *mut u8 || ptr.is_null() { return; }
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };

    let mut header_end = 0;
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

    let interner = GlobalNameInterner::new();
    let mut bitsets_raw = Vec::with_capacity(num_countries * BITSET_WORDS);
    for _ in 0..(num_countries * BITSET_WORDS) {
        bitsets_raw.push(AtomicU64::new(0));
    }
    let bitsets = bitsets_raw.into_boxed_slice();

    struct ThreadParseData {
        aggs: Vec<CountryScalarAgg>,
    }

    let thread_data: Vec<ThreadParseData> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let reg_ref = &region_to_country;
            let interner_ref = &interner;
            let bitsets_ref = &bitsets;

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

                        // 2. Parse & Unescape Name + Calculate 64-bit Hash
                        let mut name_crc = 0u32;
                        let mut name_fnv = 0xcbf29ce484222325u64;
                        let mut name_len = 0usize;
                        if *p.add(pos) == b'"' {
                            pos += 1;
                            loop {
                                let b = *p.add(pos);
                                if b == b'"' {
                                    if *p.add(pos + 1) == b'"' {
                                        name_crc = _mm_crc32_u8(name_crc, b'"');
                                        name_fnv = (name_fnv ^ (b'"' as u64)).wrapping_mul(0x100000001b3);
                                        name_len += 1;
                                        pos += 2;
                                    } else {
                                        pos += 1;
                                        break;
                                    }
                                } else {
                                    name_crc = _mm_crc32_u8(name_crc, b);
                                    name_fnv = (name_fnv ^ (b as u64)).wrapping_mul(0x100000001b3);
                                    name_len += 1;
                                    pos += 1;
                                }
                            }
                        } else {
                            while *p.add(pos) != b',' {
                                let b = *p.add(pos);
                                name_crc = _mm_crc32_u8(name_crc, b);
                                name_fnv = (name_fnv ^ (b as u64)).wrapping_mul(0x100000001b3);
                                name_len += 1;
                                pos += 1;
                            }
                        }
                        let name_hash = if name_len > 0 {
                            ((name_fnv as u64) << 32) | (name_crc as u64)
                        } else {
                            0u64
                        };
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

                        let c_raw = if reg_id < reg_ref.len() { *reg_ref.get_unchecked(reg_id) } else { 255 };
                        let c_id = if c_raw == 255 { unmatched_id as usize } else { c_raw as usize };

                        let agg = local_aggs.get_unchecked_mut(c_id);
                        agg.count += 1;
                        agg.id_sum += rec_id;
                        agg.amount_sum += amount;
                        if amount < agg.amount_min { agg.amount_min = amount; }
                        if amount > agg.amount_max { agg.amount_max = amount; }
                        agg.name_bytes += name_len as u64;
                        agg.note_bytes += note_len as u64;

                        // Distinct Name Bitset:
                        let name_id = interner_ref.get_or_insert(name_hash);
                        let word_idx = (name_id as usize) >> 6;
                        let bit_val = 1u64 << ((name_id as usize) & 63);
                        let bit_slot = bitsets_ref.get_unchecked(c_id * BITSET_WORDS + word_idx);
                        bit_slot.fetch_or(bit_val, Ordering::Relaxed);
                    }
                }

                ThreadParseData { aggs: local_aggs }
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe { munmap(ptr, len); }
    

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

    // Output formatting
    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        let mut out = Vec::with_capacity(32_768);
        let mut num_buf = [0u8; 64];

        // Format sorted countries (country_map keys are already BTreeMap sorted!)
        for (&c_bytes, &c_id_u8) in &country_map {
            let c_id = c_id_u8 as usize;
            let agg = &final_aggs[c_id];
            if agg.count == 0 { continue; }

            // Count ones in country's bitset
            let mut distinct_count = 0u64;
            for w in 0..BITSET_WORDS {
                let val = bitsets[c_id * BITSET_WORDS + w].load(Ordering::Relaxed);
                distinct_count += val.count_ones() as u64;
            }

            out.extend_from_slice(&c_bytes);
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

            let w = unsafe { write_u64_fast(distinct_count, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_u64_fast(agg.name_bytes, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_u64_fast(agg.note_bytes, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b'\n');
        }

        // _UNMATCHED group (always last)
        let unmatch_c = unmatched_id as usize;
        let agg_un = &final_aggs[unmatch_c];
        if agg_un.count > 0 {
            let mut distinct_count = 0u64;
            for w in 0..BITSET_WORDS {
                let val = bitsets[unmatch_c * BITSET_WORDS + w].load(Ordering::Relaxed);
                distinct_count += val.count_ones() as u64;
            }

            out.extend_from_slice(b"_UNMATCHED ");

            let w = unsafe { write_u64_fast(agg_un.count, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_u64_fast(agg_un.id_sum, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_cents_fast(agg_un.amount_sum, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_cents_fast(agg_un.amount_min, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_cents_fast(agg_un.amount_max, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_u64_fast(distinct_count, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_u64_fast(agg_un.name_bytes, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b' ');

            let w = unsafe { write_u64_fast(agg_un.note_bytes, num_buf.as_mut_ptr()) };
            out.extend_from_slice(&num_buf[..w]);
            out.push(b'\n');
        }

        let _ = out_file.write_all(&out);
    }
}
