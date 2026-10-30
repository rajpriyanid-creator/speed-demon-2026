use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[derive(Default)]
struct CountryAgg {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: Option<i64>,
    amount_max: Option<i64>,
    name_bytes: u64,
    note_bytes: u64,
    names: HashSet<Box<[u8]>>,
}

fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.abs();
    format!("{}{}.{:02}", sign, abs / 100, abs % 100)
}

fn main() {
    let data_dir = "/data";
    let out_dir = "/out";

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
    let mut country_aggs: Vec<CountryAgg> = (0..num_countries).map(|_| CountryAgg::default()).collect();

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
        let _ = fs::create_dir_all(out_dir);
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

    let mut pos = 0;
    // Skip BOM if present at start of file
    if pos + 3 <= len && &data[pos..pos + 3] == b"\xEF\xBB\xBF" {
        pos += 3;
    }
    // Skip header line
    while pos < len && data[pos] != b'\n' {
        pos += 1;
    }
    if pos < len {
        pos += 1;
    }

    let mut name_buf = Vec::with_capacity(256);

    while pos < len {
        // Skip mid-file BOM if present
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
        let c_id = if has_region && region_id < region_to_country_id.len() {
            region_to_country_id[region_id]
        } else {
            unmatched_id
        };

        let agg = &mut country_aggs[c_id as usize];
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
        if !agg.names.contains(name_buf.as_slice()) {
            agg.names.insert(Box::from(name_buf.as_slice()));
        }
    }

    unsafe {
        munmap(ptr, len);
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
        )
        .unwrap();
    }

    let _ = fs::create_dir_all(out_dir);
    if let Ok(mut out_file) = File::create(format!("{}/aggregates.txt", out_dir)) {
        let _ = out_file.write_all(&out_buf);
    }
}
