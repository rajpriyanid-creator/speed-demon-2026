use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn malloc_trim(pad: usize) -> i32;
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
#[repr(C, align(32))]
struct DictEntry {
    hash: u32,
    term_id: u32,
    len: u8,
    bytes: [u8; 23],
}

struct Dict {
    table: Vec<DictEntry>,
    mask: usize,
    bloom: Vec<u64>,
    bloom_mask: usize,
}

impl Dict {
    fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        let b_size = 65536; // 64K u64 = 512 KB
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, len: 0, bytes: [0; 23] }; cap],
            mask: cap - 1,
            bloom: vec![0u64; b_size],
            bloom_mask: b_size - 1,
        }
    }

    fn insert(&mut self, term: &[u8], term_id: u32) {
        if term.len() > 23 { return; }
        let h = unsafe { hash_bytes_fast(term.as_ptr(), term.len()) };
        let b_idx = (h as usize) & self.bloom_mask;
        self.bloom[b_idx] |= 1u64 << ((h >> 16) & 63);

        let mut idx = (h as usize) & self.mask;
        loop {
            let e = &mut self.table[idx];
            if e.hash == 0 {
                e.hash = h;
                e.term_id = term_id;
                e.len = term.len() as u8;
                for i in 0..term.len() {
                    e.bytes[i] = term[i] | 0x20;
                }
                return;
            }
            if e.hash == h && e.len as usize == term.len() {
                let mut match_tok = true;
                for i in 0..term.len() {
                    if e.bytes[i] != (term[i] | 0x20) {
                        match_tok = false;
                        break;
                    }
                }
                if match_tok { return; }
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get(&self, term: &[u8]) -> Option<u32> {
        if term.len() > 23 { return None; }
        let h = unsafe { hash_bytes_fast(term.as_ptr(), term.len()) };
        self.get_with_hash(term.as_ptr(), term.len(), h)
    }

    #[inline(always)]
    fn get_with_hash(&self, raw: *const u8, len: usize, h: u32) -> Option<u32> {
        if len > 23 { return None; }
        let b_idx = (h as usize) & self.bloom_mask;
        let bit = 1u64 << ((h >> 16) & 63);
        if unsafe { *self.bloom.get_unchecked(b_idx) & bit } == 0 {
            return None;
        }

        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 { return None; }
            if e.hash == h && e.len as usize == len {
                let mut match_tok = true;
                for k in 0..len {
                    if unsafe { *e.bytes.get_unchecked(k) != (*raw.add(k) | 0x20) } {
                        match_tok = false;
                        break;
                    }
                }
                if match_tok { return Some(e.term_id); }
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

#[inline(always)]
fn intersect_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            i += 1;
        } else {
            j += 1;
        }
    }
}

#[inline(always)]
fn union_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            out.push(vb);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}

#[inline(always)]
fn difference_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
        if va == vb {
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
}

#[inline(always)]
fn write_u64_fast(mut n: u64, out: &mut Vec<u8>) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
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

#[derive(Clone, Copy)]
struct QueryAccum {
    count: u32,
    id_sum: u64,
    top_k: [u32; 10],
    top_k_len: u8,
}

const NUM_SEGMENTS: usize = 4;

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

    let q_file = File::open(&queries_path).expect("open queries");
    let q_len = q_file.metadata().expect("meta queries").len() as usize;
    let q_ptr = unsafe { mmap(std::ptr::null_mut(), q_len, 1, 1, q_file.as_raw_fd(), 0) };
    let q_data = unsafe { std::slice::from_raw_parts(q_ptr, q_len) };

    let mut dict = Dict::new(131_072);
    let mut num_terms = 0u32;

    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            for part in line[5..].split(|&b| b == b' ') {
                if part == b"AND" || part == b"OR" || part == b"NOT" || part.is_empty() { continue; }
                if dict.get(part).is_none() {
                    dict.insert(part, num_terms);
                    num_terms += 1;
                }
            }
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            for part in inner.split(|&b| b == b' ') {
                if part.is_empty() { continue; }
                if dict.get(part).is_none() {
                    dict.insert(part, num_terms);
                    num_terms += 1;
                }
            }
        }
    }

    let mut parsed_queries = Vec::with_capacity(50_000);
    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            let mut parts = line[5..].split(|&b| b == b' ');
            let first = parts.next().unwrap_or(b"");
            let first_id = dict.get(first);
            let mut steps = Vec::new();
            while let Some(op_bytes) = parts.next() {
                let op = match op_bytes {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => continue,
                };
                let term_bytes = parts.next().unwrap_or(b"");
                let term_id = dict.get(term_bytes);
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
                    match dict.get(part) {
                        Some(tid) => term_ids.push(tid),
                        None => { valid = false; }
                    }
                    raw_tokens.push(part);
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { term_ids, valid, raw_tokens });
        }
    }

    let num_queries = parsed_queries.len();
    let mut query_accums: Vec<QueryAccum> = vec![
        QueryAccum { count: 0, id_sum: 0, top_k: [0; 10], top_k_len: 0 };
        num_queries
    ];

    let docs_file = File::open(&docs_path).expect("open docs");
    let docs_len = docs_file.metadata().expect("meta docs").len() as usize;
    let docs_fd = docs_file.as_raw_fd();

    let mut seg_byte_boundaries = [0usize; NUM_SEGMENTS + 1];
    for i in 1..NUM_SEGMENTS {
        let target = (docs_len * i) / NUM_SEGMENTS;
        let p_start = (target / 4096) * 4096;
        let p_len = (target - p_start + 65536).min(docs_len - p_start);
        let ptr = unsafe { mmap(std::ptr::null_mut(), p_len, 1, 1, docs_fd, p_start as i64) };
        if ptr != usize::MAX as *mut u8 && !ptr.is_null() {
            let slice = unsafe { std::slice::from_raw_parts(ptr, p_len) };
            let mut p = target - p_start;
            while p < p_len && slice[p] != b'\n' { p += 1; }
            if p < p_len { p += 1; }
            seg_byte_boundaries[i] = p_start + p;
            unsafe { munmap(ptr, p_len); }
        } else {
            seg_byte_boundaries[i] = target;
        }
    }
    seg_byte_boundaries[NUM_SEGMENTS] = docs_len;

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(12);
    let mut running_global_doc_id = 0u32;

    for seg in 0..NUM_SEGMENTS {
        let seg_base_doc = running_global_doc_id;
        let seg_start_byte = seg_byte_boundaries[seg];
        let seg_end_byte = seg_byte_boundaries[seg + 1];
        let seg_len = seg_end_byte - seg_start_byte;
        if seg_len == 0 { continue; }

        let page_off = (seg_start_byte / 4096) * 4096;
        let page_len = seg_end_byte - page_off;
        let ptr = unsafe { mmap(std::ptr::null_mut(), page_len, 1, 1, docs_fd, page_off as i64) };
        if ptr == usize::MAX as *mut u8 || ptr.is_null() { continue; }

        let rel_start = seg_start_byte - page_off;
        let seg_slice = unsafe { std::slice::from_raw_parts(ptr.add(rel_start), seg_len) };

        let mut sub_chunks = Vec::with_capacity(num_threads);
        let mut cur_sb = 0usize;
        for t in 0..num_threads {
            let target = (seg_len * (t + 1)) / num_threads;
            let mut end = target.min(seg_len);
            while end < seg_len && seg_slice[end] != b'\n' { end += 1; }
            if end < seg_len { end += 1; }
            sub_chunks.push((cur_sb, end));
            cur_sb = end;
        }

        // Pass 1 on this segment: Count term frequencies and collect starts/lens (zero big allocations!)
        let pass1: Vec<(Vec<u32>, Vec<u32>, Vec<u32>, Vec<u16>, Vec<u32>)> = std::thread::scope(|s| {
            let handles: Vec<_> = sub_chunks.iter().map(|&(sb, eb)| {
                let dict_ref = &dict;
                let chunk = &seg_slice[sb..eb];
                s.spawn(move || {
                    let mut local_counts = vec![0u32; num_terms as usize];
                    let mut last_seen = vec![u16::MAX; num_terms as usize];
                    let est_docs = ((eb - sb) / 1000 + 512).max(4000);
                    let mut local_starts = Vec::with_capacity(est_docs);
                    let mut local_lens = Vec::with_capacity(est_docs);
                    let mut doc_hits: Vec<u16> = Vec::with_capacity(65_536);
                    let mut term_hits: Vec<u32> = Vec::with_capacity(65_536);
                    let mut i = 0;
                    let clen = chunk.len();
                    let mut loc_doc: u16 = 0;
                    let base_ptr = chunk.as_ptr();

                    while i < clen {
                        while i < clen && chunk[i] != b'\t' { i += 1; }
                        if i >= clen { break; }
                        i += 1;
                        let text_start = i;
                        while i < clen && chunk[i] != b'\n' { i += 1; }
                        let text_end = i;
                        i += 1;

                        local_starts.push((sb + text_start) as u32);
                        local_lens.push((text_end - text_start) as u32);

                        let mut p = text_start;
                        while p < text_end {
                            while p < text_end && !unsafe { *IS_TOKEN.get_unchecked(*chunk.get_unchecked(p) as usize) } { p += 1; }
                            if p >= text_end { break; }
                            let tok_start = p;
                            while p < text_end && unsafe { *IS_TOKEN.get_unchecked(*chunk.get_unchecked(p) as usize) } { p += 1; }
                            let tok_len = p - tok_start;
                            if tok_len <= 23 {
                                let ptr_tok = unsafe { base_ptr.add(tok_start) };
                                let h = unsafe { hash_bytes_fast(ptr_tok, tok_len) };
                                if let Some(term_id) = dict_ref.get_with_hash(ptr_tok, tok_len, h) {
                                    let tid = term_id as usize;
                                    if unsafe { *last_seen.get_unchecked(tid) } != loc_doc {
                                        unsafe {
                                            *last_seen.get_unchecked_mut(tid) = loc_doc;
                                            *local_counts.get_unchecked_mut(tid) += 1;
                                            doc_hits.push(loc_doc);
                                            term_hits.push(term_id);
                                        }
                                    }
                                }
                            }
                        }
                        loc_doc += 1;
                    }
                    (local_counts, local_starts, local_lens, doc_hits, term_hits)
                })
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut t_start_doc = Vec::with_capacity(num_threads);
        let mut cur_d = 0u16;
        for r in &pass1 {
            t_start_doc.push(cur_d);
            cur_d += r.1.len() as u16;
        }
        let seg_docs_count = cur_d as usize;
        running_global_doc_id += cur_d as u32;

        let mut seg_doc_starts = Vec::with_capacity(seg_docs_count);
        let mut seg_doc_lens = Vec::with_capacity(seg_docs_count);
        for r in &pass1 {
            seg_doc_starts.extend_from_slice(&r.1);
            seg_doc_lens.extend_from_slice(&r.2);
        }

        let mut offsets = vec![0u32; (num_terms as usize) + 1];
        let mut t_write_bases: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_terms as usize]).collect();
        let mut total_postings = 0u32;
        for term in 0..(num_terms as usize) {
            offsets[term] = total_postings;
            let mut cur = total_postings;
            for t in 0..num_threads {
                t_write_bases[t][term] = cur;
                cur += pass1[t].0[term];
            }
            total_postings = cur;
        }
        offsets[num_terms as usize] = total_postings;

        let mut all_postings: Vec<u16> = vec![0u16; total_postings as usize];
        let base_postings_ptr = all_postings.as_mut_ptr() as usize;

        // Pass 2 on this segment: INSTANT IN-MEMORY SCATTER (zero text scanning, zero hashing!)
        std::thread::scope(|s| {
            for (t, r) in pass1.into_iter().enumerate() {
                let s_doc = t_start_doc[t];
                let mut write_pos = t_write_bases[t].clone();
                let doc_hits = r.3;
                let term_hits = r.4;
                s.spawn(move || {
                    let raw_p = base_postings_ptr as *mut u16;
                    for i in 0..doc_hits.len() {
                        let d_id = s_doc + unsafe { *doc_hits.get_unchecked(i) };
                        let tid = unsafe { *term_hits.get_unchecked(i) } as usize;
                        unsafe {
                            let slot = *write_pos.get_unchecked(tid) as usize;
                            *write_pos.get_unchecked_mut(tid) += 1;
                            *raw_p.add(slot) = d_id;
                        }
                    }
                });
            }
        });

        // Parallel Query Evaluation against this segment
        let q_chunk_size = (parsed_queries.len() + num_threads - 1) / num_threads;

        let partials: Vec<Vec<(u32, u64, [u32; 10], u8)>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..num_threads).map(|t| {
                let q_start = t * q_chunk_size;
                let q_end = (q_start + q_chunk_size).min(parsed_queries.len());
                let queries_ref = &parsed_queries[q_start..q_end];
                let offsets_ref = &offsets;
                let all_postings_ref = &all_postings;
                let seg_slice_ref = seg_slice;
                let doc_starts_ref = &seg_doc_starts;
                let doc_lens_ref = &seg_doc_lens;

                s.spawn(move || {
                    let mut seg_res = Vec::with_capacity(q_end - q_start);
                    let mut buf_a: Vec<u16> = Vec::with_capacity(8192);
                    let mut buf_b: Vec<u16> = Vec::with_capacity(8192);
                    let mut matches: Vec<u16> = Vec::with_capacity(8192);

                    let get_postings = |term_id: u32| -> &[u16] {
                        let tid = term_id as usize;
                        let s = offsets_ref[tid] as usize;
                        let e = offsets_ref[tid + 1] as usize;
                        &all_postings_ref[s..e]
                    };

                    for q in queries_ref {
                        let mut c_count = 0u32;
                        let mut c_sum = 0u64;
                        let mut c_top = [0u32; 10];
                        let mut c_top_len = 0u8;

                        match q {
                            ParsedQuery::Bool { first_term, steps } => {
                                let r_slice: &[u16] = match first_term {
                                    Some(t) => get_postings(*t),
                                    None => &[],
                                };

                                if steps.is_empty() {
                                    c_count = r_slice.len() as u32;
                                    let loc_sum: u64 = r_slice.iter().map(|&x| x as u64).sum();
                                    c_sum = (seg_base_doc as u64) * (c_count as u64) + loc_sum;
                                    for &loc_id in r_slice.iter().take(10) {
                                        c_top[c_top_len as usize] = seg_base_doc + (loc_id as u32);
                                        c_top_len += 1;
                                    }
                                } else {
                                    let (first_op, first_term_opt) = steps[0];
                                    let b_first: &[u16] = match first_term_opt {
                                        Some(t) => get_postings(t),
                                        None => &[],
                                    };
                                    match first_op {
                                        Op::And => intersect_u16(r_slice, b_first, &mut buf_a),
                                        Op::Or => union_u16(r_slice, b_first, &mut buf_a),
                                        Op::Not => difference_u16(r_slice, b_first, &mut buf_a),
                                    }
                                    for &(op, maybe_term) in &steps[1..] {
                                        let b: &[u16] = match maybe_term {
                                            Some(t) => get_postings(t),
                                            None => &[],
                                        };
                                        match op {
                                            Op::And => intersect_u16(&buf_a, b, &mut buf_b),
                                            Op::Or => union_u16(&buf_a, b, &mut buf_b),
                                            Op::Not => difference_u16(&buf_a, b, &mut buf_b),
                                        }
                                        std::mem::swap(&mut buf_a, &mut buf_b);
                                    }
                                    c_count = buf_a.len() as u32;
                                    let loc_sum: u64 = buf_a.iter().map(|&x| x as u64).sum();
                                    c_sum = (seg_base_doc as u64) * (c_count as u64) + loc_sum;
                                    for &loc_id in buf_a.iter().take(10) {
                                        c_top[c_top_len as usize] = seg_base_doc + (loc_id as u32);
                                        c_top_len += 1;
                                    }
                                }
                            }
                            ParsedQuery::Phrase { term_ids, valid, raw_tokens } => {
                                if *valid && !term_ids.is_empty() {
                                    let mut min_idx = 0;
                                    let mut min_len = usize::MAX;
                                    for (idx, &tid) in term_ids.iter().enumerate() {
                                        let plen = (offsets_ref[tid as usize + 1] - offsets_ref[tid as usize]) as usize;
                                        if plen < min_len {
                                            min_len = plen;
                                            min_idx = idx;
                                        }
                                    }
                                    if min_len > 0 {
                                        buf_a.clear();
                                        buf_a.extend_from_slice(get_postings(term_ids[min_idx]));
                                        for (idx, &tid) in term_ids.iter().enumerate() {
                                            if idx == min_idx { continue; }
                                            intersect_u16(&buf_a, get_postings(tid), &mut buf_b);
                                            std::mem::swap(&mut buf_a, &mut buf_b);
                                            if buf_a.is_empty() { break; }
                                        }
                                        if !buf_a.is_empty() {
                                            matches.clear();
                                            for &cand_loc in &buf_a {
                                                let start = doc_starts_ref[cand_loc as usize] as usize;
                                                let len = doc_lens_ref[cand_loc as usize] as usize;
                                                let doc_text = &seg_slice_ref[start..start + len];
                                                if phrase_matches(doc_text, raw_tokens) {
                                                    matches.push(cand_loc);
                                                }
                                            }
                                            c_count = matches.len() as u32;
                                            let loc_sum: u64 = matches.iter().map(|&x| x as u64).sum();
                                            c_sum = (seg_base_doc as u64) * (c_count as u64) + loc_sum;
                                            for &loc_id in matches.iter().take(10) {
                                                c_top[c_top_len as usize] = seg_base_doc + (loc_id as u32);
                                                c_top_len += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        seg_res.push((c_count, c_sum, c_top, c_top_len));
                    }
                    seg_res
                })
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // Accumulate segment results into global query_accums
        let mut idx = 0;
        for p_chunk in partials {
            for (c, s, top, top_len) in p_chunk {
                let acc = &mut query_accums[idx];
                acc.count += c as u32;
                acc.id_sum += s;
                let needed = (10 - acc.top_k_len as usize).min(top_len as usize);
                for k in 0..needed {
                    acc.top_k[acc.top_k_len as usize] = top[k];
                    acc.top_k_len += 1;
                }
                idx += 1;
            }
        }

        unsafe {
            munmap(ptr, page_len);
            malloc_trim(0);
        }
    }

    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut out_buf = Vec::with_capacity(num_queries * 32);
    for acc in query_accums {
        if acc.count == 0 {
            out_buf.extend_from_slice(b"0 0\n");
        } else {
            write_u64_fast(acc.count as u64, &mut out_buf);
            out_buf.push(b' ');
            write_u64_fast(acc.id_sum, &mut out_buf);
            for i in 0..(acc.top_k_len as usize) {
                out_buf.push(b' ');
                write_u64_fast(acc.top_k[i] as u64, &mut out_buf);
            }
            out_buf.push(b'\n');
        }
        if out_buf.len() >= 65536 {
            let _ = out_file.write_all(&out_buf);
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        let _ = out_file.write_all(&out_buf);
    }
    let _ = out_file.flush();
    unsafe { munmap(q_ptr, q_len); }
}
