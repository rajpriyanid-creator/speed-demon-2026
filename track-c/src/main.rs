use std::collections::{BTreeMap, HashSet};
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

    // 1. Load regions.csv
    let regions_path = format!("{}/regions.csv", data_dir);
    let regions_file = match File::open(&regions_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = BufReader::new(regions_file);
    let mut region_to_country: Vec<Option<Box<[u8]>>> = vec![None; 1_000_000];

    let mut lines = reader.lines();
    let _header = lines.next(); // skip region_id,country
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
                let country = &bytes[comma + 1..];
                if rid < region_to_country.len() {
                    region_to_country[rid] = Some(Box::from(country));
                }
            }
        }
    }

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

    let mut countries: BTreeMap<Box<[u8]>, CountryAgg> = BTreeMap::new();
    let unmatched_key: Box<[u8]> = Box::from(b"_UNMATCHED".as_slice());

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

        // 1. id (decimal integer)
        let mut id: u64 = 0;
        while pos < len && data[pos] != b',' {
            id = id * 10 + (data[pos] - b'0') as u64;
            pos += 1;
        }
        pos += 1; // skip ','

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
        let mut region_id: usize = 0;
        while pos < len && data[pos] != b',' {
            region_id = region_id * 10 + (data[pos] - b'0') as usize;
            pos += 1;
        }
        pos += 1; // skip ','

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
        let country_key = if region_id < region_to_country.len() {
            match &region_to_country[region_id] {
                Some(c) => c.clone(),
                None => unmatched_key.clone(),
            }
        } else {
            unmatched_key.clone()
        };

        let agg = countries.entry(country_key).or_default();
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
        agg.names.insert(Box::from(name_buf.as_slice()));
    }

    // Format output
    let mut out_buf = Vec::with_capacity(64 * 1024);
    for (key, agg) in countries {
        if agg.count == 0 {
            continue;
        }
        let country_str = std::str::from_utf8(&key).unwrap();
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

    unsafe {
        munmap(ptr, len);
    }
}
