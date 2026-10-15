//! Types that several subsystems share.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Code,
    /// JSON, YAML, TOML and similar: keyword text only, no vectors.
    Data,
    Pdf,
    Image,
    Audio,
    Video,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Code => "code",
            Kind::Data => "data",
            Kind::Pdf => "pdf",
            Kind::Image => "image",
            Kind::Audio => "audio",
            Kind::Video => "video",
            Kind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Kind {
        match s {
            "text" => Kind::Text,
            "code" => Kind::Code,
            "data" => Kind::Data,
            "pdf" => Kind::Pdf,
            "image" => Kind::Image,
            "audio" => Kind::Audio,
            "video" => Kind::Video,
            _ => Kind::Other,
        }
    }
}

/// The filter chips in the overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    #[default]
    All,
    Images,
    Media,
    Docs,
    Code,
}

impl Filter {
    pub const ALL: [Filter; 5] = [
        Filter::All,
        Filter::Images,
        Filter::Media,
        Filter::Docs,
        Filter::Code,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Images => "Images",
            Filter::Media => "Media",
            Filter::Docs => "Docs",
            Filter::Code => "Code",
        }
    }

    pub fn accepts(self, kind: Kind) -> bool {
        match self {
            Filter::All => true,
            Filter::Images => kind == Kind::Image,
            Filter::Media => matches!(kind, Kind::Audio | Kind::Video),
            Filter::Docs => matches!(kind, Kind::Text | Kind::Pdf | Kind::Other),
            Filter::Code => matches!(kind, Kind::Code | Kind::Data),
        }
    }
}

/// One search result. The ranking unit is the file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub path: PathBuf,
    pub kind: Kind,
    /// The RRF score; higher is better.
    pub score: f32,
    /// One line for the result row, from the best chunk.
    pub snippet: Option<String>,
    /// The best chunk text for the preview pane, at most about 2000 chars.
    pub preview: Option<String>,
    /// The file's modification time, in Unix seconds.
    #[serde(default)]
    pub modified: Option<i64>,
}

/// The daemon state that `mmsearch status` and the overlay status line show.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub indexed: usize,
    pub total: usize,
    /// For example "indexing", "paused (cpu busy)", "paused (battery)", "idle".
    pub phase: String,
    /// "unloaded", "loading", "ready" or "failed: <reason>".
    pub model: String,
}
