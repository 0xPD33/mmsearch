//! The winit app: window lifetime, input handling and the requests to the daemon.

use super::draw::{self, ERROR_SECS, NOTICE_SECS, PREVIEW_TEXT, ROW_H, WIN_H, WIN_W};
use super::field::Field;
use super::gfx::{Gfx, Target, Tex};
use super::voice::Recorder;
use super::{UiEvent, UiRequest, UiSender};
use crate::thumbs;
use crate::types::{Filter, Hit, Kind, Status};
use anyhow::Result;
use chrono::{DateTime, Local};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::cursor::{Cursor, CursorIcon};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::wayland::{KeyboardInteractivity, Layer, WindowAttributesWayland};
use winit::window::{WindowAttributes, WindowId};

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const PREVIEW_EDGE: u32 = 1280;
const TEXT_PREVIEW_BYTES: usize = 4000;
const CHIP_EDGE: u32 = 1536;
/// Twice the row thumbnail size, for HiDPI outputs.
const ROW_THUMB_EDGE: u32 = 96;
/// Row thumbnails kept on the GPU, at about 37 KB each.
const MAX_ROW_THUMBS: usize = 1500;
/// Rows per mouse wheel notch.
const WHEEL_ROWS: f32 = 2.0;
/// The time constant of the scroll animation.
const SCROLL_EASE: f32 = 0.045;
/// The overlay size relative to the 920 x 580 layout, and the Ctrl+= / Ctrl+- limits.
const DEFAULT_ZOOM: f32 = 1.2;
const ZOOM_RANGE: (f32, f32) = (0.8, 1.8);
/// Image files above this size go to the clipboard as a file link, not as image data.
const MAX_COPY_BYTES: u64 = 50 << 20;

pub struct Thumb {
    pub tex: Rc<Tex>,
    pub dims: (u32, u32),
}

pub struct Preview {
    pub path: PathBuf,
    pub tex: Option<Rc<Tex>>,
    pub dims: (u32, u32),
    pub text: Option<String>,
    pub meta: String,
}

#[derive(Default)]
pub struct State {
    pub query: Field,
    pub chip: Option<Arc<image::RgbImage>>,
    pub chip_tex: Option<Rc<Tex>>,
    pub audio: Option<Vec<f32>>,
    pub filter: Filter,
    pub hits: Vec<Hit>,
    pub sel: usize,
    pub scroll: f32,
    pub search_id: u64,
    pub pending: bool,
    pub status: Status,
    pub error: Option<(String, Instant)>,
    /// A confirmation for the status line, such as "Copied the path".
    pub notice: Option<(String, Instant)>,
    pub preview: Option<Preview>,
    pub preview_scroll: f32,
    /// The pointer is over the scrollbar, or drags it.
    pub scrollbar_hot: bool,
    /// The button under the pointer.
    pub hover: Option<draw::Button>,
    /// Row thumbnails of images and videos, by path.
    pub thumbs: HashMap<PathBuf, Thumb>,
}

struct Loaded {
    path: PathBuf,
    image: Option<image::RgbaImage>,
    text: Option<String>,
}

enum Msg {
    Preview(Loaded),
    Thumb(PathBuf, Option<image::RgbaImage>),
    PastedImage(image::RgbImage),
    PastedText(String),
    Error(String),
}

/// Sends a message from a worker thread and wakes the event loop.
#[derive(Clone)]
struct Wake {
    tx: Sender<Msg>,
    proxy: EventLoopProxy,
}

impl Wake {
    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        self.proxy.wake_up();
    }
}

struct App {
    requests: Sender<UiRequest>,
    /// Preview loads for the one worker thread.
    previews: Sender<(PathBuf, Kind, bool)>,
    /// Row thumbnail loads, and the paths already asked for.
    thumb_jobs: Sender<(PathBuf, Kind)>,
    thumbs_asked: HashSet<PathBuf>,
    /// `st.scroll` eases toward this value.
    scroll_target: f32,
    last_frame: Option<Instant>,
    /// The pointer position in logical window coordinates.
    pointer: Option<(f32, f32)>,
    /// True once the user moves the selection; new results then keep the selected file.
    moved: bool,
    /// While the scrollbar is dragged: where the pointer holds the thumb, from its top.
    drag: Option<f32>,
    /// The size of the whole overlay; Ctrl+= and Ctrl+- change it until the daemon restarts.
    zoom: f32,
    /// A press in the search field selects text while the pointer moves.
    field_drag: bool,
    /// The pointer shape last set on the window.
    cursor: CursorIcon,
    last_field_click: Option<Instant>,
    events: Receiver<UiEvent>,
    msgs: Receiver<Msg>,
    wake: Wake,
    gfx: Option<Gfx>,
    open: Option<Target>,
    st: State,
    mods: ModifiersState,
    /// True when the next open starts with an empty query (not after Ctrl+S).
    fresh: bool,
    rec: Option<Recorder>,
    /// The clickable areas of the last frame.
    layout: draw::Layout,
    opened_at: Option<Instant>,
}

