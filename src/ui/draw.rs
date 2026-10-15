//! Lays out the overlay and builds the scene. All numbers here are logical pixels.

use super::app::State;
use super::gfx::{lin, rgba8, Gfx, Quad, Scene, Tex, Text};
use crate::types::{Filter, Hit, Kind};
use chrono::{DateTime, Datelike, Local};
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

pub const WIN_W: f32 = 920.0;
pub const WIN_H: f32 = 580.0;
/// The transparent margin around the panel.
pub const MARGIN: f32 = 10.0;
const PANEL_W: f32 = 900.0;
const PANEL_H: f32 = 560.0;
/// x, y, w, h in panel coordinates.
const LIST: [f32; 4] = [16.0, 104.0, 424.0, 416.0];
const PREVIEW: [f32; 4] = [456.0, 104.0, 428.0, 416.0];
pub const ROW_H: f32 = 56.0;
/// Room below the last row for the "end of results" line.
const END_H: f32 = 36.0;
/// The preview text area, inside the preview pane.
pub const PREVIEW_TEXT: [f32; 4] = [470.0, 118.0, 400.0, 300.0];
const PREVIEW_TEXT_SIZE: f32 = 12.0;
const THUMB: f32 = 44.0;
pub const ERROR_SECS: u64 = 5;
pub const NOTICE_SECS: u64 = 2;

const BG: u32 = 0x15171c;
const BORDER: u32 = 0x2c303a;
const FIELD: u32 = 0x1e2128;
const SELECTED: u32 = 0x28303f;
const TEXT: u32 = 0xe6e8ee;
const DIM: u32 = 0x8a92a0;
const SOFT: u32 = 0xaab1be;
const ACCENT: u32 = 0x6ea8ff;
const ACCENT_BG: u32 = 0x25375a;
const RED: u32 = 0xff5555;

const HINTS: &str =
    "↑↓ move · Enter or click open · right-click folder · Tab filter · ^C copy · Esc close";

/// The key reference for the empty overlay.
const KEYS: [(&str, &str); 13] = [
    ("↑ ↓  PgUp PgDn", "Move"),
    ("Ctrl+Home  Ctrl+End", "First, last result"),
    ("Enter", "Open"),
    ("Ctrl+Enter", "Show in folder"),
    ("Tab  Ctrl+1 … 5", "Filter"),
    ("Ctrl+C  Ctrl+Shift+C", "Copy the path, the file"),
    ("Ctrl+V", "Paste an image or text"),
    ("Ctrl+S", "Search with a screen region"),
    ("Hold Ctrl+Space", "Search by voice"),
    ("Ctrl+A  Ctrl+X  Ctrl+Z", "Select all, cut, undo"),
    ("Ctrl+U  Ctrl+W", "Clear the query, a word"),
    ("Ctrl+=  Ctrl+-  Ctrl+0", "Zoom"),
    ("Esc", "Close"),
];

pub const EXAMPLES: [&str; 4] = [
    "a cat sleeping on a sofa",
    "the invoice from the plumber",
    "notes about the Lisbon trip",
    "function that parses config lines",
];

struct Painter {
    s: f32,
    scene: Scene,
}

impl Painter {
    fn quad(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [f32; 4],
        radius: f32,
        border: Option<(u32, f32)>,
    ) -> Quad {
        let s = self.s;
        let (border_color, border) = border.map_or((color, 0.0), |(c, w)| (lin(c, 1.0), w * s));
        Quad {
            pos: [(x + MARGIN) * s, (y + MARGIN) * s],
            size: [w * s, h * s],
            color,
            border_color,
            radius: radius * s,
            border,
            pad: [0.0; 2],
        }
    }

