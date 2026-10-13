//! The three matchers and the RRF merge.

use crate::paths::PathList;
use crate::store::Store;
use crate::tiers::kind_of;
use crate::types::{Filter, Hit, Kind};
use crate::vectors::VectorIndex;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// The RRF constant: score = sum of 1 / (K + rank).
pub const RRF_K: f32 = 60.0;
/// Each matcher returns this many results before the merge.
pub const PER_LIST: usize = 200;

const SNIPPET_CHARS: usize = 160;
const PREVIEW_CHARS: usize = 2000;

pub struct Searcher {
    store: Mutex<Store>,
    vectors: Arc<RwLock<VectorIndex>>,
    paths: Arc<PathList>,
}

/// What the merge knows about one file.
struct Cand {
    kind: Kind,
    file_id: Option<i64>,
    score: f32,
    /// The best keyword chunk, else the best vector chunk.
    chunk: Option<i64>,
    keyword_chunk: bool,
}

impl Searcher {
    /// Opens its own read connection to the database at `db`.
    pub fn new(
        db: &Path,
        vectors: Arc<RwLock<VectorIndex>>,
        paths: Arc<PathList>,
    ) -> Result<Searcher> {
        Ok(Searcher {
            store: Mutex::new(Store::open_existing(db, crate::embed::MODEL_ID)?),
            vectors,
            paths,
        })
    }

    /// Path and keyword matchers only; it never waits for the model.
    pub fn fast(&self, text: &str, filter: Filter, limit: usize) -> Result<Vec<Hit>> {
        self.run(text, None, filter, limit)
    }

    /// All three matchers. `vector` is the query embedding (768d unit vector), if there is one.
    /// `text` can be empty for an image or voice query: then only the vector list counts.
    pub fn full(
        &self,
        text: &str,
        vector: Option<&[f32]>,
        filter: Filter,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        self.run(text, vector, filter, limit)
    }

    fn run(
        &self,
        text: &str,
        vector: Option<&[f32]>,
        filter: Filter,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let store = self.store.lock().unwrap();
        let mut cands: HashMap<PathBuf, Cand> = HashMap::new();

        // Each list is ranked after the filter, and each file counts once per list.
        let mut add = |path: PathBuf,
                       kind: Kind,
                       file_id: Option<i64>,
                       chunk: Option<i64>,
                       keyword: bool,
                       rank: &mut usize,
                       seen: &mut Vec<PathBuf>| {
            if !filter.accepts(kind) || seen.contains(&path) {
                return;
            }
            seen.push(path.clone());
            *rank += 1;
            let c = cands.entry(path).or_insert(Cand {
                kind,
                file_id,
                score: 0.0,
                chunk: None,
                keyword_chunk: false,
            });
            c.score += 1.0 / (RRF_K + *rank as f32);
            if chunk.is_some() && !c.keyword_chunk {
                c.chunk = chunk;
                c.keyword_chunk = keyword;
            }
        };

        let (mut rank, mut seen) = (0, Vec::new());
        for path in self.paths.search(text, PER_LIST) {
            let row = store.file_by_path(&path)?;
            let kind = row.as_ref().map_or_else(|| kind_of(&path), |r| r.kind);
            add(
                path,
                kind,
                row.map(|r| r.id),
                None,
                false,
                &mut rank,
                &mut seen,
            );
        }

        let (mut rank, mut seen) = (0, Vec::new());
        for (file_id, chunk) in store.fts(text, PER_LIST)? {
            if let Some(f) = store.file(file_id)? {
                add(
                    f.path,
                    f.kind,
                    Some(f.id),
                    Some(chunk),
                    true,
                    &mut rank,
                    &mut seen,
                );
            }
        }

        if let Some(v) = vector {
            let (mut rank, mut seen) = (0, Vec::new());
            for (chunk, file_id, _) in self.vectors.read().unwrap().search(v, PER_LIST) {
                if let Some(f) = store.file(file_id)? {
                    add(
                        f.path,
                        f.kind,
                        Some(f.id),
                        Some(chunk),
                        false,
                        &mut rank,
                        &mut seen,
                    );
                }
            }
        }

        let mut ranked: Vec<_> = cands.into_iter().collect();
        ranked.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then_with(|| a.0.cmp(&b.0)));

        let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
        let mut hits = Vec::new();
        for (path, c) in ranked {
            if hits.len() >= limit {
                break;
            }
            if std::fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let body = match (c.chunk, c.file_id) {
                (Some(chunk), _) => store.chunk_text(chunk)?,
                _ => None,
            };
            let body = match (body, c.file_id) {
                (None, Some(id)) => store.first_text(id)?,
                (b, _) => b,
            };
            hits.push(Hit {
                snippet: body.as_deref().and_then(|b| snippet(b, &words)),
                preview: body
                    .as_deref()
                    .map(|b| b.chars().take(PREVIEW_CHARS).collect()),
                path,
                kind: c.kind,
                score: c.score,
            });
        }
        Ok(hits)
    }
}

/// The first line with a query word, else the first non-empty line.
fn snippet(body: &str, words: &[String]) -> Option<String> {
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.clone().next()?;
    let line = lines.find(|l| {
        let l = l.to_lowercase();
        words.iter().any(|w| l.contains(w.as_str()))
    });
    Some(line.unwrap_or(first).chars().take(SNIPPET_CHARS).collect())
}
