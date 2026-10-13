//! The winit app: window lifetime, input handling and the requests to the daemon.

use super::draw::{self, ERROR_SECS, WIN_H, WIN_W};
use super::gfx::{Gfx, Target, Tex};
use super::voice::Recorder;
use super::{UiEvent, UiRequest, UiSender};
use crate::thumbs;
use crate::types::{Filter, Hit, Kind, Status};
use anyhow::Result;
use chrono::{DateTime, Local};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
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

pub struct Preview {
    pub path: PathBuf,
    pub tex: Option<Rc<Tex>>,
    pub dims: (u32, u32),
    pub text: Option<String>,
    pub meta: String,
}

#[derive(Default)]
pub struct State {
    pub query: String,
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
    pub preview: Option<Preview>,
}

struct Loaded {
    path: PathBuf,
    image: Option<image::RgbaImage>,
    text: Option<String>,
}

enum Msg {
    Preview(Loaded),
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
    last_click: Option<(Instant, usize)>,
    chip_rects: Vec<[f32; 4]>,
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
    let app = App {
        requests,
        events,
        msgs,
        wake,
        previews,
        gfx: None,
        open: None,
        st: State::default(),
        mods: ModifiersState::default(),
        fresh: true,
        rec: None,
        last_click: None,
        chip_rects: Vec::new(),
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
/// large photos never queues a decode per step.
fn preview_worker(wake: Wake) -> Sender<(PathBuf, Kind, bool)> {
    let (tx, rx) = channel::<(PathBuf, Kind, bool)>();
    std::thread::spawn(move || {
        while let Ok(mut job) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                job = newer;
            }
            let (path, kind, want_text) = job;
            wake.send(Msg::Preview(load_preview(path, kind, want_text)));
        }
    });
    tx
}

