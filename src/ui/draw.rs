//! Lays out the overlay and builds the scene. All numbers here are logical pixels.

use super::app::State;
use super::gfx::{lin, rgba8, Gfx, Quad, Scene, Tex, Text};
use crate::types::{Filter, Hit, Kind};
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
const ROW_H: f32 = 56.0;
pub const ERROR_SECS: u64 = 5;

const BG: u32 = 0x15171c;
const BORDER: u32 = 0x2c303a;
const FIELD: u32 = 0x1e2128;
const SELECTED: u32 = 0x28303f;
const TEXT: u32 = 0xe6e8ee;
const DIM: u32 = 0x8a92a0;
const ACCENT: u32 = 0x6ea8ff;
const ACCENT_BG: u32 = 0x25375a;
const RED: u32 = 0xff5555;

const HINTS: &str = "↑↓ move · Tab filter · Enter open · ^Enter folder · ^C copy · ^V paste · ^S region · hold ^Space voice · Esc close";

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

    fn image(&mut self, x: f32, y: f32, w: f32, h: f32, tex: &Rc<Tex>, radius: f32) {
        let q = self.quad(x, y, w, h, [1.0; 4], radius, None);
        self.scene.images.push((q, tex.clone()));
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
            clip: [
                (clip[0] + MARGIN) * s,
                (clip[1] + MARGIN) * s,
                (clip[2] + MARGIN) * s,
                (clip[3] + MARGIN) * s,
            ],
        });
    }
}

fn width(g: &mut Gfx, s: f32, text: &str, size: f32) -> f32 {
    g.text_width(text, size * s, false) / s
}

pub fn max_scroll(rows: usize) -> f32 {
    (rows as f32 * ROW_H - LIST[3]).max(0.0)
}

/// Scroll offset that brings row `i` into view.
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

