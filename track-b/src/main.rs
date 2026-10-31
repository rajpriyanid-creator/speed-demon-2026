use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn malloc_trim(pad: usize) -> i32;
}

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
    bloom: [u64; 16384],
    mask: usize,
}

impl Dict {
    fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, len: 0, bytes: [0; 23] }; cap],
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

    fn insert(&mut self, term: &[u8], term_id: u32) {
        if term.len() > 23 { return; }
        let h = Self::hash_bytes(term);
        let b1 = (h as usize) & 0xF_FFFF;
        let b2 = ((h.rotate_left(11) ^ (h >> 16)) as usize) & 0xF_FFFF;
        self.bloom[b1 >> 6] |= 1u64 << (b1 & 63);
        self.bloom[b2 >> 6] |= 1u64 << (b2 & 63);

        let mut idx = (h as usize) & self.mask;
        loop {
            let e = &mut self.table[idx];
            if e.hash == 0 {
                e.hash = h;
                e.term_id = term_id;
                e.len = term.len() as u8;
                e.bytes[..term.len()].copy_from_slice(term);
                return;
            }
            if e.hash == h && e.len as usize == term.len() && &e.bytes[..term.len()] == term {
                return;
            }
            idx = (idx + 1) & self.mask;
        }
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
    fn get(&self, term: &[u8]) -> Option<u32> {
        if term.len() > 23 { return None; }
        let h = Self::hash_bytes(term);
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == term.len() && &e.bytes[..term.len()] == term {
                return Some(e.term_id);
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get_with_hash(&self, raw: &[u8], h: u32) -> Option<u32> {
        let len = raw.len();
        if len > 23 { return None; }
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == len {
                let mut match_tok = true;
                for k in 0..len {
                    if unsafe { *e.bytes.get_unchecked(k) != (*raw.get_unchecked(k) | 0x20) } {
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
fn phrase_matches(doc_text: &[u8], phrase: &[&[u8]]) -> bool {
    if phrase.is_empty() { return true; }
    let t0 = phrase[0];
    let l0 = t0.len();
    let b0_lower = t0[0];
    let b0_upper = b0_lower.to_ascii_uppercase();
    let tlen = doc_text.len();
    if tlen < l0 { return false; }

    let mut p = 0;
    while p + l0 <= tlen {
        let b = unsafe { *doc_text.get_unchecked(p) };
        if b == b0_lower || b == b0_upper {
            let left_ok = p == 0 || !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p - 1) as usize) };
            if left_ok {
                let mut match_t0 = true;
                for m in 1..l0 {
                    if unsafe { (*doc_text.get_unchecked(p + m) | 0x20) != *t0.get_unchecked(m) } {
                        match_t0 = false;
                        break;
                    }
                }
                if match_t0 {
                    let end_t0 = p + l0;
                    let right_ok = end_t0 == tlen || !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(end_t0) as usize) };
                    if right_ok {
                        let mut curr_p = end_t0;
                        let mut all_match = true;
                        for &next_tok in &phrase[1..] {
                            while curr_p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(curr_p) as usize) } {
                                curr_p += 1;
                            }
                            if curr_p + next_tok.len() > tlen {
                                all_match = false;
                                break;
                            }
                            let nt_start = curr_p;
                            for m in 0..next_tok.len() {
                                if unsafe { (*doc_text.get_unchecked(nt_start + m) | 0x20) != *next_tok.get_unchecked(m) } {
                                    all_match = false;
                                    break;
                                }
                            }
                            if !all_match { break; }
                            curr_p += next_tok.len();
                            let end_ok = curr_p == tlen || !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(curr_p) as usize) };
                            if !end_ok {
                                all_match = false;
                                break;
                            }
                        }
                        if all_match {
                            return true;
                        }
                    }
                }
            }
        }
        p += 1;
    }
    false
}

