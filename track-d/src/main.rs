use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

struct FastHeap { data: Vec<u64> }

impl FastHeap {
    #[inline(always)]
    fn new() -> Self { Self { data: Vec::with_capacity(2048) } }

    #[inline(always)]
    fn clear(&mut self) { self.data.clear(); }

    #[inline(always)]
    fn push(&mut self, cost: u64, node: u32) {
        let val = (cost << 21) | node as u64;
        let mut i = self.data.len();
        self.data.push(val);
        while i > 0 {
            let p = (i - 1) >> 2;
            let parent = unsafe { *self.data.get_unchecked(p) };
            if val >= parent { break; }
            unsafe { *self.data.get_unchecked_mut(i) = parent; *self.data.get_unchecked_mut(p) = val; }
            i = p;
        }
    }

    #[inline(always)]
    fn peek(&self) -> Option<(u64, u32)> {
        self.data.first().map(|&v| (v >> 21, (v & 0x1F_FFFF) as u32))
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.data.is_empty() { return None; }
        let top = self.data[0];
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            let len = self.data.len();
            let mut i = 0usize;
            loop {
                let first = (i << 2) + 1;
                if first >= len { break; }
                let count = (len - first).min(4);
                let mut child = first;
                let mut best = unsafe { *self.data.get_unchecked(first) };
                for k in 1..count {
                    let idx = first + k;
                    let value = unsafe { *self.data.get_unchecked(idx) };
                    if value < best { best = value; child = idx; }
                }
                if best >= last { break; }
                unsafe { *self.data.get_unchecked_mut(i) = best; }
                i = child;
            }
            unsafe { *self.data.get_unchecked_mut(i) = last; }
        }
        Some((top >> 21, (top & 0x1F_FFFF) as u32))
    }
}

