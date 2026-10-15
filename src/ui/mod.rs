//! The overlay: a layer-shell panel with a search field, filter chips, results and a preview.

use crate::types::{Filter, Hit, Status};
use anyhow::Result;
use std::sync::mpsc::Sender;
use winit::event_loop::EventLoopProxy;

mod app;
mod draw;
mod field;
mod gfx;
mod voice;

/// Sends events to the overlay from any thread: a channel plus a wake-up of the event loop.
#[derive(Clone)]
pub struct UiSender {
    tx: Sender<UiEvent>,
    proxy: EventLoopProxy,
}

impl UiSender {
    pub fn new(tx: Sender<UiEvent>, proxy: EventLoopProxy) -> UiSender {
        UiSender { tx, proxy }
    }

    /// False once the overlay loop is gone.
    pub fn send(&self, e: UiEvent) -> bool {
        let ok = self.tx.send(e).is_ok();
        self.proxy.wake_up();
        ok
    }
}

/// Daemon -> overlay.
#[derive(Debug)]
pub enum UiEvent {
    /// Open the overlay if it is closed, close it if it is open.
    Toggle,
    /// Results for request `id`. `done` is false for the fast (path + keyword) pass.
    Results {
        id: u64,
        hits: Vec<Hit>,
        done: bool,
    },
    Status(Status),
    /// A screen region to attach as the image chip.
    ImageChip(image::RgbImage),
    /// A message for the status line.
    Error(String),
}

/// Overlay -> daemon.
#[derive(Debug)]
pub enum UiRequest {
    /// Sent on each query change. `id` grows; the daemon drops stale ids.
    Search {
        id: u64,
        text: String,
        /// Shared, so a keystroke does not copy the image.
        image: Option<std::sync::Arc<image::RgbImage>>,
        audio: Option<Vec<f32>>,
        filter: Filter,
    },
    /// The overlay opened: refresh the path list and preload the model.
    Shown,
    /// Ctrl+S: the overlay is hidden; the daemon runs the region capture and sends ImageChip.
    CaptureRegion,
}

/// Runs the winit event loop on the calling (main) thread. `ready` gets a sender before the
/// loop starts, so other threads can send events; the overlay drains them in `proxy_wake_up`.
pub fn run(requests: Sender<UiRequest>, ready: impl FnOnce(UiSender)) -> Result<()> {
    app::run(requests, ready)
}