#[inline(always)]
fn galloping_search(slice: &[u32], target: u32) -> usize {
    let len = slice.len();
    if len == 0 || unsafe { *slice.get_unchecked(0) } >= target {
        return 0;
    }
    let mut low = 0;
    let mut high = 1;
    while high < len && unsafe { *slice.get_unchecked(high) } < target {
        low = high;
        high <<= 1;
    }
    high = high.min(len);
    while low < high {
        let mid = low + ((high - low) >> 1);
        if unsafe { *slice.get_unchecked(mid) } < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

#[inline(always)]
fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let na = a.len();
    let nb = b.len();
    if na == 0 || nb == 0 { return; }

    if na * 16 < nb {
        let mut j = 0;
        for &va in a {
            if j >= nb { break; }
            let adv = galloping_search(&b[j..], va);
            j += adv;
            if j < nb && unsafe { *b.get_unchecked(j) } == va {
                out.push(va);
                j += 1;
            }
        }
        return;
    }
    if nb * 16 < na {
        let mut i = 0;
        for &vb in b {
            if i >= na { break; }
            let adv = galloping_search(&a[i..], vb);
            i += adv;
            if i < na && unsafe { *a.get_unchecked(i) } == vb {
                out.push(vb);
                i += 1;
            }
        }
        return;
    }

    let mut i = 0;
    let mut j = 0;
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
fn union(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
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
fn difference(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let na = a.len();
    let nb = b.len();
    if nb == 0 {
        out.extend_from_slice(a);
        return;
    }
    if na == 0 {
        return;
    }

    if na * 16 < nb {
        let mut j = 0;
        for &va in a {
            if j < nb {
                let adv = galloping_search(&b[j..], va);
                j += adv;
            }
            if j >= nb || unsafe { *b.get_unchecked(j) } != va {
                out.push(va);
            } else {
                j += 1;
            }
        }
        return;
    }

    let mut i = 0;
    let mut j = 0;
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
fn write_u64(mut n: u64, out: &mut Vec<u8>) {
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

    // 1. Mmap queries.txt & populate dictionary
    let queries_mmap = MmapSlice::open(&queries_path).expect("failed to open queries.txt");
    let queries_data = queries_mmap.as_slice();

    let mut dict = Dict::new(262_144);
    let mut num_terms = 0u32;

    for line in queries_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len().saturating_sub(1)] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            for part in line[5..].split(|&b| b == b' ') {
                if !part.is_empty() && part != b"AND" && part != b"OR" && part != b"NOT" {
                    let mut lower = [0u8; 24];
                    if part.len() <= 23 {
                        for (i, &b) in part.iter().enumerate() { lower[i] = b | 0x20; }
                        let term_slice = &lower[..part.len()];
                        let prev = dict.get_with_hash(term_slice, Dict::hash_bytes(term_slice));
                        if prev.is_none() {
                            dict.insert(term_slice, num_terms);
                            num_terms += 1;
                        }
                    }
                }
            }
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            for part in inner.split(|&b| b == b' ') {
                if !part.is_empty() {
                    let mut lower = [0u8; 24];
                    if part.len() <= 23 {
                        for (i, &b) in part.iter().enumerate() { lower[i] = b | 0x20; }
                        let term_slice = &lower[..part.len()];
                        let prev = dict.get_with_hash(term_slice, Dict::hash_bytes(term_slice));
                        if prev.is_none() {
                            dict.insert(term_slice, num_terms);
                            num_terms += 1;
                        }
                    }
                }
            }
        }
    }

    // Pass 2: parse queries into token IDs
    let mut parsed_queries = Vec::with_capacity(50_000);
    for line in queries_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len().saturating_sub(1)] } else { line };
        if line.is_empty() { continue; }

        if line.starts_with(b"BOOL ") {
            let mut parts = line[5..].split(|&b| b == b' ').filter(|s| !s.is_empty());
            let first_term = parts.next().and_then(|tok| dict.get(tok));
            let mut steps = Vec::new();

            while let Some(op_bytes) = parts.next() {
                let op = match op_bytes {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => continue,
                };
                if let Some(tok) = parts.next() {
                    let term = dict.get(tok);
                    steps.push((op, term));
                }
            }
            parsed_queries.push(ParsedQuery::Bool { first_term, steps });
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

    // 2. Mmap docs.txt
    let docs_mmap = MmapSlice::open(&docs_path).expect("failed to open docs.txt");
    let docs_data = docs_mmap.as_slice();
    let n = docs_data.len();

    let num_index_threads = std::thread::available_parallelism()
        .map(|num| num.get())
        .unwrap_or(6)
        .min(12);

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

    // Count document counts per chunk to establish global document ID mapping
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

    // PASS 1: Count term frequencies and collect doc text byte spans
    let thread_term_counts: Vec<Vec<u32>> = std::thread::scope(|s| {
        let handles: Vec<_> = doc_chunks.iter().enumerate().map(|(t, &(start_byte, end_byte))| {
            let dict_ref = &dict;
            let chunk = &docs_data[start_byte..end_byte];
            let base_doc = thread_base_doc[t];

            s.spawn(move || {
                let raw_starts = doc_starts_ptr as *mut u32;
                let raw_lens = doc_lens_ptr as *mut u32;
                let mut term_counts = vec![0u32; num_terms as usize];
                let mut last_seen = vec![u32::MAX; num_terms as usize];

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
                                            *term_counts.get_unchecked_mut(tid) += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    local_doc_id += 1;
                }
                term_counts
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Compute prefix sums: CSR offsets for each term and exact thread write pointers
    let mut offsets = vec![0u32; (num_terms as usize) + 1];
    let mut thread_write_offsets: Vec<Vec<u32>> = (0..num_index_threads)
        .map(|_| vec![0u32; num_terms as usize])
        .collect();

    let mut total_postings = 0u32;
    for term in 0..num_terms as usize {
        offsets[term] = total_postings;
        let mut slot = total_postings;
        for t in 0..num_index_threads {
            thread_write_offsets[t][term] = slot;
            slot += thread_term_counts[t][term];
        }
        total_postings = slot;
    }
    offsets[num_terms as usize] = total_postings;
    drop(thread_term_counts);

    // Allocate single contiguous CSR postings array
    let mut all_postings = vec![0u32; total_postings as usize];
    let postings_ptr = all_postings.as_mut_ptr() as usize;

    // PASS 2: Stream docs second time, scattering doc_ids DIRECTLY into all_postings with zero sorting!
    std::thread::scope(|s| {
        for (t, &(start_byte, end_byte)) in doc_chunks.iter().enumerate() {
            let dict_ref = &dict;
            let chunk = &docs_data[start_byte..end_byte];
            let base_doc = thread_base_doc[t];
            let mut write_ptrs = thread_write_offsets[t].clone();

            s.spawn(move || {
                let raw_postings = postings_ptr as *mut u32;
                let mut last_seen = vec![u32::MAX; num_terms as usize];
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
                                            let slot = *write_ptrs.get_unchecked(tid);
                                            *write_ptrs.get_unchecked_mut(tid) += 1;
                                            *raw_postings.add(slot as usize) = doc_id;
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
    unsafe { malloc_trim(0); }

    // 3. Parallel query execution
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(12);

    let chunk_size = (parsed_queries.len() + num_threads - 1) / num_threads;

    let thread_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = parsed_queries.chunks(chunk_size).map(|chunk| {
            let offsets_ref = &offsets;
            let all_postings_ref = &all_postings;
            let doc_starts_ref = &doc_starts;
            let doc_lens_ref = &doc_lens;
            let docs_data_ref = docs_data;

            s.spawn(move || {
                let mut thread_out = Vec::with_capacity(chunk.len() * 32);
                let mut buf_a = Vec::with_capacity(32_768);
                let mut buf_b = Vec::with_capacity(32_768);
                let mut matches = Vec::new();

                let get_postings = |term_id: u32| -> &[u32] {
                    let tid = term_id as usize;
                    let start = offsets_ref[tid] as usize;
                    let end = offsets_ref[tid + 1] as usize;
                    &all_postings_ref[start..end]
                };

                for query in chunk {
                    match query {
                        ParsedQuery::Bool { first_term, steps } => {
                            buf_a.clear();
                            match *first_term {
                                Some(t) => buf_a.extend_from_slice(get_postings(t)),
                                None => {}
                            }

                            for &(op, term_opt) in steps {
                                let term_slice: &[u32] = match term_opt {
                                    Some(t) => get_postings(t),
                                    None => &[],
                                };

                                match op {
                                    Op::And => intersect(&buf_a, term_slice, &mut buf_b),
                                    Op::Or => union(&buf_a, term_slice, &mut buf_b),
                                    Op::Not => difference(&buf_a, term_slice, &mut buf_b),
                                }
                                std::mem::swap(&mut buf_a, &mut buf_b);
                            }

                            let count = buf_a.len();
                            let mut id_sum: u64 = 0;
                            for &id in &buf_a {
                                id_sum += id as u64;
                            }

                            write_u64(count as u64, &mut thread_out);
                            thread_out.push(b' ');
                            write_u64(id_sum, &mut thread_out);

                            let top_k = count.min(10);
                            for &id in &buf_a[..top_k] {
                                thread_out.push(b' ');
                                write_u64(id as u64, &mut thread_out);
                            }
                            thread_out.push(b'\n');
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
                                thread_out.extend_from_slice(b"0 0\n");
                                continue;
                            }

                            buf_a.clear();
                            buf_a.extend_from_slice(get_postings(terms[0].unwrap()));

                            for &t_opt in &terms[1..] {
                                let t_slice = get_postings(t_opt.unwrap());
                                intersect(&buf_a, t_slice, &mut buf_b);
                                std::mem::swap(&mut buf_a, &mut buf_b);
                                if buf_a.is_empty() {
                                    break;
                                }
                            }

                            matches.clear();
                            let mut count = 0usize;
                            let mut id_sum: u64 = 0;

                            for &doc_id in &buf_a {
                                let doc_idx = doc_id as usize;
                                let d_start = unsafe { *doc_starts_ref.get_unchecked(doc_idx) } as usize;
                                let d_len = unsafe { *doc_lens_ref.get_unchecked(doc_idx) } as usize;
                                let doc_text = &docs_data_ref[d_start..d_start + d_len];

                                if phrase_matches(doc_text, raw_tokens) {
                                    count += 1;
                                    id_sum += doc_id as u64;
                                    if matches.len() < 10 {
                                        matches.push(doc_id);
                                    }
                                }
                            }

                            write_u64(count as u64, &mut thread_out);
                            thread_out.push(b' ');
                            write_u64(id_sum, &mut thread_out);

                            for &id in &matches {
                                thread_out.push(b' ');
                                write_u64(id as u64, &mut thread_out);
                            }
                            thread_out.push(b'\n');
                        }
                    }
                }

                thread_out
            })
        }).collect();

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = File::create(&results_path).expect("failed to create results.txt");
    for out in thread_outputs {
        out_file.write_all(&out).expect("failed to write results");
    }
    out_file.flush().expect("failed to flush results");
}
