//! Choosing which kit is active: an existing kit, the built-in Embedded
//! kit, a kit from the `odj-kits` manifest, or creating a new empty one.
//!
//! [`pick_initial_kit`] is the blocking, mandatory bootstrap picker used
//! once before `App` exists (see `main.rs`) — there's always something to
//! pick, even on a fresh install, so unlike the other startup pickers it
//! can't be skipped. The in-app switch key (`k` on the Pads screen, see
//! `app.rs`'s `Screen::KitBrowser`) reuses [`KitRow`]/[`build_rows`] and
//! [`materialize_local`] for its own, simpler, non-blocking version: no
//! network there, so no `Remote` rows and no progress screen needed.

use std::path::PathBuf;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use odj_core::tui::{AMBER, centered};

use crate::kit::Kit;
use crate::kits;
use crate::starter_kit::{self, KitEntry};

/// The embedded kit's name, both for its label in the list and (once it's
/// actually saved — see [`materialize_local`]) the `Kit.name` it's given.
pub const EMBEDDED_NAME: &str = "Acoustic (Embedded)";

/// Where the embedded kit's assets live once materialized — shaped like a
/// real kit directory (a `samples/` folder) so the sample browser's
/// "browse another kit" navigation can reach it (see `app.rs`'s
/// `build_browser_entries`), but with no `kit.json` of its own, so
/// `kits::list_kits()` never lists it as a real, switchable kit.
pub fn embedded_cache_dir() -> PathBuf {
    kits::kits_dir().join(".embedded")
}

pub enum KitRow {
    Existing(PathBuf, String),
    Embedded,
    Remote(KitEntry),
    CreateNew,
}

/// What picking a row produces: a real kit already on disk, or (just the
/// built-in kit, for now) one that only exists in memory so far — nothing
/// is written until it's actually changed, see `App::ensure_kit_dir`.
pub enum MaterializedKit {
    Saved(PathBuf),
    Unsaved(Box<Kit>),
}

impl KitRow {
    pub fn label(&self) -> String {
        match self {
            KitRow::Existing(_, name) => name.clone(),
            KitRow::Embedded => EMBEDDED_NAME.to_string(),
            KitRow::Remote(entry) => entry.name.clone(),
            KitRow::CreateNew => "Create new kit…".to_string(),
        }
    }

    /// A distinguishing color for this row in the list — just the built-in
    /// kit, for now, so it stands out as the always-available option.
    pub fn color(&self) -> Option<Color> {
        match self {
            KitRow::Embedded => Some(Color::Cyan),
            _ => None,
        }
    }

    pub fn detail(&self) -> String {
        match self {
            KitRow::Existing(..) | KitRow::Embedded => String::new(),
            KitRow::Remote(entry) => format!("{} — {}", entry.license, entry.credit),
            KitRow::CreateNew => "Starts with all 16 pads empty.".to_string(),
        }
    }
}

/// Existing kits, the built-in kit, optionally the manifest's kits (a
/// network fetch — only worth it for the one-time bootstrap picker, not
/// every time the in-app browser opens), and "create new" last.
pub fn build_rows(include_remote: bool) -> Vec<KitRow> {
    let mut rows: Vec<KitRow> = kits::list_kits().into_iter().map(|(dir, name)| KitRow::Existing(dir, name)).collect();
    rows.push(KitRow::Embedded);
    if include_remote && let Some(manifest) = starter_kit::fetch_manifest() {
        rows.extend(manifest.kits.into_iter().map(KitRow::Remote));
    }
    rows.push(KitRow::CreateNew);
    rows
}

/// Resolves a row with no network and no terminal interaction — `Existing`,
/// `Embedded` and `CreateNew` are all instant. `None` for `Remote` (never
/// offered in-app, see [`build_rows`]) or an empty/blank new-kit name.
pub fn materialize_local(row: &KitRow, name: Option<&str>) -> Option<MaterializedKit> {
    match row {
        KitRow::Existing(dir, _) => Some(MaterializedKit::Saved(dir.clone())),
        KitRow::Embedded => Some(MaterializedKit::Unsaved(Box::new(embedded_kit()))),
        KitRow::CreateNew => {
            let name = name?.trim();
            if name.is_empty() {
                return None;
            }
            kits::create_kit(name).ok().map(MaterializedKit::Saved)
        }
        KitRow::Remote(_) => None,
    }
}

/// Builds the embedded kit from the binary's bundled assets, written into
/// [`embedded_cache_dir`] rather than a new kit directory of its own.
/// Picking this doesn't write anything kit-shaped to disk until it's
/// actually changed (see `App::ensure_kit_dir`), at which point the pads
/// built here get saved under a real kit directory for the first time.
fn embedded_kit() -> Kit {
    let files = starter_kit::materialize_embedded(&embedded_cache_dir().join("samples"));
    starter_kit::build_kit(EMBEDDED_NAME, &files)
}

enum Mode {
    List,
    Naming(String),
    /// About to delete `rows[_]` — not acted on until a plain `Enter`
    /// confirms it; anything else backs out (see `draw_delete_warning`).
    ConfirmingDelete(usize),
}

