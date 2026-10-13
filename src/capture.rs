//! Screen capture through ext-image-copy-capture-v1, with wlr-screencopy as the fallback.

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use std::fs::File;
use std::io::{Read, Seek};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_output, wl_registry::WlRegistry, wl_shm, wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

#[derive(Default)]
struct Output {
    name: String,
    /// The logical position from wl_output.geometry, used without niri.
    x: i32,
    y: i32,
    scale: i32,
}

/// The shm buffer layout a compositor asks for.
#[derive(Clone, Copy)]
struct Layout {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, Output)>,
    /// wlr: the first (shm) buffer event.
    layout: Option<Layout>,
    ext_formats: Vec<wl_shm::Format>,
    ext_size: (u32, u32),
    constraints_done: bool,
    stopped: bool,
    ready: bool,
    failed: Option<String>,
    y_invert: bool,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, o)) = state.outputs.iter_mut().find(|(p, _)| p == proxy) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Geometry { x, y, .. } => (o.x, o.y) = (x, y),
            wl_output::Event::Scale { factor } => o.scale = factor,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => state.ext_size = (width, height),
            Event::ShmFormat {
                format: WEnum::Value(f),
            } => state.ext_formats.push(f),
            Event::Done => state.constraints_done = true,
            Event::Stopped => state.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.failed = Some(format!("{reason:?}"))
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::{Event, Flags};
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                state.layout = Some(Layout {
                    format,
                    width,
                    height,
                    stride,
                })
            }
            Event::BufferDone => state.constraints_done = true,
            Event::Flags { flags } => {
                state.y_invert = matches!(flags, WEnum::Value(f) if f.contains(Flags::YInvert))
            }
            Event::Ready { .. } => state.ready = true,
            Event::Failed => state.failed = Some("compositor refused the copy".into()),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);

/// The shm formats that `decode` handles, best first.
const FORMATS: [wl_shm::Format; 6] = [
    wl_shm::Format::Xrgb8888,
    wl_shm::Format::Argb8888,
    wl_shm::Format::Xbgr8888,
    wl_shm::Format::Abgr8888,
    wl_shm::Format::Xrgb2101010,
    wl_shm::Format::Xbgr2101010,
];

