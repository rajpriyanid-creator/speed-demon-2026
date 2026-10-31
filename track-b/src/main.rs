use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
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

const NUM_BLOCKS: usize = 5;

struct MmapSlice {
    ptr: *mut u8,
    len: usize,
}

impl MmapSlice {
    fn open(path: &str) -> Option<Self> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as usize;
        if len == 0 {
            return Some(Self { ptr: std::ptr::null_mut(), len: 0 });
        }
        let fd = file.as_raw_fd();
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, fd, 0) };
        if ptr == usize::MAX as *mut u8 || ptr.is_null() {
            return None;
        }
        Some(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

impl Drop for MmapSlice {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe { munmap(self.ptr, self.len); }
        }
    }
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
    bloom: [u64; 4096],
}

impl Dict {
    fn new(_capacity: usize) -> Self {
        let cap = 262_144;
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, len: 0, bytes: [0; 23] }; cap],
            mask: cap - 1,
            bloom: [0u64; 4096],
        }
    }

    fn insert(&mut self, term: &[u8], term_id: u32) {
        if term.len() > 23 { return; }
        let h = unsafe { hash_bytes_fast(term.as_ptr(), term.len()) };
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = &mut self.table[idx];
            if e.hash == 0 {
                e.hash = h;
                e.term_id = term_id;
                e.len = term.len() as u8;
                for (k, &b) in term.iter().enumerate() {
                    e.bytes[k] = b | 0x20;
                }
                let b_idx = (h as usize & 0x3FFFF) >> 6;
                self.bloom[b_idx] |= 1u64 << (h & 63);
                return;
            }
            if e.hash == h && e.len as usize == term.len() {
                let mut match_tok = true;
                for k in 0..term.len() {
                    if e.bytes[k] != (term[k] | 0x20) {
                        match_tok = false;
                        break;
                    }
                }
                if match_tok {
                    return;
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get(&self, term: &[u8]) -> Option<u32> {
        if term.len() > 23 { return None; }
        let h = unsafe { hash_bytes_fast(term.as_ptr(), term.len()) };
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == term.len() {
                let mut match_tok = true;
                for k in 0..term.len() {
                    if e.bytes[k] != (term[k] | 0x20) {
                        match_tok = false;
                        break;
                    }
                }
                if match_tok {
                    return Some(e.term_id);
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    unsafe fn get_fast(&self, text_ptr: *const u8, len: usize, h: u32) -> Option<u32> {
        let b_idx = (h as usize & 0x3FFFF) >> 6;
        if (*self.bloom.get_unchecked(b_idx) & (1u64 << (h & 63))) == 0 {
            return None;
        }
        if len > 23 { return None; }
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = self.table.get_unchecked(idx);
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == len {
                let mut match_tok = true;
                for k in 0..len {
                    if e.bytes[k] != (*text_ptr.add(k) | 0x20) {
                        match_tok = false;
                        break;
                    }
                }
                if match_tok {
                    return Some(e.term_id);
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[derive(Clone, Copy)]
enum Op {
    And,
    Or,
    Not,
}

enum ParsedQuery<'a> {
    Bool {
        first_term: Option<u32>,
        steps: Vec<(Op, Option<u32>)>,
    },
    Phrase {
        terms: Vec<Option<u32>>,
        raw_tokens: Vec<&'a [u8]>,
    },
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

#[inline(always)]
fn write_u32_fast(n: u32, out: &mut Vec<u8>) {
    write_u64_fast(n as u64, out);
}

#[inline(always)]
fn phrase_matches(doc_text: &[u8], phrase_tokens: &[&[u8]]) -> bool {
    let num_tokens = phrase_tokens.len();
    if num_tokens == 0 { return true; }
    let first_target = unsafe { *phrase_tokens.get_unchecked(0) };
    let first_len = first_target.len();
    let tlen = doc_text.len();
    let mut p = 0;

    while p < tlen {
        while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } {
            p += 1;
        }
        if p >= tlen { break; }
        let w_start = p;
        while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } {
            p += 1;
        }
        let w_len = p - w_start;

        if w_len == first_len {
            let mut matches_first = true;
            for k in 0..first_len {
                if (unsafe { *doc_text.get_unchecked(w_start + k) } | 0x20) != (unsafe { *first_target.get_unchecked(k) } | 0x20) {
                    matches_first = false;
                    break;
                }
            }

            if matches_first {
                let mut scan_p = p;
                let mut matched_all = true;

                for t_idx in 1..num_tokens {
                    let target = unsafe { *phrase_tokens.get_unchecked(t_idx) };
                    let target_len = target.len();

                    while scan_p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(scan_p) as usize) } {
                        scan_p += 1;
                    }
                    if scan_p >= tlen {
                        matched_all = false;
                        break;
                    }
                    let n_start = scan_p;
                    while scan_p < tlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(scan_p) as usize) } {
                        scan_p += 1;
                    }
                    let n_len = scan_p - n_start;

                    if n_len != target_len {
                        matched_all = false;
                        break;
                    }

                    let mut tok_ok = true;
                    for k in 0..target_len {
                        if (unsafe { *doc_text.get_unchecked(n_start + k) } | 0x20) != (unsafe { *target.get_unchecked(k) } | 0x20) {
                            tok_ok = false;
                            break;
                        }
                    }

                    if !tok_ok {
                        matched_all = false;
                        break;
                    }
                }

                if matched_all {
                    return true;
                }
            }
        }
    }
    false
}

#[derive(Clone, Copy)]
struct QueryAccum {
    count: u32,
    id_sum: u64,
    top_k: [u32; 10],
    top_k_len: u8,
}

fn main() {
    unsafe { mallopt(-8, 2); }

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

    let docs_path = format!("{}/docs.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);
    let results_path = format!("{}/results.txt", out_dir);

    let q_mmap = match MmapSlice::open(&queries_path) {
        Some(m) => m,
        None => return,
    };
    let q_data = q_mmap.as_slice();

    let mut dict = Dict::new(131_072);
    let mut num_terms = 0u32;

    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            let mut parts = line[5..].split(|&b| b == b' ');
            while let Some(part) = parts.next() {
                if part == b"AND" || part == b"OR" || part == b"NOT" || part.is_empty() {
                    continue;
                }
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
            let mut terms = Vec::new();
            let mut raw_tokens = Vec::new();
            for part in inner.split(|&b| b == b' ') {
                if !part.is_empty() {
                    terms.push(dict.get(part));
                    raw_tokens.push(part);
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { terms, raw_tokens });
        }
    }

    let num_queries = parsed_queries.len();
    let mut query_accums: Vec<QueryAccum> = vec![
        QueryAccum { count: 0, id_sum: 0, top_k: [0; 10], top_k_len: 0 };
        num_queries
    ];

    let docs_file = match File::open(&docs_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let docs_len = match docs_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    if docs_len == 0 { return; }
    let docs_fd = docs_file.as_raw_fd();

    // Map 64KB around each 1/4 boundary to find exact newline
    let mut block_byte_boundaries = [0usize; NUM_BLOCKS + 1];
    for i in 1..NUM_BLOCKS {
        let target = (docs_len * i) / NUM_BLOCKS;
        let p_start = (target / 4096) * 4096;
        let p_len = (target - p_start + 65536).min(docs_len - p_start);
        let ptr = unsafe { mmap(std::ptr::null_mut(), p_len, 1, 1, docs_fd, p_start as i64) };
        if ptr != usize::MAX as *mut u8 && !ptr.is_null() {
            let slice = unsafe { std::slice::from_raw_parts(ptr, p_len) };
            let mut p = target - p_start;
            while p < p_len && slice[p] != b'\n' { p += 1; }
            if p < p_len { p += 1; }
            block_byte_boundaries[i] = p_start + p;
            unsafe { munmap(ptr, p_len); }
        } else {
            block_byte_boundaries[i] = target;
        }
    }
    block_byte_boundaries[NUM_BLOCKS] = docs_len;

    // The scoring host exposes twelve hardware threads. Keep the same worker
    // count for indexing and per-block query evaluation so the CPU-bound
    // phases do not leave two cores idle.
    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(12).min(12);

    let mut thread_postings: Vec<Vec<u64>> = (0..num_threads)
        .map(|_| Vec::with_capacity(150_000))
        .collect();
    let mut thread_counts: Vec<Vec<u32>> = (0..num_threads)
        .map(|_| vec![0u32; num_terms as usize])
        .collect();
    let mut thread_doc_info: Vec<Vec<(u32, u32)>> = (0..num_threads)
        .map(|_| Vec::with_capacity(8_000))
        .collect();
    let mut thread_last_seen: Vec<Vec<u16>> = (0..num_threads)
        .map(|_| vec![u16::MAX; num_terms as usize])
        .collect();

    let mut running_global_doc_id = 0u32;

    for b in 0..NUM_BLOCKS {
        let block_base_doc = running_global_doc_id;

        let b_start_byte = block_byte_boundaries[b];
        let b_end_byte = block_byte_boundaries[b + 1];
        let b_len = b_end_byte - b_start_byte;
        if b_len == 0 { continue; }

        let page_off = (b_start_byte / 4096) * 4096;
        let page_len = b_end_byte - page_off;
        let ptr = unsafe { mmap(std::ptr::null_mut(), page_len, 1, 1, docs_fd, page_off as i64) };
        if ptr == usize::MAX as *mut u8 || ptr.is_null() { continue; }

        let rel_start = b_start_byte - page_off;
        let block_slice = unsafe { std::slice::from_raw_parts(ptr.add(rel_start), b_len) };

        let mut sub_chunks = Vec::with_capacity(num_threads);
        let mut cur_sb = 0usize;
        for t in 0..num_threads {
            let target = (b_len * (t + 1)) / num_threads;
            let mut end = target.min(b_len);
            while end < b_len && block_slice[end] != b'\n' {
                end += 1;
            }
            if end < b_len {
                end += 1;
            }
            sub_chunks.push((cur_sb, end));
            cur_sb = end;
        }

        std::thread::scope(|s| {
            for ((((postings_buf, counts_buf), doc_info_buf), last_seen_buf), &(sb, eb)) in
                thread_postings.iter_mut()
                .zip(thread_counts.iter_mut())
                .zip(thread_doc_info.iter_mut())
                .zip(thread_last_seen.iter_mut())
                .zip(sub_chunks.iter())
            {
                let dict_ref = &dict;
                let chunk = &block_slice[sb..eb];

                postings_buf.clear();
                counts_buf.fill(0);
                doc_info_buf.clear();
                last_seen_buf.fill(u16::MAX);

                s.spawn(move || {
                    let last_seen = last_seen_buf;
                    let mut i = 0;
                    let clen = chunk.len();
                    let mut doc_offset_in_thread = 0u16;

                    while i < clen {
                        while i < clen && chunk[i] != b'\t' { i += 1; }
                        if i >= clen { break; }
                        i += 1;
                        let text_start_rel = i;
                        while i < clen && chunk[i] != b'\n' { i += 1; }
                        let text_end_rel = i;
                        i += 1;

                        let text_len = (text_end_rel - text_start_rel) as u32;
                        doc_info_buf.push(((sb + text_start_rel) as u32, text_len));

                        let text = &chunk[text_start_rel..text_end_rel];
                        let mut p = 0;
                        let tlen = text.len();
                        let text_ptr = text.as_ptr();

                        while p < tlen {
                            while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                                p += 1;
                            }
                            if p >= tlen { break; }
                            let w_start = p;
                            while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                                p += 1;
                            }
                            let w_len = p - w_start;

                            if w_len <= 23 {
                                let h = unsafe { hash_bytes_fast(text_ptr.add(w_start), w_len) };
                                if let Some(term_id) = unsafe { dict_ref.get_fast(text_ptr.add(w_start), w_len, h) } {
                                    let tid = term_id as usize;
                                    if unsafe { *last_seen.get_unchecked(tid) } != doc_offset_in_thread {
                                        unsafe {
                                            *last_seen.get_unchecked_mut(tid) = doc_offset_in_thread;
                                            *counts_buf.get_unchecked_mut(tid) += 1;
                                        }
                                        postings_buf.push(((term_id as u64) << 16) | (doc_offset_in_thread as u64));
                                    }
                                }
                            }
                        }
                        doc_offset_in_thread += 1;
                    }
                });
            }
        });

        let mut thread_base_loc = Vec::with_capacity(num_threads);
        let mut cur_loc = 0u16;
        let mut total_block_docs = 0usize;
        for t in 0..num_threads {
            thread_base_loc.push(cur_loc);
            cur_loc += thread_doc_info[t].len() as u16;
            total_block_docs += thread_doc_info[t].len();
        }
        running_global_doc_id += cur_loc as u32;

        let mut block_doc_starts = vec![0u32; total_block_docs];
        let mut block_doc_lens = vec![0u32; total_block_docs];
        for t in 0..num_threads {
            let base = thread_base_loc[t] as usize;
            for (idx, &(d_s, d_l)) in thread_doc_info[t].iter().enumerate() {
                block_doc_starts[base + idx] = d_s;
                block_doc_lens[base + idx] = d_l;
            }
        }

        let mut thread_write_ptrs: Vec<Vec<u32>> = (0..num_threads)
            .map(|_| vec![0u32; num_terms as usize])
            .collect();

        let mut b_offsets = vec![0u32; (num_terms as usize) + 1];
        let mut total_postings = 0u32;
        for term in 0..(num_terms as usize) {
            b_offsets[term] = total_postings;
            let mut cur = total_postings;
            for t in 0..num_threads {
                thread_write_ptrs[t][term] = cur;
                cur += thread_counts[t][term];
            }
            total_postings = cur;
        }
        b_offsets[num_terms as usize] = total_postings;

        let mut b_postings = vec![0u16; total_postings as usize];
        let base_ptr = b_postings.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            for (t, t_postings) in thread_postings.iter().enumerate() {
                let mut write_ptrs = std::mem::take(&mut thread_write_ptrs[t]);
                let base_loc = thread_base_loc[t];

                s.spawn(move || {
                    let raw_ptr = base_ptr as *mut u16;
                    for &rec in t_postings {
                        let tid = (rec >> 16) as usize;
                        let doc_offset = (rec & 0xFFFF) as u16;
                        let loc_id = base_loc + doc_offset;
                        let dest = unsafe { *write_ptrs.get_unchecked(tid) };
                        unsafe {
                            *write_ptrs.get_unchecked_mut(tid) += 1;
                            *raw_ptr.add(dest as usize) = loc_id;
                        }
                    }
                });
            }
        });

        // Parallel Query Evaluation against block b
        let num_query_threads = num_threads;
        let query_cursor = std::sync::atomic::AtomicUsize::new(0);
        const QUERY_BATCH: usize = 1024;
        let query_cursor_ref = &query_cursor;
        let parsed_queries_ref = &parsed_queries;
        let b_offsets_ref = &b_offsets;
        let b_postings_ref = &b_postings;
        let block_doc_starts_ref = &block_doc_starts;
        let block_doc_lens_ref = &block_doc_lens;
        let block_slice_ref = block_slice;
        let query_accums_ptr = query_accums.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..num_query_threads).map(|_| {
                s.spawn(move || {
                    let mut buf_a = Vec::with_capacity(16_384);
                    let mut buf_b = Vec::with_capacity(16_384);
                    let base_doc_id = block_base_doc as u64;
                    let raw_accums = query_accums_ptr as *mut QueryAccum;

                    loop {
                        let q_start = query_cursor_ref.fetch_add(QUERY_BATCH, Relaxed);
                        if q_start >= num_queries { break; }
                        let q_end = (q_start + QUERY_BATCH).min(num_queries);
                        for (idx, query) in parsed_queries_ref[q_start..q_end].iter().enumerate() {
                        let res = unsafe { &mut *raw_accums.add(q_start + idx) };
                        match query {
                            ParsedQuery::Bool { first_term, steps } => {
                                let get_postings = |term_id: u32| -> &[u16] {
                                    let tid = term_id as usize;
                                    let s = b_offsets_ref[tid] as usize;
                                    let e = b_offsets_ref[tid + 1] as usize;
                                    &b_postings_ref[s..e]
                                };

                                let r_slice: &[u16] = match *first_term {
                                    Some(t) => get_postings(t),
                                    None => &[],
                                };

                                let all_and = steps.iter().all(|(op, _)| matches!(op, Op::And));
                                if all_and {
                                    let mut min_term = *first_term;
                                    let mut min_len = min_term.map(|t| {
                                        let tid = t as usize;
                                        (b_offsets_ref[tid + 1] - b_offsets_ref[tid]) as usize
                                    }).unwrap_or(0);
                                    for &(_, maybe_term) in steps {
                                        match maybe_term {
                                            Some(t) => {
                                                let tid = t as usize;
                                                let len = (b_offsets_ref[tid + 1] - b_offsets_ref[tid]) as usize;
                                                if min_term.is_some() && len < min_len {
                                                    min_term = Some(t);
                                                    min_len = len;
                                                }
                                            }
                                            None => { min_term = None; break; }
                                        }
                                    }
                                    buf_a.clear();
                                    if let Some(t) = min_term {
                                        buf_a.extend_from_slice(get_postings(t));
                                        if *first_term != min_term {
                                            intersect_u16(&buf_a, r_slice, &mut buf_b);
                                            std::mem::swap(&mut buf_a, &mut buf_b);
                                        }
                                        for &(_, maybe_term) in steps {
                                            if let Some(tid) = maybe_term {
                                                if Some(tid) == min_term { continue; }
                                                intersect_u16(&buf_a, get_postings(tid), &mut buf_b);
                                                std::mem::swap(&mut buf_a, &mut buf_b);
                                                if buf_a.is_empty() { break; }
                                            }
                                        }
                                    }
                                    let count = buf_a.len();
                                    res.count += count as u32;
                                    let loc_sum: u64 = buf_a.iter().map(|&x| x as u64).sum();
                                    res.id_sum += base_doc_id * (count as u64) + loc_sum;
                                    for &loc_id in buf_a.iter().take(10 - res.top_k_len as usize) {
                                        res.top_k[res.top_k_len as usize] = (base_doc_id as u32) + (loc_id as u32);
                                        res.top_k_len += 1;
                                    }
                                } else if steps.is_empty() {
                                    let count = r_slice.len();
                                    res.count += count as u32;
                                    let loc_sum: u64 = r_slice.iter().map(|&x| x as u64).sum();
                                    res.id_sum += base_doc_id * (count as u64) + loc_sum;
                                    for &loc_id in r_slice.iter().take(10 - res.top_k_len as usize) {
                                        res.top_k[res.top_k_len as usize] = (base_doc_id as u32) + (loc_id as u32);
                                        res.top_k_len += 1;
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
                                        let b_slice: &[u16] = match maybe_term {
                                            Some(t) => get_postings(t),
                                            None => &[],
                                        };
                                        match op {
                                            Op::And => intersect_u16(&buf_a, b_slice, &mut buf_b),
                                            Op::Or => union_u16(&buf_a, b_slice, &mut buf_b),
                                            Op::Not => difference_u16(&buf_a, b_slice, &mut buf_b),
                                        }
                                        std::mem::swap(&mut buf_a, &mut buf_b);
                                    }
                                    let count = buf_a.len();
                                    res.count += count as u32;
                                    let loc_sum: u64 = buf_a.iter().map(|&x| x as u64).sum();
                                    res.id_sum += base_doc_id * (count as u64) + loc_sum;
                                    for &loc_id in buf_a.iter().take(10 - res.top_k_len as usize) {
                                        res.top_k[res.top_k_len as usize] = (base_doc_id as u32) + (loc_id as u32);
                                        res.top_k_len += 1;
                                    }
                                }
                            }
                            ParsedQuery::Phrase { terms, raw_tokens } => {
                                if !terms.iter().any(|t| t.is_none()) {
                                    let mut term_ids = [0u32; 8];
                                    let num_t = terms.len().min(8);
                                    for (k, &t) in terms.iter().take(8).enumerate() {
                                        term_ids[k] = t.unwrap();
                                    }
                                    let term_ids = &term_ids[..num_t];

                                    let get_postings = |term_id: u32| -> &[u16] {
                                        let tid = term_id as usize;
                                        let s = b_offsets_ref[tid] as usize;
                                        let e = b_offsets_ref[tid + 1] as usize;
                                        &b_postings_ref[s..e]
                                    };

                                    let mut min_idx = 0;
                                    let mut min_len = usize::MAX;
                                    for (idx, &tid) in term_ids.iter().enumerate() {
                                        let plen = (b_offsets_ref[tid as usize + 1] - b_offsets_ref[tid as usize]) as usize;
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

                                        for &loc_id in &buf_a {
                                            let loc_idx = loc_id as usize;
                                            let d_start = unsafe { *block_doc_starts_ref.get_unchecked(loc_idx) } as usize;
                                            let d_len = unsafe { *block_doc_lens_ref.get_unchecked(loc_idx) } as usize;
                                            let doc_text = &block_slice_ref[d_start..d_start + d_len];
                                            if phrase_matches(doc_text, raw_tokens) {
                                                let doc_id = (base_doc_id as u32) + (loc_id as u32);
                                                res.count += 1;
                                                res.id_sum += doc_id as u64;
                                                if (res.top_k_len as usize) < 10 {
                                                    res.top_k[res.top_k_len as usize] = doc_id;
                                                    res.top_k_len += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        }
                    }
                })
            }).collect();
            for h in handles { h.join().unwrap(); }
        });

        drop(b_postings);
        drop(b_offsets);
        drop(block_doc_starts);
        drop(block_doc_lens);
        unsafe {
            munmap(ptr, page_len);
        }
    }

    drop(dict);
    drop(thread_postings);
    drop(thread_counts);
    drop(thread_doc_info);

    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let mut out_buf = Vec::with_capacity(64 * 1024);
    for res in query_accums {
        write_u32_fast(res.count, &mut out_buf);
        out_buf.push(b' ');
        write_u64_fast(res.id_sum, &mut out_buf);
        for i in 0..(res.top_k_len as usize) {
            out_buf.push(b' ');
            write_u32_fast(res.top_k[i], &mut out_buf);
        }
        out_buf.push(b'\n');

        if out_buf.len() >= 64 * 1024 {
            let _ = out_file.write_all(&out_buf);
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        let _ = out_file.write_all(&out_buf);
    }
}
