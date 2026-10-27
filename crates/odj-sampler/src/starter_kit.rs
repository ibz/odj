//! Fetching/building starter kit content: the tiny kit embedded in the
//! binary (no network needed), or a kit from the companion `odj-kits`
//! repo's manifest. Pure logic, no UI — `kit_browser.rs` drives the picker
//! that offers these alongside existing kits and "create a new one."

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Alignment;
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use serde::Deserialize;

use odj_core::track::decode;
use odj_core::tui::{AMBER, centered};

use crate::engine::{PADS, Props};
use crate::kit::{Kit, KitPad, PadSource};

const MANIFEST_URL: &str = "https://raw.githubusercontent.com/ibz/odj-kits/main/manifest.json";
const KITS_BASE_URL: &str = "https://raw.githubusercontent.com/ibz/odj-kits/main/kits";

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub kits: Vec<KitEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KitEntry {
    pub name: String,
    pub slug: String,
    pub license: String,
    pub credit: String,
    pub pads: Vec<String>,
}

struct EmbeddedFile {
    name: &'static str,
    bytes: &'static [u8],
}

/// One of the kits built into the binary. `id` is persisted in the shared
/// settings file, so it must stay stable even if `name` changes.
pub struct EmbeddedKit {
    pub id: &'static str,
    pub name: &'static str,
    files: &'static [EmbeddedFile],
}

/// A handful of pads, not all 16 — enough to be immediately playable with
/// zero network and zero setup. See `crates/odj-sampler/assets/starter/`.
const ACOUSTIC_FILES: &[EmbeddedFile] = &[
    EmbeddedFile { name: "kick.wav", bytes: include_bytes!("../assets/starter/kick.wav") },
    EmbeddedFile { name: "snare.wav", bytes: include_bytes!("../assets/starter/snare.wav") },
    EmbeddedFile { name: "hihat_closed.wav", bytes: include_bytes!("../assets/starter/hihat_closed.wav") },
    EmbeddedFile { name: "hihat_open.wav", bytes: include_bytes!("../assets/starter/hihat_open.wav") },
    EmbeddedFile { name: "tom1.wav", bytes: include_bytes!("../assets/starter/tom1.wav") },
    EmbeddedFile { name: "tom2.wav", bytes: include_bytes!("../assets/starter/tom2.wav") },
    EmbeddedFile { name: "crash.wav", bytes: include_bytes!("../assets/starter/crash.wav") },
    EmbeddedFile { name: "clap.wav", bytes: include_bytes!("../assets/starter/clap.wav") },
];

const EMBEDDED_KITS: &[EmbeddedKit] = &[EmbeddedKit { id: "acoustic", name: "Acoustic (Embedded)", files: ACOUSTIC_FILES }];

/// Every kit built into this binary, in the order the pickers list them.
pub fn embedded_kits() -> &'static [EmbeddedKit] {
    EMBEDDED_KITS
}

/// The embedded kit with `id`, if it's still one of the built-in ones.
pub fn embedded_kit(id: &str) -> Option<&'static EmbeddedKit> {
    EMBEDDED_KITS.iter().find(|kit| kit.id == id)
}

enum DownloadMsg {
    Progress(usize, usize),
    Done(Vec<(PathBuf, String)>),
    Failed(String),
}

