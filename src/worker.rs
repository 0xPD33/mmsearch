//! The embedder thread: one owner of the llama.cpp model, a priority queue, and idle unload.

use crate::embed::{Embedder, Media};
use anyhow::{anyhow, Result};
use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const UNLOAD_AFTER: Duration = Duration::from_secs(5 * 60);
const RETRY_LOAD_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Backlog,
    Live,
    Query,
}

enum Work {
    One(String, Vec<Media>),
    Texts(Vec<String>),
}

struct Job {
    prio: Priority,
    seq: u64,
    work: Work,
    reply: mpsc::Sender<Result<Vec<Vec<f32>>>>,
}

impl PartialEq for Job {
    fn eq(&self, o: &Self) -> bool {
        self.prio == o.prio && self.seq == o.seq
    }
}
impl Eq for Job {}
impl PartialOrd for Job {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Job {
    /// Highest priority first, then oldest first.
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.prio.cmp(&o.prio).then(o.seq.cmp(&self.seq))
    }
}

#[derive(Default)]
struct State {
    queue: BinaryHeap<Job>,
    seq: u64,
    preload: bool,
    unload: bool,
    model: String,
}

#[derive(Clone)]
pub struct EmbedService {
    shared: Arc<(Mutex<State>, Condvar)>,
}

impl EmbedService {
    pub fn start(model: PathBuf, mmproj: PathBuf) -> EmbedService {
        let shared = Arc::new((
            Mutex::new(State {
                model: "unloaded".into(),
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let s = shared.clone();
        std::thread::Builder::new()
            .name("embedder".into())
            .spawn(move || run(s, model, mmproj))
            .unwrap();
        EmbedService { shared }
    }

    /// Embeds one input; blocks until done.
    pub fn embed(&self, prio: Priority, prompt: String, media: Vec<Media>) -> Result<Vec<f32>> {
        Ok(self.submit(prio, Work::One(prompt, media))?.remove(0))
    }

    /// Embeds text-only inputs in batches; blocks until done.
    pub fn embed_texts(&self, prio: Priority, prompts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if prompts.is_empty() {
            return Ok(Vec::new());
        }
        self.submit(prio, Work::Texts(prompts))
    }

    /// Starts the model load now, so the next query does not wait for it.
    pub fn preload(&self) {
        let (m, cv) = &*self.shared;
        m.lock().unwrap().preload = true;
        cv.notify_one();
    }

    /// Frees the model and its VRAM once the queue is empty. The next job loads it again.
    pub fn unload(&self) {
        let (m, cv) = &*self.shared;
        m.lock().unwrap().unload = true;
        cv.notify_one();
    }

    /// "unloaded", "loading", "ready" or "failed: <reason>".
    pub fn state(&self) -> String {
        self.shared.0.lock().unwrap().model.clone()
    }

    fn submit(&self, prio: Priority, work: Work) -> Result<Vec<Vec<f32>>> {
        let (tx, rx) = mpsc::channel();
        let (m, cv) = &*self.shared;
        {
            let mut s = m.lock().unwrap();
            s.seq += 1;
            let seq = s.seq;
            s.queue.push(Job {
                prio,
                seq,
                work,
                reply: tx,
            });
        }
        cv.notify_one();
        rx.recv()
            .map_err(|_| anyhow!("the embedder thread stopped"))?
    }
}

fn run(shared: Arc<(Mutex<State>, Condvar)>, model: PathBuf, mmproj: PathBuf) {
    let (m, cv) = &*shared;
    let mut embedder: Option<Embedder> = None;
    let mut last_used = Instant::now();
    let mut failed_at: Option<(Instant, String)> = None;

    loop {
        let job = {
            let mut s = m.lock().unwrap();
            loop {
                if let Some(job) = s.queue.pop() {
                    break Some(job);
                }
                // Clear the flag even when the model is loaded, or the next unload reloads at once.
                if std::mem::take(&mut s.preload) && embedder.is_none() {
                    break None;
                }
                let asked = std::mem::take(&mut s.unload);
                if embedder.is_some() && (asked || last_used.elapsed() >= UNLOAD_AFTER) {
                    s.model = "unloaded".into();
                    // Freeing the model takes a while; status calls must not wait on the lock.
                    drop(s);
                    embedder = None;
                    tracing::info!("embedder unloaded");
                    s = m.lock().unwrap();
                    continue;
                }
                s = cv.wait_timeout(s, Duration::from_secs(30)).unwrap().0;
            }
        };

        if embedder.is_none() {
            let retry = failed_at
                .as_ref()
                .is_none_or(|(t, _)| t.elapsed() >= RETRY_LOAD_AFTER);
            if retry {
                m.lock().unwrap().model = "loading".into();
                match Embedder::load(&model, &mmproj) {
                    Ok(e) => {
                        embedder = Some(e);
                        failed_at = None;
                        m.lock().unwrap().model = "ready".into();
                    }
                    Err(e) => {
                        tracing::error!("model load failed: {e:#}");
                        m.lock().unwrap().model = format!("failed: {e}");
                        failed_at = Some((Instant::now(), e.to_string()));
                    }
                }
            }
        }
        last_used = Instant::now();

        let Some(job) = job else { continue };
        let result = match (&mut embedder, &failed_at) {
            (Some(e), _) => match job.work {
                Work::One(prompt, media) => e.embed(&prompt, &media).map(|v| vec![v]),
                Work::Texts(prompts) => e.embed_texts(&prompts),
            },
            (None, Some((_, reason))) => Err(anyhow!("model unavailable: {reason}")),
            (None, None) => Err(anyhow!("model unavailable")),
        };
        let _ = job.reply.send(result);
    }
}
