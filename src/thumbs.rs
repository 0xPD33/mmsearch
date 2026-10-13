//! The freedesktop thumbnail cache, shared with file managers.
//! Spec: https://specifications.freedesktop.org/thumbnail-spec/latest/

use crate::config::thumbnails_dir;
use anyhow::Result;
use image::RgbaImage;
use md5::{Digest, Md5};
use std::fs::{DirBuilder, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The "large" size of the spec.
pub const SIZE: u32 = 256;

pub fn uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        // The bytes that GLib g_filename_to_uri leaves as they are, so that the MD5 file name
        // matches the thumbnails that file managers write.
        if b.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

pub fn thumbnail_path(path: &Path) -> PathBuf {
    let hash = Md5::digest(uri(path).as_bytes());
    let name: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    thumbnails_dir().join("large").join(format!("{name}.png"))
}

fn mtime(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// A cached thumbnail that is newer than the file, if there is one.
pub fn load(path: &Path) -> Option<RgbaImage> {
    let thumb = thumbnail_path(path);
    if mtime(&thumb)? < mtime(path)? {
        return None;
    }
    image::open(thumb).ok().map(|i| i.to_rgba8())
}

/// Writes `img`, scaled to fit `SIZE`, as the thumbnail of `path`.
pub fn store(path: &Path, img: &image::DynamicImage) -> Result<()> {
    let small = img.thumbnail(SIZE, SIZE).to_rgba8();
    let thumb = thumbnail_path(path);
    let dir = thumb.parent().unwrap();
    // The spec asks for owner-only access: thumbnails show private files.
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // Write to a temp name and rename, as the spec asks, so readers never see half a file.
    let tmp = dir.join(format!(".mmsearch-{}.png", std::process::id()));
    {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        let file = std::io::BufWriter::new(file);
        let mut enc = png::Encoder::new(file, small.width(), small.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.add_text_chunk("Thumb::URI".into(), uri(path))?;
        enc.add_text_chunk("Thumb::MTime".into(), mtime(path).unwrap_or(0).to_string())?;
        enc.write_header()?.write_image_data(small.as_raw())?;
    }
    std::fs::rename(&tmp, &thumb)?;
    Ok(())
}

/// Decodes by content, not by extension: many PNG files carry a .jpg name.
pub fn open_image(path: &Path) -> Result<image::DynamicImage> {
    Ok(image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?)
}
