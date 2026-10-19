//! Grabs decoded video frames near the current playback position, live,
//! from the external `ffmpeg` binary. odj-player has no video decoder of
//! its own — `symphonia` (used for audio) has none either — so this is the
//! only way to show anything from a track's video track, and it's
//! entirely optional: missing or failing `ffmpeg` just means no frames.
//!
//! Spawning `ffmpeg` costs ~300ms before it decodes a single byte (mostly
//! process/library startup, measured directly — seek depth barely moves
//! it), which would make per-frame grabs feel sluggish. So each spawn asks
//! for a short batch of consecutive frames instead of one: as long as
//! later positions keep falling inside that batch — the common case during
//! normal forward playback — they're served from it with no new spawn at
//! all. A spawn only happens again once the requested position walks off
//! the end of what's cached, e.g. after a loop, hot cue or big jump.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

/// Terminal character cells are roughly twice as tall as they are wide in
/// most monospace fonts; not something this process can query, so it's a
/// fixed assumption. This only matters for *display* — how many cells wide
/// a decoded pixel column is worth — so it's applied in `ui::draw_video`,
/// not baked into the decode resolution below (which would make `ffmpeg`
/// pad a genuinely 16:9 source with black bars to fit a non-16:9 box).
pub const CELL_ASPECT: f64 = 2.0; // cell height / cell width
/// Assumed source aspect ratio for sizing the fixed decode box — most
/// downloaded video is 16:9. A source of a different shape still displays
/// correctly either way: `FfmpegSource` pads to fit this box rather than
/// stretching, see below.
const VIDEO_ASPECT: f64 = 16.0 / 9.0;

/// Fixed decode resolution, in source pixels (plain `VIDEO_ASPECT`, no
/// cell correction) — the UI fits this onto whatever screen area is
/// actually available without distorting it, so resizing the terminal
/// never needs a new grab. Comfortably above a typical pane's cell count
/// so upscaling a blocky image isn't the limiting factor; `ui::draw_video`
/// packs 2 pixel rows into each cell (half-block rendering), so `ROWS` is
/// double a terminal's usual row budget for this pane.
const ROWS: u16 = 48;
const COLS: u16 = (ROWS as f64 * VIDEO_ASPECT) as u16; // 85

/// The shape (in cells) the UI should reserve for the video pane — e.g.
/// width = height × this — so a 16:9 source fills the space it's given
/// instead of being letterboxed down to fit a mismatched box. Unlike
/// `COLS / ROWS`, this does fold in `CELL_ASPECT`, since it's sizing cells
/// on screen rather than decoded pixels.
pub const ASPECT: f64 = VIDEO_ASPECT * CELL_ASPECT;

/// Frames per second decoded per batch.
const STREAM_FPS: f64 = 8.0;
/// Frames fetched per `ffmpeg` spawn — at `STREAM_FPS`, a few seconds of
/// coverage, so its ~300ms startup cost is paid rarely, not every frame.
const BATCH_FRAMES: u32 = 40;
/// Seconds between two consecutive frames in a batch — also how close a
/// request needs to land to one to be worth asking for.
pub const FRAME_INTERVAL: f64 = 1.0 / STREAM_FPS;

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

