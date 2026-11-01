use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
}

#[inline(always)]
unsafe fn hash_bytes_fast(tok: *const u8, len: usize) -> u32 {
    let mut crc = 0u64;
    let mut i = 0;
    while i + 8 <= len {
        let val = std::ptr::read_unaligned(tok.add(i) as *const u64) | 0x2020202020202020;
        crc = _mm_crc32_u64(crc, val);
        i += 8;
    }
    let mut c32 = crc as u32;
    while i < len {
        c32 = _mm_crc32_u8(c32, *tok.add(i) | 0x20);
        i += 1;
    }
    if c32 == 0 { 1 } else { c32 }
}

#[inline(always)]
unsafe fn make_h64(tok: *const u8, len: usize) -> u64 {
    let h32 = hash_bytes_fast(tok, len);
    ((len as u64) << 32) | (h32 as u64)
}

const IS_TOKEN: [bool; 256] = {
    let mut table = [false; 256];
    let mut b = 0u8;
    loop {
        if (b >= b'a' && b <= b'z') || (b >= b'A' && b <= b'Z') || (b >= b'0' && b <= b'9') {
            table[b as usize] = true;
        }
        if b == 255 { break; }
        b += 1;
    }
    table
};

#[derive(Clone, Copy)]
#[repr(C)]
struct DictEntry {
    hash: u64,
    term_id: u32,
    _pad: u32,
}

struct FastDict {
    table: Vec<DictEntry>,
    mask: usize,
}

