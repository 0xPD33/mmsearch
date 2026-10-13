//! The indexer: reconcile, inotify watch, live and backlog queues, gate, store writes.

use crate::embed::MODEL_ID;
use crate::extract::{self, Piece};
use crate::load;
use crate::store::{NewChunk, Store};
use crate::tiers;
use crate::types::Kind;
use crate::vectors::VectorIndex;
use crate::worker::{EmbedService, Priority};
use anyhow::{bail, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::Metadata;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

const DEBOUNCE: Duration = Duration::from_secs(2);
const MODEL_RETRY: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);
/// Live files embedded per loop pass, so events and the gate are checked in between.
const LIVE_PER_PASS: usize = 4;
/// More due files than this at once is a bulk copy or checkout: it goes to the gated backlog.
const LIVE_BURST: usize = 64;
/// Text inputs per embedder job, so a query can run between the jobs of a big file.
const TEXTS_PER_JOB: usize = 16;

#[derive(Default)]
pub struct Progress {
    /// Content-tier files with an up-to-date row.
    pub indexed: AtomicUsize,
    /// Content-tier files in total.
    pub total: AtomicUsize,
    /// "indexing", "paused (<reason>)", "idle" or "waiting for model".
    pub phase: Mutex<String>,
}

pub struct IndexerHandle {
    pub progress: Arc<Progress>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Indexed(usize),
    Unchanged,
    Failed,
    ModelDown,
}

/// The per-file pipeline and the state it keeps: store rows, RAM vectors, backlog.
struct Indexer {
    root: PathBuf,
    store: Store,
    embed: EmbedService,
    vectors: Arc<RwLock<VectorIndex>>,
    progress: Arc<Progress>,
    /// (mtime, size) of every content-tier row.
    states: HashMap<PathBuf, (i64, i64)>,
    backlog: VecDeque<PathBuf>,
    /// The paths still in `backlog`; a popped path that is not here was handled already.
    queued: HashSet<PathBuf>,
    total: usize,
    /// Holds the path being indexed. If it still exists at startup, that file crashed the
    /// previous run, and the indexer skips it until it changes.
    inflight: PathBuf,
}

