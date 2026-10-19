//! The download queue: one item downloads at a time, in arrival order.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use crate::search::Fetcher;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Queued,
    /// Percent complete (0-100), the highest seen so far — separately
    /// fetched video/audio streams each report their own percent, so raw
    /// updates aren't monotonic; 0 also covers "downloading, no progress
    /// reported yet".
    Downloading(f32),
    Done,
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
                let best = std::cell::Cell::new(0.0f32);
                let status = match fetcher.download(&url, &dest, &|pct| {
                    if pct > best.get() {
                        best.set(pct);
                        let _ = status_tx.send((seq, Status::Downloading(pct)));
                    }
                }) {
                    Ok(()) => Status::Done,
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

    #[test]
    fn downloads_one_at_a_time_in_arrival_order() {
        let fetcher = Arc::new(FakeFetcher::new(Vec::new()));
        let worker = DownloadWorker::start(fetcher.clone(), PathBuf::from("/tmp"));
        let timeout = Duration::from_secs(2);
        worker.enqueue(1, "https://a".into());
        worker.enqueue(2, "https://b".into());

        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(0.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(50.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Done));
        assert_eq!(recv_within(&worker.status_rx, timeout), (2, Status::Downloading(0.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (2, Status::Downloading(50.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (2, Status::Done));
        assert_eq!(*fetcher.downloaded.lock().unwrap(), vec!["https://a".to_string(), "https://b".to_string()]);
    }

    #[test]
    fn failed_download_reports_the_reason() {
        let fetcher = Arc::new(FakeFetcher::failing("network unreachable"));
        let worker = DownloadWorker::start(fetcher, PathBuf::from("/tmp"));
        let timeout = Duration::from_secs(2);
        worker.enqueue(1, "https://a".into());

        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Downloading(0.0)));
        assert_eq!(recv_within(&worker.status_rx, timeout), (1, Status::Failed("network unreachable".into())));
    }
}
