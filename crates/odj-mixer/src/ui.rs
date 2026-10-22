//! The startup input checklist, and the live mixing screen.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style, Stylize};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use odj_core::audio::{self, Card, Choice, Host};
use odj_core::tui::{AMBER, DIM, centered};

use crate::MixedSource;

/// The live screen's volume step per `←`/`→` press; `Shift` multiplies it,
/// same convention as odj-player's own Mixer overlay.
const VOLUME_STEP: f32 = 0.05;

/// Lets the user toggle any number of inputs on — `Space` toggles the
/// selected row, `Enter` confirms once at least one is checked, `Esc`/
/// `Ctrl+C` backs out entirely (`None`). `Some((cards, Vec::new()))` if
/// PipeWire/ALSA has no usable inputs at all — same "nothing to pick, but
/// not an error" shape as `tui::pick_output` falling back to the default
/// device, except here there's no sensible default to fall back to, so the
/// caller treats an empty pick as "nothing to route" and exits quietly.
pub fn pick_inputs(terminal: &mut DefaultTerminal, host: &Host) -> Result<Option<(Vec<Card>, Vec<Choice>)>> {
    let cards = audio::input_cards(host)?;
    let choices = audio::choices(&cards);
    if choices.is_empty() {
        return Ok(Some((cards, Vec::new())));
    }
    let mut checked = vec![false; choices.len()];
    let mut selected = choices.iter().position(|c| c.available(&cards)).unwrap_or(0);
    loop {
        let items: Vec<(String, bool, bool)> =
            choices.iter().zip(&checked).map(|(c, &on)| (c.label(&cards, "In"), c.available(&cards), on)).collect();
        terminal.draw(|f| draw_input_picker(f, &items, selected))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(choices.len() - 1),
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = choices.len() - 1,
            KeyCode::Char(' ') if choices[selected].available(&cards) => checked[selected] = !checked[selected],
            KeyCode::Enter if checked.iter().any(|&on| on) => {
                let picked =
                    choices.iter().copied().zip(checked.iter().copied()).filter(|&(_, on)| on).map(|(c, _)| c).collect();
                return Ok(Some((cards, picked)));
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => return Ok(None),
            _ => {}
        }
    }
}

fn draw_input_picker(f: &mut Frame, items: &[(String, bool, bool)], selected: usize) {
    let area = centered(f.area(), 70, 70);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Audio inputs — toggle any number on ")
        .title_bottom(" ↑↓ select  Space toggle  Enter start routing  Ctrl+C quit ")
        .border_style(Style::new().fg(AMBER));
    let rows: Vec<ListItem> = items
        .iter()
        .map(|(label, ok, on)| {
            let mark = if *on { "[x] " } else { "[ ] " };
            let item = ListItem::new(format!("{mark}{label}"));
            if *ok { item } else { item.fg(DIM) }
        })
        .collect();
    let list = List::new(rows).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

/// The live screen while mixing runs: each routed input's volume, `↑↓` to
/// select one, `←→` to adjust it by [`VOLUME_STEP`] (`Shift`: ×4), `0` to
/// reset it to 100% — the exact same convention as odj-player's own Mixer
/// overlay (`M`). `q`/`Esc`/`Ctrl+C` quits (stops routing).
pub fn run(terminal: &mut DefaultTerminal, output_name: &str, mixed: &Arc<Mutex<MixedSource>>, errors: &audio::Errors) -> Result<()> {
    let names = mixed.lock().unwrap_or_else(|e| e.into_inner()).names();
    let mut selected = 0;
    let mut status: Option<String> = None;
    loop {
        if let Some(e) = errors.take() {
            status = Some(e);
        }
        let volumes: Vec<f32> = {
            let mixed = mixed.lock().unwrap_or_else(|e| e.into_inner());
            (0..names.len()).map(|i| mixed.volume(i)).collect()
        };
        terminal.draw(|f| draw_status(f, output_name, &names, &volumes, selected, status.as_deref()))?;
        if event::poll(std::time::Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            let step = if shift { VOLUME_STEP * 4.0 } else { VOLUME_STEP };
            match key.code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(names.len().saturating_sub(1)),
                KeyCode::Left => {
                    let mut mixed = mixed.lock().unwrap_or_else(|e| e.into_inner());
                    let v = mixed.volume(selected) - step;
                    mixed.set_volume(selected, v);
                }
                KeyCode::Right => {
                    let mut mixed = mixed.lock().unwrap_or_else(|e| e.into_inner());
                    let v = mixed.volume(selected) + step;
                    mixed.set_volume(selected, v);
                }
                KeyCode::Char('0') => mixed.lock().unwrap_or_else(|e| e.into_inner()).set_volume(selected, 1.0),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Ok(()),
                _ => {}
            }
        }
    }
}

fn draw_status(f: &mut Frame, output_name: &str, names: &[String], volumes: &[f32], selected: usize, status: Option<&str>) {
    let block = Block::bordered()
        .title(format!(" odj-mixer → {output_name} "))
        .title_bottom(" ↑↓ select  ←→ volume (Shift ×4)  0 reset  q/Esc/Ctrl+C quit ")
        .border_style(Style::new().fg(AMBER));
    let items: Vec<ListItem> = names
        .iter()
        .zip(volumes)
        .map(|(name, &v)| ListItem::new(format!("{name}   {:.0}%", v * 100.0)))
        .collect();
    let list = List::new(items).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, f.area(), &mut state);
    if let Some(s) = status {
        let area = centered(f.area(), 80, 20);
        f.render_widget(Clear, area);
        let warn = Block::bordered().title(" Error ").border_style(Style::new().fg(Color::Red));
        f.render_widget(Paragraph::new(s).block(warn), area);
    }
}
