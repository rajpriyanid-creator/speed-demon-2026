use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[derive(Copy, Clone)]
struct State {
    cost: u64,
    node: u32,
}

struct MinHeap {
    data: Vec<State>,
}

impl MinHeap {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::with_capacity(cap),
        }
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[inline(always)]
    fn clear(&mut self) {
        self.data.clear();
    }

    #[inline(always)]
    fn peek(&self) -> Option<State> {
        self.data.first().copied()
    }

    #[inline(always)]
    fn push(&mut self, s: State) {
        let mut i = self.data.len();
        self.data.push(s);
        while i > 0 {
            let p = (i - 1) >> 1;
            if unsafe { self.data.get_unchecked(i).cost < self.data.get_unchecked(p).cost } {
                self.data.swap(i, p);
                i = p;
            } else {
                break;
            }
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<State> {
        if self.data.is_empty() {
            return None;
        }
        let res = self.data[0];
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            self.data[0] = last;
            let mut i = 0;
            let len = self.data.len();
            loop {
                let mut smallest = i;
                let left = 2 * i + 1;
                let right = 2 * i + 2;
                if left < len && unsafe { self.data.get_unchecked(left).cost < self.data.get_unchecked(smallest).cost } {
                    smallest = left;
                }
                if right < len && unsafe { self.data.get_unchecked(right).cost < self.data.get_unchecked(smallest).cost } {
                    smallest = right;
                }
                if smallest != i {
                    self.data.swap(i, smallest);
                    i = smallest;
                } else {
                    break;
                }
            }
        }
        Some(res)
    }
}

