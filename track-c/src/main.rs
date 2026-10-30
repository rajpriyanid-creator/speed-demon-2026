use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

struct FxHasher(u64);

impl std::hash::Hasher for FxHasher {
    #[inline(always)]
    fn finish(&self) -> u64 { self.0 }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ (b as u64)).wrapping_mul(0x517cc1b727220a95);
        }
    }
    #[inline(always)]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517cc1b727220a95);
    }
}

#[derive(Default, Clone, Copy)]
struct FxBuildHasher;

impl std::hash::BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    #[inline(always)]
    fn build_hasher(&self) -> FxHasher {
        FxHasher(0)
    }
}

type FastHashSet<T> = std::collections::HashSet<T, FxBuildHasher>;

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

struct NameDict {
    table: Vec<u64>,
    mask: usize,
}

impl NameDict {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        Self {
            table: vec![0u64; size],
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn insert_new(&mut self, hash: u64, id: u32) -> bool {
        let mut idx = (hash as usize) & self.mask;
        let hash_tag = hash & !0x3FFFF;
        let tag = hash_tag | ((id + 1) as u64);
        loop {
            let e = self.table[idx];
            if e == 0 {
                self.table[idx] = tag;
                return true;
            }
            if (e & !0x3FFFF) == hash_tag {
                return false;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get(&self, hash: u64) -> u32 {
        let mut idx = (hash as usize) & self.mask;
        let hash_tag = hash & !0x3FFFF;
        loop {
            let e = unsafe { *self.table.get_unchecked(idx) };
            if e == 0 {
                return 0;
            }
            if (e & !0x3FFFF) == hash_tag {
                return ((e & 0x3FFFF) - 1) as u32;
            }
            idx = (idx + 1) & self.mask;
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

    // 1. Load regions.csv and catalog countries
    let regions_path = format!("{}/regions.csv", data_dir);
    let regions_file = match File::open(&regions_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = BufReader::new(regions_file);

    let mut raw_regions: Vec<(usize, Box<[u8]>)> = Vec::with_capacity(250_000);
    let mut country_set: BTreeSet<Box<[u8]>> = BTreeSet::new();

    let mut lines = reader.lines();
    let _header = lines.next();
    for line in lines {
        if let Ok(line) = line {
            let bytes = line.as_bytes();
            let mut comma = 0;
            while comma < bytes.len() && bytes[comma] != b',' {
                comma += 1;
            }
            if comma < bytes.len() {
                let mut rid: usize = 0;
                for &b in &bytes[..comma] {
                    if b >= b'0' && b <= b'9' {
                        rid = rid * 10 + (b - b'0') as usize;
                    }
                }
                let country: Box<[u8]> = Box::from(&bytes[comma + 1..]);
                country_set.insert(country.clone());
                raw_regions.push((rid, country));
            }
        }
    }

    let mut country_names: Vec<Box<[u8]>> = country_set.into_iter().collect();
    let unmatched_id = country_names.len() as u16;
    let unmatched_name = Box::from(b"_UNMATCHED".as_slice());
    country_names.push(unmatched_name);

    let mut region_to_country_id: Vec<u16> = vec![unmatched_id; 1_000_000];
    for (rid, country) in raw_regions {
        if rid < region_to_country_id.len() {
            if let Ok(c_id) = country_names.binary_search(&country) {
                region_to_country_id[rid] = c_id as u16;
            }
        }
    }

    let num_countries = country_names.len();

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

    // Pass 1: Discover all unique names rapidly (skipping notes, regions, amounts)
    let thread_names: Vec<FastHashSet<u64>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];

            handles.push(s.spawn(move || {
                let mut local_names: FastHashSet<u64> = FastHashSet::default();
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

                    // Skip id
                    while pos < len && data[pos] != b',' {
                        pos += 1;
                    }
                    if pos < len && data[pos] == b',' {
                        pos += 1;
                    }

                    // Parse name and stream hash
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
                    } else {
                        while pos < len && data[pos] != b',' {
                            let b = data[pos];
                            n_hash = (n_hash ^ (b as u64)).wrapping_mul(0x100000001b3);
                            name_len += 1;
                            pos += 1;
                        }
                    }
                    let n_hash = n_hash ^ (name_len << 48);
                    local_names.insert(n_hash);

                    // Skip rest of row to next newline (handling quoted fields)
                    let mut in_quotes = false;
                    while pos < len {
                        let b = data[pos];
                        if b == b'"' {
                            in_quotes = !in_quotes;
                        } else if !in_quotes && b == b'\n' {
                            pos += 1;
                            break;
                        }
                        pos += 1;
                    }
                }
                local_names
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Main thread builds dense NameDict
    let mut name_dict = NameDict::new(262_144);
    let mut total_names = 0u32;
    for mut set in thread_names {
        for hash in set.drain() {
            if name_dict.insert_new(hash, total_names) {
                total_names += 1;
            }
        }
    }

    let num_words = ((total_names as usize) + 63) / 64;
    let total_bitset_words = num_countries * num_words;
    let atomic_bitsets: Vec<AtomicU64> = (0..total_bitset_words).map(|_| AtomicU64::new(0)).collect();

    // Pass 2: Full parse with zero-allocation atomic bitset setting
    let thread_scalars: Vec<Vec<CountryScalarAgg>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let region_to_country_ref = &region_to_country_id;
            let name_dict_ref = &name_dict;
            let bitsets_ref = &atomic_bitsets;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![CountryScalarAgg::default(); num_countries];
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

                    let name_id = name_dict_ref.get(n_hash);
                    let word_offset = (c_id as usize) * num_words + ((name_id as usize) >> 6);
                    let bit_mask = 1u64 << (name_id & 63);
                    bitsets_ref[word_offset].fetch_or(bit_mask, Ordering::Relaxed);
                }
                local_aggs
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe {
        munmap(ptr, len);
    }

    // Merge scalar aggregates
    let mut country_aggs = vec![CountryScalarAgg::default(); num_countries];
    for thread_agg in thread_scalars {
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

        let start_word = c_id * num_words;
        let distinct_names: usize = (0..num_words)
            .map(|w| atomic_bitsets[start_word + w].load(Ordering::Relaxed).count_ones() as usize)
            .sum();

        writeln!(
            out_buf,
            "{} {} {} {} {} {} {} {} {}",
            country_str,
            agg.count,
            agg.id_sum,
            amt_sum,
            amt_min,
            amt_max,
            distinct_names,
            agg.name_bytes,
            agg.note_bytes
        ).unwrap();
    }

    fs::create_dir_all(&out_dir).unwrap();
    let out_file_path = format!("{}/aggregates.txt", out_dir);
    let mut out_file = File::create(&out_file_path).unwrap();
    out_file.write_all(&out_buf).unwrap();
}