/// The daemon indexer: startup reconcile, inotify watch, live queue, and the gated backlog.
pub fn spawn(
    root: PathBuf,
    db: PathBuf,
    embed: EmbedService,
    vectors: Arc<RwLock<VectorIndex>>,
) -> Result<IndexerHandle> {
    let progress = Arc::new(Progress::default());
    let (ready_tx, ready_rx) = mpsc::channel();
    let p = progress.clone();
    std::thread::Builder::new()
        .name("indexer".into())
        .spawn(move || {
            let store = match Store::open(&db, MODEL_ID) {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let ix = Indexer::new(root, &db, store, embed, vectors, p);
            if let Err(e) = daemon_loop(ix) {
                tracing::error!("indexer stopped: {e:#}");
            }
        })?;
    ready_rx.recv()??;
    Ok(IndexerHandle { progress })
}

/// One pass over the content tier of `root`, with no gate and no watch. Prints progress to stderr.
pub fn run_once(root: &Path, db: &Path, embed: &EmbedService) -> Result<()> {
    let start = Instant::now();
    let store = Store::open(db, MODEL_ID)?;
    let vectors = Arc::new(RwLock::new(VectorIndex::new()));
    let mut ix = Indexer::new(
        root.to_path_buf(),
        db,
        store,
        embed.clone(),
        vectors,
        Arc::default(),
    );
    ix.reconcile()?;

    let todo = ix.backlog.len();
    eprintln!("{} content files, {todo} to index", ix.total);
    let (mut files, mut chunks, mut errors) = (0, 0, 0);
    let mut last_report = Instant::now();
    while let Some(path) = ix.backlog.pop_front() {
        match ix.process(&path, Priority::Backlog) {
            Ok(Outcome::Indexed(n)) => {
                files += 1;
                chunks += n;
            }
            Ok(Outcome::Failed) => errors += 1,
            Ok(Outcome::ModelDown) => bail!("the model is unavailable: {}", ix.embed.state()),
            Ok(Outcome::Unchanged) => {}
            Err(e) => {
                tracing::warn!("{}: {e:#}", path.display());
                errors += 1;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(3) {
            last_report = Instant::now();
            eprintln!("  {} / {todo} files", files + errors);
        }
    }
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "indexed {files} files, {chunks} chunks, {errors} errors in {secs:.1} s ({:.1} files/s)",
        files as f64 / secs.max(0.001)
    );
    Ok(())
}

impl Indexer {
    fn new(
        root: PathBuf,
        db: &Path,
        store: Store,
        embed: EmbedService,
        vectors: Arc<RwLock<VectorIndex>>,
        progress: Arc<Progress>,
    ) -> Self {
        Indexer {
            root,
            store,
            embed,
            vectors,
            progress,
            states: HashMap::new(),
            backlog: VecDeque::new(),
            queued: HashSet::new(),
            total: 0,
            inflight: db.with_extension("inflight"),
        }
    }

    /// Compares the content tier with the store. Returns the directories of the tier.
    fn reconcile(&mut self) -> Result<Vec<PathBuf>> {
        if let Ok(bytes) = std::fs::read(&self.inflight) {
            let path = PathBuf::from(OsString::from_vec(bytes));
            tracing::error!(
                "{} crashed the previous run; it stays unindexed until it changes",
                path.display()
            );
            if let Ok(md) = std::fs::symlink_metadata(&path) {
                self.store.set_error(
                    &path,
                    tiers::kind_of(&path),
                    signature(&md).0,
                    signature(&md).1,
                    "crashed the indexer",
                )?;
            }
            std::fs::remove_file(&self.inflight)?;
        }
        // Rows outside this root belong to another `index --root` run; they are not ours to delete.
        let mut states = self.store.file_states()?;
        states.retain(|p, _| tiers::in_content_tier(&self.root, p));
        self.states = states;

        let (mut dirs, mut seen) = (Vec::new(), HashSet::new());
        for entry in tiers::content_walker(&self.root).build().flatten() {
            let Some(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                dirs.push(entry.path().to_path_buf());
            } else if ft.is_file()
                && tiers::kind_of(entry.path()) != Kind::Other
                && !tiers::skip_content(entry.path())
            {
                let Ok(md) = entry.metadata() else { continue };
                seen.insert(entry.path().to_path_buf());
                if self.states.get(entry.path()) != Some(&signature(&md)) {
                    self.queued.insert(entry.path().to_path_buf());
                    self.backlog.push_back(entry.path().to_path_buf());
                }
            }
        }
        self.total = seen.len();
        // Media first: photo search is the main use, and text is the bulk of a home.
        self.backlog
            .make_contiguous()
            .sort_by_key(|p| !matches!(tiers::kind_of(p), Kind::Image | Kind::Video | Kind::Audio));

        let gone: Vec<PathBuf> = self
            .states
            .keys()
            .filter(|p| !seen.contains(*p))
            .cloned()
            .collect();
        for p in &gone {
            self.remove(p)?;
        }
        self.sync_progress();
        Ok(dirs)
    }

    fn sync_progress(&self) {
        self.progress.total.store(self.total, Ordering::Relaxed);
        self.progress.indexed.store(
            self.total.saturating_sub(self.queued.len()),
            Ordering::Relaxed,
        );
    }

    /// Drops the rows of every file under the given directories.
    fn remove_trees(&mut self, dirs: &[PathBuf]) -> Result<()> {
        let under: Vec<PathBuf> = self
            .states
            .keys()
            .filter(|p| dirs.iter().any(|d| p.starts_with(d)))
            .cloned()
            .collect();
        for p in under {
            self.remove(&p)?;
            self.queued.remove(&p);
            self.total = self.total.saturating_sub(1);
        }
        self.sync_progress();
        Ok(())
    }

    /// Moves a file to the gated backlog.
    fn enqueue(&mut self, path: PathBuf) {
        if self.queued.insert(path.clone()) {
            self.total += !self.states.contains_key(&path) as usize;
            self.backlog.push_back(path);
        }
    }

    fn remove(&mut self, path: &Path) -> Result<()> {
        if let Some(id) = self.store.delete_file(path)? {
            self.vectors.write().unwrap().remove_file(id);
        }
        self.states.remove(path);
        Ok(())
    }

    /// Indexes one file. It skips a file whose (mtime, size) is unchanged, so a chmod or a
    /// touch costs no embedding.
    fn process(&mut self, path: &Path, prio: Priority) -> Result<Outcome> {
        let counted = self.states.contains_key(path) || self.queued.contains(path);
        let outcome = self.process_inner(path, prio, counted);
        let _ = std::fs::remove_file(&self.inflight);
        let outcome = outcome?;
        if outcome != Outcome::ModelDown {
            self.queued.remove(path);
        }
        self.sync_progress();
        Ok(outcome)
    }

    fn process_inner(&mut self, path: &Path, prio: Priority, counted: bool) -> Result<Outcome> {
        let kind = tiers::kind_of(path);
        let md = match std::fs::symlink_metadata(path) {
            Ok(md) if md.is_file() => md,
            _ => {
                self.remove(path)?;
                self.total = self.total.saturating_sub(counted as usize);
                return Ok(Outcome::Unchanged);
            }
        };
        if kind == Kind::Other || tiers::skip_content(path) {
            return Ok(Outcome::Unchanged);
        }
        let sig = signature(&md);
        if self.states.get(path) == Some(&sig) {
            return Ok(Outcome::Unchanged);
        }
        self.total += !counted as usize;
        std::fs::write(&self.inflight, path.as_os_str().as_bytes())?;

        let pieces = match extract::extract(&self.root, path, kind) {
            Ok(p) => p,
            Err(e) => return self.fail(path, kind, sig, &format!("{e:#}")),
        };
        let vecs = match embed_pieces(&self.embed, prio, &pieces) {
            Ok(v) => v,
            Err(e) if model_down(&e) => {
                self.total = self.total.saturating_sub(!counted as usize);
                return Ok(Outcome::ModelDown);
            }
            // An embedding error can be temporary, such as VRAM pressure from a game. A zero
            // mtime makes the next start or the next change of the file retry it.
            Err(e) => return self.fail(path, kind, (0, sig.1), &format!("{e:#}")),
        };
        let chunks: Vec<NewChunk> = pieces
            .into_iter()
            .zip(vecs)
            .map(|(p, vec)| NewChunk { text: p.fts, vec })
            .collect();
        self.write(path, kind, sig, &chunks)?;
        Ok(Outcome::Indexed(chunks.len()))
    }

    fn write(
        &mut self,
        path: &Path,
        kind: Kind,
        (mtime, size): (i64, i64),
        chunks: &[NewChunk],
    ) -> Result<()> {
        let (file_id, ids) = self.store.replace_file(path, kind, mtime, size, chunks)?;
        let mut vectors = self.vectors.write().unwrap();
        vectors.remove_file(file_id);
        for (chunk, id) in chunks.iter().zip(ids) {
            if let Some(v) = &chunk.vec {
                vectors.insert(id, file_id, v);
            }
        }
        self.states.insert(path.to_path_buf(), (mtime, size));
        Ok(())
    }

    fn fail(&mut self, path: &Path, kind: Kind, sig: (i64, i64), error: &str) -> Result<Outcome> {
        tracing::warn!("{}: {error}", path.display());
        let id = self.store.set_error(path, kind, sig.0, sig.1, error)?;
        self.vectors.write().unwrap().remove_file(id);
        self.states.insert(path.to_path_buf(), sig);
        Ok(Outcome::Failed)
    }
}

fn signature(md: &Metadata) -> (i64, i64) {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64);
    (mtime, md.len() as i64)
}

fn model_down(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("model unavailable")
}

/// One vector per piece that has an embedding input. Text pieces go in jobs of TEXTS_PER_JOB.
fn embed_pieces(
    embed: &EmbedService,
    prio: Priority,
    pieces: &[Piece],
) -> Result<Vec<Option<Vec<f32>>>> {
    let mut out: Vec<Option<Vec<f32>>> = vec![None; pieces.len()];
    let mut text_idx = Vec::new();
    let mut prompts = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        match &p.embed {
            Some((prompt, media)) if media.is_empty() => {
                text_idx.push(i);
                prompts.push(prompt.clone());
            }
            Some((prompt, media)) => {
                out[i] = Some(embed.embed(prio, prompt.clone(), media.clone())?)
            }
            None => {}
        }
    }
    for (idx, group) in text_idx
        .chunks(TEXTS_PER_JOB)
        .zip(prompts.chunks(TEXTS_PER_JOB))
    {
        for (&i, v) in idx.iter().zip(embed.embed_texts(prio, group.to_vec())?) {
            out[i] = Some(v);
        }
    }
    Ok(out)
}

