//! Starter drum kits, offered on first run (no `kit.json` yet, see
//! `main.rs`): download a kit from the companion `odj-kits` repo's
//! manifest, use the tiny kit embedded in the binary (no network needed),
//! or skip and build a kit from scratch as usual.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use serde::Deserialize;

use odj_core::memory::data_dir;
use odj_core::track::decode;
use odj_core::tui::{AMBER, centered};

use crate::engine::{PADS, Props};
use crate::kit::{Kit, KitPad, PadSource};

const MANIFEST_URL: &str = "https://raw.githubusercontent.com/ibz/odj-kits/main/manifest.json";
const KITS_BASE_URL: &str = "https://raw.githubusercontent.com/ibz/odj-kits/main/kits";

#[derive(Debug, Deserialize)]
struct Manifest {
    kits: Vec<KitEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct KitEntry {
    name: String,
    slug: String,
    license: String,
    credit: String,
    pads: Vec<String>,
}

struct EmbeddedFile {
    name: &'static str,
    bytes: &'static [u8],
}

/// A handful of pads, not all 16 — enough to be immediately playable with
/// zero network and zero setup. See `crates/odj-sampler/assets/starter/`.
const EMBEDDED: &[EmbeddedFile] = &[
    EmbeddedFile { name: "kick.wav", bytes: include_bytes!("../assets/starter/kick.wav") },
    EmbeddedFile { name: "snare.wav", bytes: include_bytes!("../assets/starter/snare.wav") },
    EmbeddedFile { name: "hihat_closed.wav", bytes: include_bytes!("../assets/starter/hihat_closed.wav") },
    EmbeddedFile { name: "hihat_open.wav", bytes: include_bytes!("../assets/starter/hihat_open.wav") },
    EmbeddedFile { name: "tom1.wav", bytes: include_bytes!("../assets/starter/tom1.wav") },
    EmbeddedFile { name: "tom2.wav", bytes: include_bytes!("../assets/starter/tom2.wav") },
    EmbeddedFile { name: "crash.wav", bytes: include_bytes!("../assets/starter/crash.wav") },
    EmbeddedFile { name: "clap.wav", bytes: include_bytes!("../assets/starter/clap.wav") },
];

enum Row {
    Embedded,
    Remote(KitEntry),
    Skip,
}

impl Row {
    fn label(&self) -> &str {
        match self {
            Row::Embedded => "Embedded — 8 pads, built in, no download",
            Row::Remote(entry) => &entry.name,
            Row::Skip => "Skip — use my own samples",
        }
    }
}

/// Lets the user pick a starter kit — downloaded, embedded, or none at all
/// — and returns it ready to save. `Ok(None)` means "leave the kit empty",
/// same meaning as today: the user skipped, or backed out of a download.
pub fn pick_starter_kit(terminal: &mut DefaultTerminal) -> Result<Option<Kit>> {
    let manifest = fetch_manifest();
    let rows = build_rows(manifest);
    let mut selected = 0;
    loop {
        terminal.draw(|f| draw_picker(f, &rows, selected))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(rows.len() - 1),
            KeyCode::Enter => match &rows[selected] {
                Row::Skip => return Ok(None),
                Row::Embedded => return Ok(Some(build_kit(&materialize_embedded()))),
                // A failed or cancelled download falls back to the list
                // rather than giving up on the whole picker.
                Row::Remote(entry) => {
                    if let Some(kit) = download_kit(terminal, entry)? {
                        return Ok(Some(kit));
                    }
                }
            },
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Esc | KeyCode::Char('q' | 'Q' | 'n' | 'N') => return Ok(None),
            _ => {}
        }
    }
}

fn build_rows(manifest: Option<Manifest>) -> Vec<Row> {
    let mut rows = vec![Row::Embedded];
    rows.extend(manifest.into_iter().flat_map(|m| m.kits).map(Row::Remote));
    rows.push(Row::Skip);
    rows
}

