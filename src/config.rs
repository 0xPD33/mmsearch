//! File locations.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("mmsearch")
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn db_path() -> PathBuf {
    data_dir().join("index.db")
}

pub fn thumbnails_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("thumbnails")
}

/// The IPC socket. It must live in the private runtime directory: a socket in /tmp would let
/// other users talk to the daemon.
pub fn socket_path() -> anyhow::Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(dir).join("mmsearch.sock"))
}
