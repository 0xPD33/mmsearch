//! The daemon: wires the embedder, store, indexer, search, IPC and the overlay.

use crate::config;
use crate::embed::{self, Media, MODEL_ID};
use crate::index::{self, Progress};
use crate::ipc::{self, Request};
use crate::paths::PathList;
use crate::search::Searcher;
use crate::store::Store;
use crate::types::Status;
use crate::ui::{self, UiEvent, UiRequest, UiSender};
use crate::worker::{EmbedService, Priority};
use crate::{capture, model};
use anyhow::Result;
use serde_json::json;
use std::collections::VecDeque;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const LIMIT: usize = 50;
/// The typing pause before the semantic matcher runs.
const DEBOUNCE: Duration = Duration::from_millis(150);

pub fn run() -> Result<()> {
    // Bind first: a second daemon stops here, before it walks $HOME or loads a model.
    let listener = ipc::bind()?;
    let root = config::home();
    let db = config::db_path();
    // The index holds extracted document text.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(config::data_dir())?;
    std::fs::set_permissions(config::data_dir(), std::fs::Permissions::from_mode(0o700))?;

    // The first download takes minutes. Path and keyword search work meanwhile, and the
    // embedder retries its load every minute until the files exist.
    let (model_path, mmproj_path) = model::paths();
    let embed = EmbedService::start(model_path, mmproj_path);
    std::thread::spawn(|| {
        if let Err(e) = model::ensure() {
            tracing::error!("model download failed: {e:#}");
        }
    });

    let vectors = Arc::new(RwLock::new(Store::open(&db, MODEL_ID)?.load_vectors()?));
    let paths = PathList::new();
    paths.refresh(&root);
    let indexer = index::spawn(root.clone(), db.clone(), embed.clone(), vectors.clone())?;
    let searcher = Searcher::new(&db, vectors, paths.clone())?;

    let status = {
        let (progress, embed) = (indexer.progress.clone(), embed.clone());
        Arc::new(move || status(&progress, &embed))
    };
    let (req_tx, req_rx) = mpsc::channel();

    ui::run(req_tx, move |proxy| {
        let ipc_proxy = proxy.clone();
        let ipc_status = status.clone();
        let served = ipc::serve(listener, move |req| match req {
            Request::Toggle => {
                let _ = ipc_proxy.send(UiEvent::Toggle);
                json!({ "ok": true })
            }
            Request::Status => serde_json::to_value(ipc_status()).unwrap(),
        });
        if let Err(e) = served {
            tracing::error!("IPC: {e:#}");
            let _ = proxy.send(UiEvent::Error(format!("IPC: {e}")));
        }

        let status_proxy = proxy.clone();
        std::thread::spawn(move || loop {
            if !status_proxy.send(UiEvent::Status(status())) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        });

        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || search_loop(req_rx, proxy, searcher, embed, paths, root))
            .unwrap();
    })
}

fn status(progress: &Progress, embed: &EmbedService) -> Status {
    use std::sync::atomic::Ordering::Relaxed;
    Status {
        indexed: progress.indexed.load(Relaxed),
        total: progress.total.load(Relaxed),
        phase: progress.phase.lock().unwrap().clone(),
        model: embed.state(),
    }
}

fn search_loop(
    rx: Receiver<UiRequest>,
    proxy: UiSender,
    searcher: Searcher,
    embed: EmbedService,
    paths: Arc<PathList>,
    root: PathBuf,
) {
    let send = |e: UiEvent| {
        proxy.send(e);
    };
    // Requests read ahead of their turn, in arrival order.
    let mut pending: VecDeque<UiRequest> = VecDeque::new();
    loop {
        let mut req = match pending.pop_front() {
            Some(r) => r,
            None => match rx.recv() {
                Ok(r) => r,
                Err(_) => return,
            },
        };
        // The path matcher takes up to ~0.4 s on millions of paths, so typing can queue
        // several queries. Answer only the newest one.
        while matches!(req, UiRequest::Search { .. }) {
            match rx.try_recv() {
                Ok(newer @ UiRequest::Search { .. }) => req = newer,
                Ok(other) => {
                    pending.push_back(other);
                    break;
                }
                Err(_) => break,
            }
        }
        match req {
            UiRequest::Shown => {
                paths.refresh(&root);
                embed.preload();
            }
            UiRequest::CaptureRegion => {
                let proxy = proxy.clone();
                std::thread::spawn(move || {
                    let event = match capture::capture_region() {
                        Ok(Some(img)) => UiEvent::ImageChip(img),
                        // Cancelled: reopen the overlay as it was.
                        Ok(None) => UiEvent::Toggle,
                        Err(e) => UiEvent::Error(format!("capture: {e}")),
                    };
                    let _ = proxy.send(event);
                });
            }
            UiRequest::Search {
                id,
                text,
                image,
                audio,
                filter,
            } => {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    match searcher.fast(&text, filter, LIMIT) {
                        Ok(hits) => send(UiEvent::Results {
                            id,
                            hits,
                            done: false,
                        }),
                        Err(e) => send(UiEvent::Error(format!("search: {e}"))),
                    }
                }
                match rx.recv_timeout(DEBOUNCE) {
                    Ok(newer) => {
                        pending.push_back(newer);
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }

                let mut prompt = if text.is_empty() {
                    String::new()
                } else {
                    embed::query_prompt(&text)
                };
                let mut media = Vec::new();
                if let Some(img) = &image {
                    prompt.push_str(embed::marker());
                    media.push(Media::from_image(img));
                }
                if let Some(samples) = audio {
                    prompt.push_str(embed::marker());
                    media.push(Media::Audio(samples));
                }
                if prompt.is_empty() {
                    send(UiEvent::Results {
                        id,
                        hits: Vec::new(),
                        done: true,
                    });
                    continue;
                }
                let vector = embed.embed(Priority::Query, prompt, media);
                // The user typed on while the model ran: this result is stale.
                if let Ok(newer) = rx.try_recv() {
                    pending.push_back(newer);
                    continue;
                }
                let vector = match vector {
                    Ok(v) => Some(v),
                    Err(e) => {
                        send(UiEvent::Error(format!("semantic search off: {e}")));
                        None
                    }
                };
                match searcher.full(&text, vector.as_deref(), filter, LIMIT) {
                    Ok(hits) => send(UiEvent::Results {
                        id,
                        hits,
                        done: true,
                    }),
                    Err(e) => send(UiEvent::Error(format!("search: {e}"))),
                }
            }
        }
    }
}