/// An unlinked file in $XDG_RUNTIME_DIR (tmpfs) backs the shm pool.
fn shm_file(size: u64) -> Result<File> {
    // Only the private runtime directory: in /tmp, another user could plant a file or link here.
    let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let path = std::path::PathBuf::from(dir).join(format!("mmsearch-shm-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .context("shm file")?;
    std::fs::remove_file(&path).ok();
    file.set_len(size)?;
    Ok(file)
}

fn decode(l: Layout, y_invert: bool, raw: &[u8]) -> RgbImage {
    let mut img = RgbImage::new(l.width, l.height);
    for (y, row) in img.chunks_exact_mut(l.width as usize * 3).enumerate() {
        let sy = if y_invert {
            l.height as usize - 1 - y
        } else {
            y
        };
        let src = &raw[sy * l.stride as usize..][..l.width as usize * 4];
        for (dst, px) in row.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
            let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            let ten = |s: u32| ((v >> s & 0x3ff) >> 2) as u8;
            let rgb = match l.format {
                wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => [px[0], px[1], px[2]],
                wl_shm::Format::Xrgb2101010 => [ten(20), ten(10), ten(0)],
                wl_shm::Format::Xbgr2101010 => [ten(0), ten(10), ten(20)],
                _ => [px[2], px[1], px[0]],
            };
            dst.copy_from_slice(&rgb);
        }
    }
    img
}

/// Waits until `done` holds, failing on a compositor error.
fn wait(
    queue: &mut EventQueue<State>,
    state: &mut State,
    done: impl Fn(&State) -> bool,
) -> Result<()> {
    while !done(state) {
        if let Some(r) = &state.failed {
            bail!("capture failed: {r}");
        }
        if state.stopped {
            bail!("capture session stopped");
        }
        queue.blocking_dispatch(state)?;
    }
    Ok(())
}

/// Captures one whole output and returns its logical origin and the pixels per logical unit.
fn capture(output: Option<&str>) -> Result<(RgbImage, (i32, i32), f64)> {
    let conn = Connection::connect_to_env().context("no Wayland connection")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let mut state = State::default();

    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| anyhow!("no wl_shm"))?;
    for g in globals
        .contents()
        .clone_list()
        .iter()
        .filter(|g| g.interface == "wl_output")
    {
        let o: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, ());
        state.outputs.push((
            o,
            Output {
                scale: 1,
                ..Default::default()
            },
        ));
    }
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    let (wl_out, info) = state
        .outputs
        .iter()
        .find(|(_, o)| output.is_none_or(|n| o.name == n))
        .ok_or_else(|| anyhow!("output {} not found", output.unwrap_or("(first)")))?;
    let (wl_out, name, origin, wl_scale) = (
        wl_out.clone(),
        info.name.clone(),
        (info.x, info.y),
        info.scale,
    );

    let sources = globals.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ());
    let copier = globals.bind::<ExtImageCopyCaptureManagerV1, _, _>(&qh, 1..=1, ());
    let (layout, y_invert, raw) = if let (Ok(sources), Ok(copier)) = (sources, copier) {
        let session = copier.create_session(
            &sources.create_source(&wl_out, &qh, ()),
            Options::empty(),
            &qh,
            (),
        );
        wait(&mut queue, &mut state, |s| s.constraints_done)?;
        let format = FORMATS
            .into_iter()
            .find(|f| state.ext_formats.contains(f))
            .ok_or_else(|| anyhow!("no supported shm format in {:?}", state.ext_formats))?;
        let (width, height) = state.ext_size;
        let layout = Layout {
            format,
            width,
            height,
            stride: width * 4,
        };
        let (mut file, buffer) = shm_buffer(&shm, &qh, layout)?;
        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(&buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        wait(&mut queue, &mut state, |s| s.ready)?;
        frame.destroy();
        session.destroy();
        (layout, false, read_all(&mut file, layout)?)
    } else {
        let manager: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ()).map_err(|_| {
            anyhow!("the compositor offers neither ext-image-copy-capture nor wlr-screencopy")
        })?;
        let frame = manager.capture_output(0, &wl_out, &qh, ());
        let v3 = manager.version() >= 3;
        wait(&mut queue, &mut state, |s| {
            s.layout.is_some() && (s.constraints_done || !v3)
        })?;
        let layout = state.layout.unwrap();
        if !FORMATS.contains(&layout.format) {
            bail!("unsupported shm format {:?}", layout.format);
        }
        let (mut file, buffer) = shm_buffer(&shm, &qh, layout)?;
        frame.copy(&buffer);
        wait(&mut queue, &mut state, |s| s.ready)?;
        frame.destroy();
        (layout, state.y_invert, read_all(&mut file, layout)?)
    };
    let img = decode(layout, y_invert, &raw);

    // niri reports fractional scales that wl_output rounds up.
    let (origin, scale) = match crate::niri::output_logical(&name) {
        Some(l) => ((l.x, l.y), layout.width as f64 / l.width as f64),
        None => (origin, wl_scale as f64),
    };
    Ok((img, origin, scale))
}

fn shm_buffer(shm: &WlShm, qh: &QueueHandle<State>, l: Layout) -> Result<(File, WlBuffer)> {
    let size = l.stride as u64 * l.height as u64;
    let file = shm_file(size)?;
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        l.width as i32,
        l.height as i32,
        l.stride as i32,
        l.format,
        qh,
        (),
    );
    pool.destroy();
    Ok((file, buffer))
}

fn read_all(file: &mut File, l: Layout) -> Result<Vec<u8>> {
    let mut raw = vec![0u8; l.stride as usize * l.height as usize];
    file.rewind()?;
    file.read_exact(&mut raw)?;
    Ok(raw)
}

/// Runs `slurp` so the user selects a region, then captures it.
/// Returns None if the user cancels the selection.
pub fn capture_region() -> Result<Option<RgbImage>> {
    let out = Command::new("slurp")
        .args(["-f", "%o\n%x %y %w %h"])
        .output()
        .context("cannot run slurp")?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8(out.stdout)?;
    let mut lines = text.lines();
    let name = lines.next().unwrap_or_default();
    let nums: Vec<i32> = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let [x, y, w, h] = nums[..] else {
        bail!("unexpected slurp output: {text:?}")
    };

    let (img, origin, scale) = capture(Some(name).filter(|n| !n.is_empty()))?;
    // slurp reports global logical coordinates; the buffer starts at the output origin.
    let px = |v: i32| (v as f64 * scale).round() as i64;
    let x0 = px(x - origin.0).clamp(0, img.width() as i64 - 1) as u32;
    let y0 = px(y - origin.1).clamp(0, img.height() as i64 - 1) as u32;
    let cw = (px(w) as u32).clamp(1, img.width() - x0);
    let ch = (px(h) as u32).clamp(1, img.height() - y0);
    Ok(Some(
        image::imageops::crop_imm(&img, x0, y0, cw, ch).to_image(),
    ))
}