    fn rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        hex: u32,
        a: f32,
        radius: f32,
        border: Option<(u32, f32)>,
    ) {
        let q = self.quad(x, y, w, h, lin(hex, a), radius, border);
        self.scene.quads.push(q);
    }

    /// `clip` is x0, y0, x1, y1.
    fn image(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        tex: &Rc<Tex>,
        radius: f32,
        clip: [f32; 4],
    ) {
        let q = self.quad(x, y, w, h, [1.0; 4], radius, None);
        let clip = clip.map(|c| (c + MARGIN) * self.s);
        self.scene.images.push((q, tex.clone(), clip));
    }

    /// `clip` is x0, y0, x1, y1.
    fn text(&mut self, text: &str, x: f32, y: f32, size: f32, hex: u32, clip: [f32; 4]) {
        self.text_with(text, x, y, size, hex, clip, None, false);
    }

    fn text_with(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        hex: u32,
        clip: [f32; 4],
        wrap: Option<f32>,
        mono: bool,
    ) {
        let s = self.s;
        self.scene.texts.push(Text {
            text: text.to_string(),
            x: (x + MARGIN) * s,
            y: (y + MARGIN) * s,
            size: size * s,
            color: rgba8(hex, 1.0),
            wrap: wrap.map(|w| w * s),
            mono,
            clip: clip.map(|c| (c + MARGIN) * s),
        });
    }
}

fn width(g: &mut Gfx, s: f32, text: &str, size: f32) -> f32 {
    g.text_width(text, size * s, false) / s
}

const QUERY_SIZE: f32 = 17.0;
/// The search field: x, y, w, h in panel coordinates.
const FIELD_RECT: [f32; 4] = [16.0, 16.0, 868.0, 44.0];

/// The width of a query prefix. A trailing space can have no width in a line layout, so
/// the prefix is measured with a marker after it.
pub fn query_width(g: &mut Gfx, s: f32, prefix: &str) -> f32 {
    if prefix.is_empty() {
        return 0.0;
    }
    width(g, s, &format!("{prefix}|"), QUERY_SIZE) - width(g, s, "|", QUERY_SIZE)
}

/// True if a point in window coordinates is over the search field.
pub fn over_field(x: f32, y: f32) -> bool {
    inside(FIELD_RECT, x, y)
}

/// Converts a window x coordinate to panel coordinates.
pub fn panel_x(x: f32) -> f32 {
    x - MARGIN
}

/// The height of the preview text in logical pixels.
pub fn preview_text_height(g: &mut Gfx, s: f32, text: &str) -> f32 {
    g.text_height(text, PREVIEW_TEXT_SIZE * s, PREVIEW_TEXT[2] * s, true) / s
}

pub fn max_scroll(rows: usize) -> f32 {
    if rows == 0 {
        return 0.0;
    }
    (rows as f32 * ROW_H + END_H - LIST[3]).max(0.0)
}

/// The number of rows that fit in the list.
pub fn page_rows() -> usize {
    (LIST[3] / ROW_H) as usize
}

/// Scroll offset that brings row `i` fully into view.
pub fn scroll_to(i: usize, scroll: f32) -> f32 {
    let top = i as f32 * ROW_H;
    if top < scroll {
        top
    } else if top + ROW_H > scroll + LIST[3] {
        top + ROW_H - LIST[3]
    } else {
        scroll
    }
}

/// The rows that are fully visible at `scroll`.
pub fn full_rows(scroll: f32, rows: usize) -> Range<usize> {
    let first = (scroll / ROW_H).ceil() as usize;
    let end = ((scroll + LIST[3]) / ROW_H).floor() as usize;
    first.min(rows)..end.min(rows)
}

/// The rows that are at least partly visible at `scroll`.
pub fn visible_rows(scroll: f32, rows: usize) -> Range<usize> {
    let first = (scroll / ROW_H).floor() as usize;
    let end = ((scroll + LIST[3]) / ROW_H).ceil() as usize;
    first.min(rows)..end.min(rows)
}

fn inside(r: [f32; 4], x: f32, y: f32) -> bool {
    let (x, y) = (x - MARGIN, y - MARGIN);
    (r[0]..=r[0] + r[2]).contains(&x) && (r[1]..=r[1] + r[3]).contains(&y)
}

/// True if a point in window coordinates is over the preview pane.
pub fn over_preview(x: f32, y: f32) -> bool {
    inside(PREVIEW, x, y)
}

/// The result row under a point in window coordinates.
pub fn row_at(x: f32, y: f32, scroll: f32, rows: usize) -> Option<usize> {
    // The scrollbar zone belongs to the scrollbar only when there is one.
    let bar = over_scrollbar(x, y) && scrollbar(scroll, rows).is_some();
    if !inside(LIST, x, y) || bar {
        return None;
    }
    let i = ((y - MARGIN - LIST[1] + scroll) / ROW_H) as usize;
    (i < rows).then_some(i)
}