pub fn run(requests: Sender<UiRequest>, ready: impl FnOnce(UiSender)) -> Result<()> {
    let event_loop = EventLoop::new()?;
    let proxy = event_loop.create_proxy();
    let (tx, events) = channel();
    ready(UiSender::new(tx, proxy.clone()));
    let (mtx, msgs) = channel();
    let wake = Wake { tx: mtx, proxy };
    let previews = preview_worker(wake.clone());
    let thumb_jobs = thumb_worker(wake.clone());
    let app = App {
        requests,
        events,
        msgs,
        wake,
        previews,
        thumb_jobs,
        thumbs_asked: HashSet::new(),
        scroll_target: 0.0,
        last_frame: None,
        pointer: None,
        moved: false,
        drag: None,
        zoom: DEFAULT_ZOOM,
        field_drag: false,
        cursor: CursorIcon::Default,
        last_field_click: None,
        gfx: None,
        open: None,
        st: State::default(),
        mods: ModifiersState::default(),
        fresh: true,
        rec: None,
        layout: draw::Layout::default(),
        opened_at: None,
    };
    event_loop.run_app(app)?;
    Ok(())
}

fn spawn_command(mut cmd: Command, stdin: Option<Vec<u8>>, wake: Wake) {
    // The Nix wrapper sets LD_LIBRARY_PATH for mmsearch; opened apps must not inherit it.
    cmd.env_remove("LD_LIBRARY_PATH");
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    std::thread::spawn(move || match cmd.spawn() {
        Ok(mut child) => {
            if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
                let _ = pipe.write_all(&data);
            }
            let _ = child.wait();
        }
        Err(e) => wake.send(Msg::Error(format!(
            "{}: {e}",
            cmd.get_program().to_string_lossy()
        ))),
    });
}

fn human_size(n: u64) -> String {
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < 4 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", ["B", "KB", "MB", "GB", "TB"][unit])
    }
}

fn meta_line(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => {
            let when = m.modified().map(|t| {
                DateTime::<Local>::from(t)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            });
            format!(
                "{} · modified {}",
                human_size(m.len()),
                when.unwrap_or_else(|_| "unknown".into())
            )
        }
        Err(_) => "file not found".into(),
    }
}

fn load_preview(path: PathBuf, kind: Kind, want_text: bool) -> Loaded {
    let mut out = Loaded {
        path,
        image: None,
        text: None,
    };
    match kind {
        Kind::Image => {
            out.image = thumbs::open_image(&out.path)
                .ok()
                .map(|i| i.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE).to_rgba8());
        }
        Kind::Video => out.image = thumbs::load(&out.path),
        _ if want_text => {
            use std::io::Read;
            let mut buf = Vec::new();
            if let Ok(f) = std::fs::File::open(&out.path) {
                let _ = f.take(TEXT_PREVIEW_BYTES as u64).read_to_end(&mut buf);
            }
            // Binary files show no text.
            if !buf.is_empty() && !buf.contains(&0) {
                out.text = Some(String::from_utf8_lossy(&buf).into_owned());
            }
        }
        _ => {}
    }
    out
}

/// One thread loads previews and skips to the newest request, so holding an arrow key over
/// large photos never queues a decode per step. A photo shows its cached thumbnail first.
fn preview_worker(wake: Wake) -> Sender<(PathBuf, Kind, bool)> {
    let (tx, rx) = channel::<(PathBuf, Kind, bool)>();
    std::thread::spawn(move || {
        let mut next = None;
        loop {
            let mut job = match next.take() {
                Some(j) => j,
                None => match rx.recv() {
                    Ok(j) => j,
                    Err(_) => return,
                },
            };
            while let Ok(newer) = rx.try_recv() {
                job = newer;
            }
            let (path, kind, want_text) = job;
            if kind == Kind::Image {
                if let Some(small) = thumbs::load(&path) {
                    wake.send(Msg::Preview(Loaded {
                        path: path.clone(),
                        image: Some(small),
                        text: None,
                    }));
                    // The selection moved on: the full decode of this photo is wasted work.
                    if let Ok(newer) = rx.try_recv() {
                        next = Some(newer);
                        continue;
                    }
                }
            }
            wake.send(Msg::Preview(load_preview(path, kind, want_text)));
        }
    });
    tx
}

/// Loads row thumbnails, newest request first: those are the rows on screen now.
fn thumb_worker(wake: Wake) -> Sender<(PathBuf, Kind)> {
    let (tx, rx) = channel::<(PathBuf, Kind)>();
    std::thread::spawn(move || {
        let mut queue = Vec::new();
        loop {
            if queue.is_empty() {
                match rx.recv() {
                    Ok(j) => queue.push(j),
                    Err(_) => return,
                }
            }
            queue.extend(rx.try_iter());
            let Some((path, kind)) = queue.pop() else {
                continue;
            };
            let img = thumbs::load(&path)
                .map(image::DynamicImage::ImageRgba8)
                .or_else(|| {
                    // Not indexed yet: make the thumbnail now and share it through the cache.
                    let img = (kind == Kind::Image).then(|| thumbs::open_image(&path).ok())??;
                    let _ = thumbs::store(&path, &img);
                    Some(img)
                });
            let small = img.map(|i| i.thumbnail(ROW_THUMB_EDGE, ROW_THUMB_EDGE).to_rgba8());
            wake.send(Msg::Thumb(path, small));
        }
    });
    tx
}

impl App {
    fn redraw(&self) {
        if let Some(t) = &self.open {
            t.window.request_redraw();
        }
    }

