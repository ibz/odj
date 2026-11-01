//! The download queue: one item downloads at a time, in arrival order.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use crate::search::Fetcher;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Queued,
    /// Percent complete (0-100) of whichever stream is downloading right
    /// now — a video+audio download is two separately fetched streams,
    /// each its own 0-100 run, so this can genuinely dip back down once
    /// when the second one starts; 0 also covers "downloading, no
    /// progress reported yet".
    Downloading(f32),
    /// The downloaded file's own final path, so it can be registered into
    /// the owning crate.
    Done(PathBuf),
    Failed(String),
}

pub struct QueueItem {
    /// Identifies this item across the channel back from the download
    /// worker; stable even as items are removed and indices shift.
    pub seq: u64,
    pub title: String,
    pub url: String,
    pub status: Status,
}

/// A single long-lived background thread that downloads whatever arrives
/// on `work_rx`, one at a time: blocking on `recv()` between items gives
/// "one download at a time" for free, with no scheduling of its own.
pub struct DownloadWorker {
    work_tx: mpsc::Sender<(u64, String)>,
    pub status_rx: mpsc::Receiver<(u64, Status)>,
}

impl DownloadWorker {
    pub fn start(fetcher: Arc<dyn Fetcher>, dest: PathBuf) -> Self {
        let (work_tx, work_rx) = mpsc::channel::<(u64, String)>();
        let (status_tx, status_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok((seq, url)) = work_rx.recv() {
                let _ = status_tx.send((seq, Status::Downloading(0.0)));
                // Forwarded as-is, not clamped to only-increasing: a
                // video+audio download is two separately fetched streams,
                // each its own 0-100 run, so a real dip back down when the
                // second one starts is expected — pinning the display to
                // the highest percent seen so far just made it look stuck
                // near 100% for however long the second stream took.
                let status = match fetcher.download(&url, &dest, &|pct| {
                    let _ = status_tx.send((seq, Status::Downloading(pct)));
                }) {
                    Ok(path) => Status::Done(path),
                    Err(e) => Status::Failed(e),
                };
                let _ = status_tx.send((seq, status));
            }
        });
        Self { work_tx, status_rx }
    }

    /// Queues a download; never blocks the caller.
    pub fn enqueue(&self, seq: u64, url: String) {
        let _ = self.work_tx.send((seq, url));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_util::FakeFetcher;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn recv_within(rx: &mpsc::Receiver<(u64, Status)>, timeout: Duration) -> (u64, Status) {
        let deadline = Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(msg) => return msg,
                Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("no status received in time: {e:?}"),
            }
        }
    }

    fn dest(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("odj-crates-queue-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A video+audio download is two separately fetched streams, each
    /// its own 0-100 run — reports a dip (as if the second stream just
    /// started) to confirm it isn't suppressed as "non-increasing".
    struct DippingFetcher;

    impl crate::search::Fetcher for DippingFetcher {
        fn search(&self, _query: &str) -> Result<Vec<crate::search::SearchResult>, String> {
            Ok(Vec::new())
        }

        fn download(&self, _url: &str, into: &Path, on_progress: &dyn Fn(f32)) -> Result<PathBuf, String> {
            on_progress(90.0);
            on_progress(20.0); // the "second stream" starting over
            let path = into.join("done.wav");
            std::fs::create_dir_all(into).map_err(|e| e.to_string())?;
            std::fs::write(&path, b"").map_err(|e| e.to_string())?;
            Ok(path)
        }
    }

    #[test]
    fn a_percent_dip_from_a_second_stream_is_not_suppressed() {
        let worker = DownloadWorker::start(Arc::new(DippingFetcher), dest("dip"));
        let timeout = Duration::from_secs(2);
        worker.enqueue(1, "https://a".into());

        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(0.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(90.0)));
        assert_eq!(
            recv_within(&worker.status_rx, timeout),
            (1, Status::Downloading(20.0)),
            "a real dip from a new stream must still reach the UI, not get clamped to the previous high"
        );
    }

    #[test]
    fn downloads_one_at_a_time_in_arrival_order() {
        let fetcher = Arc::new(FakeFetcher::new(Vec::new()));
        let dest = dest("order");
        let worker = DownloadWorker::start(fetcher.clone(), dest.clone());
        let timeout = Duration::from_secs(2);
        worker.enqueue(1, "https://a".into());
        worker.enqueue(2, "https://b".into());

        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(0.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(50.0)));
        let (seq, status) = recv_within(&worker.status_rx, timeout);
        assert_eq!(seq, 1);
        assert!(matches!(status, Status::Done(p) if p.is_file()));
        assert_eq!(recv_within(&worker.status_rx, timeout), (2, Status::Downloading(0.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (2, Status::Downloading(50.0)));
        let (seq, status) = recv_within(&worker.status_rx, timeout);
        assert_eq!(seq, 2);
        assert!(matches!(status, Status::Done(p) if p.is_file()));
        assert_eq!(*fetcher.downloaded.lock().unwrap(), vec!["https://a".to_string(), "https://b".to_string()]);
    }

    #[test]
    fn failed_download_reports_the_reason() {
        let fetcher = Arc::new(FakeFetcher::failing("network unreachable"));
        let worker = DownloadWorker::start(fetcher, dest("failed"));
        let timeout = Duration::from_secs(2);
        worker.enqueue(1, "https://a".into());

        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(0.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Failed("network unreachable".into())));
    }
}