/// The grab zone of the scrollbar, wider than the bar so it is easy to hit.
const SCROLLBAR_ZONE: f32 = 14.0;

/// The scrollbar: the track top, the track length, the thumb top and the thumb length.
/// None if the list fits without scrolling.
pub fn scrollbar(scroll: f32, rows: usize) -> Option<[f32; 4]> {
    let max = max_scroll(rows);
    if max <= 0.0 {
        return None;
    }
    let (top, track) = (LIST[1] + 4.0, LIST[3] - 8.0);
    let thumb = (track * LIST[3] / (LIST[3] + max)).max(24.0);
    Some([top, track, top + (track - thumb) * scroll / max, thumb])
}

/// True if a point in window coordinates is over the scrollbar zone.
pub fn over_scrollbar(x: f32, y: f32) -> bool {
    let zone = [
        LIST[0] + LIST[2] - SCROLLBAR_ZONE,
        LIST[1],
        SCROLLBAR_ZONE,
        LIST[3],
    ];
    inside(zone, x, y)
}

/// The scroll offset that puts the thumb top at `thumb_top`, in panel coordinates.
pub fn scroll_for_thumb(thumb_top: f32, rows: usize) -> f32 {
    let Some([top, track, _, thumb]) = scrollbar(0.0, rows) else {
        return 0.0;
    };
    let room = (track - thumb).max(1.0);
    ((thumb_top - top) / room).clamp(0.0, 1.0) * max_scroll(rows)
}

/// Converts a window y coordinate to panel coordinates.
pub fn panel_y(y: f32) -> f32 {
    y - MARGIN
}

fn file_name(hit: &Hit) -> String {
    hit.path.file_name().map_or_else(
        || hit.path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

pub fn home_relative(dir: &Path) -> String {
    let rel =
        std::env::var_os("HOME").and_then(|h| dir.strip_prefix(h).ok().map(Path::to_path_buf));
    match rel {
        Some(r) if r.as_os_str().is_empty() => "~".into(),
        Some(r) => format!("~/{}", r.display()),
        None => dir.display().to_string(),
    }
}

/// A short label and a color for the kind of a hit.
fn tag(hit: &Hit) -> (String, u32) {
    let ext: String = hit
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_default()
        .chars()
        .take(4)
        .collect();
    let or = |fallback: &str| {
        if ext.is_empty() {
            fallback.to_string()
        } else {
            ext.clone()
        }
    };
    match hit.kind {
        Kind::Image => (or("IMG"), 0xc08cff),
        Kind::Audio => (or("AUD"), 0xf0c060),
        Kind::Video => (or("VID"), 0xff8cc0),
        Kind::Pdf => ("PDF".into(), 0xff9060),
        Kind::Text => (or("TXT"), 0x7ed49a),
        Kind::Code => (or("CODE"), ACCENT),
        Kind::Data => (or("DATA"), 0x66d4d4),
        Kind::Other => (or("FILE"), DIM),
    }
}

/// "now", "5 min", "3 h", "4 d", then a date.
fn age(modified: i64) -> String {
    let now = Local::now();
    match now.timestamp() - modified {
        ..60 => "now".into(),
        d @ ..3_600 => format!("{} min", d / 60),
        d @ ..86_400 => format!("{} h", d / 3_600),
        d @ ..604_800 => format!("{} d", d / 86_400),
        _ => match DateTime::from_timestamp(modified, 0).map(|t| t.with_timezone(&Local)) {
            Some(t) if t.year() == now.year() => t.format("%-d %b").to_string(),
            Some(t) => t.format("%b %Y").to_string(),
            None => String::new(),
        },
    }
}

fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What a click on a button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Open,
    Reveal,
    CopyPath,
    CopyFile,
    Clear,
    RemoveImage,
    RemoveVoice,
    /// One of the example queries of the empty overlay.
    Example(usize),
    Filter(usize),
}

/// The clickable areas of the last frame, and where the query text starts.
#[derive(Default)]
pub struct Layout {
    /// x, y, w, h in window coordinates.
    pub buttons: Vec<([f32; 4], Button)>,
    /// The query text start in panel coordinates.
    pub field_x: f32,
}

impl Layout {
    pub fn button_at(&self, x: f32, y: f32) -> Option<Button> {
        self.buttons
            .iter()
            .find(|(r, _)| x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3])
            .map(|&(_, b)| b)
    }

    fn add(&mut self, x: f32, y: f32, w: f32, h: f32, b: Button) {
        self.buttons.push(([x + MARGIN, y + MARGIN, w, h], b));
    }
}