/// The one-time, mandatory "pick a kit to start with" picker — run only
/// when nothing is active yet (fresh install, or nothing left to migrate).
pub fn pick_initial_kit(terminal: &mut DefaultTerminal) -> Result<MaterializedKit> {
    let mut rows = build_rows(true);
    let mut selected = 0;
    let mut mode = Mode::List;
    loop {
        terminal.draw(|f| match &mode {
            Mode::List => draw_picker(f, &rows, selected),
            Mode::Naming(name) => draw_naming(f, name),
            Mode::ConfirmingDelete(i) => draw_delete_warning(f, &rows[*i].label()),
        })?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match &mut mode {
            Mode::List => match key.code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(rows.len() - 1),
                KeyCode::Enter => match &rows[selected] {
                    KitRow::CreateNew => mode = Mode::Naming(String::new()),
                    KitRow::Existing(dir, _) => return Ok(MaterializedKit::Saved(dir.clone())),
                    KitRow::Embedded => return Ok(MaterializedKit::Unsaved(Box::new(embedded_kit()))),
                    KitRow::Remote(entry) => {
                        let entry = entry.clone();
                        let dir = kits::create_kit(&entry.name)?;
                        match starter_kit::download_kit(terminal, &entry, &dir.join("samples"))? {
                            Some(files) => {
                                starter_kit::build_kit(&entry.name, &files).save(&dir.join("kit.json"))?;
                                return Ok(MaterializedKit::Saved(dir));
                            }
                            // Cancelled or failed (already shown): don't
                            // leave an empty kit directory behind.
                            None => drop(std::fs::remove_dir_all(&dir)),
                        }
                    }
                },
                KeyCode::Delete | KeyCode::Backspace if matches!(rows[selected], KitRow::Existing(..)) => {
                    mode = Mode::ConfirmingDelete(selected);
                }
                // No skip/Esc here: this only ever runs when nothing exists yet.
                _ => {}
            },
            Mode::Naming(name) => {
                let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                match key.code {
                    KeyCode::Char(c) if typing => name.push(c),
                    KeyCode::Backspace => {
                        name.pop();
                    }
                    KeyCode::Enter if !name.trim().is_empty() => {
                        return Ok(MaterializedKit::Saved(kits::create_kit(name.trim())?));
                    }
                    KeyCode::Esc => mode = Mode::List,
                    _ => {}
                }
            }
            // Only a plain Enter confirms; anything else cancels, so a
            // deletion this permanent never happens by reflex.
            Mode::ConfirmingDelete(i) => match key.code {
                KeyCode::Enter => {
                    let KitRow::Existing(dir, _) = &rows[*i] else { mode = Mode::List; continue };
                    let _ = std::fs::remove_dir_all(dir);
                    // Just drop the row rather than rebuilding the whole
                    // list, which would re-hit the network for no reason.
                    rows.remove(*i);
                    selected = selected.min(rows.len().saturating_sub(1));
                    mode = Mode::List;
                }
                _ => mode = Mode::List,
            },
        }
    }
}

fn draw_picker(f: &mut Frame, rows: &[KitRow], selected: usize) {
    let area = centered(f.area(), 70, 60);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Pick a kit ")
        .title_bottom(" ↑↓ select  Enter choose  Del/Backspace delete ")
        .border_style(Style::new().fg(AMBER));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [list_area, detail_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(inner);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let item = ListItem::new(r.label());
            match r.color() {
                Some(c) => item.style(Style::new().fg(c)),
                None => item,
            }
        })
        .collect();
    let list =
        List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, list_area, &mut state);

    let paragraph =
        Paragraph::new(rows[selected].detail()).wrap(Wrap { trim: true }).style(Style::new().fg(AMBER));
    f.render_widget(paragraph, detail_area);
}

fn draw_naming(f: &mut Frame, name: &str) {
    let area = centered(f.area(), 60, 20);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Name the new kit ")
        .title_bottom(" Enter create  Esc back ")
        .border_style(Style::new().fg(AMBER));
    let paragraph = Paragraph::new(format!("{name}_")).style(Style::new().fg(AMBER)).block(block);
    f.render_widget(paragraph, area);
}

/// A hard-to-miss, all-red confirmation before a kit (its whole directory,
/// every sample in it) is deleted with no way to undo it.
fn draw_delete_warning(f: &mut Frame, kit_name: &str) {
    let area = centered(f.area(), 60, 30);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(Span::styled(" DELETE KIT ", Style::new().fg(Color::Black).bg(Color::Red).bold()))
        .title_bottom(" Enter: DELETE permanently   any other key: cancel ")
        .border_style(Style::new().fg(Color::Red));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(format!("Delete \"{kit_name}\"?"), Style::new().fg(Color::Red).bold())),
        Line::from(""),
        Line::from(Span::styled(
            "THIS DELETES THE WHOLE KIT FOLDER — EVERY SAMPLE IN IT.",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )),
        Line::from(Span::styled("THIS CANNOT BE UNDONE.", Style::new().fg(Color::Red).bold())),
    ];
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center).wrap(Wrap { trim: true }), inner);
}