    fn fail(&mut self, msg: String) {
        tracing::warn!("{msg}");
        self.st.error = Some((msg, Instant::now()));
        self.redraw();
    }

    fn selected(&self) -> Option<&Hit> {
        self.st.hits.get(self.st.sel)
    }

    fn open_window(&mut self, el: &dyn ActiveEventLoop) {
        if self.open.is_some() {
            return;
        }
        self.opened_at = Some(Instant::now());
        if self.fresh {
            let status = std::mem::take(&mut self.st.status);
            let error = self.st.error.take();
            let thumbs = std::mem::take(&mut self.st.thumbs);
            // A thumbnail that was missing, say for a video not indexed yet, gets another try.
            self.thumbs_asked.retain(|p| thumbs.contains_key(p));
            let id = self.st.search_id;
            self.st = State {
                status,
                error,
                thumbs,
                search_id: id,
                ..State::default()
            };
            self.scroll_target = 0.0;
            self.moved = false;
            self.fresh = false;
        }
        if self.gfx.is_none() {
            match Gfx::new() {
                Ok(g) => self.gfx = Some(g),
                Err(e) => return self.fail(format!("GPU: {e:#}")),
            }
        }
        let keyboard = if std::env::var("MMSEARCH_NO_GRAB").is_ok_and(|v| v == "1") {
            KeyboardInteractivity::None
        } else {
            KeyboardInteractivity::Exclusive
        };
        let wayland = WindowAttributesWayland::default()
            .with_layer_shell()
            .with_layer(Layer::Overlay)
            .with_exclusive_zone(-1)
            .with_keyboard_interactivity(keyboard);
        let attrs = WindowAttributes::default()
            .with_title("mmsearch")
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_surface_size(LogicalSize::new(
                (WIN_W * self.zoom) as f64,
                (WIN_H * self.zoom) as f64,
            ))
            .with_platform_attributes(Box::new(wayland));
        let window = match el.create_window(attrs) {
            Ok(w) => std::sync::Arc::from(w),
            Err(e) => return self.fail(format!("window: {e}")),
        };
        match self
            .gfx
            .as_ref()
            .expect("gfx was created above")
            .target(window)
        {
            Ok(t) => {
                t.window.request_redraw();
                self.open = Some(t);
            }
            Err(e) => return self.fail(format!("surface: {e:#}")),
        }
        let _ = self.requests.send(UiRequest::Shown);
        self.refresh_preview();
    }

    /// Destroys the window and surface; the GPU state stays.
    fn close(&mut self, keep_state: bool) {
        if self.open.take().is_none() {
            return;
        }
        self.rec = None;
        self.drag = None;
        self.field_drag = false;
        self.st.scrollbar_hot = false;
        self.st.hover = None;
        self.pointer = None;
        // A new surface starts with the default pointer.
        self.cursor = CursorIcon::Default;
        self.fresh = !keep_state;
    }

    fn notice(&mut self, msg: &str) {
        self.st.notice = Some((msg.to_string(), Instant::now()));
        self.redraw();
    }

    fn search(&mut self) {
        self.st.search_id += 1;
        self.st.pending = true;
        self.moved = false;
        let _ = self.requests.send(UiRequest::Search {
            id: self.st.search_id,
            text: self.st.query.text.clone(),
            image: self.st.chip.clone(),
            audio: self.st.audio.clone(),
            filter: self.st.filter,
        });
        self.redraw();
    }

    fn set_chip(&mut self, img: image::RgbImage) {
        // The model scales an image to 280 tokens anyway; a full-page screenshot would only
        // cost memory and time.
        let img = image::DynamicImage::ImageRgb8(img);
        let img = if img.width().max(img.height()) > CHIP_EDGE {
            img.thumbnail(CHIP_EDGE, CHIP_EDGE)
        } else {
            img
        };
        if let Some(g) = &self.gfx {
            self.st.chip_tex = Some(g.upload(&img.to_rgba8()));
        }
        self.st.chip = Some(Arc::new(img.to_rgb8()));
        self.search();
    }

    /// Moves the list to `target`, clamped. Wheel and keys animate; a touchpad moves at once.
    fn set_scroll(&mut self, target: f32, animate: bool) {
        self.scroll_target = target.clamp(0.0, draw::max_scroll(self.st.hits.len()));
        if !animate {
            self.st.scroll = self.scroll_target;
        }
        self.redraw();
    }

    fn select(&mut self, i: usize) {
        if i >= self.st.hits.len() {
            return;
        }
        self.st.sel = i;
        self.moved = true;
        self.set_scroll(draw::scroll_to(i, self.scroll_target), true);
        self.refresh_preview();
    }

    fn step(&mut self, delta: isize) {
        let n = self.st.hits.len() as isize;
        if n > 0 {
            self.select((self.st.sel as isize + delta).clamp(0, n - 1) as usize);
        }
    }

    /// After a wheel scroll, moves the selection to the nearest fully visible row, so the
    /// preview always belongs to a row on screen and the arrow keys go on from there.
    fn keep_selection_visible(&mut self) {
        let rows = draw::full_rows(self.scroll_target, self.st.hits.len());
        if rows.is_empty() || rows.contains(&self.st.sel) {
            return;
        }
        self.st.sel = if self.st.sel < rows.start {
            rows.start
        } else {
            rows.end - 1
        };
        self.moved = true;
        self.refresh_preview();
    }