/// The buttons under the preview.
const ACTIONS: [(&str, Button); 4] = [
    ("Open", Button::Open),
    ("Show in folder", Button::Reveal),
    ("Copy path", Button::CopyPath),
    ("Copy file", Button::CopyFile),
];

pub fn build(g: &mut Gfx, st: &State, rec_level: Option<f32>, width_px: u32) -> (Scene, Layout) {
    let s = width_px as f32 / WIN_W;
    let mut p = Painter {
        s,
        scene: Scene::default(),
    };
    let mut layout = Layout::default();
    let hot = |b: Button| st.hover == Some(b);

    p.rect(
        0.0,
        0.0,
        PANEL_W,
        PANEL_H,
        BG,
        1.0,
        14.0,
        Some((BORDER, 1.0)),
    );

    // Search field
    let (fx, fy, fw, fh) = (16.0, 16.0, 868.0, 44.0);
    p.rect(fx, fy, fw, fh, FIELD, 1.0, 10.0, Some((BORDER, 1.0)));
    let mid = fy + fh / 2.0;
    let mut cx = fx + 14.0;
    let mut right = fx + fw - 14.0;
    if let Some(tex) = &st.chip_tex {
        p.image(
            cx,
            mid - 15.0,
            30.0,
            30.0,
            tex,
            6.0,
            [fx, fy, fx + fw, fy + fh],
        );
        layout.add(cx, mid - 15.0, 30.0, 30.0, Button::RemoveImage);
        if hot(Button::RemoveImage) {
            // Images draw after rectangles, so the remove mark is a text glyph on top.
            p.text(
                "×",
                cx + 9.0,
                mid - 12.0,
                18.0,
                RED,
                [fx, fy, fx + fw, fy + fh],
            );
        }
        cx += 40.0;
    }
    if let Some(audio) = &st.audio {
        let hover = hot(Button::RemoveVoice);
        let label = if hover {
            "remove voice ×".to_string()
        } else {
            format!("voice {:.1}s", audio.len() as f32 / 16_000.0)
        };
        let w = width(g, s, &label, 12.0) + 18.0;
        p.rect(cx, mid - 11.0, w, 22.0, ACCENT_BG, 1.0, 11.0, None);
        p.text(
            &label,
            cx + 9.0,
            mid - 7.5,
            12.0,
            if hover { RED } else { ACCENT },
            [cx, mid - 11.0, cx + w, mid + 11.0],
        );
        layout.add(cx, mid - 11.0, w, 22.0, Button::RemoveVoice);
        cx += w + 10.0;
    }
    if !st.query.is_empty() && rec_level.is_none() {
        let (bx, d) = (right - 24.0, 24.0);
        let bg = if hot(Button::Clear) { SELECTED } else { FIELD };
        p.rect(bx, mid - d / 2.0, d, d, bg, 1.0, d / 2.0, None);
        let xw = width(g, s, "×", 17.0);
        let color = if hot(Button::Clear) { TEXT } else { DIM };
        p.text(
            "×",
            bx + (d - xw) / 2.0,
            mid - 11.5,
            17.0,
            color,
            [bx, fy, bx + d, fy + fh],
        );
        layout.add(bx, mid - d / 2.0, d, d, Button::Clear);
        right = bx - 8.0;
    }
    if let Some(level) = rec_level {
        let bar_w = 170.0;
        let bx = right - bar_w;
        p.rect(bx, mid - 4.0, bar_w, 8.0, BORDER, 1.0, 4.0, None);
        p.rect(
            bx,
            mid - 4.0,
            (bar_w * (level * 6.0).min(1.0)).max(8.0),
            8.0,
            RED,
            1.0,
            4.0,
            None,
        );
        p.rect(bx - 34.0, mid - 4.0, 8.0, 8.0, RED, 1.0, 4.0, None);
        p.text(
            "rec",
            bx - 22.0,
            mid - 7.5,
            12.0,
            RED,
            [bx - 24.0, mid - 11.0, bx, mid + 11.0],
        );
        right = bx - 44.0;
    }
    let qy = mid - 10.5;
    let clip = [cx, fy, right, fy + fh];
    let q = &st.query;
    let mut field_x = cx;
    if q.is_empty() {
        if st.chip_tex.is_none() && st.audio.is_none() {
            p.text(
                "Search files, images, audio and video",
                cx,
                qy,
                QUERY_SIZE,
                DIM,
                clip,
            );
        }
        p.rect(cx, mid - 10.0, 2.0, 20.0, ACCENT, 1.0, 1.0, None);
    } else {
        // The text scrolls left only as far as the caret needs.
        let caret_x = query_width(g, s, &q.text[..q.caret]);
        let shift = (caret_x + 3.0 - (right - cx)).max(0.0);
        field_x = cx - shift;
        let sel = q.selection();
        if !sel.is_empty() {
            let x0 = (field_x + query_width(g, s, &q.text[..sel.start])).max(cx);
            let x1 = (field_x + query_width(g, s, &q.text[..sel.end])).min(right);
            if x1 > x0 {
                p.rect(x0, mid - 12.0, x1 - x0, 24.0, ACCENT_BG, 1.0, 3.0, None);
            }
        }
        p.text(&q.text, field_x, qy, QUERY_SIZE, TEXT, clip);
        p.rect(
            field_x + caret_x,
            mid - 10.0,
            2.0,
            20.0,
            ACCENT,
            1.0,
            1.0,
            None,
        );
    }

    // Filter chips
    let mut x = 16.0;
    for (i, f) in Filter::ALL.into_iter().enumerate() {
        let label = f.label();
        let w = width(g, s, label, 12.5) + 26.0;
        let (bg, border, fg) = if f == st.filter {
            (ACCENT_BG, ACCENT, ACCENT)
        } else if hot(Button::Filter(i)) {
            (SELECTED, DIM, TEXT)
        } else {
            (FIELD, BORDER, DIM)
        };
        p.rect(x, 68.0, w, 26.0, bg, 1.0, 13.0, Some((border, 1.0)));
        p.text(label, x + 13.0, 73.0, 12.5, fg, [x, 68.0, x + w, 94.0]);
        layout.add(x, 68.0, w, 26.0, Button::Filter(i));
        x += w + 8.0;
    }
    let count = match (st.pending, st.hits.len()) {
        (true, 0) => "searching…".to_string(),
        (true, n) => format!("{} results · searching…", group(n)),
        (false, 1) => "1 result".to_string(),
        (false, n) => format!("{} results", group(n)),
    };
    let cw = width(g, s, &count, 12.0);
    p.text(&count, 884.0 - cw, 74.0, 12.0, DIM, [x, 68.0, 884.0, 94.0]);

    let [px, py, pw, ph] = PREVIEW;
    p.rect(px, py, pw, ph, FIELD, 1.0, 10.0, Some((BORDER, 1.0)));
    let empty = q.is_empty() && st.chip_tex.is_none() && st.audio.is_none();
    if empty && st.hits.is_empty() {
        empty_state(&mut p, st, &mut layout);
    } else {
        list(g, &mut p, st);
        preview(g, &mut p, st, &mut layout);
    }

    // Footer
    let fy = 531.0;
    let hw = width(g, s, HINTS, 11.0);
    p.text(HINTS, 16.0, fy, 11.0, DIM, [16.0, fy, 884.0, PANEL_H]);
    let fresh = |m: &Option<(String, std::time::Instant)>, secs| {
        m.as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(secs))
            .map(|(m, _)| m.clone())
    };
    let (status, color) = if let Some(msg) = fresh(&st.error, ERROR_SECS) {
        (msg, RED)
    } else if let Some(msg) = fresh(&st.notice, NOTICE_SECS) {
        (msg, ACCENT)
    } else {
        let t = &st.status;
        let line = format!(
            "{} / {} · {} · {}",
            group(t.indexed),
            group(t.total),
            t.phase,
            t.model
        );
        (line, DIM)
    };
    let avail = (884.0 - 16.0 - hw - 24.0).max(0.0);
    let sw = width(g, s, &status, 11.0).min(avail);
    let sx = 884.0 - sw;
    p.text(&status, sx, fy, 11.0, color, [sx, fy, 884.0, PANEL_H]);

    layout.field_x = field_x;
    (p.scene, layout)
}