/// How frames are actually fetched — a seam so the batching/throttling
/// logic is testable without a real `ffmpeg` process, mirroring
/// odj-downloader's `Fetcher` trait.
pub trait VideoSource: Send + Sync {
    /// Up to `BATCH_FRAMES` consecutive frames, `FRAME_INTERVAL` apart,
    /// starting at `pos_secs`. `None` on any failure (missing binary, bad
    /// seek, unsupported codec, etc. — all collapse to "no frames"); an
    /// empty or short batch near the end of the track is fine.
    fn grab_batch(&self, pos_secs: f64) -> Option<Vec<Frame>>;
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
    fn grab_batch(&self, pos_secs: f64) -> Option<Vec<Frame>> {
        // `-ss` before `-i` is a fast keyframe-based input seek —
        // approximate, not frame-accurate, which is fine for a
        // thumbnail-style preview and is what lets this handle
        // reverse/scratch/loops for free: every batch is an independent
        // seek, never a continuous stream that reverse would break.
        //
        // `scale=...:force_original_aspect_ratio=decrease,pad=...` fits
        // the source into the fixed COLS×ROWS box without stretching it —
        // a source that isn't actually 16:9 gets letterboxed/pillarboxed
        // instead of squished.
        let vf = format!(
            "fps={STREAM_FPS},scale={COLS}:{ROWS}:force_original_aspect_ratio=decrease,pad={COLS}:{ROWS}:(ow-iw)/2:(oh-ih)/2:color=black"
        );
        let output = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-ss", &format!("{:.3}", pos_secs.max(0.0))])
            .arg("-i")
            .arg(&self.path)
            .args(["-an", "-vf", &vf, "-frames:v", &BATCH_FRAMES.to_string(), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let frame_bytes = COLS as usize * ROWS as usize * 3;
        let frames: Vec<Frame> = output
            .stdout
            .chunks_exact(frame_bytes)
            .map(|chunk| {
                let pixels = chunk.as_chunks::<3>().0.iter().map(|&[r, g, b]| (r, g, b)).collect();
                Frame { width: COLS, height: ROWS, pixels }
            })
            .collect();
        (!frames.is_empty()).then_some(frames)
    }
}

pub enum Event {
    Frame(Frame),
    /// Gave up after repeated failures; stop reserving screen space for it.
    Unavailable,
}

/// One long-lived background thread per loaded track, serving frames from
/// its current batch and fetching a new one only once a request walks off
/// the end of it.
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
            let mut batch: Vec<Frame> = Vec::new();
            let mut batch_start = 0.0f64;
            while let Ok(mut pos) = request_rx.recv() {
                // Only the newest position matters — one grab is far
                // slower than the ticks requesting it, so a burst of
                // position updates during normal playback should only
                // act on the latest.
                while let Ok(newer) = request_rx.try_recv() {
                    pos = newer;
                }
                let offset = ((pos - batch_start) / FRAME_INTERVAL).round();
                let cached = (offset >= 0.0 && (offset as usize) < batch.len()).then(|| batch[offset as usize].clone());
                let frame = match cached {
                    Some(frame) => Some(frame),
                    None => match source.grab_batch(pos) {
                        Some(frames) => {
                            let first = frames[0].clone();
                            batch = frames;
                            batch_start = pos;
                            Some(first)
                        }
                        None => None,
                    },
                };
                match frame {
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

    /// A `VideoSource` that returns a canned batch (or nothing) and counts
    /// calls, for tests that don't want a real `ffmpeg` process.
    pub struct FakeSource {
        batch: Option<Vec<Frame>>,
        pub calls: AtomicUsize,
    }

    impl FakeSource {
        pub fn working() -> Self {
            Self::with_frames(1)
        }

        /// A batch of `n` frames, `FRAME_INTERVAL` apart — enough to test
        /// that positions within it are served without a new grab.
        pub fn with_frames(n: usize) -> Self {
            let frame = Frame { width: 1, height: 1, pixels: vec![(10, 20, 30)] };
            Self { batch: Some(vec![frame; n.max(1)]), calls: AtomicUsize::new(0) }
        }

        pub fn failing() -> Self {
            Self { batch: None, calls: AtomicUsize::new(0) }
        }
    }

    impl VideoSource for FakeSource {
        fn grab_batch(&self, _pos_secs: f64) -> Option<Vec<Frame>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.batch.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::FakeSource;
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn recv_within(rx: &mpsc::Receiver<Event>, timeout: Duration) -> Event {
        let deadline = Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(e) => return e,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("no event received in time: {e:?}"),
            }
        }
    }

    #[test]
    fn requests_within_the_same_batch_are_served_without_a_new_grab() {
        let fake = Arc::new(FakeSource::with_frames(4)); // covers 0..0.5s at 8fps
        let worker = VideoWorker::start(fake.clone());

        worker.request(0.0);
        assert!(matches!(recv_within(&worker.events, Duration::from_secs(1)), Event::Frame(_)));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);

        // Still inside the batch's span: no new spawn.
        worker.request(0.3);
        assert!(matches!(recv_within(&worker.events, Duration::from_secs(1)), Event::Frame(_)));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1, "should reuse the cached batch");

        // Past it: a new grab is needed.
        worker.request(10.0);
        assert!(matches!(recv_within(&worker.events, Duration::from_secs(1)), Event::Frame(_)));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn gives_up_after_repeated_failures() {
        let fake = Arc::new(FakeSource::failing());
        let worker = VideoWorker::start(fake);
        // Keeps nudging it (like `App::update_video` does every tick while
        // there's no frame yet) until it gives up; a bursty send wouldn't
        // reliably force `MAX_FAILURES` *distinct* attempts, since the
        // worker collapses a pile-up of identical requests to just the
        // latest one before each grab.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            worker.request(0.0);
            match worker.events.try_recv() {
                Ok(Event::Unavailable) => break,
                Ok(Event::Frame(_)) => panic!("a failing source shouldn't produce a frame"),
                Err(_) => {}
            }
            assert!(Instant::now() < deadline, "never gave up");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
