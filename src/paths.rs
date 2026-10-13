//! The path tier: every path under the root in RAM, fuzzy-matched with nucleo.

use crate::config::home;
use crate::tiers::path_walker;
use ignore::WalkState;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Injector, Nucleo, Utf32String};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_AGE: Duration = Duration::from_secs(60);
/// Paths a walker thread buffers before it hands them over.
const BATCH: usize = 4096;
/// The longest a search waits for nucleo, so a streaming walk cannot stall it.
const MAX_WAIT: Duration = Duration::from_secs(2);

struct Live {
    nucleo: Nucleo<PathBuf>,
    injector: Injector<PathBuf>,
    /// The last pattern, so a query that only grew can reuse the previous matches.
    last: String,
}

pub struct PathList {
    live: Mutex<Live>,
    /// When the last walk finished; `None` until the first one does.
    walked: Mutex<Option<Instant>>,
    walking: AtomicBool,
    home: PathBuf,
}

fn new_live() -> Live {
    let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1);
    let injector = nucleo.injector();
    Live {
        nucleo,
        injector,
        last: String::new(),
    }
}

/// Matching uses the path relative to `$HOME`, so the home prefix does not score.
fn push(injector: &Injector<PathBuf>, home: &Path, path: PathBuf) {
    injector.push(path, |p, cols| {
        cols[0] = Utf32String::from(p.strip_prefix(home).unwrap_or(p).to_string_lossy().as_ref());
    });
}

/// A walker thread's buffer; it flushes on drop, when the thread ends.
struct Batch<F: Fn(&mut Vec<PathBuf>)> {
    paths: Vec<PathBuf>,
    flush: F,
}

impl<F: Fn(&mut Vec<PathBuf>)> Batch<F> {
    fn add(&mut self, p: PathBuf) {
        self.paths.push(p);
        if self.paths.len() >= BATCH {
            (self.flush)(&mut self.paths);
        }
    }
}

impl<F: Fn(&mut Vec<PathBuf>)> Drop for Batch<F> {
    fn drop(&mut self) {
        (self.flush)(&mut self.paths);
    }
}

/// Walks `root` in parallel and hands the file paths to `flush` in batches.
fn walk(root: &Path, flush: impl Fn(&mut Vec<PathBuf>) + Send + Sync + Clone) {
    path_walker(root).build_parallel().run(|| {
        let mut batch = Batch {
            paths: Vec::new(),
            flush: flush.clone(),
        };
        Box::new(move |entry| {
            if let Ok(e) = entry {
                if e.file_type().is_some_and(|t| !t.is_dir()) {
                    batch.add(e.into_path());
                }
            }
            WalkState::Continue
        })
    });
}

impl PathList {
    pub fn new() -> Arc<PathList> {
        Arc::new(PathList {
            live: Mutex::new(new_live()),
            walked: Mutex::new(None),
            walking: AtomicBool::new(false),
            home: home(),
        })
    }

    /// Walks `root` with `tiers::path_walker` in a background thread, if the list is empty
    /// or older than 60 s. The old list stays searchable, and new paths stream in.
    pub fn refresh(self: &Arc<Self>, root: &Path) {
        let stale = self
            .walked
            .lock()
            .unwrap()
            .is_none_or(|t| t.elapsed() > MAX_AGE);
        if !stale || self.walking.swap(true, Ordering::AcqRel) {
            return;
        }
        let (this, root) = (self.clone(), root.to_path_buf());
        std::thread::spawn(move || {
            this.walk_into_list(&root);
            *this.walked.lock().unwrap() = Some(Instant::now());
            this.walking.store(false, Ordering::Release);
        });
    }

    fn walk_into_list(&self, root: &Path) {
        if self.walked.lock().unwrap().is_none() {
            // The first walk streams into the live list.
            let injector = self.live.lock().unwrap().injector.clone();
            let home = self.home.clone();
            walk(root, move |b| {
                b.drain(..).for_each(|p| push(&injector, &home, p))
            });
        } else {
            let all = Arc::new(Mutex::new(Vec::new()));
            let sink = all.clone();
            walk(root, move |b| sink.lock().unwrap().append(b));
            self.set(std::mem::take(&mut *all.lock().unwrap()));
        }
    }

    /// Replaces the list at once, for the CLI query over a database.
    pub fn set(&self, paths: Vec<PathBuf>) {
        let fresh = new_live();
        for p in paths {
            push(&fresh.injector, &self.home, p);
        }
        *self.live.lock().unwrap() = fresh;
    }

    /// Best matches first, like fzf with `--scheme path`. Empty query: empty result.
    pub fn search(&self, query: &str, limit: usize) -> Vec<PathBuf> {
        if query.trim().is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut live = self.live.lock().unwrap();
        let append = !live.last.is_empty()
            && query.starts_with(live.last.as_str())
            && !live.last.ends_with('\\');
        live.nucleo
            .pattern
            .reparse(0, query, CaseMatching::Smart, Normalization::Smart, append);
        live.last = query.to_string();
        let start = Instant::now();
        while live.nucleo.tick(10).running && start.elapsed() < MAX_WAIT {}
        let snap = live.nucleo.snapshot();
        let n = (snap.matched_item_count() as usize).min(limit) as u32;
        snap.matched_items(..n).map(|i| i.data.clone()).collect()
    }
}