fn empty_state(p: &mut Painter, st: &State, layout: &mut Layout) {
    let [lx, ly, lw, lh] = LIST;
    let clip = [lx, ly, lx + lw, ly + lh];
    p.text(
        "Search by name, by text inside, or by meaning.",
        lx + 12.0,
        ly + 12.0,
        14.0,
        TEXT,
        clip,
    );
    p.text(
        "Describe what a file shows or says. For example:",
        lx + 12.0,
        ly + 36.0,
        12.0,
        DIM,
        clip,
    );
    for (i, ex) in EXAMPLES.iter().enumerate() {
        let y = ly + 64.0 + i as f32 * 26.0;
        let hot = st.hover == Some(Button::Example(i));
        if hot {
            p.rect(lx + 4.0, y - 4.0, lw - 8.0, 24.0, SELECTED, 1.0, 6.0, None);
        }
        p.rect(lx + 12.0, y, 3.0, 16.0, ACCENT, 0.6, 1.5, None);
        p.text(ex, lx + 24.0, y, 13.0, if hot { TEXT } else { SOFT }, clip);
        layout.add(lx + 4.0, y - 4.0, lw - 8.0, 24.0, Button::Example(i));
    }
    p.text(
        "Paste or capture an image, or hold Ctrl+Space and speak.",
        lx + 12.0,
        ly + 182.0,
        12.0,
        DIM,
        clip,
    );

    let [px, py, pw, ph] = PREVIEW;
    let clip = [px, py, px + pw, py + ph];
    p.text("Keys", px + 16.0, py + 14.0, 13.0, TEXT, clip);
    for (i, (key, action)) in KEYS.iter().enumerate() {
        let y = py + 42.0 + i as f32 * 25.0;
        p.text(key, px + 16.0, y, 12.0, ACCENT, clip);
        p.text(action, px + 196.0, y, 12.0, SOFT, clip);
    }
}