    /// Scrolls long preview text. Returns false if the preview has no text to scroll.
    fn scroll_preview(&mut self, dy: f32) -> bool {
        let s = self.scale();
        let text = match &self.st.preview {
            Some(p) if p.tex.is_none() => p.text.clone(),
            _ => None,
        };
        let (Some(text), Some(g)) = (text, self.gfx.as_mut()) else {
            return false;
        };
        let max = (draw::preview_text_height(g, s, &text) - PREVIEW_TEXT[3]).max(0.0);
        self.st.preview_scroll = (self.st.preview_scroll - dy).clamp(0.0, max);
        self.redraw();
        true
    }

    /// Asks for the thumbnails of the visible rows and of the next page.
    fn request_thumbs(&mut self) {
        if self.thumbs_asked.len() > MAX_ROW_THUMBS {
            self.thumbs_asked.clear();
            self.st.thumbs.clear();
        }
        let n = self.st.hits.len();
        let rows = draw::visible_rows(self.st.scroll, n);
        for hit in &self.st.hits[rows.start..(rows.end + draw::page_rows()).min(n)] {
            if matches!(hit.kind, Kind::Image | Kind::Video)
                && self.thumbs_asked.insert(hit.path.clone())
            {
                let _ = self.thumb_jobs.send((hit.path.clone(), hit.kind));
            }
        }
    }

    fn set_filter(&mut self, f: Filter) {
        if f != self.st.filter {
            self.st.filter = f;
            self.search();
        }
    }

    fn cycle_filter(&mut self, delta: isize) {
        let all = Filter::ALL;
        let at = all.iter().position(|&f| f == self.st.filter).unwrap_or(0) as isize;
        self.set_filter(all[(at + delta).rem_euclid(all.len() as isize) as usize]);
    }

    /// Points the preview at the selected hit and loads its image or text off the UI thread.
    fn refresh_preview(&mut self) {
        let Some(hit) = self.selected() else {
            self.st.preview = None;
            return;
        };
        if self.st.preview.as_ref().is_some_and(|p| p.path == hit.path) {
            // A later search pass can find a better chunk of the same file.
            let text = hit.preview.clone();
            if let (Some(pv), Some(text)) = (self.st.preview.as_mut(), text) {
                if pv.tex.is_none() && pv.text.as_ref() != Some(&text) {
                    pv.text = Some(text);
                    self.st.preview_scroll = 0.0;
                    self.redraw();
                }
            }
            return;
        }
        let (path, kind) = (hit.path.clone(), hit.kind);
        let text = hit
            .preview
            .clone()
            .filter(|_| !matches!(kind, Kind::Image | Kind::Video));
        self.st.preview_scroll = 0.0;
        self.redraw();
        let want_text =
            text.is_none() && matches!(kind, Kind::Text | Kind::Code | Kind::Data | Kind::Other);
        let loads = want_text || matches!(kind, Kind::Image | Kind::Video);
        self.st.preview = Some(Preview {
            path: path.clone(),
            tex: None,
            dims: (0, 0),
            text,
            meta: meta_line(&path),
        });
        if loads {
            let _ = self.previews.send((path, kind, want_text));
        }
    }

    fn open_file(&mut self) {
        if let Some(hit) = self.selected() {
            let mut cmd = Command::new("setsid");
            cmd.args(["-f", "xdg-open"]).arg(&hit.path);
            spawn_command(cmd, None, self.wake.clone());
            self.close(false);
        }
    }

    fn reveal_file(&mut self) {
        if let Some(hit) = self.selected() {
            let mut cmd = Command::new("busctl");
            cmd.args([
                "--user",
                "call",
                "org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
                "ShowItems",
                "ass",
                "1",
            ])
            .arg(crate::thumbs::uri(&hit.path))
            .arg("");
            spawn_command(cmd, None, self.wake.clone());
            self.close(false);
        }
    }

    fn copy_path(&mut self) {
        if let Some(hit) = self.selected() {
            let data = hit.path.to_string_lossy().into_owned().into_bytes();
            spawn_command(Command::new("wl-copy"), Some(data), self.wake.clone());
            self.notice("Copied the path");
        }
    }