/// The non-recursive inotify watches. A recursive watch on $HOME would exhaust the watch limit.
struct Watch {
    watcher: RecommendedWatcher,
    watched: HashSet<PathBuf>,
    warned: bool,
}

impl Watch {
    /// Returns true if `dir` was not watched yet.
    fn add_dir(&mut self, dir: &Path) -> bool {
        if !self.watched.insert(dir.to_path_buf()) {
            return false;
        }
        if let Err(e) = self.watcher.watch(dir, RecursiveMode::NonRecursive) {
            if !self.warned {
                self.warned = true;
                tracing::error!("cannot watch {}: {e}; raise fs.inotify.max_user_watches. New files wait for a restart.", dir.display());
            }
        }
        true
    }

    /// Watches a new directory and the content-tier directories below it. Files already inside
    /// go to `pending`, because they may have arrived before the watch.
    fn add_tree(&mut self, root: &Path, dir: &Path, pending: &mut HashMap<PathBuf, Instant>) {
        if !self.add_dir(dir) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !tiers::in_content_tier(root, &path) {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => self.add_tree(root, &path, pending),
                Ok(t) if t.is_file() => {
                    pending.insert(path, Instant::now() + DEBOUNCE);
                }
                _ => {}
            }
        }
    }

    fn on_event(
        &mut self,
        root: &Path,
        ev: notify::Result<notify::Event>,
        pending: &mut HashMap<PathBuf, Instant>,
        gone: &mut Vec<PathBuf>,
    ) {
        let ev = match ev {
            Ok(ev) => ev,
            Err(e) => {
                tracing::debug!("watch error: {e}");
                return;
            }
        };
        if !matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        for path in ev.paths {
            if !tiers::in_content_tier(root, &path) {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() => self.add_tree(root, &path, pending),
                // A watched directory that is gone (deleted or moved away): forget its watches,
                // so that a new directory at the same path is watched again, and drop its rows.
                Err(_) if self.watched.contains(&path) => {
                    self.watched.retain(|d| !d.starts_with(&path));
                    let _ = self.watcher.unwatch(&path);
                    gone.push(path);
                }
                _ => {
                    pending.insert(path, Instant::now() + DEBOUNCE);
                }
            }
        }
    }
}