fn list(g: &mut Gfx, p: &mut Painter, st: &State) {
    let s = p.s;
    let [lx, ly, lw, lh] = LIST;
    let list_clip = [lx, ly, lx + lw, ly + lh];
    if st.hits.is_empty() && !st.pending {
        p.text("No results", lx + 12.0, ly + 12.0, 14.0, DIM, list_clip);
        return;
    }
    let row_clip = [lx, ly, lx + lw - 14.0, ly + lh];
    for i in visible_rows(st.scroll, st.hits.len()) {
        let hit = &st.hits[i];
        let y = ly + i as f32 * ROW_H - st.scroll;
        // Clamped by hand: plain rectangles have no clip.
        let band = |top: f32, h: f32| {
            let (t, b) = (top.max(ly), (top + h).min(ly + lh));
            (t < b).then_some((t, b - t))
        };
        if i == st.sel {
            if let Some((t, h)) = band(y, ROW_H - 2.0) {
                p.rect(lx, t, lw - 8.0, h, SELECTED, 1.0, 8.0, None);
            }
        }

        // A thumbnail for images and videos, else a kind badge.
        let (label, color) = tag(hit);
        let thumb = matches!(hit.kind, Kind::Image | Kind::Video)
            .then(|| st.thumbs.get(&hit.path))
            .flatten();
        if let Some(t) = thumb {
            let (iw, ih) = (t.dims.0.max(1) as f32, t.dims.1.max(1) as f32);
            let k = (THUMB / iw).min(THUMB / ih);
            let (w, h) = (iw * k, ih * k);
            let (tx, ty) = (lx + 8.0 + (THUMB - w) / 2.0, y + 5.0 + (THUMB - h) / 2.0);
            p.image(tx, ty, w, h, &t.tex, 5.0, list_clip);
        } else {
            let bw = 44.0;
            if let Some((t, h)) = band(y + 18.0, 20.0) {
                p.rect(lx + 8.0, t, bw, h, color, 0.14, 5.0, None);
            }
            let tw = width(g, s, &label, 10.0);
            p.text(
                &label,
                lx + 8.0 + (bw - tw) / 2.0,
                y + 21.5,
                10.0,
                color,
                list_clip,
            );
        }

        let tx = lx + 64.0;
        let mut name_right = row_clip[2];
        if let Some(m) = hit.modified {
            let when = age(m);
            let ww = width(g, s, &when, 11.0);
            let wx = lx + lw - 20.0 - ww;
            p.text(&when, wx, y + 9.0, 11.0, DIM, row_clip);
            name_right = wx - 10.0;
        }
        p.text(
            &file_name(hit),
            tx,
            y + 7.0,
            14.0,
            TEXT,
            [lx, ly, name_right, ly + lh],
        );
        p.text(
            &home_relative(hit.path.parent().unwrap_or(&hit.path)),
            tx,
            y + 27.0,
            11.0,
            DIM,
            row_clip,
        );
        if let Some(snippet) = &hit.snippet {
            let line = snippet
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            p.text(line, tx, y + 40.0, 11.0, SOFT, row_clip);
        }
    }

    let n = st.hits.len();
    if !st.pending && n > page_rows() {
        let y = ly + n as f32 * ROW_H - st.scroll + 10.0;
        let label = format!("End of results · {}", group(n));
        let w = width(g, s, &label, 11.5);
        p.text(&label, lx + (lw - w) / 2.0, y, 11.5, DIM, list_clip);
    }

    if let Some([top, track, at, thumb]) = scrollbar(st.scroll, n) {
        // The bar widens while the pointer is over it or drags it.
        let (w, a) = if st.scrollbar_hot {
            (6.0, 0.9)
        } else {
            (4.0, 0.5)
        };
        if st.scrollbar_hot {
            p.rect(lx + lw - 8.0, top, 6.0, track, BORDER, 0.6, 3.0, None);
        }
        p.rect(lx + lw - 2.0 - w, at, w, thumb, DIM, a, w / 2.0, None);
    }
}

