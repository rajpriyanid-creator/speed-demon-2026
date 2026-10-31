use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::collections::BinaryHeap;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

#[derive(Copy, Clone, Eq, PartialEq)]
struct State {
    cost: u64,
    node: u32,
}

impl Ord for State {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.cost.cmp(&self.cost)
    }
}

impl PartialOrd for State {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
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
        } else { break; }
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
        f_head[i] = cur_f;
        b_head[i] = cur_b;
        f_write[i].store(cur_f, Relaxed);
        b_write[i].store(cur_b, Relaxed);
        cur_f += f_deg[i].load(Relaxed);
        cur_b += b_deg[i].load(Relaxed);
    }
    f_head[n] = cur_f;
    b_head[n] = cur_b;
    drop(f_deg);
    drop(b_deg);

    let mut f_edges = vec![0u64; m];
    let mut b_edges = vec![0u64; m];
    let f_edges_addr = f_edges.as_mut_ptr() as usize;
    let b_edges_addr = b_edges.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for &(start, end) in &edge_chunks {
            let f_write_ref = &f_write;
            let b_write_ref = &b_write;
            s.spawn(move || {
                let g_ptr = g_addr as *const u8;
                let f_raw = f_edges_addr as *mut u64;
                let b_raw = b_edges_addr as *mut u64;
                let mut pos = start;
                while pos < end {
                    let u = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let v = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let w = unsafe { parse_u32_fast(g_ptr, &mut pos) } as u64;
                    if pos < end && unsafe { *g_ptr.add(pos) } == b'\r' { pos += 1; }
                    if pos < end && unsafe { *g_ptr.add(pos) } == b'\n' { pos += 1; }
                    let packed_f = (v as u64) | (w << 32);
                    let packed_b = (u as u64) | (w << 32);
                    if u < n {
                        let idx = f_write_ref[u].fetch_add(1, Relaxed);
                        unsafe { *f_raw.add(idx as usize) = packed_f; }
                    }
                    if v < n {
                        let idx = b_write_ref[v].fetch_add(1, Relaxed);
                        unsafe { *b_raw.add(idx as usize) = packed_b; }
                    }
                }
            });
        }
    });

    drop(f_write);
    drop(b_write);
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

    // Use 3 query threads for optimal balance of speed and memory
    let q_threads = 3;

    std::thread::scope(|s| {
        let f_head_ref = &f_head;
        let b_head_ref = &b_head;
        let f_edges_ref = &f_edges;
        let b_edges_ref = &b_edges;
        let query_cursor_ref = &query_cursor;
        let queries_ref = &queries;

        for _ in 0..q_threads {
            s.spawn(move || {
                let mut qf: BinaryHeap<State> = BinaryHeap::with_capacity(2048);
                let mut qb: BinaryHeap<State> = BinaryHeap::with_capacity(2048);
                let mut df: Vec<u64> = vec![u64::MAX; n];
                let mut db: Vec<u64> = vec![u64::MAX; n];
                let mut visited_f: Vec<u32> = Vec::with_capacity(2048);
                let mut visited_b: Vec<u32> = Vec::with_capacity(2048);
                let res_raw = results_ptr as *mut i64;

                const BATCH_SIZE: usize = 16;
                loop {
                    let start_idx = query_cursor_ref.fetch_add(BATCH_SIZE, Relaxed);
                    if start_idx >= num_queries { break; }
                    let end_idx = (start_idx + BATCH_SIZE).min(num_queries);

                    for q_idx in start_idx..end_idx {
                        let (src, dst) = unsafe { *queries_ref.get_unchecked(q_idx) };
                        if src == dst { unsafe { *res_raw.add(q_idx) = 0; } continue; }
                        let s_idx = src as usize;
                        let t_idx = dst as usize;
                        if s_idx >= n || t_idx >= n { unsafe { *res_raw.add(q_idx) = -1; } continue; }

                        for &node in &visited_f { unsafe { *df.get_unchecked_mut(node as usize) = u64::MAX; } }
                        for &node in &visited_b { unsafe { *db.get_unchecked_mut(node as usize) = u64::MAX; } }
                        visited_f.clear();
                        visited_b.clear();
                        qf.clear();
                        qb.clear();

                        unsafe { *df.get_unchecked_mut(s_idx) = 0; }
                        visited_f.push(src);
                        qf.push(State { cost: 0, node: src });

                        unsafe { *db.get_unchecked_mut(t_idx) = 0; }
                        visited_b.push(dst);
                        qb.push(State { cost: 0, node: dst });

                        let mut best = u64::MAX;

                        while let (Some(&top_f), Some(&top_b)) = (qf.peek(), qb.peek()) {
                            if top_f.cost.saturating_add(top_b.cost) >= best { break; }

                            if top_f.cost <= top_b.cost {
                                let State { cost: curr_cost, node: curr_node } = qf.pop().unwrap();
                                if curr_cost > unsafe { *df.get_unchecked(curr_node as usize) } { continue; }
                                let u = curr_node as usize;
                                let e_start = unsafe { *f_head_ref.get_unchecked(u) } as usize;
                                let e_end = unsafe { *f_head_ref.get_unchecked(u + 1) } as usize;
                                for edge_idx in e_start..e_end {
                                    let packed = unsafe { *f_edges_ref.get_unchecked(edge_idx) };
                                    let v = (packed & 0xFFFF_FFFF) as u32;
                                    let w = packed >> 32;
                                    let new_dist = curr_cost + w;
                                    let v_idx = v as usize;
                                    let old_dist = unsafe { *df.get_unchecked(v_idx) };
                                    if new_dist < old_dist {
                                        if old_dist == u64::MAX { visited_f.push(v); }
                                        unsafe { *df.get_unchecked_mut(v_idx) = new_dist; }
                                        qf.push(State { cost: new_dist, node: v });

                                        let db_val = unsafe { *db.get_unchecked(v_idx) };
                                        if db_val != u64::MAX {
                                            let total_cost = new_dist.saturating_add(db_val);
                                            if total_cost < best { best = total_cost; }
                                        }
                                    }
                                }
                            } else {
                                let State { cost: curr_cost, node: curr_node } = qb.pop().unwrap();
                                if curr_cost > unsafe { *db.get_unchecked(curr_node as usize) } { continue; }
                                let u = curr_node as usize;
                                let e_start = unsafe { *b_head_ref.get_unchecked(u) } as usize;
                                let e_end = unsafe { *b_head_ref.get_unchecked(u + 1) } as usize;
                                for edge_idx in e_start..e_end {
                                    let packed = unsafe { *b_edges_ref.get_unchecked(edge_idx) };
                                    let v = (packed & 0xFFFF_FFFF) as u32;
                                    let w = packed >> 32;
                                    let new_dist = curr_cost + w;
                                    let v_idx = v as usize;
                                    let old_dist = unsafe { *db.get_unchecked(v_idx) };
                                    if new_dist < old_dist {
                                        if old_dist == u64::MAX { visited_b.push(v); }
                                        unsafe { *db.get_unchecked_mut(v_idx) = new_dist; }
                                        qb.push(State { cost: new_dist, node: v });

                                        let df_val = unsafe { *df.get_unchecked(v_idx) };
                                        if df_val != u64::MAX {
                                            let total_cost = new_dist.saturating_add(df_val);
                                            if total_cost < best { best = total_cost; }
                                        }
                                    }
                                }
                            }
                        }

                        unsafe { *res_raw.add(q_idx) = if best == u64::MAX { -1 } else { best as i64 }; }
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
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
        if out_buf.len() >= 32 * 1024 {
            let _ = out_file.write_all(&out_buf);
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        let _ = out_file.write_all(&out_buf);
    }
}