/// Downloads `entry`'s files into `dir` on a background thread (progress
/// relayed over an `mpsc` channel, same shape as odj-downloader's download
/// worker), while the draw loop keeps redrawing a progress screen.
/// `Esc`/`Ctrl+C` sets a shared cancel flag the thread checks between
/// files. `Ok(None)` covers both cancellation and failure (already shown).
pub fn download_kit(
    terminal: &mut DefaultTerminal,
    entry: &KitEntry,
    dir: &Path,
) -> Result<Option<Vec<(PathBuf, String)>>> {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = entry.clone();
    let job_cancel = cancel.clone();
    let job_dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || run_download(job, job_dir, job_cancel, tx));

    let (mut done, mut total) = (0, entry.pads.len());
    loop {
        terminal.draw(|f| draw_progress(f, &entry.name, done, total))?;
        if event::poll(Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && (key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            cancel.store(true, Ordering::Relaxed);
            let _ = handle.join();
            return Ok(None);
        }
        match rx.try_recv() {
            Ok(DownloadMsg::Progress(d, t)) => (done, total) = (d, t),
            Ok(DownloadMsg::Done(files)) => {
                let _ = handle.join();
                return Ok(Some(files));
            }
            Ok(DownloadMsg::Failed(message)) => {
                let _ = handle.join();
                show_error(terminal, &message)?;
                return Ok(None);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                let _ = handle.join();
                return Ok(None);
            }
        }
    }
}

fn run_download(entry: KitEntry, dir: PathBuf, cancel: Arc<AtomicBool>, tx: mpsc::Sender<DownloadMsg>) {
    if std::fs::create_dir_all(&dir).is_err() {
        let _ = tx.send(DownloadMsg::Failed("couldn't create the kit's folder".into()));
        return;
    }
    let agent = new_agent();
    let total = entry.pads.len();
    let mut files = Vec::with_capacity(total);
    for (i, file) in entry.pads.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let url = format!("{KITS_BASE_URL}/{}/{file}", entry.slug);
        let bytes = match fetch_bytes(&agent, &url) {
            Ok(bytes) => bytes,
            Err(e) => {
                let _ = tx.send(DownloadMsg::Failed(format!("downloading {file}: {e}")));
                return;
            }
        };
        let path = dir.join(file);
        if std::fs::write(&path, bytes).is_err() {
            let _ = tx.send(DownloadMsg::Failed(format!("couldn't save {file}")));
            return;
        }
        files.push((path, pad_title(file)));
        let _ = tx.send(DownloadMsg::Progress(i + 1, total));
    }
    let _ = tx.send(DownloadMsg::Done(files));
}

fn draw_progress(f: &mut Frame, name: &str, done: usize, total: usize) {
    let area = centered(f.area(), 60, 30);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(format!(" Downloading {name} "))
        .title_bottom(" Esc cancel ")
        .border_style(Style::new().fg(AMBER));
    let text = if total == 0 { "Starting…".to_string() } else { format!("{done} of {total} files") };
    let paragraph = Paragraph::new(text).alignment(Alignment::Center).style(Style::new().fg(AMBER)).block(block);
    f.render_widget(paragraph, area);
}

fn show_error(terminal: &mut DefaultTerminal, message: &str) -> Result<()> {
    loop {
        terminal.draw(|f| {
            let area = centered(f.area(), 60, 30);
            f.render_widget(Clear, area);
            let block = Block::bordered()
                .title(" Download failed ")
                .title_bottom(" any key: back to the list ")
                .border_style(Style::new().fg(AMBER));
            let paragraph = Paragraph::new(message)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(AMBER))
                .block(block);
            f.render_widget(paragraph, area);
        })?;
        if let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            return Ok(());
        }
    }
}

fn new_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build())
}

fn fetch_bytes(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let mut body = agent.get(url).call()?.into_body();
    Ok(body.read_to_vec()?)
}

