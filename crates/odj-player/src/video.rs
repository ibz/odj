//! Grabs a decoded video frame near the current playback position, live,
//! from the external `ffmpeg` binary. odj-player has no video decoder of
//! its own — `symphonia` (used for audio) has none either — so this is the
//! only way to show anything from a track's video track, and it's
//! entirely optional: missing or failing `ffmpeg` just means no frames.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

/// Fixed decode resolution; the UI maps this onto whatever screen area is
/// actually available, so resizing the terminal never needs a new grab.
const COLS: u16 = 48;
const ROWS: u16 = 24;

/// Give up after this many grabs fail in a row — e.g. `ffmpeg` missing, or
/// a codec it can't read — rather than retrying forever.
const MAX_FAILURES: u32 = 3;

#[derive(Clone)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    /// Row-major, `width * height` long.
    pub pixels: Vec<(u8, u8, u8)>,
}

/// How a frame is actually fetched — a seam so the throttling/give-up
/// logic in `App` is testable without a real `ffmpeg` process, mirroring
/// odj-downloader's `Fetcher` trait.
pub trait VideoSource: Send + Sync {
    /// One decoded frame nearest `pos_secs`, at this source's fixed
    /// resolution, or `None` on any failure (missing binary, bad seek,
    /// unsupported codec, etc. — all collapse to "no frame available").
    fn grab(&self, pos_secs: f64) -> Option<Frame>;
}

pub struct FfmpegSource {
    path: PathBuf,
}

impl FfmpegSource {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl VideoSource for FfmpegSource {
    fn grab(&self, pos_secs: f64) -> Option<Frame> {
        // `-ss` before `-i` is a fast keyframe-based input seek —
        // approximate, not frame-accurate, which is fine for a
        // thumbnail-style preview and is what lets this handle
        // reverse/scratch/loops for free: every request is an independent
        // seek, never a continuous stream that reverse would break.
        let output = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-ss", &format!("{:.3}", pos_secs.max(0.0))])
            .arg("-i")
            .arg(&self.path)
            .args(["-an", "-frames:v", "1", "-vf", &format!("scale={COLS}:{ROWS}"), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let expected = COLS as usize * ROWS as usize * 3;
        if output.stdout.len() != expected {
            return None;
        }
        let pixels = output.stdout.as_chunks::<3>().0.iter().map(|&[r, g, b]| (r, g, b)).collect();
        Some(Frame { width: COLS, height: ROWS, pixels })
    }
}

pub enum Event {
    Frame(Frame),
    /// Gave up after repeated failures; stop reserving screen space for it.
    Unavailable,
}

/// One long-lived background thread per loaded track, grabbing frames one
/// at a time.
pub struct VideoWorker {
    request_tx: mpsc::Sender<f64>,
    pub events: mpsc::Receiver<Event>,
}

impl VideoWorker {
    pub fn start(source: Arc<dyn VideoSource>) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<f64>();
        let (event_tx, events) = mpsc::channel();
        thread::spawn(move || {
            let mut failures = 0u32;
            while let Ok(mut pos) = request_rx.recv() {
                // Only the newest position matters — one grab is far
                // slower than the ticks requesting it, so a burst of
                // position updates during normal playback should only
                // act on the latest.
                while let Ok(newer) = request_rx.try_recv() {
                    pos = newer;
                }
                match source.grab(pos) {
                    Some(frame) => {
                        failures = 0;
                        if event_tx.send(Event::Frame(frame)).is_err() {
                            break;
                        }
                    }
                    None => {
                        failures += 1;
                        if failures >= MAX_FAILURES {
                            let _ = event_tx.send(Event::Unavailable);
                            break;
                        }
                    }
                }
            }
        });
        Self { request_tx, events }
    }

    /// Requests a frame near `pos_secs`; never blocks the caller.
    pub fn request(&self, pos_secs: f64) {
        let _ = self.request_tx.send(pos_secs);
    }
}

#[cfg(test)]
pub mod test_util {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `VideoSource` that returns a canned frame (or nothing) and counts
    /// calls, for tests that don't want a real `ffmpeg` process.
    pub struct FakeSource {
        frame: Option<Frame>,
        pub calls: AtomicUsize,
    }

    impl FakeSource {
        pub fn working() -> Self {
            Self { frame: Some(Frame { width: 1, height: 1, pixels: vec![(10, 20, 30)] }), calls: AtomicUsize::new(0) }
        }

        pub fn failing() -> Self {
            Self { frame: None, calls: AtomicUsize::new(0) }
        }
    }

    impl VideoSource for FakeSource {
        fn grab(&self, _pos_secs: f64) -> Option<Frame> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.frame.clone()
        }
    }
}
