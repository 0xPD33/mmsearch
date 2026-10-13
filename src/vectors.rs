//! The in-RAM vector matrix and the f16 blob format of the store.

use half::f16;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::OnceLock;

/// Matryoshka cut: the RAM matrix keeps the first DIM of the 768 values, renormalized.
pub const DIM: usize = 512;

/// Rows above which the scan splits over threads.
const PARALLEL_ROWS: usize = 20_000;
const THREADS: usize = 4;

/// One contiguous row-major f16 matrix, with the chunk and file id of each row.
pub struct VectorIndex {
    data: Vec<f16>,
    chunk_ids: Vec<i64>,
    file_ids: Vec<i64>,
    rows_of: HashMap<i64, Vec<usize>>,
}

/// A row score that orders as a min-heap entry: the worst score on top.
struct Scored(f32, usize);

impl PartialEq for Scored {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Scored {}
impl PartialOrd for Scored {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Scored {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0)
    }
}

/// All 65536 f16 values as f32; a table lookup beats software conversion.
fn lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| (0..=u16::MAX).map(|b| f16::from_bits(b).to_f32()).collect())
}

fn cut(full: &[f32]) -> Vec<f32> {
    let mut v = full[..DIM].to_vec();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

fn dot(row: &[f16], q: &[f32], lut: &[f32]) -> f32 {
    let mut acc = [0.0f32; 4];
    for (r, q) in row.chunks_exact(4).zip(q.chunks_exact(4)) {
        for i in 0..4 {
            acc[i] += lut[r[i].to_bits() as usize] * q[i];
        }
    }
    acc.iter().sum()
}

impl VectorIndex {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            chunk_ids: Vec::new(),
            file_ids: Vec::new(),
            rows_of: HashMap::new(),
        }
    }

    /// `full` is a unit vector of any length >= DIM. The index cuts and renormalizes it.
    pub fn insert(&mut self, chunk_id: i64, file_id: i64, full: &[f32]) {
        let row = self.chunk_ids.len();
        self.data.extend(cut(full).into_iter().map(f16::from_f32));
        self.chunk_ids.push(chunk_id);
        self.file_ids.push(file_id);
        self.rows_of.entry(file_id).or_default().push(row);
    }

    pub fn remove_file(&mut self, file_id: i64) {
        let Some(mut rows) = self.rows_of.remove(&file_id) else {
            return;
        };
        // Descending order: the last row never belongs to this file when it moves.
        rows.sort_unstable_by(|a, b| b.cmp(a));
        for row in rows {
            let last = self.chunk_ids.len() - 1;
            if row != last {
                self.data
                    .copy_within(last * DIM..(last + 1) * DIM, row * DIM);
                let moved = self.file_ids[last];
                if let Some(r) = self
                    .rows_of
                    .get_mut(&moved)
                    .and_then(|v| v.iter_mut().find(|r| **r == last))
                {
                    *r = row;
                }
            }
            self.data.truncate(last * DIM);
            self.chunk_ids.swap_remove(row);
            self.file_ids.swap_remove(row);
        }
    }

    /// The top `k` rows by dot product, best first: (chunk_id, file_id, score).
    pub fn search(&self, query_full: &[f32], k: usize) -> Vec<(i64, i64, f32)> {
        let rows = self.chunk_ids.len();
        if rows == 0 || k == 0 {
            return Vec::new();
        }
        let q = cut(query_full);
        let lut = lut();
        let scan = |start: usize, data: &[f16]| {
            let mut heap = BinaryHeap::with_capacity(k + 1);
            for (i, row) in data.chunks_exact(DIM).enumerate() {
                let s = dot(row, &q, lut);
                if heap.len() < k {
                    heap.push(Scored(s, start + i));
                } else if s > heap.peek().map_or(f32::MIN, |w: &Scored| w.0) {
                    heap.pop();
                    heap.push(Scored(s, start + i));
                }
            }
            heap.into_vec()
        };
        let mut top: Vec<Scored> = if rows > PARALLEL_ROWS {
            let per = rows.div_ceil(THREADS);
            std::thread::scope(|sc| {
                let parts: Vec<_> = self
                    .data
                    .chunks(per * DIM)
                    .enumerate()
                    .map(|(t, d)| {
                        let scan = &scan;
                        sc.spawn(move || scan(t * per, d))
                    })
                    .collect();
                parts.into_iter().flat_map(|h| h.join().unwrap()).collect()
            })
        } else {
            scan(0, &self.data)
        };
        top.sort_by(|a, b| b.0.total_cmp(&a.0));
        top.truncate(k);
        top.into_iter()
            .map(|Scored(s, r)| (self.chunk_ids[r], self.file_ids[r], s))
            .collect()
    }
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// f16 little-endian bytes for the `chunks.vec` column.
pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter()
        .flat_map(|x| f16::from_f32(*x).to_le_bytes())
        .collect()
}

pub fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(2)
        .map(|c| f16::from_le_bytes([c[0], c[1]]).to_f32())
        .collect()
}