    /// Copies an image as image data, and any other file as a file link that file managers
    /// and chat apps paste as the file itself.
    fn copy_file(&mut self) {
        let Some(hit) = self.selected() else {
            return;
        };
        let ext = hit
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mime = match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "webp" => Some("image/webp"),
            "gif" => Some("image/gif"),
            _ => None,
        };
        let small = std::fs::metadata(&hit.path).is_ok_and(|m| m.len() <= MAX_COPY_BYTES);
        let image = mime
            .filter(|_| hit.kind == Kind::Image && small)
            .and_then(|m| Some((m, std::fs::read(&hit.path).ok()?)));
        let (mime, data, msg) = match image {
            Some((m, bytes)) => (m, bytes, "Copied the image"),
            None => (
                "text/uri-list",
                format!("{}\r\n", thumbs::uri(&hit.path)).into_bytes(),
                "Copied the file",
            ),
        };
        let mut cmd = Command::new("wl-copy");
        cmd.args(["--type", mime]);
        spawn_command(cmd, Some(data), self.wake.clone());
        self.notice(msg);
    }

    fn paste(&self) {
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let out = |args: &[&str]| {
                Command::new("wl-paste")
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
            };
            let types =
                out(&["--list-types"]).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
            let image_type = types
                .as_deref()
                .and_then(|t| t.lines().find(|l| l.starts_with("image/")));
            if let Some(ty) = image_type {
                let decoded =
                    out(&["--type", ty]).and_then(|o| image::load_from_memory(&o.stdout).ok());
                if let Some(img) = decoded {
                    return wake.send(Msg::PastedImage(img.to_rgb8()));
                }
            }
            match out(&["--no-newline"]) {
                Some(o) => wake.send(Msg::PastedText(
                    String::from_utf8_lossy(&o.stdout).into_owned(),
                )),
                None => wake.send(Msg::Error("wl-paste: clipboard is empty".into())),
            }
        });
    }

    fn start_recording(&mut self) {
        if self.rec.is_some() {
            return;
        }
        match Recorder::start() {
            Ok(r) => self.rec = Some(r),
            Err(e) => self.fail(format!("microphone: {e:#}")),
        }
        self.redraw();
    }

    fn stop_recording(&mut self) {
        if let Some(r) = self.rec.take() {
            let audio = r.finish();
            // Under 0.2 s is a stray key press.
            if audio.len() >= 3200 {
                self.st.audio = Some(audio);
                self.search();
            }
            self.redraw();
        }
    }

    fn on_event(&mut self, el: &dyn ActiveEventLoop, e: UiEvent) {
        match e {
            UiEvent::Toggle => {
                if self.open.is_some() {
                    self.close(false);
                } else {
                    self.open_window(el);
                    if self.open.is_some() {
                        self.search();
                    }
                }
            }
            UiEvent::Results { id, hits, done } => {
                if id != self.st.search_id {
                    return;
                }
                // After the user moved, the selected file stays at the same place on screen
                // while the list reorders around it. Before that, the top result is selected.
                let keep = self
                    .selected()
                    .filter(|_| self.moved)
                    .map(|h| (h.path.clone(), self.st.sel as f32 * ROW_H - self.st.scroll));
                self.st.hits = hits;
                self.st.pending = !done;
                let at = keep.and_then(|(p, offset)| {
                    let i = self.st.hits.iter().position(|h| h.path == p)?;
                    Some((i, offset))
                });
                match at {
                    Some((i, offset)) => {
                        self.st.sel = i;
                        self.set_scroll(i as f32 * ROW_H - offset, false);
                        self.set_scroll(draw::scroll_to(i, self.st.scroll), false);
                    }
                    None => {
                        self.st.sel = 0;
                        self.moved = false;
                        self.set_scroll(0.0, false);
                    }
                }
                self.refresh_preview();
            }
            UiEvent::Status(s) => {
                self.st.status = s;
                self.redraw();
            }
            UiEvent::ImageChip(img) => {
                self.open_window(el);
                if self.open.is_some() {
                    self.set_chip(img);
                }
            }
            UiEvent::Error(msg) => self.fail(msg),
        }
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Preview(l) => {
                let Some(pv) = self.st.preview.as_mut().filter(|p| p.path == l.path) else {
                    return;
                };
                if let (Some(img), Some(g)) = (&l.image, &self.gfx) {
                    pv.dims = img.dimensions();
                    pv.tex = Some(g.upload(img));
                }
                if l.text.is_some() {
                    pv.text = l.text;
                }
                self.redraw();
            }
            Msg::Thumb(path, img) => {
                if let (Some(img), Some(g)) = (img, &self.gfx) {
                    let thumb = Thumb {
                        tex: g.upload(&img),
                        dims: img.dimensions(),
                    };
                    self.st.thumbs.insert(path, thumb);
                    self.redraw();
                }
            }
            Msg::PastedImage(img) => {
                if self.open.is_some() {
                    self.set_chip(img);
                }
            }
            Msg::PastedText(t) => {
                if self.open.is_some() && self.st.query.insert(&t) {
                    self.search();
                }
            }
            Msg::Error(e) => self.fail(e),
        }
    }

    /// Runs a query edit: a changed text searches again, a caret move only redraws.
    fn edit(&mut self, changed: bool) {
        if changed {
            self.search();
        } else {
            self.redraw();
        }
    }

    fn set_zoom(&mut self, el: &dyn ActiveEventLoop, zoom: f32) {
        let zoom = ((zoom * 10.0).round() / 10.0).clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        // A layer surface gets its size when it is created, so the overlay opens again.
        self.close(true);
        self.open_window(el);
        self.notice(&format!("Zoom {:.0}%", zoom * 100.0));
    }

    fn copy_text(&mut self, text: String, msg: &str) {
        spawn_command(
            Command::new("wl-copy"),
            Some(text.into_bytes()),
            self.wake.clone(),
        );
        self.notice(msg);
    }

    fn key(&mut self, el: &dyn ActiveEventLoop, ev: KeyEvent) {
        if ev.state == ElementState::Released {
            if ev.physical_key == PhysicalKey::Code(KeyCode::Space) {
                self.stop_recording();
            }
            return;
        }
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();
        if ctrl && ev.physical_key == PhysicalKey::Code(KeyCode::Space) {
            if !ev.repeat {
                self.start_recording();
            }
            return;
        }
        // A held key repeats editing and movement, never an action that opens or closes something.
        let action = matches!(
            ev.logical_key,
            Key::Named(NamedKey::Escape | NamedKey::Enter | NamedKey::Tab)
        ) || (ctrl && matches!(ev.logical_key, Key::Character(_)));
        if ev.repeat && action {
            return;
        }
        // Physical keys, so these work on layouts that need Shift for digits or symbols.
        if ctrl {
            let PhysicalKey::Code(code) = ev.physical_key else {
                return;
            };
            let digits = [
                KeyCode::Digit1,
                KeyCode::Digit2,
                KeyCode::Digit3,
                KeyCode::Digit4,
                KeyCode::Digit5,
            ];
            if let Some(i) = digits.iter().position(|&d| code == d) {
                return self.set_filter(Filter::ALL[i]);
            }
            match code {
                KeyCode::Equal | KeyCode::NumpadAdd => return self.set_zoom(el, self.zoom + 0.1),
                KeyCode::Minus | KeyCode::NumpadSubtract => {
                    return self.set_zoom(el, self.zoom - 0.1)
                }
                KeyCode::Digit0 | KeyCode::Numpad0 => return self.set_zoom(el, DEFAULT_ZOOM),
                _ => {}
            }
        }
        let page = draw::page_rows() as isize;
        let q = &mut self.st.query;
        match &ev.logical_key {
            Key::Named(NamedKey::Escape) => self.close(false),
            Key::Named(NamedKey::Enter) if ctrl => self.reveal_file(),
            Key::Named(NamedKey::Enter) => self.open_file(),
            Key::Named(NamedKey::ArrowUp) => self.step(-1),
            Key::Named(NamedKey::ArrowDown) => self.step(1),
            Key::Named(NamedKey::PageUp) => self.step(-page),
            Key::Named(NamedKey::PageDown) => self.step(page),
            Key::Named(NamedKey::Home) if ctrl => self.select(0),
            Key::Named(NamedKey::End) if ctrl => self.select(self.st.hits.len().saturating_sub(1)),
            Key::Named(NamedKey::ArrowLeft) => {
                q.left(shift, ctrl);
                self.redraw();
            }
            Key::Named(NamedKey::ArrowRight) => {
                q.right(shift, ctrl);
                self.redraw();
            }
            Key::Named(NamedKey::Home) => {
                q.home(shift);
                self.redraw();
            }
            Key::Named(NamedKey::End) => {
                q.end(shift);
                self.redraw();
            }
            Key::Named(NamedKey::Delete) => {
                let changed = q.delete(ctrl);
                self.edit(changed);
            }
            Key::Named(NamedKey::Tab) => self.cycle_filter(if shift { -1 } else { 1 }),
            Key::Named(NamedKey::Backspace) => {
                if !q.is_empty() {
                    let changed = q.backspace(ctrl);
                    self.edit(changed);
                } else if self.st.chip.take().is_some() {
                    self.st.chip_tex = None;
                    self.search();
                } else if self.st.audio.take().is_some() {
                    self.search();
                }
            }
            Key::Character(c) if ctrl => match c.to_lowercase().as_str() {
                "a" => {
                    q.select_all();
                    self.redraw();
                }
                "j" => self.step(1),
                "k" => self.step(-1),
                "u" => {
                    let changed = q.clear();
                    self.edit(changed);
                }
                "w" => {
                    let changed = q.backspace(true);
                    self.edit(changed);
                }
                "z" if shift => {
                    let changed = q.redo();
                    self.edit(changed);
                }
                "z" => {
                    let changed = q.undo();
                    self.edit(changed);
                }
                "y" => {
                    let changed = q.redo();
                    self.edit(changed);
                }
                "x" => {
                    if let Some(text) = q.take_selection() {
                        self.copy_text(text, "Cut the text");
                        self.search();
                    }
                }
                // With text selected in the query, Ctrl+C copies that text, as in any field.
                "c" if !shift && q.selected().is_some() => {
                    let text = q.selected().unwrap_or_default().to_string();
                    self.copy_text(text, "Copied the text");
                }
                "c" if shift => self.copy_file(),
                "c" => self.copy_path(),
                "v" => self.paste(),
                "s" => {
                    self.close(true);
                    let _ = self.requests.send(UiRequest::CaptureRegion);
                }
                _ => {}
            },
            _ if ctrl || self.mods.alt_key() => {}
            _ => {
                if let Some(text) = &ev.text {
                    let changed = q.insert(text);
                    self.edit(changed);
                }
            }
        }
    }

    /// The caret position in the query nearest to window x coordinate `x`.
    fn field_index(&mut self, x: f32) -> usize {
        let s = self.scale();
        let target = draw::panel_x(x) - self.layout.field_x;
        let Some(g) = self.gfx.as_mut() else {
            return self.st.query.caret;
        };
        let q = &self.st.query;
        let bounds: Vec<usize> = q.boundaries().collect();
        let mut w = |i: usize| draw::query_width(g, s, &q.text[..bounds[i]]);
        // Prefix widths grow with the prefix, so a binary search measures O(log n) prefixes:
        // `lo` ends at the first boundary at or right of the pointer.
        let (mut lo, mut hi) = (0, bounds.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if w(mid) < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        match lo {
            0 => bounds[0],
            n if n == bounds.len() => bounds[n - 1],
            n if target - w(n - 1) < w(n) - target => bounds[n - 1],
            n => bounds[n],
        }
    }

    /// Moves the scrollbar thumb so the pointer at window `y` holds it at the grab point.
    fn drag_to(&mut self, y: f32, grab: f32) {
        let n = self.st.hits.len();
        let top = draw::panel_y(y) - grab;
        self.set_scroll(draw::scroll_for_thumb(top, n), false);
        self.keep_selection_visible();
    }

    fn set_scrollbar_hot(&mut self, hot: bool) {
        if hot != self.st.scrollbar_hot {
            self.st.scrollbar_hot = hot;
            self.redraw();
        }
    }

    fn press(&mut self, b: draw::Button) {
        use draw::Button as B;
        match b {
            B::Open => self.open_file(),
            B::Reveal => self.reveal_file(),
            B::CopyPath => self.copy_path(),
            B::CopyFile => self.copy_file(),
            B::Clear => {
                if self.st.query.clear() {
                    self.search();
                }
            }
            B::RemoveImage => {
                self.st.chip = None;
                self.st.chip_tex = None;
                self.search();
            }
            B::RemoveVoice => {
                self.st.audio = None;
                self.search();
            }
            B::Filter(i) => self.set_filter(Filter::ALL[i]),
            B::Example(i) => {
                self.st.query.clear();
                self.st.query.insert(draw::EXAMPLES[i]);
                self.search();
            }
        }
    }

    fn click(&mut self, x: f32, y: f32) {
        if let Some(b) = self.layout.button_at(x, y) {
            return self.press(b);
        }
        if draw::over_field(x, y) {
            let i = self.field_index(x);
            let double = self
                .last_field_click
                .is_some_and(|t| t.elapsed() < DOUBLE_CLICK);
            if double {
                self.st.query.select_word(i);
                self.last_field_click = None;
            } else {
                self.st.query.click(i, self.mods.shift_key());
                self.field_drag = true;
                self.last_field_click = Some(Instant::now());
            }
            self.redraw();
            return;
        }
        // A press on the thumb drags it. A press on the track jumps the thumb's middle there.
        if draw::over_scrollbar(x, y) {
            if let Some([_, _, at, thumb]) = draw::scrollbar(self.st.scroll, self.st.hits.len()) {
                let py = draw::panel_y(y);
                let grab = if (at..=at + thumb).contains(&py) {
                    py - at
                } else {
                    thumb / 2.0
                };
                self.drag = Some(grab);
                self.set_scrollbar_hot(true);
                self.drag_to(y, grab);
                return;
            }
        }
        // One click opens: pointing already selected the row and showed its preview.
        if let Some(i) = draw::row_at(x, y, self.st.scroll, self.st.hits.len()) {
            self.select(i);
            self.open_file();
        }
    }

    /// A right click on a row shows the file in its folder.
    fn right_click(&mut self, x: f32, y: f32) {
        if let Some(i) = draw::row_at(x, y, self.st.scroll, self.st.hits.len()) {
            self.select(i);
            self.reveal_file();
        }
    }

    /// The hovered button and the pointer shape for a point in window coordinates.
    fn hover(&mut self, x: f32, y: f32) {
        let button = self.layout.button_at(x, y);
        if button != self.st.hover {
            self.st.hover = button;
            self.redraw();
        }
        let n = self.st.hits.len();
        let icon = if self.drag.is_some() {
            CursorIcon::Grabbing
        } else if button.is_some() || draw::row_at(x, y, self.st.scroll, n).is_some() {
            CursorIcon::Pointer
        } else if draw::over_field(x, y) || self.field_drag {
            CursorIcon::Text
        } else {
            CursorIcon::Default
        };
        if icon != self.cursor {
            self.cursor = icon;
            if let Some(t) = &self.open {
                t.window.set_cursor(Cursor::Icon(icon));
            }
        }
    }

    /// Moves `st.scroll` one frame toward the target. Returns true while it still moves.
    fn ease_scroll(&mut self) -> bool {
        let now = Instant::now();
        let dt = self
            .last_frame
            .map_or(1.0 / 60.0, |t| now.duration_since(t).as_secs_f32())
            .min(0.1);
        let d = self.scroll_target - self.st.scroll;
        if d.abs() < 0.5 {
            self.st.scroll = self.scroll_target;
            self.last_frame = None;
            return false;
        }
        self.st.scroll += d * (1.0 - (-dt / SCROLL_EASE).exp());
        self.last_frame = Some(now);
        true
    }

    fn draw(&mut self) {
        let moving = self.ease_scroll();
        self.request_thumbs();
        let (Some(gfx), Some(target)) = (self.gfx.as_mut(), self.open.as_mut()) else {
            return;
        };
        let width = target.window.surface_size().width;
        if width == 0 {
            return;
        }
        let level = self.rec.as_ref().map(Recorder::level);
        let (scene, layout) = draw::build(gfx, &self.st, level, width);
        self.layout = layout;
        match gfx.render(target, &scene) {
            Ok(true) => {
                if let Some(t) = self.opened_at.take() {
                    tracing::debug!("first frame {:?} after open", t.elapsed());
                }
                // The compositor paces the next frame, so the animation runs at its rate.
                if moving {
                    target.window.request_redraw();
                }
            }
            Ok(false) => target.window.request_redraw(),
            Err(e) => tracing::warn!("render: {e:#}"),
        }
    }

    fn scale(&self) -> f32 {
        self.open
            .as_ref()
            .map_or(1.0, |t| t.window.surface_size().width as f32 / WIN_W)
    }
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, _el: &dyn ActiveEventLoop) {
        // Build the GPU state before the first toggle so the first open is as fast as the rest.
        if self.gfx.is_none() {
            match Gfx::new() {
                Ok(g) => self.gfx = Some(g),
                Err(e) => tracing::error!("GPU: {e:#}"),
            }
        }
    }

    fn proxy_wake_up(&mut self, el: &dyn ActiveEventLoop) {
        while let Ok(e) = self.events.try_recv() {
            self.on_event(el, e);
        }
        while let Ok(m) = self.msgs.try_recv() {
            self.on_msg(m);
        }
    }

    fn about_to_wait(&mut self, el: &dyn ActiveEventLoop) {
        if self.rec.as_ref().is_some_and(Recorder::full) {
            self.stop_recording();
        }
        let expired = |m: &Option<(String, Instant)>, secs| {
            m.as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(secs))
        };
        if expired(&self.st.error, ERROR_SECS) {
            self.st.error = None;
            self.redraw();
        }
        if expired(&self.st.notice, NOTICE_SECS) {
            self.st.notice = None;
            self.redraw();
        }
        // The recording level bar moves, so it redraws. A message only needs a timer to expire.
        let timed = self.st.error.is_some() || self.st.notice.is_some();
        if self.open.is_some() && (self.rec.is_some() || timed) {
            if self.rec.is_some() {
                self.redraw();
            }
            el.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(50)));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(&mut self, el: &dyn ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // winit's key-repeat timer stops only on a release or a focus change. The compositor
        // sends neither for a destroyed surface, so a held Enter keeps repeating to the closed window.
        if self.open.as_ref().map(|t| t.window.id()) != Some(id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => self.close(false),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::SurfaceResized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.redraw()
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                if !self.mods.control_key() {
                    self.stop_recording();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => self.key(el, event),
            WindowEvent::PointerButton {
                state: ElementState::Pressed,
                position,
                button,
                ..
            } if button.mouse_button() == MouseButton::Left => {
                let s = self.scale();
                self.click(position.x as f32 / s, position.y as f32 / s);
            }
            WindowEvent::PointerButton {
                state: ElementState::Pressed,
                position,
                button,
                ..
            } if button.mouse_button() == MouseButton::Right => {
                let s = self.scale();
                self.right_click(position.x as f32 / s, position.y as f32 / s);
            }
            WindowEvent::PointerButton {
                state: ElementState::Released,
                button,
                ..
            } if button.mouse_button() == MouseButton::Left => {
                self.field_drag = false;
                if self.drag.take().is_some() {
                    let hot = self
                        .pointer
                        .is_some_and(|(x, y)| draw::over_scrollbar(x, y));
                    self.set_scrollbar_hot(hot);
                }
            }
            WindowEvent::PointerMoved { position, .. } => {
                let s = self.scale();
                let (x, y) = (position.x as f32 / s, position.y as f32 / s);
                self.pointer = Some((x, y));
                self.hover(x, y);
                if let Some(grab) = self.drag {
                    self.drag_to(y, grab);
                    return;
                }
                if self.field_drag {
                    let i = self.field_index(x);
                    self.st.query.click(i, true);
                    self.redraw();
                    return;
                }
                let has_bar = draw::scrollbar(self.st.scroll, self.st.hits.len()).is_some();
                self.set_scrollbar_hot(has_bar && draw::over_scrollbar(x, y));
                // Pointing at a row selects it. Only real motion counts: a list that scrolls
                // under a resting pointer keeps the selection.
                if let Some(i) = draw::row_at(x, y, self.st.scroll, self.st.hits.len()) {
                    if i != self.st.sel {
                        self.st.sel = i;
                        self.moved = true;
                        self.refresh_preview();
                    }
                }
            }
            // Entering sets the pointer but selects no row: the overlay can open under a
            // resting pointer, and the top result must stay selected then.
            WindowEvent::PointerEntered { position, .. } => {
                let s = self.scale();
                let (x, y) = (position.x as f32 / s, position.y as f32 / s);
                self.pointer = Some((x, y));
                self.hover(x, y);
            }
            WindowEvent::PointerLeft { .. } => {
                self.pointer = None;
                if self.st.hover.take().is_some() {
                    self.redraw();
                }
                if self.drag.is_none() {
                    self.set_scrollbar_hot(false);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dy, animate) = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * WHEEL_ROWS * ROW_H, true),
                    MouseScrollDelta::PixelDelta(p) => (p.y as f32 / self.scale(), false),
                };
                let over_preview = self.pointer.is_some_and(|(x, y)| draw::over_preview(x, y));
                if !(over_preview && self.scroll_preview(dy)) {
                    self.set_scroll(self.scroll_target - dy, animate);
                    self.keep_selection_visible();
                }
            }
            _ => {}
        }
    }
}