fn phase(backlog_empty: bool, model_wait: Option<Instant>, busy: Option<&str>) -> String {
    if backlog_empty {
        "idle".into()
    } else if model_wait.is_some_and(|t| t > Instant::now()) {
        "waiting for model".into()
    } else if let Some(reason) = busy {
        format!("paused ({reason})")
    } else {
        "indexing".into()
    }
}

fn daemon_loop(mut ix: Indexer) -> Result<()> {
    let (ev_tx, ev_rx) = mpsc::channel();
    let watcher = notify::recommended_watcher(move |ev| {
        let _ = ev_tx.send(ev);
    })?;
    let mut watch = Watch {
        watcher,
        watched: HashSet::new(),
        warned: false,
    };
    let load = load::watch();

    let dirs = ix.reconcile()?;
    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let root = ix.root.clone();
    for d in &dirs {
        watch.add_dir(d);
    }
    tracing::info!(
        "indexer ready: {} files, {} in the backlog, {} directories watched",
        ix.total,
        ix.backlog.len(),
        watch.watched.len()
    );

    let mut model_wait: Option<Instant> = None;
    let mut gone = Vec::new();
    let mut paused = false;
    loop {
        let mut busy = false;

        while let Ok(ev) = ev_rx.try_recv() {
            watch.on_event(&root, ev, &mut pending, &mut gone);
        }
        if !gone.is_empty() {
            if let Err(e) = ix.remove_trees(&gone) {
                tracing::warn!("removing rows of gone directories: {e:#}");
            }
            gone.clear();
        }

        let now = Instant::now();
        let mut due: Vec<PathBuf> = pending
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(p, _)| p.clone())
            .collect();
        if due.len() > LIVE_BURST {
            for p in due.drain(LIVE_PER_PASS..) {
                pending.remove(&p);
                ix.enqueue(p);
            }
        }
        // A closed gate holds live changes too: during a game nothing but queries uses the GPU.
        let gate = *load.lock().unwrap();
        if gate.is_some() && !paused {
            ix.embed.unload();
        }
        paused = gate.is_some();
        let live = if paused { 0 } else { LIVE_PER_PASS };
        for path in due.into_iter().take(live) {
            pending.remove(&path);
            busy = true;
            match ix.process(&path, Priority::Live) {
                Ok(Outcome::ModelDown) => {
                    pending.insert(path, now + MODEL_RETRY);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("{}: {e:#}", path.display()),
            }
        }

        if model_wait.is_some_and(|t| t <= Instant::now()) {
            model_wait = None;
        }
        let ph = phase(
            ix.backlog.is_empty() && pending.is_empty(),
            model_wait,
            gate,
        );
        let run = ph == "indexing";
        *ix.progress.phase.lock().unwrap() = ph;
        if run {
            if let Some(path) = ix.backlog.pop_front() {
                if ix.queued.contains(&path) {
                    busy = true;
                    match ix.process(&path, Priority::Backlog) {
                        Ok(Outcome::ModelDown) => {
                            ix.backlog.push_front(path);
                            model_wait = Some(Instant::now() + MODEL_RETRY);
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("{}: {e:#}", path.display()),
                    }
                }
            }
        }

        if !busy {
            if let Ok(ev) = ev_rx.recv_timeout(POLL) {
                watch.on_event(&root, ev, &mut pending, &mut gone);
            }
        }
    }
}