impl FastDict {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, _pad: 0 }; size],
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn insert(&mut self, h: u64, term_id: u32) {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { self.table.get_unchecked_mut(idx) };
            if slot.hash == 0 || slot.hash == h {
                slot.hash = h;
                slot.term_id = term_id;
                return;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get(&self, h: u64) -> Option<u32> {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.hash == h {
                return Some(slot.term_id);
            }
            if slot.hash == 0 {
                return None;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[derive(Clone, Copy)]
enum Op { And, Or, Not }

enum ParsedQuery<'a> {
    Bool {
        first_term: Option<u32>,
        steps: Vec<(Op, Option<u32>)>,
    },
    Phrase {
        term_ids: Vec<u32>,
        valid: bool,
        raw_tokens: Vec<&'a [u8]>,
    },
}

#[inline(always)]
fn phrase_matches(doc_text: &[u8], tokens: &[&[u8]]) -> bool {
    let mut p = 0;
    let dlen = doc_text.len();
    let first = tokens[0];
    let flen = first.len();

    while p < dlen {
        while p < dlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } { p += 1; }
        if p >= dlen { break; }
        let tok_start = p;
        while p < dlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } { p += 1; }
        let tok_len = p - tok_start;

        if tok_len == flen {
            let mut matches_first = true;
            for m in 0..flen {
                if unsafe { (*doc_text.get_unchecked(tok_start + m) | 0x20) != (*first.get_unchecked(m) | 0x20) } {
                    matches_first = false;
                    break;
                }
            }

            if matches_first {
                let mut cur_p = p;
                let mut all_match = true;
                for &next_tok in &tokens[1..] {
                    while cur_p < dlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(cur_p) as usize) } { cur_p += 1; }
                    if cur_p >= dlen {
                        all_match = false;
                        break;
                    }
                    let nt_start = cur_p;
                    while cur_p < dlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(cur_p) as usize) } { cur_p += 1; }
                    let nt_len = cur_p - nt_start;
                    if nt_len != next_tok.len() {
                        all_match = false;
                        break;
                    }
                    for m in 0..nt_len {
                        if unsafe { (*doc_text.get_unchecked(nt_start + m) | 0x20) != (*next_tok.get_unchecked(m) | 0x20) } {
                            all_match = false;
                            break;
                        }
                    }
                    if !all_match { break; }
                }
                if all_match { return true; }
            }
        }
    }
    false
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

    let queries_path = format!("{}/queries.txt", data_dir);
    let docs_path = format!("{}/docs.txt", data_dir);
    let results_path = format!("{}/results.txt", out_dir);

    let q_file = match File::open(&queries_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let q_len = match q_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let q_ptr = unsafe { mmap(std::ptr::null_mut(), q_len, 1, 1, q_file.as_raw_fd(), 0) };
    if q_ptr == usize::MAX as *mut u8 || q_ptr.is_null() { return; }
    let q_data = unsafe { std::slice::from_raw_parts(q_ptr, q_len) };

    let mut dict = FastDict::new(32768);
    let mut num_terms = 0u32;

    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            for part in line[5..].split(|&b| b == b' ') {
                if part == b"AND" || part == b"OR" || part == b"NOT" || part.is_empty() { continue; }
                let h64 = unsafe { make_h64(part.as_ptr(), part.len()) };
                if dict.get(h64).is_none() {
                    dict.insert(h64, num_terms);
                    num_terms += 1;
                }
            }
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            for part in inner.split(|&b| b == b' ') {
                if part.is_empty() { continue; }
                let h64 = unsafe { make_h64(part.as_ptr(), part.len()) };
                if dict.get(h64).is_none() {
                    dict.insert(h64, num_terms);
                    num_terms += 1;
                }
            }
        }
    }

    let mut parsed_queries = Vec::with_capacity(5000);
    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            let mut parts = line[5..].split(|&b| b == b' ');
            let first = parts.next().unwrap_or(b"");
            let first_h = unsafe { make_h64(first.as_ptr(), first.len()) };
            let first_id = dict.get(first_h);
            let mut steps = Vec::new();
            while let Some(op_bytes) = parts.next() {
                let op = match op_bytes {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => continue,
                };
                let term_bytes = parts.next().unwrap_or(b"");
                let th = unsafe { make_h64(term_bytes.as_ptr(), term_bytes.len()) };
                let term_id = dict.get(th);
                steps.push((op, term_id));
            }
            parsed_queries.push(ParsedQuery::Bool { first_term: first_id, steps });
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            let mut term_ids = Vec::new();
            let mut raw_tokens = Vec::new();
            let mut valid = true;
            for part in inner.split(|&b| b == b' ') {
                if !part.is_empty() {
                    let th = unsafe { make_h64(part.as_ptr(), part.len()) };
                    match dict.get(th) {
                        Some(tid) => term_ids.push(tid),
                        None => { valid = false; }
                    }
                    raw_tokens.push(part);
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { term_ids, valid, raw_tokens });
        }
    }

    let docs_file = match File::open(&docs_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let docs_len = match docs_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let docs_ptr = unsafe { mmap(std::ptr::null_mut(), docs_len, 1, 1, docs_file.as_raw_fd(), 0) };
    if docs_ptr == usize::MAX as *mut u8 || docs_ptr.is_null() { return; }
    let docs_data = unsafe { std::slice::from_raw_parts(docs_ptr, docs_len) };

    // Find all doc offsets
    let mut doc_starts = Vec::with_capacity(30000);
    let mut doc_lens = Vec::with_capacity(30000);
    let mut p = 0;
    while p < docs_len {
        while p < docs_len && docs_data[p] != b'\t' { p += 1; }
        if p >= docs_len { break; }
        p += 1;
        let s = p;
        while p < docs_len && docs_data[p] != b'\n' { p += 1; }
        let e = p;
        if p < docs_len { p += 1; }
        doc_starts.push(s as u32);
        doc_lens.push((e - s) as u32);
    }
    let total_docs = doc_starts.len();
    let num_words = (total_docs + 63) / 64;

    // Allocate flat bitsets: num_terms * num_words
    let total_bitset_u64s = (num_terms as usize) * num_words;
    let mut bitsets = vec![0u64; total_bitset_u64s];
    let bitsets_ptr = bitsets.as_mut_ptr() as usize;

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(12);
    let docs_per_thread = ((total_docs + num_threads - 1) / num_threads + 63) & !63;

    std::thread::scope(|s| {
        for t in 0..num_threads {
            let d_start = t * docs_per_thread;
            let d_end = (d_start + docs_per_thread).min(total_docs);
            if d_start >= total_docs { continue; }

            let dict_ref = &dict;
            let doc_starts_ref = &doc_starts;
            let doc_lens_ref = &doc_lens;

            s.spawn(move || {
                let raw_bitsets = bitsets_ptr as *mut u64;
                let cptr = docs_data.as_ptr();

                for doc_id in d_start..d_end {
                    let ts = doc_starts_ref[doc_id] as usize;
                    let tl = doc_lens_ref[doc_id] as usize;
                    let te = ts + tl;

                    let word_idx = doc_id >> 6;
                    let bit_mask = 1u64 << (doc_id & 63);

                    let mut pos = ts;
                    while pos < te {
                        while pos < te && !unsafe { *IS_TOKEN.get_unchecked(*cptr.add(pos) as usize) } { pos += 1; }
                        if pos >= te { break; }
                        let tok_start = pos;
                        while pos < te && unsafe { *IS_TOKEN.get_unchecked(*cptr.add(pos) as usize) } { pos += 1; }
                        let tok_len = pos - tok_start;

                        if tok_len <= 23 {
                            let ptr_tok = unsafe { cptr.add(tok_start) };
                            let h64 = unsafe { make_h64(ptr_tok, tok_len) };
                            if let Some(term_id) = dict_ref.get(h64) {
                                let offset = (term_id as usize) * num_words + word_idx;
                                unsafe {
                                    *raw_bitsets.add(offset) |= bit_mask;
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    // Parallel Query Evaluation
    let q_chunk = (parsed_queries.len() + num_threads - 1) / num_threads;
    let formatted_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_threads).map(|t| {
            let q_s = t * q_chunk;
            let q_e = (q_s + q_chunk).min(parsed_queries.len());
            let queries_ref = &parsed_queries[q_s..q_e];
            let doc_starts_ref = &doc_starts;
            let doc_lens_ref = &doc_lens;
            let bitsets_ref = &bitsets;

            s.spawn(move || {
                let mut out = Vec::with_capacity((q_e - q_s) * 48);
                let mut num_buf = [0u8; 32];
                let mut query_bits = vec![0u64; num_words];
                let mut term_bits = vec![0u64; num_words];

                let get_term_bits = |term_id: u32, buf: &mut [u64]| {
                    let s = (term_id as usize) * num_words;
                    buf.copy_from_slice(&bitsets_ref[s..s + num_words]);
                };

                for q in queries_ref {
                    let mut count = 0u32;
                    let mut id_sum = 0u64;
                    let mut top_k = [0u32; 10];
                    let mut top_k_len = 0usize;

                    match q {
                        ParsedQuery::Bool { first_term, steps } => {
                            match first_term {
                                Some(tid) => get_term_bits(*tid, &mut query_bits),
                                None => query_bits.fill(0),
                            }
                            for &(op, maybe_term) in steps {
                                match maybe_term {
                                    Some(tid) => get_term_bits(tid, &mut term_bits),
                                    None => term_bits.fill(0),
                                }
                                match op {
                                    Op::And => {
                                        for w in 0..num_words { query_bits[w] &= term_bits[w]; }
                                    }
                                    Op::Or => {
                                        for w in 0..num_words { query_bits[w] |= term_bits[w]; }
                                    }
                                    Op::Not => {
                                        for w in 0..num_words { query_bits[w] &= !term_bits[w]; }
                                    }
                                }
                            }

                            for w in 0..num_words {
                                let mut bits = query_bits[w];
                                if bits == 0 { continue; }
                                count += bits.count_ones();
                                let base_doc = (w << 6) as u32;
                                while bits != 0 {
                                    let b = bits.trailing_zeros();
                                    let doc_id = base_doc + b;
                                    id_sum += doc_id as u64;
                                    if top_k_len < 10 {
                                        top_k[top_k_len] = doc_id;
                                        top_k_len += 1;
                                    }
                                    bits &= bits - 1;
                                }
                            }
                        }
                        ParsedQuery::Phrase { term_ids, valid, raw_tokens } => {
                            if *valid && !term_ids.is_empty() {
                                get_term_bits(term_ids[0], &mut query_bits);
                                for &tid in &term_ids[1..] {
                                    get_term_bits(tid, &mut term_bits);
                                    for w in 0..num_words { query_bits[w] &= term_bits[w]; }
                                }

                                for w in 0..num_words {
                                    let mut bits = query_bits[w];
                                    if bits == 0 { continue; }
                                    let base_doc = (w << 6) as u32;
                                    while bits != 0 {
                                        let b = bits.trailing_zeros();
                                        let doc_id = (base_doc + b) as usize;
                                        let ts = doc_starts_ref[doc_id] as usize;
                                        let tl = doc_lens_ref[doc_id] as usize;
                                        let doc_text = &docs_data[ts..ts + tl];
                                        if phrase_matches(doc_text, raw_tokens) {
                                            count += 1;
                                            id_sum += doc_id as u64;
                                            if top_k_len < 10 {
                                                top_k[top_k_len] = doc_id as u32;
                                                top_k_len += 1;
                                            }
                                        }
                                        bits &= bits - 1;
                                    }
                                }
                            }
                        }
                    }

                    let w = unsafe { write_u64_fast(count as u64, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    out.push(b' ');
                    let w = unsafe { write_u64_fast(id_sum, num_buf.as_mut_ptr()) };
                    out.extend_from_slice(&num_buf[..w]);
                    for k in 0..top_k_len {
                        out.push(b' ');
                        let w = unsafe { write_u64_fast(top_k[k] as u64, num_buf.as_mut_ptr()) };
                        out.extend_from_slice(&num_buf[..w]);
                    }
                    out.push(b'\n');
                }
                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut out_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in formatted_outputs {
        let _ = out_file.write_all(&chunk);
    }
}
