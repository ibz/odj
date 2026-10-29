//! Streams the mixed main output to a WAV file on disk. [`Recording`] is
//! the hardware-free half: a ring-buffer consumer draining into an
//! [`odj_core::wav::WavWriter`] on its own thread, the same
//! background-thread shape as [`crate::devices::Devices`]. [`Recorder`] is
//! the thin half wiring it to whatever [`Routing`] currently calls the
//! main output.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Split};

use odj_core::wav::WavWriter;

use crate::routing::Routing;

/// Ring buffer capacity for the tap, in stereo samples (~1 s at 48 kHz) —
/// much more slack than `routing::RING_STEREO_SAMPLES` (~43 ms) needs
/// between a capture and an output callback, since this one's consumer is
/// a disk-writing thread, not another realtime audio callback; a moment
/// of disk latency shouldn't drop frames. The audio thread's own push
/// (`MixedSource::render`) never blocks regardless — on overrun, it
/// simply drops whatever doesn't fit.
const REC_RING_STEREO_SAMPLES: usize = 96_000;
/// How long the writer thread sleeps between polls when there's nothing
/// to write and it hasn't been told to stop yet.
const POLL_INTERVAL: Duration = Duration::from_millis(15);

/// A WAV recording in progress, written by a background thread until
/// [`stop`](Self::stop) (or [`Drop`], if `stop` was never called — an
/// abrupt quit mid-recording still leaves a complete, playable file).
pub struct Recording {
    path: PathBuf,
    started: Instant,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Recording {
    pub fn start(path: PathBuf, channels: u16, rate: u32, mut cons: ringbuf::HeapCons<f32>) -> io::Result<Self> {
        let mut writer = WavWriter::create(&path, channels, rate)?;
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let (stop2, error2) = (stop.clone(), error.clone());
        let thread = std::thread::Builder::new()
            .name("odj-recorder".into())
            .spawn(move || {
                let mut buf = vec![0.0f32; 4096];
                loop {
                    let n = cons.pop_slice(&mut buf);
                    if n > 0 {
                        if let Err(e) = writer.write(&buf[..n]) {
                            *error2.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                            return;
                        }
                    } else if stop2.load(Ordering::Relaxed) {
                        break;
                    } else {
                        std::thread::sleep(POLL_INTERVAL);
                    }
                }
                if let Err(e) = writer.finish() {
                    *error2.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                }
            })
            .expect("spawns the recorder thread");
        Ok(Self { path, started: Instant::now(), stop, error, thread: Some(thread) })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// `Some` once the writer thread's hit an error on its own (a failed
    /// write or a failed `finish`) — taken once, same convention as
    /// `odj_core::audio::Errors`.
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Signals the thread to drain what's left and finish, joins it, and
    /// reports whichever error (if any) it hit along the way.
    pub fn stop(mut self) -> io::Result<PathBuf> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        match self.take_error() {
            Some(e) => Err(io::Error::other(e)),
            None => Ok(self.path.clone()),
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `~/.local/share/odj/recordings/<timestamp>.wav`, creating the
/// directory if needed; disambiguates with `-2`, `-3`, … on the rare
/// collision of two recordings started within the same second.
fn recording_path() -> io::Result<PathBuf> {
    let dir = odj_core::memory::data_dir().join("recordings");
    std::fs::create_dir_all(&dir)?;
    let base = chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string();
    let mut path = dir.join(format!("{base}.wav"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("{base}-{n}.wav"));
    }
    Ok(path)
}

/// Live-screen-only recording state, owned locally by `ui::run` — knows
/// nothing about keys or drawing; `ui::run` calls these in response to
/// `R` and shows the result through the existing status popup.
#[derive(Default)]
pub struct Recorder(Option<Recording>);

impl Recorder {
    pub fn is_active(&self) -> bool {
        self.0.is_some()
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.0.as_ref().map(Recording::elapsed)
    }

    pub fn start(&mut self, routing: &Mutex<Routing>) -> Result<PathBuf> {
        ensure!(self.0.is_none(), "already recording");
        let rate = routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .main_output_rate()
            .ok_or_else(|| anyhow!("nothing's playing yet — add a channel before recording"))?;
        let path = recording_path()?;
        let (prod, cons) = HeapRb::<f32>::new(REC_RING_STEREO_SAMPLES).split();
        let rec = Recording::start(path.clone(), 2, rate, cons)?;
        routing.lock().unwrap_or_else(|e| e.into_inner()).set_main_output_tap(Some(prod));
        self.0 = Some(rec);
        Ok(path)
    }

    pub fn stop(&mut self, routing: &Mutex<Routing>) -> Result<PathBuf> {
        let rec = self.0.take().ok_or_else(|| anyhow!("not recording"))?;
        routing.lock().unwrap_or_else(|e| e.into_inner()).set_main_output_tap(None);
        Ok(rec.stop()?)
    }

    /// Checked every UI loop tick: `Some` once the writer thread's hit an
    /// error on its own (full disk, permissions) mid-recording — tears
    /// the tap down the same way `stop` does, so a dead recording
    /// doesn't block the next `R` with "already recording".
    pub fn poll_error(&mut self, routing: &Mutex<Routing>) -> Option<String> {
        let e = self.0.as_ref()?.take_error()?;
        self.0 = None;
        routing.lock().unwrap_or_else(|e| e.into_inner()).set_main_output_tap(None);
        Some(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points `XDG_DATA_HOME` at a fresh temp directory for the span of
    /// one test — same isolation trick `log.rs`'s own tests use, since
    /// tests across the crate share this process's environment.
    fn isolate() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("odj-mixer-recorder-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &root);
        }
        root
    }

    #[test]
    fn recording_writes_pushed_samples_to_a_playable_wav_file() {
        let root = isolate();
        let path = root.join("take.wav");
        let (mut prod, cons) = HeapRb::<f32>::new(64).split();
        use ringbuf::traits::Producer;
        let samples = [0.1f32, -0.2, 0.3, -0.4];
        prod.push_slice(&samples);

        let rec = Recording::start(path.clone(), 2, 48_000, cons).unwrap();
        // Give the writer thread a moment to drain what's already queued.
        std::thread::sleep(Duration::from_millis(50));
        assert!(rec.take_error().is_none());
        let saved = rec.stop().unwrap();
        assert_eq!(saved, path);

        let decoded = odj_core::track::decode(&path).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.samples.len(), samples.len());
        for (i, &s) in samples.iter().enumerate() {
            assert!((decoded.samples[i] - s).abs() < 0.01, "sample {i}: {} vs {s}", decoded.samples[i]);
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn recorder_start_is_an_error_without_anything_routed_to_a_main_output() {
        let routing = Mutex::new(Routing::default());
        let mut recorder = Recorder::default();
        assert!(recorder.start(&routing).is_err());
        assert!(!recorder.is_active());
    }

    #[test]
    fn recorder_stop_is_an_error_when_not_recording() {
        let routing = Mutex::new(Routing::default());
        let mut recorder = Recorder::default();
        assert!(recorder.stop(&routing).is_err());
    }

    #[test]
    fn recorder_poll_error_is_none_when_idle() {
        let routing = Mutex::new(Routing::default());
        let mut recorder = Recorder::default();
        assert!(recorder.poll_error(&routing).is_none());
    }
}