fn preview(g: &mut Gfx, p: &mut Painter, st: &State, layout: &mut Layout) {
    let [px, py, pw, ph] = PREVIEW;
    let (Some(pv), Some(hit)) = (&st.preview, st.hits.get(st.sel)) else {
        return;
    };
    let [ax, ay, aw, ah] = PREVIEW_TEXT;
    let area = [ax, ay, ax + aw, ay + ah];
    // A click on the preview opens the file, like a click on its row.
    layout.add(ax, ay, aw, ah, Button::Open);
    if let Some(tex) = &pv.tex {
        let (iw, ih) = (pv.dims.0.max(1) as f32, pv.dims.1.max(1) as f32);
        let k = (aw / iw).min(ah / ih);
        let (w, h) = (iw * k, ih * k);
        p.image(
            ax + (aw - w) / 2.0,
            ay + (ah - h) / 2.0,
            w,
            h,
            tex,
            6.0,
            area,
        );
    } else if let Some(text) = &pv.text {
        p.text_with(
            text,
            ax,
            ay - st.preview_scroll,
            PREVIEW_TEXT_SIZE,
            0xcfd3dc,
            area,
            Some(aw),
            true,
        );
    } else {
        let (label, color) = tag(hit);
        p.text(&label, ax, ay, 22.0, color, area);
        if matches!(hit.kind, Kind::Image | Kind::Video) {
            p.text("loading preview…", ax, ay + 32.0, 13.0, DIM, area);
        }
    }

    let my = ay + ah + 12.0;
    p.rect(px + 14.0, my - 6.0, pw - 28.0, 1.0, BORDER, 1.0, 0.0, None);
    let mclip = [px + 14.0, my, px + pw - 14.0, py + ph];
    p.text(&pv.meta, px + 14.0, my + 2.0, 11.5, SOFT, mclip);
    p.text(
        &home_relative(&hit.path),
        px + 14.0,
        my + 20.0,
        11.0,
        DIM,
        mclip,
    );

    // The action bar.
    let (by, bh) = (py + ph - 40.0, 28.0);
    let mut x = px + 14.0;
    for (i, (label, b)) in ACTIONS.iter().enumerate() {
        let w = width(g, p.s, label, 12.0) + 24.0;
        let (bg, border, fg) = match (i == 0, st.hover == Some(*b)) {
            (true, true) => (0x2f4570, ACCENT, TEXT),
            (true, false) => (ACCENT_BG, ACCENT, ACCENT),
            (false, true) => (SELECTED, DIM, TEXT),
            (false, false) => (BG, BORDER, SOFT),
        };
        p.rect(x, by, w, bh, bg, 1.0, 7.0, Some((border, 1.0)));
        p.text(label, x + 12.0, by + 6.5, 12.0, fg, [x, by, x + w, by + bh]);
        layout.add(x, by, w, bh, *b);
        x += w + 8.0;
    }
}
