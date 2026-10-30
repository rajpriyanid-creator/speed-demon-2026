use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;

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

#[derive(Default)]
struct CountryAgg {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: Option<i64>,
    amount_max: Option<i64>,
    name_bytes: u64,
    note_bytes: u64,
    names: FastHashSet<u64>,
}

#[inline(always)]
fn hash_name(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &byte in b {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^ ((b.len() as u64) << 48)
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

    // Partition across 6 physical P-cores
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

    let thread_aggs_list: Vec<Vec<CountryAgg>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for k in 0..num_threads {
            let start = boundaries[k];
            let end = boundaries[k + 1];
            let region_to_country_ref = &region_to_country_id;

            handles.push(s.spawn(move || {
                let mut local_aggs: Vec<CountryAgg> = (0..num_countries).map(|_| CountryAgg::default()).collect();
                let mut pos = start;
                let mut name_buf = Vec::with_capacity(256);

                while pos < end {
                    // Check BOM
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
                    name_buf.clear();
                    if pos < len && data[pos] == b'"' {
                        pos += 1; // skip opening "
                        while pos < len {
                            let b = data[pos];
                            if b == b'"' {
                                if pos + 1 < len && data[pos + 1] == b'"' {
                                    name_buf.push(b'"');
                                    pos += 2;
                                } else {
                                    pos += 1; // skip closing "
                                    break;
                                }
                            } else {
                                name_buf.push(b);
                                pos += 1;
                            }
                        }
                        if pos < len && data[pos] == b',' {
                            pos += 1;
                        }
                    } else {
                        while pos < len && data[pos] != b',' {
                            name_buf.push(data[pos]);
                            pos += 1;
                        }
                        if pos < len && data[pos] == b',' {
                            pos += 1;
                        }
                    }
                    let name_len = name_buf.len() as u64;
                    let n_hash = hash_name(&name_buf);

                    // 3. note
                    let mut note_len: u64 = 0;
                    if pos < len && data[pos] == b'"' {
                        pos += 1; // skip opening "
                        while pos < len {
                            let b = data[pos];
                            if b == b'"' {
                                if pos + 1 < len && data[pos + 1] == b'"' {
                                    note_len += 1;
                                    pos += 2;
                                } else {
                                    pos += 1; // skip closing "
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

                    // Join on region_id
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
                    agg.names.insert(n_hash);
                }
                local_aggs
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    unsafe {
        munmap(ptr, len);
    }

    // Merge thread aggregates
    let mut country_aggs: Vec<CountryAgg> = (0..num_countries).map(|_| CountryAgg::default()).collect();
    for thread_aggs in thread_aggs_list {
        for c_id in 0..num_countries {
            let t_agg = &thread_aggs[c_id];
            if t_agg.count == 0 { continue; }
            let g_agg = &mut country_aggs[c_id];
            g_agg.count += t_agg.count;
            g_agg.id_sum += t_agg.id_sum;
            g_agg.amount_sum += t_agg.amount_sum;
            g_agg.amount_min = Some(match g_agg.amount_min {
                Some(cur) => match t_agg.amount_min {
                    Some(t_min) => cur.min(t_min),
                    None => cur,
                },
                None => t_agg.amount_min.unwrap_or(0),
            });
            g_agg.amount_max = Some(match g_agg.amount_max {
                Some(cur) => match t_agg.amount_max {
                    Some(t_max) => cur.max(t_max),
                    None => cur,
                },
                None => t_agg.amount_max.unwrap_or(0),
            });
            g_agg.name_bytes += t_agg.name_bytes;
            g_agg.note_bytes += t_agg.note_bytes;
            for &n_hash in &t_agg.names {
                g_agg.names.insert(n_hash);
            }
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

        writeln!(
            out_buf,
            "{} {} {} {} {} {} {} {} {}",
            country_str,
            agg.count,
            agg.id_sum,
            amt_sum,
            amt_min,
            amt_max,
            agg.names.len(),
            agg.name_bytes,
            agg.note_bytes
        ).unwrap();
    }

    fs::create_dir_all(&out_dir).unwrap();
    let out_file_path = format!("{}/aggregates.txt", out_dir);
    let mut out_file = File::create(&out_file_path).unwrap();
    out_file.write_all(&out_buf).unwrap();
}
