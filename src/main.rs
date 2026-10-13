mod capture;
mod config;
mod daemon;
mod embed;
mod extract;
mod index;
mod ipc;
mod load;
mod model;
mod niri;
mod paths;
mod search;
mod store;
mod thumbs;
mod tiers;
mod types;
mod ui;
mod vectors;
mod worker;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Parser)]
#[command(about = "Multimodal search overlay for Wayland")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the indexer, the store, search and the overlay.
    Daemon,
    /// Open or close the overlay.
    Toggle,
    /// Show index progress and model state.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Search without the overlay.
    Query {
        #[arg(default_value = "")]
        text: String,
        #[arg(long)]
        image: Option<PathBuf>,
        #[arg(long)]
        audio: Option<PathBuf>,
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Index one directory once, with no load gate.
    Index {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Print the vector of one input as JSON. Put one media marker in PROMPT per --media file.
    Embed {
        #[arg(long, default_value = "")]
        prompt: String,
        #[arg(long)]
        media: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mmsearch=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().cmd {
        Cmd::Daemon => daemon::run(),
        Cmd::Toggle => {
            if let Err(e) = ipc::send(&ipc::Request::Toggle) {
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "mmsearch", "mmsearch", &format!("{e:#}")])
                    .status();
                bail!(e);
            }
            Ok(())
        }
        Cmd::Status { json } => {
            let v = ipc::send(&ipc::Request::Status)?;
            if json {
                println!("{v}");
            } else {
                let s: types::Status = serde_json::from_value(v)?;
                println!(
                    "index:  {} / {} · {}\nmodel:  {}",
                    s.indexed, s.total, s.phase, s.model
                );
            }
            Ok(())
        }
        Cmd::Query {
            text,
            image,
            audio,
            db,
            limit,
            json,
        } => query(
            text,
            image,
            audio,
            db.unwrap_or_else(config::db_path),
            limit,
            json,
        ),
        Cmd::Index { root, db } => {
            let embed = start_embedder()?;
            index::run_once(&root, &db.unwrap_or_else(config::db_path), &embed)
        }
        Cmd::Embed { prompt, media } => {
            let (model, mmproj) = model::ensure()?;
            let mut e = embed::Embedder::load(&model, &mmproj)?;
            let media: Vec<_> = media.into_iter().map(embed::Media::File).collect();
            let prompt = if prompt.is_empty() {
                embed::marker().repeat(media.len())
            } else {
                prompt
            };
            println!("{}", serde_json::to_string(&e.embed(&prompt, &media)?)?);
            Ok(())
        }
    }
}

fn start_embedder() -> Result<worker::EmbedService> {
    let (model, mmproj) = model::ensure()?;
    Ok(worker::EmbedService::start(model, mmproj))
}

/// A one-shot search over a database, with the path list taken from the database itself.
fn query(
    text: String,
    image: Option<PathBuf>,
    audio: Option<PathBuf>,
    db: PathBuf,
    limit: usize,
    json: bool,
) -> Result<()> {
    let store = store::Store::open_existing(&db, embed::MODEL_ID)?;
    let vectors = Arc::new(RwLock::new(store.load_vectors()?));
    let paths = paths::PathList::new();
    paths.set(store.file_states()?.into_keys().collect());
    let searcher = search::Searcher::new(&db, vectors, paths)?;

    let embed = start_embedder()?;
    let mut prompt = if text.is_empty() {
        String::new()
    } else {
        embed::query_prompt(&text)
    };
    let mut media = Vec::new();
    if let Some(p) = image {
        prompt.push_str(embed::marker());
        media.push(embed::Media::File(p));
    }
    if let Some(p) = audio {
        prompt.push_str(embed::marker());
        media.push(embed::Media::File(p));
    }
    let vector = if prompt.is_empty() {
        None
    } else {
        Some(embed.embed(worker::Priority::Query, prompt, media)?)
    };
    let hits = searcher.full(&text, vector.as_deref(), types::Filter::All, limit)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        for h in &hits {
            println!(
                "{:.4}  {:<6}  {}",
                h.score,
                h.kind.as_str(),
                h.path.display()
            );
        }
    }
    Ok(())
}
