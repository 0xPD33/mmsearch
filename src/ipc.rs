//! Newline-delimited JSON over a Unix socket between the CLI and the daemon.

use crate::config::socket_path;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

/// A client that sends nothing must not block the next `toggle`.
const SERVER_TIMEOUT: Duration = Duration::from_secs(1);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LINE: u64 = 4096;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Toggle,
    Status,
}

pub fn send(req: &Request) -> Result<Value> {
    let path = socket_path()?;
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("no daemon at {}", path.display()))?;
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    writeln!(stream, "{}", serde_json::to_string(req)?)?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_LINE)).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// Binds the socket. It fails if a daemon already listens, so a second daemon stops at once.
pub fn bind() -> Result<UnixListener> {
    let path = socket_path()?;
    if UnixStream::connect(&path).is_ok() {
        bail!("a daemon already listens on {}", path.display());
    }
    let _ = std::fs::remove_file(&path);
    UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))
}

/// Answers each request with `handler` on a background thread.
pub fn serve(
    listener: UnixListener,
    handler: impl Fn(Request) -> Value + Send + Sync + 'static,
) -> Result<()> {
    std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if stream.set_read_timeout(Some(SERVER_TIMEOUT)).is_err() {
                    continue;
                }
                let mut line = String::new();
                if BufReader::new((&stream).take(MAX_LINE))
                    .read_line(&mut line)
                    .is_err()
                {
                    continue;
                }
                let reply = match serde_json::from_str::<Request>(&line) {
                    Ok(req) => handler(req),
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                };
                let _ = writeln!(&stream, "{reply}");
            }
        })?;
    Ok(())
}