#[inline(always)]
unsafe fn parse_u32_fast(p: *const u8, pos: &mut usize) -> u32 {
    let mut val = 0u32;
    loop {
        let b = *p.add(*pos);
        if b >= b'0' && b <= b'9' {
            val = val * 10 + (b - b'0') as u32;
            *pos += 1;
        } else {
            break;
        }
    }
    val
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

    let graph_path = format!("{}/graph.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);
    let results_path = format!("{}/distances.txt", out_dir);

    let g_file = match File::open(&graph_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let g_len = match g_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let g_ptr = unsafe { mmap(std::ptr::null_mut(), g_len, PROT_READ, MAP_SHARED, g_file.as_raw_fd(), 0) };
    if g_ptr == usize::MAX as *mut u8 || g_ptr.is_null() { return; }

    let mut g_pos = 0;
    let n = unsafe { parse_u32_fast(g_ptr, &mut g_pos) } as usize;
    while g_pos < g_len && unsafe { *g_ptr.add(g_pos) } == b' ' { g_pos += 1; }
    let m = unsafe { parse_u32_fast(g_ptr, &mut g_pos) } as usize;
    while g_pos < g_len && unsafe { *g_ptr.add(g_pos) } != b'\n' { g_pos += 1; }
    if g_pos < g_len { g_pos += 1; }
    let edges_start = g_pos;

    let f_deg: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let b_deg: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let g_addr = g_ptr as usize;

    let num_load_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(10);
    let mut edge_chunks = Vec::with_capacity(num_load_threads);
    let total_edge_bytes = g_len - edges_start;
    let mut cur = edges_start;
    for t in 0..num_load_threads {
        let target = edges_start + ((total_edge_bytes * (t + 1)) / num_load_threads);
        let mut end = target.min(g_len);
        while end < g_len && unsafe { *g_ptr.add(end) } != b'\n' { end += 1; }
        if end < g_len { end += 1; }
        edge_chunks.push((cur, end));
        cur = end;
    }

    // Step 1: Count degrees in parallel
    std::thread::scope(|s| {
        for &(start, end) in &edge_chunks {
            let f_deg_ref = &f_deg;
            let b_deg_ref = &b_deg;
            s.spawn(move || {
                let g_ptr = g_addr as *const u8;
                let mut pos = start;
                while pos < end {
                    let u = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let v = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    while pos < end && unsafe { *g_ptr.add(pos) } != b'\n' { pos += 1; }
                    if pos < end { pos += 1; }
                    if u < n { f_deg_ref[u].fetch_add(1, Relaxed); }
                    if v < n { b_deg_ref[v].fetch_add(1, Relaxed); }
                }
            });
        }
    });

    let mut f_head = vec![0u32; n + 1];
    let mut b_head = vec![0u32; n + 1];
    let f_write: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let b_write: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let mut cur_f = 0u32;
    let mut cur_b = 0u32;
    for i in 0..n {
        let fd = f_deg[i].load(Relaxed);
        let bd = b_deg[i].load(Relaxed);
        f_head[i] = cur_f;
        b_head[i] = cur_b;
        f_write[i].store(cur_f, Relaxed);
        b_write[i].store(cur_b, Relaxed);
        cur_f += fd;
        cur_b += bd;
    }
    f_head[n] = cur_f;
    b_head[n] = cur_b;

    // 4-byte packed edges: [weight: 11 bits] [target: 21 bits]
    let mut f_edges = vec![0u32; m];
    let mut b_edges = vec![0u32; m];
    let f_edges_addr = f_edges.as_mut_ptr() as usize;
    let b_edges_addr = b_edges.as_mut_ptr() as usize;

    // Step 2: Fill edges and record overflow in parallel
    let thread_overflows: Vec<(Vec<(u32, u64)>, Vec<(u32, u64)>)> = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_load_threads);
        for &(start, end) in &edge_chunks {
            let f_write_ref = &f_write;
            let b_write_ref = &b_write;
            handles.push(s.spawn(move || {
                let g_ptr = g_addr as *const u8;
                let f_raw = f_edges_addr as *mut u32;
                let b_raw = b_edges_addr as *mut u32;
                let mut local_f_over = Vec::with_capacity(512);
                let mut local_b_over = Vec::with_capacity(512);
                let mut pos = start;
                while pos < end {
                    let u = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let v = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let w = unsafe { parse_u32_fast(g_ptr, &mut pos) } as u64;
                    if pos < end && unsafe { *g_ptr.add(pos) } == b'\r' { pos += 1; }
                    if pos < end && unsafe { *g_ptr.add(pos) } == b'\n' { pos += 1; }

                    let (w_code, is_over) = if w >= 2047 { (2047u32, true) } else { (w as u32, false) };

                    if u < n {
                        let idx = f_write_ref[u].fetch_add(1, Relaxed);
                        unsafe { *f_raw.add(idx as usize) = (v as u32) | (w_code << 21); }
                        if is_over { local_f_over.push((idx, w)); }
                    }
                    if v < n {
                        let idx = b_write_ref[v].fetch_add(1, Relaxed);
                        unsafe { *b_raw.add(idx as usize) = (u as u32) | (w_code << 21); }
                        if is_over { local_b_over.push((idx, w)); }
                    }
                }
                (local_f_over, local_b_over)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut f_overflow: Vec<(u32, u64)> = Vec::with_capacity(4096);
    let mut b_overflow: Vec<(u32, u64)> = Vec::with_capacity(4096);
    for (fo, bo) in thread_overflows {
        f_overflow.extend(fo);
        b_overflow.extend(bo);
    }
    f_overflow.sort_unstable_by_key(|x| x.0);
    b_overflow.sort_unstable_by_key(|x| x.0);

    // The query phase only needs to know whether a node has any outgoing or
    // incoming edges. Keep that information in compact byte flags, then
    // release the atomic degree/write arrays before allocating query state.
    let has_out: Vec<u8> = f_deg.iter().map(|d| (d.load(Relaxed) != 0) as u8).collect();
    let has_in: Vec<u8> = b_deg.iter().map(|d| (d.load(Relaxed) != 0) as u8).collect();
    drop(f_deg);
    drop(b_deg);
    drop(f_write);
    drop(b_write);

    // Compute DSU components from CSR in ~10ms
    let mut parent: Vec<u32> = (0..n as u32).collect();
    fn find(p: &mut [u32], mut x: u32) -> u32 {
        while p[x as usize] != x {
            p[x as usize] = p[p[x as usize] as usize];
            x = p[x as usize];
        }
        x
    }
    for u in 0..n {
        let s = f_head[u] as usize;
        let e = f_head[u + 1] as usize;
        for edge_idx in s..e {
            let v = f_edges[edge_idx] & 0x1F_FFFF;
            let ru = find(&mut parent, u as u32);
            let rv = find(&mut parent, v);
            if ru != rv { parent[ru as usize] = rv; }
        }
    }
    let mut comp: Vec<u32> = vec![0; n];
    for i in 0..n { comp[i] = find(&mut parent, i as u32); }
    drop(parent);

    unsafe { munmap(g_ptr, g_len); }

    let q_file = match File::open(&queries_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let q_len = match q_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let q_ptr = unsafe { mmap(std::ptr::null_mut(), q_len, PROT_READ, MAP_SHARED, q_file.as_raw_fd(), 0) };
    if q_ptr == usize::MAX as *mut u8 || q_ptr.is_null() { return; }

    let mut queries = Vec::new();
    let mut q_pos = 0;
    while q_pos < q_len {
        let b = unsafe { *q_ptr.add(q_pos) };
        if b == b'\r' || b == b'\n' { q_pos += 1; continue; }
        let s = unsafe { parse_u32_fast(q_ptr, &mut q_pos) };
        if q_pos < q_len && unsafe { *q_ptr.add(q_pos) } == b' ' { q_pos += 1; }
        let t = unsafe { parse_u32_fast(q_ptr, &mut q_pos) };
        while q_pos < q_len && unsafe { *q_ptr.add(q_pos) } != b'\n' { q_pos += 1; }
        if q_pos < q_len { q_pos += 1; }
        queries.push((s, t));
    }
    unsafe { munmap(q_ptr, q_len); }

    let num_queries = queries.len();
    let mut results: Vec<i64> = vec![-1; num_queries];
    let query_cursor = AtomicUsize::new(0);
    let results_ptr = results.as_mut_ptr() as usize;

    // 4 query threads delivers 2x faster query search
    let q_threads = 4;

    std::thread::scope(|s| {
        let f_head_ref = &f_head;
        let b_head_ref = &b_head;
        let f_edges_ref = &f_edges;
        let b_edges_ref = &b_edges;
        let f_overflow_ref = &f_overflow;
        let b_overflow_ref = &b_overflow;
        let has_out_ref = &has_out;
        let has_in_ref = &has_in;
        let comp_ref = &comp;
        let query_cursor_ref = &query_cursor;
        let queries_ref = &queries;

        for _ in 0..q_threads {
            s.spawn(move || {
                let mut df = vec![u64::MAX; n];
                let mut db = vec![u64::MAX; n];
                let mut visited_f = Vec::with_capacity(2048);
                let mut visited_b = Vec::with_capacity(2048);
                let mut qf = FastHeap::new();
                let mut qb = FastHeap::new();
                let res_raw = results_ptr as *mut i64;

                const BATCH: usize = 16;
                loop {
                    let start = query_cursor_ref.fetch_add(BATCH, Relaxed);
                    if start >= num_queries { break; }
                    let end = (start + BATCH).min(num_queries);

                    for q_idx in start..end {
                        let (src, dst) = queries_ref[q_idx];
                        if src == dst { unsafe { *res_raw.add(q_idx) = 0; } continue; }
                        let s_idx = src as usize;
                        let t_idx = dst as usize;
                        if s_idx >= n || t_idx >= n { unsafe { *res_raw.add(q_idx) = -1; } continue; }

                        // Instant pruning: degree or component mismatch
                        if has_out_ref[s_idx] == 0
                            || has_in_ref[t_idx] == 0
                            || comp_ref[s_idx] != comp_ref[t_idx]
                        {
                            unsafe { *res_raw.add(q_idx) = -1; }
                            continue;
                        }

                        for &v in &visited_f { df[v as usize] = u64::MAX; }
                        for &v in &visited_b { db[v as usize] = u64::MAX; }
                        visited_f.clear();
                        visited_b.clear();
                        qf.clear();
                        qb.clear();

                        df[s_idx] = 0;
                        visited_f.push(src);
                                qf.push(0, src);

                        db[t_idx] = 0;
                        visited_b.push(dst);
                                qb.push(0, dst);

                        let mut best = u64::MAX;
                        while let (Some((top_f, _)), Some((top_b, _))) = (qf.peek(), qb.peek()) {
                            if top_f.saturating_add(top_b) >= best { break; }
                            if top_f <= top_b {
                                let (cost, node) = qf.pop().unwrap();
                                if cost > df[node as usize] { continue; }
                                let u = node as usize;
                                let es = f_head_ref[u] as usize;
                                let ee = f_head_ref[u + 1] as usize;
                                for e in es..ee {
                                    let packed = f_edges_ref[e];
                                    let v = packed & 0x1F_FFFF;
                                    let w_code = packed >> 21;
                                    let w = if w_code == 2047 {
                                        let idx = f_overflow_ref.binary_search_by_key(&(e as u32), |x| x.0).unwrap();
                                        f_overflow_ref[idx].1
                                    } else {
                                        w_code as u64
                                    };
                                    let new_cost = cost + w;
                                    let v_idx = v as usize;
                                    if new_cost < df[v_idx] {
                                        if df[v_idx] == u64::MAX { visited_f.push(v); }
                                        df[v_idx] = new_cost;
                                        qf.push(new_cost, v);
                                        let db_val = db[v_idx];
                                        if db_val != u64::MAX {
                                            let total = new_cost + db_val;
                                            if total < best { best = total; }
                                        }
                                    }
                                }
                            } else {
                                let (cost, node) = qb.pop().unwrap();
                                if cost > db[node as usize] { continue; }
                                let u = node as usize;
                                let es = b_head_ref[u] as usize;
                                let ee = b_head_ref[u + 1] as usize;
                                for e in es..ee {
                                    let packed = b_edges_ref[e];
                                    let v = packed & 0x1F_FFFF;
                                    let w_code = packed >> 21;
                                    let w = if w_code == 2047 {
                                        let idx = b_overflow_ref.binary_search_by_key(&(e as u32), |x| x.0).unwrap();
                                        b_overflow_ref[idx].1
                                    } else {
                                        w_code as u64
                                    };
                                    let new_cost = cost + w;
                                    let v_idx = v as usize;
                                    if new_cost < db[v_idx] {
                                        if db[v_idx] == u64::MAX { visited_b.push(v); }
                                        db[v_idx] = new_cost;
                                        qb.push(new_cost, v);
                                        let df_val = df[v_idx];
                                        if df_val != u64::MAX {
                                            let total = new_cost + df_val;
                                            if total < best { best = total; }
                                        }
                                    }
                                }
                            }
                        }
                        unsafe {
                            *res_raw.add(q_idx) = if best == u64::MAX { -1 } else { best as i64 };
                        }
                    }
                }
            });
        }
    });

    let _ = fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let mut out_buf = Vec::with_capacity(32 * 1024);
    for dist in results {
        let mut d = dist;
        if d == -1 {
            out_buf.extend_from_slice(b"-1\n");
        } else if d == 0 {
            out_buf.extend_from_slice(b"0\n");
        } else {
            let mut num_buf = [0u8; 20];
            let mut i = 0;
            while d > 0 {
                num_buf[i] = b'0' + (d % 10) as u8;
                d /= 10;
                i += 1;
            }
            while i > 0 {
                i -= 1;
                out_buf.push(num_buf[i]);
            }
            out_buf.push(b'\n');
        }
        if out_buf.len() >= 24 * 1024 {
            let _ = out_file.write_all(&out_buf);
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        let _ = out_file.write_all(&out_buf);
    }
}