#[inline(always)]
fn parse_u32(data: &[u8], pos: &mut usize) -> u32 {
    let mut val = 0u32;
    while *pos < data.len() {
        let b = unsafe { *data.get_unchecked(*pos) };
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

    // 1. Mmap graph.txt
    let graph_path = format!("{}/graph.txt", data_dir);
    let graph_file = match File::open(&graph_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let meta = match graph_file.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };
    let g_len = meta.len() as usize;
    if g_len == 0 {
        return;
    }

    let g_ptr = unsafe {
        mmap(std::ptr::null_mut(), g_len, PROT_READ, MAP_SHARED, graph_file.as_raw_fd(), 0)
    };
    if g_ptr == usize::MAX as *mut u8 {
        return;
    }
    let g_data = unsafe { std::slice::from_raw_parts(g_ptr, g_len) };

    let mut g_pos = 0;
    let n = parse_u32(g_data, &mut g_pos) as usize;
    while g_pos < g_len && g_data[g_pos] == b' ' { g_pos += 1; }
    let m = parse_u32(g_data, &mut g_pos) as usize;
    while g_pos < g_len && g_data[g_pos] != b'\n' { g_pos += 1; }
    if g_pos < g_len { g_pos += 1; }
    let edges_start = g_pos;

    let num_load_threads = 4;
    let mut edge_chunks = Vec::with_capacity(num_load_threads);
    {
        let total_edge_bytes = g_len - edges_start;
        let mut cur = edges_start;
        for t in 0..num_load_threads {
            let target = edges_start + ((total_edge_bytes * (t + 1)) / num_load_threads);
            let mut end = target.min(g_len);
            while end < g_len && g_data[end] != b'\n' {
                end += 1;
            }
            if end < g_len {
                end += 1;
            }
            edge_chunks.push((cur, end));
            cur = end;
        }
    }

    // Pass 1: count forward and reverse degrees in parallel
    let deg_results: Vec<(Vec<u32>, Vec<u32>)> = std::thread::scope(|s| {
        let handles: Vec<_> = edge_chunks.iter().map(|&(start, end)| {
            s.spawn(move || {
                let mut local_f_deg = vec![0u32; n];
                let mut local_r_deg = vec![0u32; n];
                let mut pos = start;
                while pos < end {
                    let u = parse_u32(g_data, &mut pos) as usize;
                    if pos < end { pos += 1; }
                    let v = parse_u32(g_data, &mut pos) as usize;
                    if pos < end { pos += 1; }
                    while pos < end && g_data[pos] != b'\n' { pos += 1; }
                    if pos < end { pos += 1; }

                    if u < n { local_f_deg[u] += 1; }
                    if v < n { local_r_deg[v] += 1; }
                }
                (local_f_deg, local_r_deg)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut f_head = vec![0u32; n + 1];
    let mut r_head = vec![0u32; n + 1];
    let mut thread_f_write: Vec<Vec<u32>> = (0..num_load_threads).map(|_| vec![0u32; n]).collect();
    let mut thread_r_write: Vec<Vec<u32>> = (0..num_load_threads).map(|_| vec![0u32; n]).collect();

    let mut cur_f = 0u32;
    let mut cur_r = 0u32;
    for i in 0..n {
        f_head[i] = cur_f;
        r_head[i] = cur_r;
        let mut f_sum = cur_f;
        let mut r_sum = cur_r;
        for t in 0..num_load_threads {
            thread_f_write[t][i] = f_sum;
            thread_r_write[t][i] = r_sum;
            f_sum += deg_results[t].0[i];
            r_sum += deg_results[t].1[i];
        }
        cur_f = f_sum;
        cur_r = r_sum;
    }
    f_head[n] = cur_f;
    r_head[n] = cur_r;
    drop(deg_results);

    // Packed edges: lower 32 bits = dest node, upper 32 bits = weight
    let mut f_edges = vec![0u64; m];
    let mut r_edges = vec![0u64; m];
    let f_edges_addr = f_edges.as_mut_ptr() as usize;
    let r_edges_addr = r_edges.as_mut_ptr() as usize;

    // Pass 2: populate forward and reverse CSR in parallel
    std::thread::scope(|s| {
        for (t, &(start, end)) in edge_chunks.iter().enumerate() {
            let mut f_write = thread_f_write[t].clone();
            let mut r_write = thread_r_write[t].clone();

            s.spawn(move || {
                let f_raw = f_edges_addr as *mut u64;
                let r_raw = r_edges_addr as *mut u64;
                let mut pos = start;
                while pos < end {
                    let u = parse_u32(g_data, &mut pos) as usize;
                    if pos < end { pos += 1; }
                    let v = parse_u32(g_data, &mut pos) as usize;
                    if pos < end { pos += 1; }
                    let w = parse_u32(g_data, &mut pos) as u64;
                    if pos < end && g_data[pos] == b'\r' { pos += 1; }
                    if pos < end && g_data[pos] == b'\n' { pos += 1; }

                    let packed_f = (v as u64) | (w << 32);
                    let packed_r = (u as u64) | (w << 32);

                    if u < n {
                        let slot = f_write[u] as usize;
                        f_write[u] += 1;
                        unsafe { *f_raw.add(slot) = packed_f; }
                    }
                    if v < n {
                        let slot = r_write[v] as usize;
                        r_write[v] += 1;
                        unsafe { *r_raw.add(slot) = packed_r; }
                    }
                }
            });
        }
    });
    drop(thread_f_write);
    drop(thread_r_write);
    unsafe { munmap(g_ptr, g_len); }

    // 2. Mmap queries.txt
    let q_path = format!("{}/queries.txt", data_dir);
    let q_file = match File::open(&q_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let q_meta = match q_file.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };
    let q_len = q_meta.len() as usize;
    if q_len == 0 {
        return;
    }

    let q_ptr = unsafe {
        mmap(std::ptr::null_mut(), q_len, PROT_READ, MAP_SHARED, q_file.as_raw_fd(), 0)
    };
    if q_ptr == usize::MAX as *mut u8 {
        return;
    }
    let q_data = unsafe { std::slice::from_raw_parts(q_ptr, q_len) };

    let mut queries: Vec<(u32, u32)> = Vec::with_capacity(10_000);
    let mut q_pos = 0;
    while q_pos < q_len {
        if q_data[q_pos] == b'\r' || q_data[q_pos] == b'\n' {
            q_pos += 1;
            continue;
        }
        let s = parse_u32(q_data, &mut q_pos);
        if q_pos < q_len && q_data[q_pos] == b' ' { q_pos += 1; }
        let t = parse_u32(q_data, &mut q_pos);
        while q_pos < q_len && q_data[q_pos] != b'\n' { q_pos += 1; }
        if q_pos < q_len { q_pos += 1; }
        queries.push((s, t));
    }
    unsafe { munmap(q_ptr, q_len); }

    let num_queries = queries.len();
    let num_threads = 6;
    let chunk_size = (num_queries + num_threads - 1) / num_threads;

    let mut results: Vec<i64> = vec![-1; num_queries];

    std::thread::scope(|s| {
        let f_head_ref = &f_head;
        let r_head_ref = &r_head;
        let f_edges_ref = &f_edges;
        let r_edges_ref = &r_edges;

        let mut chunks: Vec<&mut [i64]> = Vec::new();
        let mut rest = results.as_mut_slice();
        for _ in 0..num_threads {
            let take = chunk_size.min(rest.len());
            let (head_slice, tail_slice) = rest.split_at_mut(take);
            chunks.push(head_slice);
            rest = tail_slice;
        }

        for (thread_idx, res_chunk) in chunks.into_iter().enumerate() {
            let start_q = thread_idx * chunk_size;
            let query_slice = &queries[start_q..start_q + res_chunk.len()];

            s.spawn(move || {
                let mut q_f = MinHeap::new(4096);
                let mut q_r = MinHeap::new(4096);

                let mut dist_f: Vec<u64> = vec![u64::MAX; n];
                let mut dist_r: Vec<u64> = vec![u64::MAX; n];
                let mut visited_f: Vec<u32> = Vec::with_capacity(4096);
                let mut visited_r: Vec<u32> = Vec::with_capacity(4096);

                for (i, &(src, dst)) in query_slice.iter().enumerate() {
                    if src == dst {
                        res_chunk[i] = 0;
                        continue;
                    }
                    let s_idx = src as usize;
                    let t_idx = dst as usize;
                    if s_idx >= n || t_idx >= n {
                        res_chunk[i] = -1;
                        continue;
                    }

                    // Reset visited nodes from previous query
                    for &node in &visited_f {
                        dist_f[node as usize] = u64::MAX;
                    }
                    visited_f.clear();

                    for &node in &visited_r {
                        dist_r[node as usize] = u64::MAX;
                    }
                    visited_r.clear();

                    q_f.clear();
                    q_r.clear();

                    dist_f[s_idx] = 0;
                    visited_f.push(src);
                    q_f.push(State { cost: 0, node: src });

                    dist_r[t_idx] = 0;
                    visited_r.push(dst);
                    q_r.push(State { cost: 0, node: dst });

                    let mut mu = u64::MAX;

                    while !q_f.is_empty() && !q_r.is_empty() {
                        let top_f = q_f.peek().unwrap().cost;
                        let top_r = q_r.peek().unwrap().cost;

                        if top_f.saturating_add(top_r) >= mu {
                            break;
                        }

                        if top_f <= top_r {
                            let curr = q_f.pop().unwrap();
                            if curr.cost > dist_f[curr.node as usize] {
                                continue;
                            }
                            let u = curr.node as usize;
                            let e_start = f_head_ref[u] as usize;
                            let e_end = f_head_ref[u + 1] as usize;

                            for edge_idx in e_start..e_end {
                                let packed = unsafe { *f_edges_ref.get_unchecked(edge_idx) };
                                let v = (packed & 0xFFFF_FFFF) as u32;
                                let w = packed >> 32;
                                let new_dist = curr.cost + w;

                                let v_idx = v as usize;
                                if new_dist < dist_f[v_idx] {
                                    if dist_f[v_idx] == u64::MAX {
                                        visited_f.push(v);
                                    }
                                    dist_f[v_idx] = new_dist;
                                    q_f.push(State { cost: new_dist, node: v });

                                    let r_dist = dist_r[v_idx];
                                    if r_dist != u64::MAX {
                                        let total = new_dist.saturating_add(r_dist);
                                        if total < mu {
                                            mu = total;
                                        }
                                    }
                                }
                            }
                        } else {
                            let curr = q_r.pop().unwrap();
                            if curr.cost > dist_r[curr.node as usize] {
                                continue;
                            }
                            let v = curr.node as usize;
                            let e_start = r_head_ref[v] as usize;
                            let e_end = r_head_ref[v + 1] as usize;

                            for edge_idx in e_start..e_end {
                                let packed = unsafe { *r_edges_ref.get_unchecked(edge_idx) };
                                let u = (packed & 0xFFFF_FFFF) as u32;
                                let w = packed >> 32;
                                let new_dist = curr.cost + w;

                                let u_idx = u as usize;
                                if new_dist < dist_r[u_idx] {
                                    if dist_r[u_idx] == u64::MAX {
                                        visited_r.push(u);
                                    }
                                    dist_r[u_idx] = new_dist;
                                    q_r.push(State { cost: new_dist, node: u });

                                    let f_dist = dist_f[u_idx];
                                    if f_dist != u64::MAX {
                                        let total = new_dist.saturating_add(f_dist);
                                        if total < mu {
                                            mu = total;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    res_chunk[i] = if mu == u64::MAX { -1 } else { mu as i64 };
                }
            });
        }
    });

    // 3. Write output
    fs::create_dir_all(&out_dir).unwrap();
    let out_file_path = format!("{}/distances.txt", out_dir);
    let mut out_file = File::create(&out_file_path).unwrap();

    let mut out_buf = Vec::with_capacity(32 * 1024);
    for d in results {
        let _ = writeln!(out_buf, "{}", d);
        if out_buf.len() > 24 * 1024 {
            out_file.write_all(&out_buf).unwrap();
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        out_file.write_all(&out_buf).unwrap();
    }
}