fn delete_word(q: &mut String) {
    while q.ends_with(char::is_whitespace) {
        q.pop();
    }
    while q.chars().last().is_some_and(|c| !c.is_whitespace()) {
        q.pop();
    }
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
            let id = self.st.search_id;
            self.st = State {
                status,
                error,
                search_id: id,
                ..State::default()
            };
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
            .with_surface_size(LogicalSize::new(WIN_W as f64, WIN_H as f64))
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
        self.fresh = !keep_state;
    }

    fn search(&mut self) {
        self.st.search_id += 1;
        self.st.pending = true;
        let _ = self.requests.send(UiRequest::Search {
            id: self.st.search_id,
            text: self.st.query.clone(),
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

    fn select(&mut self, i: usize) {
        if i >= self.st.hits.len() {
            return;
        }
        self.st.sel = i;
        self.st.scroll = draw::scroll_to(i, self.st.scroll);
        self.refresh_preview();
        self.redraw();
    }

    fn step(&mut self, delta: isize) {
        let n = self.st.hits.len() as isize;
        if n > 0 {
            self.select((self.st.sel as isize + delta).clamp(0, n - 1) as usize);
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
            return;
        }
        let (path, kind) = (hit.path.clone(), hit.kind);
        let text = hit
            .preview
            .clone()
            .filter(|_| !matches!(kind, Kind::Image | Kind::Video));
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
        }
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
                let keep = self.selected().map(|h| h.path.clone());
                self.st.sel = keep
                    .and_then(|p| hits.iter().position(|h| h.path == p))
                    .unwrap_or(0);
                self.st.hits = hits;
                self.st.pending = !done;
                self.st.scroll = draw::scroll_to(self.st.sel, self.st.scroll)
                    .min(draw::max_scroll(self.st.hits.len()));
                self.refresh_preview();
                self.redraw();
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
            Msg::PastedImage(img) => {
                if self.open.is_some() {
                    self.set_chip(img);
                }
            }
            Msg::PastedText(t) => {
                if self.open.is_some() {
                    self.st
                        .query
                        .extend(t.chars().map(|c| if c.is_control() { ' ' } else { c }));
                    self.search();
                }
            }
            Msg::Error(e) => self.fail(e),
        }
    }

    fn key(&mut self, ev: KeyEvent) {
        if ev.state == ElementState::Released {
            if ev.physical_key == PhysicalKey::Code(KeyCode::Space) {
                self.stop_recording();
            }
            return;
        }
        let ctrl = self.mods.control_key();
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
        ) || (ctrl && ev.logical_key != Key::Named(NamedKey::Backspace));
        if ev.repeat && action {
            return;
        }
        match &ev.logical_key {
            Key::Named(NamedKey::Escape) => self.close(false),
            Key::Named(NamedKey::Enter) if ctrl => self.reveal_file(),
            Key::Named(NamedKey::Enter) => self.open_file(),
            Key::Named(NamedKey::ArrowUp) => self.step(-1),
            Key::Named(NamedKey::ArrowDown) => self.step(1),
            Key::Named(NamedKey::Tab) => {
                self.cycle_filter(if self.mods.shift_key() { -1 } else { 1 })
            }
            Key::Named(NamedKey::Backspace) => {
                if !self.st.query.is_empty() {
                    if ctrl {
                        delete_word(&mut self.st.query);
                    } else {
                        self.st.query.pop();
                    }
                } else if self.st.chip.take().is_some() {
                    self.st.chip_tex = None;
                } else if self.st.audio.take().is_none() {
                    return;
                }
                self.search();
            }
            Key::Character(c) if ctrl => match c.to_lowercase().as_str() {
                "j" => self.step(1),
                "k" => self.step(-1),
                "u" => {
                    self.st.query.clear();
                    self.search();
                }
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
                    let before = self.st.query.len();
                    self.st
                        .query
                        .extend(text.chars().filter(|c| !c.is_control()));
                    if self.st.query.len() != before {
                        self.search();
                    }
                }
            }
        }
    }

    fn click(&mut self, x: f32, y: f32) {
        if let Some(i) = draw::row_at(x, y, self.st.scroll, self.st.hits.len()) {
            let double = self
                .last_click
                .is_some_and(|(at, row)| row == i && at.elapsed() < DOUBLE_CLICK);
            self.select(i);
            self.last_click = Some((Instant::now(), i));
            if double {
                self.open_file();
            }
            return;
        }
        let hit = self
            .chip_rects
            .iter()
            .position(|r| x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3]);
        if let Some(i) = hit {
            self.set_filter(Filter::ALL[i]);
        }
    }

    fn draw(&mut self) {
        let (Some(gfx), Some(target)) = (self.gfx.as_mut(), self.open.as_mut()) else {
            return;
        };
        let width = target.window.surface_size().width;
        if width == 0 {
            return;
        }
        let level = self.rec.as_ref().map(Recorder::level);
        let (scene, chips) = draw::build(gfx, &self.st, level, width);
        self.chip_rects = chips;
        match gfx.render(target, &scene) {
            Ok(true) => {
                if let Some(t) = self.opened_at.take() {
                    tracing::debug!("first frame {:?} after open", t.elapsed());
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
        if self
            .st
            .error
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(ERROR_SECS))
        {
            self.st.error = None;
            self.redraw();
        }
        if self.open.is_some() && (self.rec.is_some() || self.st.error.is_some()) {
            self.redraw();
            el.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(50)));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(&mut self, _el: &dyn ActiveEventLoop, id: WindowId, event: WindowEvent) {
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
            WindowEvent::KeyboardInput { event, .. } => self.key(event),
            WindowEvent::PointerButton {
                state: ElementState::Pressed,
                position,
                button,
                ..
            } if button.mouse_button() == MouseButton::Left => {
                let s = self.scale();
                self.click(position.x as f32 / s, position.y as f32 / s);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 56.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / self.scale(),
                };
                self.st.scroll =
                    (self.st.scroll - dy).clamp(0.0, draw::max_scroll(self.st.hits.len()));
                self.redraw();
            }
            _ => {}
        }
    }
}
