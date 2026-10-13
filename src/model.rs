//! The GGUF model files: paths and first-run download.

use crate::config::models_dir;
use anyhow::{ensure, Context, Result};
use std::path::PathBuf;
use std::process::Command;

const REPO: &str = "ggml-org/embeddinggemma-2-GGUF";
/// Pinned so that every install embeds into the same vector space.
const REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";
const MODEL: &str = "embeddinggemma-2-Q8_0.gguf";
const MMPROJ: &str = "mmproj-embeddinggemma-2-Q8_0.gguf";

pub fn paths() -> (PathBuf, PathBuf) {
    (models_dir().join(MODEL), models_dir().join(MMPROJ))
}

/// Downloads any missing model file with curl. Each file lands under a `.part` name first.
pub fn ensure() -> Result<(PathBuf, PathBuf)> {
    let dir = models_dir();
    std::fs::create_dir_all(&dir)?;
    for name in [MODEL, MMPROJ] {
        let dest = dir.join(name);
        if dest.exists() {
            continue;
        }
        let part = dir.join(format!("{name}.part"));
        let url = format!("https://huggingface.co/{REPO}/resolve/{REVISION}/{name}");
        tracing::info!("downloading {url}");
        let status = Command::new("curl")
            .args(["-fL", "--retry", "3", "-C", "-", "-o"])
            .arg(&part)
            .arg(&url)
            .status()
            .context("cannot run curl")?;
        ensure!(status.success(), "download of {name} failed: {status}");
        std::fs::rename(&part, &dest)?;
    }
    Ok(paths())
}
