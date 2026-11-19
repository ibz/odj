//! Streams a ring-buffer consumer into an [`odj_core::wav::WavWriter`] on
//! its own background thread, the same shape as [`crate::devices::Devices`].
//! [`Recording`] is reused once per actively-capturing record-mode track
//! (see `record_mode.rs`), not just for one main-output recording — it
//! knows nothing about `Routing`, only about draining whatever ring
//! buffer it's handed.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ringbuf::traits::Consumer;

use odj_core::wav::WavWriter;

/// How long the writer thread sleeps between polls when there's nothing
/// to write and it hasn't been told to stop yet.
const POLL_INTERVAL: Duration = Duration::from_millis(15);

/// A WAV recording in progress, written by a background thread until
/// [`stop`](Self::stop) (or [`Drop`], if `stop` was never called — an
/// abrupt quit mid-recording still leaves a complete, playable file).
pub struct Recording {
    path: PathBuf,
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
        Ok(Self { path, stop, error, thread: Some(thread) })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp directory for the span of one test — tests across
    /// the crate share this process's filesystem, so each test needs
    /// its own.
    fn isolate() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("odj-mixer-recorder-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn recording_writes_pushed_samples_to_a_playable_wav_file() {
        use ringbuf::HeapRb;
        use ringbuf::traits::{Producer, Split};

        let root = isolate();
        let path = root.join("take.wav");
        let (mut prod, cons) = HeapRb::<f32>::new(64).split();
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
}
