//! Per-kind extractors: file -> keyword text and embedding inputs.

use crate::embed::{document_prompt, marker, Media};
use crate::thumbs;
use crate::tiers::MAX_TEXT_BYTES;
use crate::types::Kind;
use anyhow::{anyhow, bail, ensure, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CHUNK_CHARS: usize = 2000;
const OVERLAP_CHARS: usize = 200;
const TIMEOUT: Duration = Duration::from_secs(60);
const MIN_IMAGE_SIDE: u32 = 64;
const MAX_IMAGE_EDGE: u32 = 1536;
const VIDEO_FRAMES: usize = 8;
const VIDEO_EDGE: u32 = 768;
const THUMB_FRAME: usize = 4;
/// A downloaded file can be a playlist that names network URLs; allow local reads only.
const INPUT_PROTOCOLS: [&str; 2] = ["-protocol_whitelist", "file,pipe"];
const AUDIO_SECONDS: u32 = 300;
const AUDIO_RATE: usize = 16_000;
const MIN_AUDIO_SAMPLES: usize = AUDIO_RATE / 2;

/// One chunk of a file.
pub struct Piece {
    /// Keyword text for FTS5.
    pub fts: Option<String>,
    /// The embedding input: a prompt with one media marker per media item.
    pub embed: Option<(String, Vec<Media>)>,
}

/// Extracts the pieces of `path`. `root` gives the relative path for the document title.
pub fn extract(root: &Path, path: &Path, kind: Kind) -> Result<Vec<Piece>> {
    match kind {
        Kind::Text => Ok(text_pieces(root, path, read_text(path)?, usize::MAX)),
        // One vector per code file, from its start: a vector per chunk made code about 90%
        // of the vectors in a developer's home. Keyword search still covers every chunk.
        Kind::Code => Ok(text_pieces(root, path, read_text(path)?, 1)),
        Kind::Data => Ok(text_pieces(root, path, read_text(path)?, 0)),
        Kind::Pdf => {
            let out = run(Command::new("pdftotext")
                .args(["-layout", "-l", "50"])
                .arg(path)
                .arg("-"))?;
            Ok(text_pieces(
                root,
                path,
                Some(String::from_utf8_lossy(&out).into_owned()),
                usize::MAX,
            ))
        }
        Kind::Image => image_pieces(path),
        Kind::Audio => audio_pieces(path),
        Kind::Video => video_pieces(path),
        Kind::Other => Ok(Vec::new()),
    }
}

/// None if the file is too big, not UTF-8, or binary.
fn read_text(path: &Path) -> Result<Option<String>> {
    if std::fs::metadata(path)?.len() > MAX_TEXT_BYTES {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

/// The first `vectors` chunks get an embedding input; all chunks get keyword text.
fn text_pieces(root: &Path, path: &Path, text: Option<String>, vectors: usize) -> Vec<Piece> {
    let Some(text) = text else { return Vec::new() };
    let title = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    chunks(&text)
        .into_iter()
        .enumerate()
        .map(|(i, c)| Piece {
            embed: (i < vectors).then(|| (document_prompt(&title, &c), Vec::new())),
            fts: Some(c),
        })
        .collect()
}

/// Chunks of about CHUNK_CHARS that start on line boundaries and overlap by about OVERLAP_CHARS.
/// A line longer than CHUNK_CHARS is cut in the middle.
fn chunks(text: &str) -> Vec<String> {
    let mut segments = Vec::new();
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while let Some((at, _)) = rest.char_indices().nth(CHUNK_CHARS) {
            let (a, b) = rest.split_at(at);
            segments.push(a);
            rest = b;
        }
        segments.push(rest);
    }

    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut len = 0;
    for seg in segments {
        let n = seg.chars().count();
        if len + n > CHUNK_CHARS && !cur.is_empty() {
            out.push(cur.concat());
            let (mut k, mut kept) = (cur.len(), 0);
            while k > 0 && kept + cur[k - 1].chars().count() <= OVERLAP_CHARS {
                k -= 1;
                kept += cur[k].chars().count();
            }
            cur.drain(..k);
            len = kept;
        }
        cur.push(seg);
        len += n;
    }
    if !cur.is_empty() {
        out.push(cur.concat());
    }
    out.retain(|c| !c.trim().is_empty());
    out
}

fn image_pieces(path: &Path) -> Result<Vec<Piece>> {
    let mut img = crate::thumbs::open_image(path)?;
    if img.width().min(img.height()) < MIN_IMAGE_SIDE {
        return Ok(Vec::new());
    }
    if img.width().max(img.height()) > MAX_IMAGE_EDGE {
        img = img.resize(
            MAX_IMAGE_EDGE,
            MAX_IMAGE_EDGE,
            image::imageops::FilterType::Triangle,
        );
    }
    if let Err(e) = thumbs::store(path, &img) {
        tracing::debug!("thumbnail for {} failed: {e:#}", path.display());
    }
    Ok(vec![media_piece(
        marker().to_string(),
        vec![Media::from_image(&img.to_rgb8())],
    )])
}

fn audio_pieces(path: &Path) -> Result<Vec<Piece>> {
    let raw = run(Command::new("ffmpeg")
        .args(["-v", "error"])
        .args(INPUT_PROTOCOLS)
        .arg("-i")
        .arg(path)
        .args([
            "-t",
            &AUDIO_SECONDS.to_string(),
            "-ac",
            "1",
            "-ar",
            &AUDIO_RATE.to_string(),
            "-f",
            "f32le",
            "-",
        ]))?;
    let samples: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    if samples.len() < MIN_AUDIO_SAMPLES {
        return Ok(Vec::new());
    }
    Ok(vec![media_piece(
        marker().to_string(),
        vec![Media::Audio(samples)],
    )])
}

fn video_pieces(path: &Path) -> Result<Vec<Piece>> {
    let probe = run(Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:format=duration",
            "-of",
            "csv=p=0",
        ])
        .args(INPUT_PROTOCOLS)
        .arg(path))?;
    // One line per stream type, then the duration. Side data adds fields: "video,".
    let probe = String::from_utf8_lossy(&probe);
    let lines: Vec<&str> = probe.lines().map(str::trim).collect();
    if !lines.iter().any(|l| l.split(',').next() == Some("video")) {
        // An audio-only download, such as a YouTube .webm.
        return audio_pieces(path);
    }
    let duration: f64 = lines
        .last()
        .and_then(|d| d.parse().ok())
        .context("the video has no duration")?;
    ensure!(duration > 0.0, "the video has no duration");

    let scale = format!(
        "scale=min({VIDEO_EDGE}\\,iw):min({VIDEO_EDGE}\\,ih):force_original_aspect_ratio=decrease"
    );
    let grab = |seek: Option<String>| {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error"]).args(INPUT_PROTOCOLS);
        if let Some(t) = seek {
            cmd.args(["-ss", &t]);
        }
        cmd.arg("-i").arg(path).args([
            "-frames:v",
            "1",
            "-vf",
            &scale,
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-",
        ]);
        run(&mut cmd)
    };
    let mut frames = Vec::with_capacity(VIDEO_FRAMES);
    for i in 0..VIDEO_FRAMES {
        let t = duration * (i as f64 + 0.5) / VIDEO_FRAMES as f64;
        let mut png = grab(Some(format!("{t:.3}")))?;
        if png.is_empty() {
            // A clip shorter than its frame spacing, such as a one-frame sticker: the seek
            // lands past the last frame. The first frame stands in.
            png = grab(None)?;
        }
        ensure!(!png.is_empty(), "ffmpeg returned no frame at {t:.1} s");
        frames.push(image::load_from_memory(&png)?);
    }
    if let Err(e) = thumbs::store(path, &frames[THUMB_FRAME]) {
        tracing::debug!("thumbnail for {} failed: {e:#}", path.display());
    }
    let media = frames
        .iter()
        .map(|f| Media::from_image(&f.to_rgb8()))
        .collect();
    Ok(vec![media_piece(marker().repeat(VIDEO_FRAMES), media)])
}

fn media_piece(prompt: String, media: Vec<Media>) -> Piece {
    Piece {
        fts: None,
        embed: Some((prompt, media)),
    }
}

fn drain(mut r: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

/// Runs `cmd` and returns its stdout. Kills it after TIMEOUT.
fn run(cmd: &mut Command) -> Result<Vec<u8>> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {name}"))?;
    let out = drain(child.stdout.take().unwrap());
    let err = drain(child.stderr.take().unwrap());
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{name} timed out after {} s", TIMEOUT.as_secs());
        }
        thread::sleep(Duration::from_millis(20));
    };
    let (out, err) = (
        out.join().map_err(|_| anyhow!("reader panicked"))?,
        err.join().map_err(|_| anyhow!("reader panicked"))?,
    );
    if !status.success() {
        let msg = String::from_utf8_lossy(&err);
        bail!(
            "{name} failed ({status}): {}",
            msg.trim().lines().last().unwrap_or("")
        );
    }
    Ok(out)
}