fn draw_picker(f: &mut Frame, rows: &[Row], selected: usize) {
    let area = centered(f.area(), 70, 60);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Starter kit ")
        .title_bottom(" ↑↓ select  Enter choose  Esc/N skip  Ctrl+C quit ")
        .border_style(Style::new().fg(AMBER));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [list_area, detail_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(inner);

    let items: Vec<ListItem> = rows.iter().map(|r| ListItem::new(r.label())).collect();
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, list_area, &mut state);

    let detail = match &rows[selected] {
        Row::Remote(entry) => format!("{} — {}", entry.license, entry.credit),
        Row::Embedded => "Plays instantly, offline; fewer pads than a downloaded kit.".to_string(),
        Row::Skip => "Pads stay empty — assign your own samples from odj-player's cues later.".to_string(),
    };
    let paragraph = Paragraph::new(detail).wrap(Wrap { trim: true }).style(Style::new().fg(AMBER));
    f.render_widget(paragraph, detail_area);
}

enum DownloadMsg {
    Progress(usize, usize),
    Done(Vec<(PathBuf, String)>),
    Failed(String),
}

/// Downloads `entry`'s files on a background thread (progress relayed over
/// an `mpsc` channel, same shape as odj-downloader's download worker),
/// while the draw loop keeps redrawing a progress screen. `Esc`/`Ctrl+C`
/// sets a shared cancel flag the thread checks between files. `Ok(None)`
/// covers both cancellation and failure (already shown to the user).
fn download_kit(terminal: &mut DefaultTerminal, entry: &KitEntry) -> Result<Option<Kit>> {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = entry.clone();
    let job_cancel = cancel.clone();
    let handle = std::thread::spawn(move || run_download(job, job_cancel, tx));

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
                return Ok(Some(build_kit(&files)));
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

fn run_download(entry: KitEntry, cancel: Arc<AtomicBool>, tx: mpsc::Sender<DownloadMsg>) {
    let dir = data_dir().join("kits").join(&entry.slug);
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
fn fetch_manifest() -> Option<Manifest> {
    let bytes = fetch_bytes(&new_agent(), MANIFEST_URL).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn pad_title(file_name: &str) -> String {
    file_name.strip_suffix(".wav").unwrap_or(file_name).replace('_', " ")
}

/// Writes the embedded WAVs out to `$XDG_DATA_HOME/odj/kits/embedded/` once
/// (skipped if already there) so [`decode`] has a real path to read, same
/// as a downloaded kit's files.
fn materialize_embedded() -> Vec<(PathBuf, String)> {
    let dir = data_dir().join("kits").join("embedded");
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    EMBEDDED
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

/// Builds a `Kit` from decoded files, one per pad in order — shared by the
/// embedded and downloaded paths. A file that fails to decode just leaves
/// that pad empty rather than failing the whole kit; fewer than `PADS`
/// files leaves the rest empty too.
fn build_kit(files: &[(PathBuf, String)]) -> Kit {
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
    Kit::new(pads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &std::path::Path, seconds: f64) {
        // 1 second of silence at 8kHz mono — tiny, just needs to decode.
        let rate = 8_000u32;
        let frames = (rate as f64 * seconds) as u32;
        let data_len = frames * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
        bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
        bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.extend(std::iter::repeat_n(0u8, data_len as usize));
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn build_kit_fills_pads_in_order_and_skips_what_wont_decode() {
        let dir = std::env::temp_dir().join(format!("odj-starter-kit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.wav");
        write_wav(&good, 0.5);
        let bad = dir.join("bad.wav");
        std::fs::write(&bad, b"not a wav file").unwrap();

        let kit = build_kit(&[(good.clone(), "Good".into()), (bad, "Bad".into())]);

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
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only, same pattern odj-player's tests already use to
        // point `data_dir()` at a scratch directory instead of the user's.
        unsafe { std::env::set_var("XDG_DATA_HOME", &dir) };
        let kit = build_kit(&materialize_embedded());
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        assert!(kit.pads[0].source.is_some(), "the embedded kit should decode and fill its pads");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "hits the real network"]
    fn fetch_manifest_reaches_the_real_repo() {
        assert!(fetch_manifest().is_some(), "the published manifest should fetch and parse");
    }
}
