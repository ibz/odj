//! Searching YouTube: runs on a background thread so the UI never blocks.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use youtube_dl::{SearchOptions, YoutubeDl};

#[derive(Clone, Debug, PartialEq)]
pub struct SearchResult {
    /// The raw YouTube video id (not the whole URL) — `YtDlp::download`'s
    /// own output template always brackets this in the downloaded file's
    /// name, so a previous download (this session, an earlier one, or for
    /// a different crate) can be found by it; see `find_downloaded`.
    pub id: String,
    pub title: String,
    pub url: String,
    pub channel: Option<String>,
    pub duration: Option<f64>,
}

/// A file already sitting in `downloads_dir` for `id`, if any — relies on
/// `YtDlp::download`'s `-o` template always bracketing the video id in
/// the filename it writes.
pub fn find_downloaded(downloads_dir: &Path, id: &str) -> Option<PathBuf> {
    let marker = format!("[{id}]");
    std::fs::read_dir(downloads_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().contains(&marker)))
}

/// How results are actually fetched and files actually downloaded — a seam
/// so tests exercise queue/search state without a network or a `yt-dlp`
/// binary on PATH.
pub trait Fetcher: Send + Sync {
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String>;
    /// Downloads `url` into `into`, calling `on_progress` with a percent
    /// (0-100) each time `yt-dlp` reports one. There's no guarantee it's
    /// called at all (e.g. the size isn't known yet) or that values are
    /// monotonic (separately-fetched video/audio streams each report their
    /// own percent). Returns the downloaded file's own final path, so the
    /// caller can register it into a crate.
    fn download(&self, url: &str, into: &Path, on_progress: &dyn Fn(f32)) -> Result<PathBuf, String>;
}

/// The real thing: shells out to `yt-dlp`.
pub struct YtDlp;

/// Prefixes the one progress line we ask `yt-dlp` to print per update, so
/// it's unambiguous among its other stdout chatter.
const PROGRESS_PREFIX: &str = "ODJ-PROGRESS ";
/// Prefixes the one line `--print after_move:…` emits once the file is in
/// its final place (after any post-processing/embedding), giving us the
/// path to register with the crate.
const PATH_PREFIX: &str = "ODJ-PATH:";

impl Fetcher for YtDlp {
    /// Uses the `youtube_dl` crate, which runs `yt-dlp -J` (dump JSON, no
    /// download) and parses the result — exactly what a search needs, with
    /// no need for progress. `--flat-playlist` matters a lot here: without
    /// it, yt-dlp fully resolves each result (formats included) before
    /// returning, turning a search into N full video extractions — tens of
    /// seconds instead of about one.
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String> {
        let options = SearchOptions::youtube(query).with_count(20);
        let output = YoutubeDl::search_for(&options).flat_playlist(true).run().map_err(describe)?;
        let entries = output.into_playlist().and_then(|p| p.entries).unwrap_or_default();
        Ok(entries
            .into_iter()
            .map(|v| SearchResult {
                title: v.title.unwrap_or_else(|| v.id.clone()),
                url: v.webpage_url.unwrap_or_else(|| format!("https://www.youtube.com/watch?v={}", v.id)),
                channel: v.channel,
                duration: v.duration.as_ref().and_then(|d| d.as_f64()),
                id: v.id,
            })
            .collect())
    }

    /// Downloads the best video+audio exactly as published — no
    /// re-encoding — for odj-player to eventually play video too. Prefers
    /// an audio stream that isn't Opus: odj-player's decoder (`symphonia`)
    /// has no Opus codec at all, and YouTube's default best-audio choice
    /// almost always is Opus, which would otherwise mux in a track nothing
    /// can decode. This only ever picks among streams YouTube already
    /// offers — e.g. AAC instead of Opus — never a transcode; falls back
    /// to the plain best if nothing else is offered. Embeds the video's
    /// own title/uploader as container tags. Spawns `yt-dlp` directly
    /// rather than through the `youtube_dl` crate: its `download_to`
    /// passes `--no-progress` and only returns once the process exits,
    /// with no way to observe progress as it happens. The explicit `-o`
    /// template always brackets the video id in the filename — not
    /// yt-dlp's own default, but relied on by `find_downloaded` to spot
    /// a file from a previous download without re-fetching it.
    fn download(&self, url: &str, into: &Path, on_progress: &dyn Fn(f32)) -> Result<PathBuf, String> {
        let mut child = Command::new("yt-dlp")
            .args(["--newline", "--progress-template", &format!("download:{PROGRESS_PREFIX}%(progress._percent_str)s")])
            .args(["--print", &format!("after_move:{PATH_PREFIX}%(filepath)s")])
            .args(["-f", "bestvideo+bestaudio[acodec!*=opus]/best"])
            .args(["-o", "%(title)s [%(id)s].%(ext)s"])
            .args(["--no-simulate", "--embed-metadata", "-P"])
            .arg(into)
            .arg(url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run yt-dlp: {e}"))?;

        let stderr = child.stderr.take().expect("stderr was piped");
        let stderr_thread = thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            text
        });

