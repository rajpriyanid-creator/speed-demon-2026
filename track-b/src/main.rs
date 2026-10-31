use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

const NUM_BLOCKS: usize = 4;
const BLOCK_CAP: u32 = 65_536;

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
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, fd, 0) };
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
struct DictEntry<'a> {
    hash: u32,
    term_id: u32,
    key: &'a [u8],
}

struct Dict<'a> {
    table: Vec<DictEntry<'a>>,
    bloom: [u64; 16384],
    mask: usize,
}

impl<'a> Dict<'a> {
    fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, key: &[] }; cap],
            bloom: [0u64; 16384],
            mask: cap - 1,
        }
    }

    #[inline(always)]
    fn hash_bytes(b: &[u8]) -> u32 {
        let mut h: u32 = 0x811c9dc5;
        for &byte in b {
            h ^= (byte | 0x20) as u32;
            h = h.wrapping_mul(0x01000193);
        }
        if h == 0 { 1 } else { h }
    }

    #[inline(always)]
    fn bloom_check(&self, h: u32) -> bool {
        let b1 = (h as usize) & 0xF_FFFF;
        if (unsafe { *self.bloom.get_unchecked(b1 >> 6) } & (1u64 << (b1 & 63))) == 0 {
            return false;
        }
        let b2 = ((h.rotate_left(11) ^ (h >> 16)) as usize) & 0xF_FFFF;
        (unsafe { *self.bloom.get_unchecked(b2 >> 6) } & (1u64 << (b2 & 63))) != 0
    }

    #[inline(always)]
    fn get_with_hash(&self, raw: &[u8], h: u32) -> Option<u32> {
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.key.eq_ignore_ascii_case(raw) {
                return Some(e.term_id);
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get(&self, raw: &[u8]) -> Option<u32> {
        let h = Self::hash_bytes(raw);
        self.get_with_hash(raw, h)
    }
}

struct FastCatalog<'a> {
    keys: Vec<&'a [u8]>,
    hashes: Vec<u32>,
    table: Vec<(u32, u32)>,
    mask: usize,
}