/// The result row under a point in window coordinates.
pub fn row_at(x: f32, y: f32, scroll: f32, rows: usize) -> Option<usize> {
    let (x, y) = (x - MARGIN, y - MARGIN);
    if !(LIST[0]..=LIST[0] + LIST[2]).contains(&x) || !(LIST[1]..=LIST[1] + LIST[3]).contains(&y) {
        return None;
    }
    let i = ((y - LIST[1] + scroll) / ROW_H) as usize;
    (i < rows).then_some(i)
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

fn tag(hit: &Hit) -> (String, u32) {
    let ext = hit
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match hit.kind {
        Kind::Image => ("[img]".into(), 0xc08cff),
        Kind::Audio => ("[aud]".into(), 0xf0c060),
        Kind::Video => ("[vid]".into(), 0xff8cc0),
        Kind::Pdf => ("[pdf]".into(), 0xff9060),
        Kind::Text if ext == "md" => ("[md]".into(), 0x7ed49a),
        Kind::Text => ("[txt]".into(), 0x7ed49a),
        Kind::Code if !ext.is_empty() => (
            format!("[{}]", ext.chars().take(4).collect::<String>()),
            0x6ea8ff,
        ),
        Kind::Code => ("[code]".into(), 0x6ea8ff),
        Kind::Data => ("[dat]".into(), 0x66d4d4),
        Kind::Other => ("[file]".into(), DIM),
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

/// Returns the scene and the filter chip rectangles in window coordinates.
pub fn build(
    g: &mut Gfx,
    st: &State,
    rec_level: Option<f32>,
    width_px: u32,
) -> (Scene, Vec<[f32; 4]>) {
    let s = width_px as f32 / WIN_W;
    let mut p = Painter {
        s,
        scene: Scene::default(),
    };
    let mut chip_rects = Vec::new();

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
        p.image(cx, mid - 15.0, 30.0, 30.0, tex, 6.0);
        cx += 40.0;
    }
    if let Some(audio) = &st.audio {
        let label = format!("voice {:.1}s", audio.len() as f32 / 16_000.0);
        let w = width(g, s, &label, 12.0) + 18.0;
        p.rect(cx, mid - 11.0, w, 22.0, ACCENT_BG, 1.0, 11.0, None);
        p.text(
            &label,
            cx + 9.0,
            mid - 7.5,
            12.0,
            ACCENT,
            [cx, mid - 11.0, cx + w, mid + 11.0],
        );
        cx += w + 10.0;
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
    if st.query.is_empty() {
        if st.chip_tex.is_none() && st.audio.is_none() {
            p.text(
                "Search files, images, audio and video",
                cx,
                qy,
                17.0,
                DIM,
                clip,
            );
        }
        p.rect(cx, mid - 10.0, 2.0, 20.0, ACCENT, 1.0, 1.0, None);
    } else {
        let qw = width(g, s, &st.query, 17.0);
        let shift = (qw + 3.0 - (right - cx)).max(0.0);
        p.text(&st.query, cx - shift, qy, 17.0, TEXT, clip);
        p.rect(
            cx - shift + qw + 1.0,
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
    for f in Filter::ALL {
        let label = f.label();
        let w = width(g, s, label, 12.5) + 26.0;
        let active = f == st.filter;
        let (bg, border, fg) = if active {
            (ACCENT_BG, ACCENT, ACCENT)
        } else {
            (FIELD, BORDER, DIM)
        };
        p.rect(x, 68.0, w, 26.0, bg, 1.0, 13.0, Some((border, 1.0)));
        p.text(label, x + 13.0, 73.0, 12.5, fg, [x, 68.0, x + w, 94.0]);
        chip_rects.push([x + MARGIN, 68.0 + MARGIN, w, 26.0]);
        x += w + 8.0;
    }
    let count = if st.pending && st.hits.is_empty() {
        "searching…".to_string()
    } else if st.pending {
        format!("{} results · searching…", st.hits.len())
    } else {
        format!("{} results", st.hits.len())
    };
    let cw = width(g, s, &count, 12.0);
    p.text(&count, 884.0 - cw, 74.0, 12.0, DIM, [x, 68.0, 884.0, 94.0]);

    // Result list
    let [lx, ly, lw, lh] = LIST;
    let list_clip = [lx, ly, lx + lw, ly + lh];
    if st.hits.is_empty() && !st.pending {
        p.text("No results", lx + 12.0, ly + 12.0, 14.0, DIM, list_clip);
    }
    let first = (st.scroll / ROW_H) as usize;
    for (i, hit) in st.hits.iter().enumerate().skip(first) {
        let y = ly + i as f32 * ROW_H - st.scroll;
        if y > ly + lh {
            break;
        }
        if i == st.sel {
            let (top, bottom) = (y.max(ly), (y + ROW_H - 2.0).min(ly + lh));
            p.rect(lx, top, lw - 8.0, bottom - top, SELECTED, 1.0, 8.0, None);
        }
        let clip = [lx, ly, lx + lw - 14.0, ly + lh];
        let (tag, tag_color) = tag(hit);
        p.text(&tag, lx + 10.0, y + 9.0, 11.0, tag_color, clip);
        let tx = lx + 58.0;
        p.text(&file_name(hit), tx, y + 7.0, 14.0, TEXT, clip);
        p.text(
            &home_relative(hit.path.parent().unwrap_or(&hit.path)),
            tx,
            y + 27.0,
            11.0,
            DIM,
            clip,
        );
        if let Some(snippet) = &hit.snippet {
            let line = snippet
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            p.text(line, tx, y + 40.0, 11.0, 0xaab1be, clip);
        }
    }
    let max = max_scroll(st.hits.len());
    if max > 0.0 {
        let track = lh - 8.0;
        let thumb = (track * lh / (lh + max)).max(24.0);
        let at = ly + 4.0 + (track - thumb) * st.scroll / max;
        p.rect(lx + lw - 5.0, at, 3.0, thumb, DIM, 0.5, 1.5, None);
    }

    // Preview pane
    let [px, py, pw, ph] = PREVIEW;
    p.rect(px, py, pw, ph, FIELD, 1.0, 10.0, Some((BORDER, 1.0)));
    if let (Some(pv), Some(hit)) = (&st.preview, st.hits.get(st.sel)) {
        let (ax, ay, aw, ah) = (px + 14.0, py + 14.0, pw - 28.0, ph - 28.0 - 50.0);
        let area = [ax, ay, ax + aw, ay + ah];
        if let Some(tex) = &pv.tex {
            let (iw, ih) = (pv.dims.0 as f32, pv.dims.1 as f32);
            let k = (aw / iw).min(ah / ih);
            let (w, h) = (iw * k, ih * k);
            p.image(ax + (aw - w) / 2.0, ay + (ah - h) / 2.0, w, h, tex, 6.0);
        } else if let Some(text) = &pv.text {
            p.text_with(text, ax, ay, 12.0, 0xcfd3dc, area, Some(aw), true);
        } else {
            let (label, color) = tag(hit);
            p.text(&label, ax, ay, 22.0, color, area);
            if matches!(hit.kind, Kind::Image | Kind::Video) {
                p.text("loading preview…", ax, ay + 32.0, 13.0, DIM, area);
            }
        }
        let my = py + ph - 50.0;
        p.rect(px + 14.0, my - 6.0, pw - 28.0, 1.0, BORDER, 1.0, 0.0, None);
        let mclip = [px + 14.0, my, px + pw - 14.0, py + ph];
        p.text(&pv.meta, px + 14.0, my + 2.0, 11.5, 0xaab1be, mclip);
        p.text(
            &home_relative(&hit.path),
            px + 14.0,
            my + 22.0,
            11.0,
            DIM,
            mclip,
        );
    }

    // Footer
    let fy = 531.0;
    let hw = width(g, s, HINTS, 11.0);
    p.text(HINTS, 16.0, fy, 11.0, DIM, [16.0, fy, 884.0, PANEL_H]);
    let error = st
        .error
        .as_ref()
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(ERROR_SECS));
    let status = if let Some((msg, _)) = error {
        msg.clone()
    } else {
        let t = &st.status;
        format!(
            "{} / {} · {} · {}",
            group(t.indexed),
            group(t.total),
            t.phase,
            t.model
        )
    };
    let avail = (884.0 - 16.0 - hw - 24.0).max(0.0);
    let sw = width(g, s, &status, 11.0).min(avail);
    let sx = 884.0 - sw;
    let color = if error.is_some() { RED } else { DIM };
    p.text(&status, sx, fy, 11.0, color, [sx, fy, 884.0, PANEL_H]);

    (p.scene, chip_rects)
}
