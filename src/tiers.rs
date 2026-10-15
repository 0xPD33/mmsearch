//! Which files each index tier covers, and the kind of each file.

use crate::types::Kind;
use ignore::{DirEntry, WalkBuilder};
use std::path::Path;

/// The excludes of the old `fd` file search. Entries with a slash match a path relative to the root.
const PATH_EXCLUDES: &[&str] = &[
    ".git",
    "node_modules",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".local/state",
    ".Trash",
];

/// Directory names the content tier skips in addition to dot-directories and `PATH_EXCLUDES`.
const CONTENT_EXCLUDES: &[&str] = &[
    "site-packages",
    "__pycache__",
    "venv",
    ".venv",
    "target",
    "dist",
    "build",
    "Sdk",
];

/// Files bigger than this get path search only.
pub const MAX_TEXT_BYTES: u64 = 1 << 20;

/// All paths under `root`, for fuzzy path search.
pub fn path_walker(root: &Path) -> WalkBuilder {
    let root = root.to_path_buf();
    let mut b = WalkBuilder::new(&root);
    b.hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(false)
        .parents(false)
        // Network and FUSE mounts under the root (sshfs, rclone) stay out: walking them is slow
        // and indexing them downloads every file.
        .same_file_system(true);
    b.filter_entry(move |e| !excluded(&root, e.path(), PATH_EXCLUDES) && !hidden_dir(&root, e));
    b
}

/// Hidden directories hold app data, caches and game files: 73% of the 2M paths in one
/// home, and the fuzzy match scans every path on each keystroke. `~/.config` stays for
/// dotfiles, and so do hidden files such as `~/.zshrc`.
fn hidden_dir(root: &Path, e: &DirEntry) -> bool {
    e.depth() > 0
        && e.file_type().is_some_and(|t| t.is_dir())
        && e.file_name().to_string_lossy().starts_with('.')
        && e.path() != root.join(".config")
}

/// The files that get keyword text and vectors.
pub fn content_walker(root: &Path) -> WalkBuilder {
    let root = root.to_path_buf();
    let mut b = WalkBuilder::new(&root);
    b.hidden(true)
        .follow_links(false)
        .git_ignore(true)
        .git_global(false)
        .parents(false)
        .same_file_system(true);
    b.filter_entry(move |e| content_entry(&root, e));
    b
}

fn content_entry(root: &Path, e: &DirEntry) -> bool {
    let p = e.path();
    if excluded(root, p, PATH_EXCLUDES) || excluded(root, p, CONTENT_EXCLUDES) {
        return false;
    }
    // A Python venv can have any name; pyvenv.cfg marks it.
    !(e.file_type().is_some_and(|t| t.is_dir()) && p.join("pyvenv.cfg").exists())
}

fn excluded(root: &Path, path: &Path, names: &[&str]) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    names.iter().any(|n| {
        if n.contains('/') {
            rel.starts_with(n)
        } else {
            rel.components().any(|c| c.as_os_str() == *n)
        }
    })
}

/// True if a watcher event for `path` belongs to the content tier.
/// It checks the path rules only, not `.gitignore`.
pub fn in_content_tier(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    if rel
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    {
        return false;
    }
    !excluded(root, path, PATH_EXCLUDES) && !excluded(root, path, CONTENT_EXCLUDES)
}

/// Lockfiles and minified files carry no searchable meaning.
pub fn skip_content(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.ends_with(".lock")
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name.contains(".min.")
        || name.ends_with(".map")
}

pub fn kind_of(path: &Path) -> Kind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" | "txt" | "org" | "rst" | "adoc" | "tex" => Kind::Text,
        "rs" | "py" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cc"
        | "cpp" | "hpp" | "cxx" | "java" | "kt" | "kts" | "swift" | "rb" | "php" | "lua" | "sh"
        | "bash" | "zsh" | "fish" | "nix" | "zig" | "gd" | "svelte" | "vue" | "qml" | "cs"
        | "scala" | "hs" | "ml" | "ex" | "exs" | "clj" | "sql" | "css" | "scss" | "html"
        | "htm" | "wgsl" | "glsl" | "dart" | "r" | "jl" | "el" | "vim" => Kind::Code,
        "json" | "jsonc" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "xml" | "csv"
        | "kdl" => Kind::Data,
        "pdf" => Kind::Pdf,
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" => Kind::Image,
        "mp3" | "flac" | "wav" | "ogg" | "oga" | "opus" | "m4a" | "aac" => Kind::Audio,
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" => Kind::Video,
        _ => Kind::Other,
    }
}
