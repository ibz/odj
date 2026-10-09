//! Terminal plumbing both apps share: setup with key-release events, the startup
//! audio output picker, and the common colours.

use std::io::stdout;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState};
use ratatui::{DefaultTerminal, Frame};

use crate::audio;

pub const AMBER: Color = Color::Rgb(255, 170, 30);
pub const DIM: Color = Color::DarkGray;

/// Opens the full-screen terminal and asks for key-release events where the terminal
/// supports them (kitty keyboard protocol). The flag says whether it does.
pub fn init() -> (DefaultTerminal, bool) {
    let terminal = ratatui::init();
    let key_release = supports_keyboard_enhancement().unwrap_or(false);
    if key_release {
        // Release events for plain text keys also need "all keys as escape codes".
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        );
    }
    (terminal, key_release)
}

/// Undoes `init`.
pub fn restore(key_release: bool) {
    if key_release {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
}

/// Plays on the only stereo output there is; with more to choose from, or when a card
/// is unavailable, asks. `None` means the user quit the picker.
pub fn pick_output(terminal: &mut DefaultTerminal, host: &audio::Host) -> Result<Option<audio::Output>> {
    let cards = audio::cards(host)?;
    let choices = audio::choices(&cards);
    if choices.is_empty() {
        return audio::Output::open_default(host)
            .context("no audio outputs found (does your user have access to /dev/snd, e.g. via the `audio` group?)")
            .map(Some);
    }
    let all_available = choices.iter().all(|c| c.available(&cards));
    let mut stereo = choices.iter().filter(|c| matches!(c.route, audio::Route::Stereo(..)));
    if let (true, Some(only), None) = (all_available, stereo.next(), stereo.next()) {
        return audio::Output::open(&cards, only).map(Some);
    }
    let items: Vec<(String, bool)> = choices.iter().map(|c| (c.label(&cards), c.available(&cards))).collect();
    let mut selected = items.iter().position(|(_, ok)| *ok).unwrap_or(0);
    loop {
        terminal.draw(|f| draw_output_picker(f, &items, selected))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(items.len() - 1),
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = items.len() - 1,
            KeyCode::Enter if items[selected].1 => {
                return audio::Output::open(&cards, &choices[selected]).map(Some);
            }
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => return Ok(None),
            _ => {}
        }
    }
}

/// Startup list of sound card outputs. Unavailable entries (`false`) are dimmed.
fn draw_output_picker(f: &mut Frame, items: &[(String, bool)], selected: usize) {
    let area = centered(f.area(), 70, 70);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Audio output ")
        .title_bottom(" ↑↓ select  Enter use  Esc quit ")
        .border_style(Style::new().fg(AMBER));
    let items: Vec<ListItem> = items
        .iter()
        .map(|(label, ok)| {
            let item = ListItem::new(label.as_str());
            if *ok { item } else { item.fg(DIM) }
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().fg(Color::Black).bg(AMBER))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

/// The middle `pct_x` × `pct_y` percent of `area`.
pub fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let [_, mid, _] = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .areas(area);
    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .areas(mid);
    center
}