/// `None` on any network or parse failure — the picker just shows fewer
/// rows, the same degrade-quietly behaviour as `midi::list_ports`.
pub fn fetch_manifest() -> Option<Manifest> {
    let bytes = fetch_bytes(&new_agent(), MANIFEST_URL).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn pad_title(file_name: &str) -> String {
    file_name.strip_suffix(".wav").unwrap_or(file_name).replace('_', " ")
}

/// Writes the embedded WAVs for `id` into `dir` once (skipped if already
/// there) so [`decode`] has a real path to read, same as a downloaded
/// kit's files. Unknown ids produce no files.
pub fn materialize_embedded(id: &str, dir: &Path) -> Vec<(PathBuf, String)> {
    let Some(kit) = embedded_kit(id) else { return Vec::new() };
    if std::fs::create_dir_all(dir).is_err() {
        return Vec::new();
    }
    kit.files
        .iter()
        .filter_map(|file| {
            let path = dir.join(file.name);
            if !path.is_file() && std::fs::write(&path, file.bytes).is_err() {
                return None;
            }
            Some((path, pad_title(file.name)))
        })
        .collect()
}

/// Builds a `Kit` from decoded files, one per pad in order. A file that
/// fails to decode just leaves that pad empty rather than failing the
/// whole kit; fewer than `PADS` files leaves the rest empty too.
pub fn build_kit(name: &str, files: &[(PathBuf, String)]) -> Kit {
    let mut pads: [KitPad; PADS] = Default::default();
    for (pad, (path, title)) in pads.iter_mut().zip(files.iter()) {
        let Ok(decoded) = decode(path) else { continue };
        *pad = KitPad {
            source: Some(PadSource {
                track_id: decoded.id,
                path: path.clone(),
                title: title.clone(),
                artist: None,
                sample_rate: decoded.sample_rate,
                cue: "kit".into(),
                start: 0.0,
                end: (decoded.samples.len() / 2) as f64,
            }),
            props: Props::default(),
        };
    }
    Kit::new(name.to_string(), pads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &std::path::Path, seconds: f64) {
        // Silence at 8kHz mono — tiny, just needs to decode.
        let rate = 8_000u32;
        let frames = (rate as f64 * seconds) as usize;
        odj_core::wav::write_wav(path, &vec![0.0; frames], 1, rate).unwrap();
    }

    #[test]
    fn build_kit_fills_pads_in_order_and_skips_what_wont_decode() {
        let dir = std::env::temp_dir().join(format!("odj-starter-kit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.wav");
        write_wav(&good, 0.5);
        let bad = dir.join("bad.wav");
        std::fs::write(&bad, b"not a wav file").unwrap();

        let kit = build_kit("Test", &[(good.clone(), "Good".into()), (bad, "Bad".into())]);

        let first = kit.pads[0].source.as_ref().expect("decodable file fills its pad");
        assert_eq!(first.path, good);
        assert_eq!(first.title, "Good");
        assert_eq!(first.start, 0.0);
        assert_eq!(first.cue, "kit");
        assert!(first.end > 0.0, "end should be the file's full length in frames");

        assert!(kit.pads[1].source.is_none(), "a file that fails to decode leaves its pad empty");
        assert!(kit.pads[2].source.is_none(), "pads past the given files stay empty");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pad_title_strips_extension_and_underscores() {
        assert_eq!(pad_title("hihat_closed.wav"), "hihat closed");
        assert_eq!(pad_title("kick.wav"), "kick");
    }

    #[test]
    fn manifest_parses_from_json() {
        let json = r#"{"kits": [{"name": "Acoustic", "slug": "acoustic-vcsl", "license": "CC0-1.0",
            "credit": "VCSL", "pads": ["kick.wav", "snare.wav"]}]}"#;
        let manifest: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.kits.len(), 1);
        assert_eq!(manifest.kits[0].slug, "acoustic-vcsl");
        assert_eq!(manifest.kits[0].pads, vec!["kick.wav", "snare.wav"]);
    }

    #[test]
    fn embedded_kit_builds_without_network() {
        let dir = std::env::temp_dir().join(format!("odj-starter-embedded-{}", std::process::id()));
        let kit = build_kit("Embedded", &materialize_embedded("acoustic", &dir));
        assert!(kit.pads[0].source.is_some(), "the embedded kit should decode and fill its pads");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn embedded_kits_have_unique_safe_ids() {
        let mut ids: Vec<&str> = embedded_kits().iter().map(|kit| kit.id).collect();
        assert!(ids.iter().all(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')));
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    #[ignore = "hits the real network"]
    fn fetch_manifest_reaches_the_real_repo() {
        assert!(fetch_manifest().is_some(), "the published manifest should fetch and parse");
    }
}