        let stdout = child.stdout.take().expect("stdout was piped");
        let mut final_path = None;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(pct) = line.strip_prefix(PROGRESS_PREFIX).and_then(|s| s.trim().trim_end_matches('%').parse().ok())
            {
                on_progress(pct);
            } else if let Some(path) = line.strip_prefix(PATH_PREFIX) {
                final_path = Some(PathBuf::from(path));
            }
        }

        let status = child.wait().map_err(|e| e.to_string())?;
        let stderr_text = stderr_thread.join().unwrap_or_default();
        if !status.success() {
            return Err(stderr_text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed").to_string());
        }
        final_path.ok_or_else(|| "yt-dlp didn't report the downloaded file's path".to_string())
    }
}

/// A one-line description of a `youtube_dl` error, suitable for the queue's
/// status display.
fn describe(e: youtube_dl::Error) -> String {
    match e {
        youtube_dl::Error::ExitCode { stderr, .. } => stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("yt-dlp failed")
            .to_string(),
        youtube_dl::Error::Io(e) => format!("could not run yt-dlp: {e}"),
        other => other.to_string(),
    }
}

/// Spawns a thread running `fetcher.search(query)`, returning a receiver for
/// its result.
pub fn search_in_background(
    fetcher: Arc<dyn Fetcher>,
    query: String,
) -> mpsc::Receiver<Result<Vec<SearchResult>, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(fetcher.search(&query));
    });
    rx
}

#[cfg(test)]
pub mod test_util {
    use super::*;
    use std::sync::Mutex;

    /// A `Fetcher` that returns canned results and records what was
    /// "downloaded", for tests that need neither network access nor a
    /// `yt-dlp` binary.
    pub struct FakeFetcher {
        results: Vec<SearchResult>,
        fail: Option<String>,
        pub downloaded: Mutex<Vec<String>>,
    }

    impl FakeFetcher {
        pub fn new(results: Vec<SearchResult>) -> Self {
            Self { results, fail: None, downloaded: Mutex::new(Vec::new()) }
        }

        pub fn failing(reason: &str) -> Self {
            Self { results: Vec::new(), fail: Some(reason.to_string()), downloaded: Mutex::new(Vec::new()) }
        }
    }

    impl Fetcher for FakeFetcher {
        fn search(&self, _query: &str) -> Result<Vec<SearchResult>, String> {
            match &self.fail {
                Some(reason) => Err(reason.clone()),
                None => Ok(self.results.clone()),
            }
        }

        fn download(&self, url: &str, into: &Path, on_progress: &dyn Fn(f32)) -> Result<PathBuf, String> {
            match &self.fail {
                Some(reason) => Err(reason.clone()),
                None => {
                    on_progress(50.0);
                    self.downloaded.lock().unwrap().push(url.to_string());
                    // A real (tiny) WAV, not just a touched empty file —
                    // tests that register a finished download need
                    // something `odj_core::track::decode` can actually read.
                    let _ = std::fs::create_dir_all(into);
                    let path = into.join(format!("{}.wav", slug(url)));
                    odj_core::wav::write_wav(&path, &[0.1, -0.1, 0.2, -0.2], 2, 44_100).map_err(|e| e.to_string())?;
                    Ok(path)
                }
            }
        }
    }

    /// Filesystem-safe stand-in for a URL, so each one gets its own file.
    fn slug(url: &str) -> String {
        url.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
    }

    pub fn result(title: &str) -> SearchResult {
        SearchResult {
            id: slug(title),
            title: title.to_string(),
            url: format!("https://example.com/{title}"),
            channel: Some("Some Channel".to_string()),
            duration: Some(185.0),
        }
    }
}