impl<'a> FastCatalog<'a> {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        Self {
            keys: Vec::with_capacity(cap),
            hashes: Vec::with_capacity(cap),
            table: vec![(0, 0); size],
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, key: &'a [u8], h: u32) -> u32 {
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.0 == h && unsafe { *self.keys.get_unchecked(slot.1 as usize) } == key {
                return slot.1;
            }
            if slot.0 == 0 {
                let id = self.keys.len() as u32;
                self.keys.push(key);
                self.hashes.push(h);
                unsafe { *self.table.get_unchecked_mut(idx) = (h, id); }
                return id;
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

#[derive(Clone, Copy)]
struct QueryResult {
    count: u64,
    id_sum: u64,
    top_k_len: u8,
    top_k: [u32; 10],
}

impl Default for QueryResult {
    fn default() -> Self {
        Self {
            count: 0,
            id_sum: 0,
            top_k_len: 0,
            top_k: [0; 10],
        }
    }
}

#[inline(always)]
fn intersect_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let len_a = a.len();
    let len_b = b.len();

    while i < len_a && j < len_b {
        let val_a = unsafe { *a.get_unchecked(i) };
        let val_b = unsafe { *b.get_unchecked(j) };
        if val_a == val_b {
            out.push(val_a);
            i += 1;
            j += 1;
        } else if val_a < val_b {
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
    let len_a = a.len();
    let len_b = b.len();

    while i < len_a && j < len_b {
        let val_a = unsafe { *a.get_unchecked(i) };
        let val_b = unsafe { *b.get_unchecked(j) };
        if val_a == val_b {
            out.push(val_a);
            i += 1;
            j += 1;
        } else if val_a < val_b {
            out.push(val_a);
            i += 1;
        } else {
            out.push(val_b);
            j += 1;
        }
    }
    if i < len_a {
        out.extend_from_slice(&a[i..]);
    }
    if j < len_b {
        out.extend_from_slice(&b[j..]);
    }
}

#[inline(always)]
fn difference_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let len_a = a.len();
    let len_b = b.len();

    while i < len_a && j < len_b {
        let val_a = unsafe { *a.get_unchecked(i) };
        let val_b = unsafe { *b.get_unchecked(j) };
        if val_a == val_b {
            i += 1;
            j += 1;
        } else if val_a < val_b {
            out.push(val_a);
            i += 1;
        } else {
            j += 1;
        }
    }
    if i < len_a {
        out.extend_from_slice(&a[i..]);
    }
}

#[inline(always)]
fn phrase_matches(doc: &[u8], phrase_tokens: &[&[u8]]) -> bool {
    let num_toks = phrase_tokens.len();
    if num_toks == 0 { return true; }

    let dlen = doc.len();
    let mut p = 0;
    let f_tok = phrase_tokens[0];
    let f_len = f_tok.len();

    while p < dlen {
        while p < dlen && !unsafe { *IS_TOKEN.get_unchecked(*doc.get_unchecked(p) as usize) } {
            p += 1;
        }
        if p >= dlen { break; }
        let tok_start = p;
        while p < dlen && unsafe { *IS_TOKEN.get_unchecked(*doc.get_unchecked(p) as usize) } {
            p += 1;
        }
        let tok_len = p - tok_start;
        if tok_len == f_len && doc[tok_start..p].eq_ignore_ascii_case(f_tok) {
            if num_toks == 1 { return true; }
            let mut curr = p;
            let mut matched = true;
            for &target_tok in &phrase_tokens[1..] {
                while curr < dlen && !unsafe { *IS_TOKEN.get_unchecked(*doc.get_unchecked(curr) as usize) } {
                    curr += 1;
                }
                if curr >= dlen {
                    matched = false;
                    break;
                }
                let t_start = curr;
                while curr < dlen && unsafe { *IS_TOKEN.get_unchecked(*doc.get_unchecked(curr) as usize) } {
                    curr += 1;
                }
                let t_len = curr - t_start;
                if t_len != target_tok.len() || !doc[t_start..curr].eq_ignore_ascii_case(target_tok) {
                    matched = false;
                    break;
                }
            }
            if matched {
                return true;
            }
        }
    }
    false
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

    let docs_path = format!("{}/docs.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);
    let results_path = format!("{}/results.txt", out_dir);

    // 1. Mmap queries.txt & populate dictionary in parallel
    let queries_mmap = MmapSlice::open(&queries_path).expect("failed to open queries.txt");
    let queries_data = queries_mmap.as_slice();
    let q_len = queries_data.len();

    let num_threads = std::thread::available_parallelism()
        .map(|num| num.get())
        .unwrap_or(8)
        .min(12);

    let mut q_boundaries = Vec::with_capacity(num_threads + 1);
    q_boundaries.push(0);
    for i in 1..num_threads {
        let mut pos = (q_len * i) / num_threads;
        while pos < q_len && queries_data[pos] != b'\n' { pos += 1; }
        if pos < q_len { pos += 1; }
        q_boundaries.push(pos.min(q_len));
    }
    q_boundaries.push(q_len);

    // Harvest tokens in parallel
    let thread_catalogs: Vec<FastCatalog> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = q_boundaries[i];
            let end = q_boundaries[i + 1];
            let chunk = &queries_data[start..end];

            handles.push(s.spawn(move || {
                let mut cat = FastCatalog::new(16384);
                for line in chunk.split(|&b| b == b'\n') {
                    let line = if line.ends_with(b"\r") { &line[..line.len().saturating_sub(1)] } else { line };
                    if line.is_empty() { continue; }
                    if line.starts_with(b"BOOL ") {
                        for part in line[5..].split(|&b| b == b' ') {
                            if !part.is_empty() && part != b"AND" && part != b"OR" && part != b"NOT" {
                                let h = Dict::hash_bytes(part);
                                cat.get_or_insert(part, h);
                            }
                        }
                    } else if line.starts_with(b"PHRASE \"") {
                        let inner = &line[8..line.len().saturating_sub(1)];
                        for part in inner.split(|&b| b == b' ') {
                            if !part.is_empty() {
                                let h = Dict::hash_bytes(part);
                                cat.get_or_insert(part, h);
                            }
                        }
                    }
                }
                cat
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Populate global dictionary & bloom filter
    let mut dict = Dict::new(262_144);
    let mut num_terms = 0u32;
    for cat in &thread_catalogs {
        for (&key, &h) in cat.keys.iter().zip(cat.hashes.iter()) {
            let b1 = (h as usize) & 0xF_FFFF;
            let b2 = ((h.rotate_left(11) ^ (h >> 16)) as usize) & 0xF_FFFF;
            dict.bloom[b1 >> 6] |= 1u64 << (b1 & 63);
            dict.bloom[b2 >> 6] |= 1u64 << (b2 & 63);

            let mut idx = (h as usize) & dict.mask;
            loop {
                let slot = &mut dict.table[idx];
                if slot.hash == 0 {
                    slot.hash = h;
                    slot.term_id = num_terms;
                    slot.key = key;
                    num_terms += 1;
                    break;
                }
                if slot.hash == h && slot.key.eq_ignore_ascii_case(key) {
                    break;
                }
                idx = (idx + 1) & dict.mask;
            }
        }
    }
    drop(thread_catalogs);

    // Parallel parse queries
    let thread_parsed_queries: Vec<Vec<ParsedQuery>> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let start = q_boundaries[i];
            let end = q_boundaries[i + 1];
            let chunk = &queries_data[start..end];
            let dict_ref = &dict;

            handles.push(s.spawn(move || {
                let mut local_queries = Vec::with_capacity(5000);
                for line in chunk.split(|&b| b == b'\n') {
                    let line = if line.ends_with(b"\r") { &line[..line.len().saturating_sub(1)] } else { line };
                    if line.is_empty() { continue; }

                    if line.starts_with(b"BOOL ") {
                        let mut parts = line[5..].split(|&b| b == b' ').filter(|s| !s.is_empty());
                        let first_term = parts.next().and_then(|tok| dict_ref.get(tok));
                        let mut steps = Vec::new();

                        while let Some(op_bytes) = parts.next() {
                            let op = match op_bytes {
                                b"AND" => Op::And,
                                b"OR" => Op::Or,
                                b"NOT" => Op::Not,
                                _ => continue,
                            };
                            if let Some(tok) = parts.next() {
                                let term = dict_ref.get(tok);
                                steps.push((op, term));
                            }
                        }
                        local_queries.push(ParsedQuery::Bool { first_term, steps });
                    } else if line.starts_with(b"PHRASE \"") {
                        let inner = &line[8..line.len().saturating_sub(1)];
                        let mut terms = Vec::new();
                        let mut raw_tokens = Vec::new();
                        for part in inner.split(|&b| b == b' ') {
                            if !part.is_empty() {
                                terms.push(dict_ref.get(part));
                                raw_tokens.push(part);
                            }
                        }
                        local_queries.push(ParsedQuery::Phrase { terms, raw_tokens });
                    }
                }
                local_queries
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut parsed_queries = Vec::with_capacity(50_000);
    for chunk in thread_parsed_queries {
        parsed_queries.extend(chunk);
    }

    // 2. Mmap docs.txt
    let docs_mmap = MmapSlice::open(&docs_path).expect("failed to open docs.txt");
    let docs_data = docs_mmap.as_slice();
    let n = docs_data.len();

    let num_index_threads = num_threads;
    let mut doc_chunks = Vec::with_capacity(num_index_threads);
    let mut cur_byte = 0;
    for t in 0..num_index_threads {
        let target_byte = ((n * (t + 1)) / num_index_threads).min(n);
        let mut end_byte = target_byte;
        while end_byte < n && docs_data[end_byte] != b'\n' {
            end_byte += 1;
        }
        if end_byte < n {
            end_byte += 1;
        }
        doc_chunks.push((cur_byte, end_byte));
        cur_byte = end_byte;
    }

    let doc_counts: Vec<u32> = std::thread::scope(|s| {
        let handles: Vec<_> = doc_chunks.iter().map(|&(start, end)| {
            s.spawn(move || {
                let mut count = 0u32;
                let chunk = &docs_data[start..end];
                for &b in chunk {
                    if b == b'\n' { count += 1; }
                }
                count
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut thread_base_doc = vec![0u32; num_index_threads];
    let mut total_docs = 0u32;
    for t in 0..num_index_threads {
        thread_base_doc[t] = total_docs;
        total_docs += doc_counts[t];
    }

    let mut doc_starts = vec![0u32; total_docs as usize];
    let mut doc_lens = vec![0u32; total_docs as usize];
    let doc_starts_ptr = doc_starts.as_mut_ptr() as usize;
    let doc_lens_ptr = doc_lens.as_mut_ptr() as usize;

    // PASS 1: Count term frequencies per (thread, block, term) in flat buffer
    let stride = num_terms as usize;
    let thread_term_counts: Vec<Vec<u32>> = std::thread::scope(|s| {
        let handles: Vec<_> = doc_chunks.iter().enumerate().map(|(t, &(start_byte, end_byte))| {
            let dict_ref = &dict;
            let chunk = &docs_data[start_byte..end_byte];
            let base_doc = thread_base_doc[t];

            s.spawn(move || {
                let raw_starts = doc_starts_ptr as *mut u32;
                let raw_lens = doc_lens_ptr as *mut u32;
                let mut counts = vec![0u32; NUM_BLOCKS * stride];
                let mut last_seen = vec![u32::MAX; stride];

                let mut i = 0;
                let clen = chunk.len();
                let mut local_doc_id = 0u32;

                while i < clen {
                    while i < clen && chunk[i] != b'\t' { i += 1; }
                    if i >= clen { break; }
                    i += 1;
                    let text_start_rel = i;
                    while i < clen && chunk[i] != b'\n' { i += 1; }
                    let text_end_rel = i;
                    i += 1;

                    let doc_id = base_doc + local_doc_id;
                    let block_idx = (doc_id / BLOCK_CAP) as usize;
                    let text_len = text_end_rel - text_start_rel;
                    unsafe {
                        *raw_starts.add(doc_id as usize) = (start_byte + text_start_rel) as u32;
                        *raw_lens.add(doc_id as usize) = text_len as u32;
                    }

                    let text = &chunk[text_start_rel..text_end_rel];
                    let mut p = 0;
                    let tlen = text.len();

                    while p < tlen {
                        while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            p += 1;
                        }
                        if p >= tlen { break; }
                        let tok_start = p;
                        let mut h = 0x811c9dc5u32;
                        while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            let b = unsafe { *text.get_unchecked(p) };
                            h ^= (b | 0x20) as u32;
                            h = h.wrapping_mul(0x01000193);
                            p += 1;
                        }
                        let tok_len = p - tok_start;
                        if tok_len <= 23 {
                            if h == 0 { h = 1; }
                            if dict_ref.bloom_check(h) {
                                if let Some(term_id) = dict_ref.get_with_hash(&text[tok_start..p], h) {
                                    let tid = term_id as usize;
                                    if unsafe { *last_seen.get_unchecked(tid) } != doc_id {
                                        unsafe {
                                            *last_seen.get_unchecked_mut(tid) = doc_id;
                                            *counts.get_unchecked_mut(block_idx * stride + tid) += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    local_doc_id += 1;
                }
                counts
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // 4 separate CSR arrays: one per block
    let mut block_offsets: [Vec<u32>; NUM_BLOCKS] = [
        vec![0u32; stride + 1],
        vec![0u32; stride + 1],
        vec![0u32; stride + 1],
        vec![0u32; stride + 1],
    ];

    let mut thread_write_offsets: Vec<Vec<u32>> = (0..num_index_threads)
        .map(|_| vec![0u32; NUM_BLOCKS * stride])
        .collect();

    let mut block_totals = [0u32; NUM_BLOCKS];
    for b in 0..NUM_BLOCKS {
        let mut total = 0u32;
        let b_offset = b * stride;
        for term in 0..stride {
            block_offsets[b][term] = total;
            let mut slot = total;
            for t in 0..num_index_threads {
                thread_write_offsets[t][b_offset + term] = slot;
                slot += thread_term_counts[t][b_offset + term];
            }
            total = slot;
        }
        block_offsets[b][stride] = total;
        block_totals[b] = total;
    }
    drop(thread_term_counts);

    // Allocate 4 contiguous u16 postings arrays (total 44.4 MB)
    let mut block_postings: [Vec<u16>; NUM_BLOCKS] = [
        vec![0u16; block_totals[0] as usize],
        vec![0u16; block_totals[1] as usize],
        vec![0u16; block_totals[2] as usize],
        vec![0u16; block_totals[3] as usize],
    ];
    let block_ptrs = [
        block_postings[0].as_mut_ptr() as usize,
        block_postings[1].as_mut_ptr() as usize,
        block_postings[2].as_mut_ptr() as usize,
        block_postings[3].as_mut_ptr() as usize,
    ];

    // PASS 2: Stream docs second time, scattering local_id (u16) directly into block_postings
    std::thread::scope(|s| {
        for (t, &(start_byte, end_byte)) in doc_chunks.iter().enumerate() {
            let dict_ref = &dict;
            let chunk = &docs_data[start_byte..end_byte];
            let base_doc = thread_base_doc[t];
            let mut write_ptrs = thread_write_offsets[t].clone();

            s.spawn(move || {
                let raw_bptrs = [
                    block_ptrs[0] as *mut u16,
                    block_ptrs[1] as *mut u16,
                    block_ptrs[2] as *mut u16,
                    block_ptrs[3] as *mut u16,
                ];
                let mut last_seen = vec![u32::MAX; stride];

                let mut i = 0;
                let clen = chunk.len();
                let mut local_doc_id = 0u32;

                while i < clen {
                    while i < clen && chunk[i] != b'\t' { i += 1; }
                    if i >= clen { break; }
                    i += 1;
                    let text_start_rel = i;
                    while i < clen && chunk[i] != b'\n' { i += 1; }
                    let text_end_rel = i;
                    i += 1;

                    let doc_id = base_doc + local_doc_id;
                    let block_idx = (doc_id / BLOCK_CAP) as usize;
                    let local_id = (doc_id % BLOCK_CAP) as u16;

                    let text = &chunk[text_start_rel..text_end_rel];
                    let mut p = 0;
                    let tlen = text.len();

                    while p < tlen {
                        while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            p += 1;
                        }
                        if p >= tlen { break; }
                        let tok_start = p;
                        let mut h = 0x811c9dc5u32;
                        while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            let b = unsafe { *text.get_unchecked(p) };
                            h ^= (b | 0x20) as u32;
                            h = h.wrapping_mul(0x01000193);
                            p += 1;
                        }
                        let tok_len = p - tok_start;
                        if tok_len <= 23 {
                            if h == 0 { h = 1; }
                            if dict_ref.bloom_check(h) {
                                if let Some(term_id) = dict_ref.get_with_hash(&text[tok_start..p], h) {
                                    let tid = term_id as usize;
                                    if unsafe { *last_seen.get_unchecked(tid) } != doc_id {
                                        unsafe {
                                            *last_seen.get_unchecked_mut(tid) = doc_id;
                                            let slot_idx = block_idx * stride + tid;
                                            let slot = *write_ptrs.get_unchecked(slot_idx);
                                            *write_ptrs.get_unchecked_mut(slot_idx) += 1;
                                            *raw_bptrs[block_idx].add(slot as usize) = local_id;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    local_doc_id += 1;
                }
            });
        }
    });

    drop(thread_write_offsets);
    drop(thread_base_doc);

    // 3. Parallel query execution across 4 blocks
    let num_queries = parsed_queries.len();
    let mut results = vec![QueryResult::default(); num_queries];
    let results_ptr = results.as_mut_ptr() as usize;
    let query_cursor = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for _ in 0..num_threads {
            let block_offsets_ref = &block_offsets;
            let block_postings_ref = &block_postings;
            let doc_starts_ref = &doc_starts;
            let doc_lens_ref = &doc_lens;
            let docs_data_ref = docs_data;
            let cursor_ref = &query_cursor;
            let queries_ref = &parsed_queries;

            s.spawn(move || {
                let mut buf_a = Vec::with_capacity(32_768);
                let mut buf_b = Vec::with_capacity(32_768);
                let mut top_k = Vec::with_capacity(10);
                let res_raw = results_ptr as *mut QueryResult;

                const BATCH_SIZE: usize = 32;
                loop {
                    let start_idx = cursor_ref.fetch_add(BATCH_SIZE, Relaxed);
                    if start_idx >= num_queries {
                        break;
                    }
                    let end_idx = (start_idx + BATCH_SIZE).min(num_queries);

                    for q_idx in start_idx..end_idx {
                        let query = &queries_ref[q_idx];
                        top_k.clear();
                        let mut total_count: u64 = 0;
                        let mut total_id_sum: u64 = 0;

                        match query {
                            ParsedQuery::Bool { first_term, steps } => {
                                for b in 0..NUM_BLOCKS {
                                    let offsets = &block_offsets_ref[b];
                                    let postings = &block_postings_ref[b];
                                    let get_postings = |term_id: u32| -> &[u16] {
                                        let tid = term_id as usize;
                                        let start = offsets[tid] as usize;
                                        let end = offsets[tid + 1] as usize;
                                        &postings[start..end]
                                    };

                                    buf_a.clear();
                                    match *first_term {
                                        Some(t) => buf_a.extend_from_slice(get_postings(t)),
                                        None => {}
                                    }

                                    for &(op, term_opt) in steps {
                                        let term_slice: &[u16] = match term_opt {
                                            Some(t) => get_postings(t),
                                            None => &[],
                                        };

                                        match op {
                                            Op::And => intersect_u16(&buf_a, term_slice, &mut buf_b),
                                            Op::Or => union_u16(&buf_a, term_slice, &mut buf_b),
                                            Op::Not => difference_u16(&buf_a, term_slice, &mut buf_b),
                                        }
                                        std::mem::swap(&mut buf_a, &mut buf_b);
                                    }

                                    let count = buf_a.len();
                                    total_count += count as u64;
                                    let base_doc_id = (b as u64) * (BLOCK_CAP as u64);
                                    for &loc_id in &buf_a {
                                        let doc_id = base_doc_id + (loc_id as u64);
                                        total_id_sum += doc_id;
                                        if top_k.len() < 10 {
                                            top_k.push(doc_id as u32);
                                        }
                                    }
                                }

                                let top_k_len = top_k.len().min(10);
                                let mut top_k_arr = [0u32; 10];
                                top_k_arr[..top_k_len].copy_from_slice(&top_k[..top_k_len]);
                                unsafe {
                                    *res_raw.add(q_idx) = QueryResult {
                                        count: total_count,
                                        id_sum: total_id_sum,
                                        top_k_len: top_k_len as u8,
                                        top_k: top_k_arr,
                                    };
                                }
                            }
                            ParsedQuery::Phrase { terms, raw_tokens } => {
                                let mut has_none = false;
                                for t in terms {
                                    if t.is_none() {
                                        has_none = true;
                                        break;
                                    }
                                }

                                if has_none || terms.is_empty() {
                                    unsafe {
                                        *res_raw.add(q_idx) = QueryResult::default();
                                    }
                                    continue;
                                }

                                for b in 0..NUM_BLOCKS {
                                    let offsets = &block_offsets_ref[b];
                                    let postings = &block_postings_ref[b];
                                    let get_postings = |term_id: u32| -> &[u16] {
                                        let tid = term_id as usize;
                                        let start = offsets[tid] as usize;
                                        let end = offsets[tid + 1] as usize;
                                        &postings[start..end]
                                    };

                                    buf_a.clear();
                                    buf_a.extend_from_slice(get_postings(terms[0].unwrap()));

                                    for &t_opt in &terms[1..] {
                                        let t_slice = get_postings(t_opt.unwrap());
                                        intersect_u16(&buf_a, t_slice, &mut buf_b);
                                        std::mem::swap(&mut buf_a, &mut buf_b);
                                        if buf_a.is_empty() {
                                            break;
                                        }
                                    }

                                    let base_doc_id = (b as u32) * BLOCK_CAP;
                                    for &loc_id in &buf_a {
                                        let doc_id = base_doc_id + (loc_id as u32);
                                        let doc_idx = doc_id as usize;
                                        let d_start = unsafe { *doc_starts_ref.get_unchecked(doc_idx) } as usize;
                                        let d_len = unsafe { *doc_lens_ref.get_unchecked(doc_idx) } as usize;
                                        let doc_text = &docs_data_ref[d_start..d_start + d_len];

                                        if phrase_matches(doc_text, raw_tokens) {
                                            total_count += 1;
                                            total_id_sum += doc_id as u64;
                                            if top_k.len() < 10 {
                                                top_k.push(doc_id);
                                            }
                                        }
                                    }
                                }

                                let top_k_len = top_k.len().min(10);
                                let mut top_k_arr = [0u32; 10];
                                top_k_arr[..top_k_len].copy_from_slice(&top_k[..top_k_len]);
                                unsafe {
                                    *res_raw.add(q_idx) = QueryResult {
                                        count: total_count,
                                        id_sum: total_id_sum,
                                        top_k_len: top_k_len as u8,
                                        top_k: top_k_arr,
                                    };
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = File::create(&results_path).expect("failed to create results.txt");
    let mut out_buf = Vec::with_capacity(num_queries * 48);

    for r in &results {
        write_u64(r.count, &mut out_buf);
        out_buf.push(b' ');
        write_u64(r.id_sum, &mut out_buf);
        for i in 0..r.top_k_len as usize {
            out_buf.push(b' ');
            write_u64(r.top_k[i] as u64, &mut out_buf);
        }
        out_buf.push(b'\n');
    }
    out_file.write_all(&out_buf).expect("failed to write results.txt");
}
