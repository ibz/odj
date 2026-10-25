//! Terminal plumbing both apps share: setup with key-release events, the startup
//! audio output picker, and the common colours.

use std::cell::Cell;
use std::io::stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
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
    // A keystroke the app never got around to reading — most likely a
    // trailing release event right as the very key that's quitting it
    // (e.g. Ctrl+C) is read — would otherwise sit unread in the
    // terminal's own input buffer and reach the shell as this
    // protocol's raw escape codes once this app's gone and the shell
    // itself (which doesn't speak it) reads it back. Drain it first,
    // while the terminal's still sending in the mode that produced it.
    while let Ok(true) = event::poll(Duration::ZERO) {
        let _ = event::read();
    }
    if key_release {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
}

/// Always asks which output and route to play on, even with only one card: a card with
/// just two channels still offers left-only/right-only mono alongside stereo, and those
/// would otherwise be unreachable. Only the system default is used without asking, and
/// only when no outputs were found at all. `None` means the user quit the picker.
pub fn pick_output(terminal: &mut DefaultTerminal, host: &audio::Host) -> Result<Option<audio::Output>> {
    let cards = audio::output_cards(host)?;
    let choices = audio::choices(&cards);
    if choices.is_empty() {
        return audio::Output::open_default(host)
            .context("no audio outputs found (does your user have access to /dev/snd, e.g. via the `audio` group?)")
            .map(Some);
    }
    let items: Vec<(String, bool)> = choices.iter().map(|c| (c.label(&cards, "Out"), c.available(&cards))).collect();
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
                return audio::Output::open(host, &cards, &choices[selected]).map(Some);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
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
        .title_bottom(" ↑↓ select  Enter use  Ctrl+C quit ")
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

/// What the About box says about an app; fill it from the app's own Cargo
/// metadata with [`about!`](crate::about).
pub struct About {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub authors: &'static str,
    pub repository: &'static str,
    pub license: &'static str,
}

/// The calling crate's [`About`].
#[macro_export]
macro_rules! about {
    () => {
        $crate::tui::About {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            description: env!("CARGO_PKG_DESCRIPTION"),
            authors: env!("CARGO_PKG_AUTHORS"),
            repository: env!("CARGO_PKG_REPOSITORY"),
            license: env!("CARGO_PKG_LICENSE"),
        }
    };
}

/// A DJ at the decks, hood up.
const DJ: &str = r"
             .-~~~~~~~~~-.
           .'  .-------.  '.
     .--. /  .'         '.  \ .--.
    |####|  /  _________  \  |####|
    |####| |  [####|####]  | |####|
    |####| |               | |####|
     '--'   \    `---'    /   '--'
             '.         .'
          .---''-.___.-''---.
        /  01  |  | |  |  10  \
      /  1101  |  | |  |  0110  \
.--------------------------------------.
|  .-----.  .--------------.  .-----.  |
| ( ( @ ) ) | $ sync --d1_ | ( ( @ ) ) |
|  '-----'  '--------------'  '-----'  |
'--------------------------------------'";
enum AboutLayout {
    Beside,
    Above,
    TextOnly,
}

/// The DJ's last rows are the decks.
const DECK_ROWS: usize = 5;
const HACKER_GREEN: Color = Color::Rgb(80, 220, 120);
/// The licence link, indented, and a column of margin on either side.
const INFO_WIDTH: u16 = 57;
const ART_GAP: u16 = 3;

fn about_lines(about: &About) -> Vec<Line<'static>> {
    let label = |k: &'static str| Span::styled(format!("{k:<9}"), Style::new().fg(AMBER).bold());
    let mut info = vec![
        Line::from(vec![
            Span::styled(about.name, Style::new().fg(AMBER).bold()),
            Span::raw(format!(" {}", about.version)),
        ]),
        Line::from(about.description),
        Line::default(),
    ];
    for author in about.authors.split(':') {
        info.push(Line::from(vec![label("By"), Span::raw(author)]));
    }
    info.push(Line::from(vec![label("Code"), Span::raw(about.repository)]));
    info.push(Line::from(vec![label("License"), Span::raw(about.license)]));
    info.push(Line::from(vec![label(""), Span::raw(format!("{}/blob/master/LICENSE", about.repository))]));
    info
}

/// The DJ beside the text where `width` × `height` is wide enough, above it
/// where it is tall enough, else the text alone; and the rows that takes.
fn about_layout(about: &About, width: u16, height: u16) -> (AboutLayout, u16) {
    let art: Vec<&str> = DJ.lines().skip(1).collect();
    let art_width = art.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let art_rows = art.len() as u16;
    // Wrapped, the description may take two rows.
    let rows = about_lines(about).len() as u16 + 1;
    if width >= 1 + art_width + ART_GAP + INFO_WIDTH && height >= art_rows {
        (AboutLayout::Beside, art_rows)
    } else if width >= INFO_WIDTH && height >= art_rows + 1 + rows {
        (AboutLayout::Above, art_rows + 1 + rows)
    } else {
        (AboutLayout::TextOnly, rows)
    }
}

/// The About page: the DJ, and beside or under it name, version, author,
/// where the code lives and the license. Small screens get the text alone.
fn draw_about(f: &mut Frame, area: Rect, about: &About) {
    let art: Vec<&str> = DJ.lines().skip(1).collect();
    let art_width = art.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let art_rows = art.len() as u16;
    let figure = art.len() - DECK_ROWS;
    let dj = Paragraph::new(
        art.iter()
            .enumerate()
            .map(|(i, l)| Line::styled(*l, Style::new().fg(if i < figure { HACKER_GREEN } else { Color::Gray })))
            .collect::<Vec<_>>(),
    );
    let text_area = match about_layout(about, area.width, area.height).0 {
        AboutLayout::Beside => {
            // A column of space on the left.
            f.render_widget(dj, Rect { x: area.x + 1, width: art_width, height: art_rows, ..area });
            let x = area.x + 1 + art_width + ART_GAP;
            Rect { x, width: area.width.saturating_sub(1 + art_width + ART_GAP), height: art_rows, ..area }
        }
        AboutLayout::Above => {
            let x = area.x + (area.width - art_width) / 2;
            f.render_widget(dj, Rect { x, width: art_width, height: art_rows, ..area });
            let text = Rect { y: area.y + art_rows + 1, height: area.height - art_rows - 1, ..area };
            text.inner(Margin::new(1, 0))
        }
        AboutLayout::TextOnly => area.inner(Margin::new(1, 0)),
    };
    let info = about_lines(about);
    let rows = info.len() as u16 + 1;
    let text_area = Rect { y: text_area.y + text_area.height.saturating_sub(rows) / 2, ..text_area };
    f.render_widget(Paragraph::new(info).wrap(Wrap { trim: false }), text_area);
}

/// A page of the help: keys with what they do, or About.
pub enum Page {
    Keys(&'static str, &'static [(&'static str, &'static str)]),
    About(About),
}

impl Page {
    fn title(&self) -> &'static str {
        match self {
            Page::Keys(title, _) => title,
            Page::About(_) => "About",
        }
    }
}

/// The help box: whether it's open, which page shows and how far it's
/// scrolled. Drawing sets how far it can scroll.
#[derive(Default)]
pub struct Help {
    open: bool,
    page: usize,
    top: Cell<u16>,
    max: Cell<u16>,
    shown: Cell<u16>,
}

impl Help {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn page(&self) -> usize {
        self.page
    }

    /// Opens on the first page, or closes.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.page = 0;
        self.top.set(0);
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Turns the pages on Tab, Shift+Tab, ← and →, and scrolls on ↑ ↓ PgUp
    /// PgDn Home End; false for any other key. Shift+Tab may come as BackTab
    /// or as Tab with `shift`.
    pub fn key(&mut self, code: KeyCode, shift: bool, pages: usize) -> bool {
        match code {
            KeyCode::Tab if shift => self.page = (self.page + pages - 1) % pages,
            KeyCode::Tab | KeyCode::Right => self.page = (self.page + 1) % pages,
            KeyCode::BackTab | KeyCode::Left => self.page = (self.page + pages - 1) % pages,
            _ => return self.scroll(code),
        }
        self.top.set(0);
        true
    }

    fn scroll(&self, code: KeyCode) -> bool {
        let (top, page) = (self.top.get(), self.shown.get().max(2) - 1);
        let top = match code {
            KeyCode::Up => top.saturating_sub(1),
            KeyCode::Down => top + 1,
            KeyCode::PageUp => top.saturating_sub(page),
            KeyCode::PageDown => top + page,
            KeyCode::Home => 0,
            KeyCode::End => u16::MAX,
            _ => return false,
        };
        self.top.set(top.min(self.max.get()));
        true
    }
}

/// The help box: a row of tabs for the pages, then the page, which scrolls
/// when it doesn't fit.
pub fn draw_help(f: &mut Frame, help: &Help, pages: &[Page]) {
    let screen = centered(f.area(), 90, 100);
    let inner_width = screen.width.saturating_sub(2);
    // A column of margin either side; the scroll bar goes on the border.
    let text_width = (inner_width as usize).saturating_sub(2).max(1);
    let page = &pages[help.page.min(pages.len() - 1)];
    let lines = match page {
        Page::Keys(_, keys) => keys_lines(keys, text_width),
        Page::About(_) => Vec::new(),
    };
    let tab_lines = tab_lines(pages, help.page, text_width);
    // Borders, the tabs and a row under them.
    let tab_rows = tab_lines.len() as u16;
    let around = tab_rows + 3;
    let rows = match page {
        Page::Keys(..) => lines.len() as u16,
        Page::About(about) => about_layout(about, inner_width, screen.height.saturating_sub(around)).1,
    };
    let height = (rows + around).min(screen.height);
    let area = Rect { y: screen.y + (screen.height - height) / 2, height, ..screen };
    let inner = area.inner(Margin::new(1, 1));
    let [tabs, _, body] =
        Layout::vertical([Constraint::Length(tab_rows), Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    let overflow = (lines.len() as u16).saturating_sub(body.height);
    help.max.set(overflow);
    help.shown.set(body.height);
    let top = help.top.get().min(overflow);
    help.top.set(top);

    // The hints on the bottom edge, down to just their keys when narrow.
    let mut hints = vec![("Esc", "Close", AMBER), ("Tab", "Next page", PAGE_HINT)];
    if overflow > 0 {
        hints.push(("↑↓", "Scroll", AMBER));
    }
    let spare = |h: &[(&str, &str, Color)]| {
        h.iter().map(|(k, a, _)| k.chars().count() + a.len() + 5).sum::<usize>() < area.width as usize
    };
    if !spare(&hints) {
        hints.iter_mut().for_each(|h| h.1 = "");
    }
    let hint = |(key, action, color): (&str, &str, Color)| {
        if action.is_empty() {
            Line::styled(format!(" {key} "), Style::new().fg(Color::Black).bg(color).bold())
        } else {
            key_hint(key, action, color)
        }
    };
    let mut block = Block::bordered()
        .title(" Help ")
        .title_bottom(hint(hints[0]).left_aligned())
        .title_bottom(hint(hints[1]).right_aligned())
        .border_style(Style::new().fg(AMBER));
    if let Some(&h) = hints.get(2) {
        block = block.title_bottom(hint(h).centered());
    }
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(tab_lines), tabs.inner(Margin::new(1, 0)));
    match page {
        Page::Keys(..) => f.render_widget(Paragraph::new(lines).scroll((top, 0)), body.inner(Margin::new(1, 0))),
        Page::About(about) => draw_about(f, body, about),
    }
    if overflow > 0 {
        let mut state = ScrollbarState::new(overflow as usize + 1).position(top as usize);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .style(Style::new().fg(AMBER));
        f.render_stateful_widget(bar, Rect { y: body.y, height: body.height, ..area }, &mut state);
    }
}

/// One tab's own span, styled the way the help overlay's own tabs are
/// — black on amber, bold, for the one that's showing, else plain
/// amber — so any other multi-tab screen can look the same rather than
/// inventing its own "selected" look.
pub fn tab_span(label: &str, current: bool) -> Span<'static> {
    let style = if current { Style::new().fg(Color::Black).bg(AMBER).bold() } else { Style::new().fg(AMBER) };
    Span::styled(format!(" {label} "), style)
}

/// The pages' titles with the one showing picked out, on as many lines as
/// they need in `width`.
fn tab_lines(pages: &[Page], current: usize, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    for (i, p) in pages.iter().enumerate() {
        let tab = tab_span(p.title(), i == current);
        let line = lines.last_mut().unwrap();
        if line.width() > 0 && line.width() + 1 + tab.width() > width {
            lines.push(Line::from(tab));
        } else {
            if line.width() > 0 {
                line.push_span(" ");
            }
            line.push_span(tab);
        }
    }
    lines
}

/// Each key with what it does, the descriptions wrapped under themselves,
/// or under the key when `width` is too narrow for both side by side.
fn keys_lines(keys: &[(&str, &str)], width: usize) -> Vec<Line<'static>> {
    const MIN_TEXT: usize = 24;
    let key_width = keys.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let key_style = Style::new().fg(AMBER).bold();
    let mut lines = Vec::new();
    for (k, v) in keys {
        if width >= key_width + 2 + MIN_TEXT {
            for (i, t) in wrap(v, width - key_width - 2).into_iter().enumerate() {
                let key = if i == 0 { format!("{k:>key_width$}  ") } else { " ".repeat(key_width + 2) };
                lines.push(Line::from(vec![Span::styled(key, key_style), Span::raw(t)]));
            }
        } else {
            lines.extend(wrap(k, width).into_iter().map(|t| Line::styled(t, key_style)));
            let indent = 2.min(width - 1);
            lines.extend(wrap(v, width - indent).into_iter().map(|t| Line::raw(format!("{}{t}", " ".repeat(indent)))));
        }
    }
    lines
}

/// `text` in lines of at most `width` columns, broken at spaces where it
/// can. Also used directly (not just via [`hint_lines`]) wherever a box
/// needs to size itself to however many lines some arbitrary text —
/// typically an error message of unpredictable length — actually takes
/// at a known width, rather than assuming a fixed height and clipping
/// whatever doesn't fit.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let (n, w) = (line.chars().count(), word.chars().count());
        if n > 0 && n + 1 + w <= width {
            line.push(' ');
            line.push_str(word);
            continue;
        }
        if n > 0 {
            lines.push(std::mem::take(&mut line));
        }
        let mut chars: Vec<char> = word.chars().collect();
        while chars.len() > width {
            lines.push(chars.drain(..width).collect());
        }
        line = chars.into_iter().collect();
    }
    lines.push(line);
    lines
}

/// Key hints for a box, as many to a line as fit in `width`; a hint is
/// split only when it's wider than a line.
pub fn hint_lines(hints: &[&'static str], width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = vec![Line::default()];
    for &hint in hints {
        let line = lines.last_mut().unwrap();
        if hint.chars().count() > width as usize {
            if line.width() == 0 {
                lines.pop();
            }
            lines.extend(wrap(hint, (width as usize).max(1)).into_iter().map(Line::from));
            lines.push(Line::default());
        } else if line.width() > 0 && line.width() + 2 + hint.chars().count() > width as usize {
            lines.push(Line::from(hint));
        } else {
            if line.width() > 0 {
                line.push_span("  ");
            }
            line.push_span(hint);
        }
    }
    lines.into_iter().filter(|l| l.width() > 0).map(|l| l.style(Style::new().fg(DIM))).collect()
}

/// A bordered list box with its key hints along the bottom, wrapped to fit:
/// the area for the list, and draws the box and the hints.
pub fn hinted_box(f: &mut Frame, area: Rect, block: Block, hints: &[&'static str]) -> Rect {
    let inner = block.inner(area);
    // A column of margin either side.
    let lines = hint_lines(hints, inner.width.saturating_sub(2));
    let [list, hints] = Layout::vertical([Constraint::Fill(1), Constraint::Length(lines.len() as u16)]).areas(inner);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), hints.inner(Margin::new(1, 0)));
    list
}

/// A key on a badge, then what it does, both in `color`.
pub fn key_hint(key: &str, action: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key} "), Style::new().fg(Color::Black).bg(color).bold()),
        Span::styled(format!(" {action} "), Style::new().fg(color).bold()),
    ])
}

const PAGE_HINT: Color = Color::Rgb(90, 200, 230);

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

/// Renders a `TestBackend` buffer back to the ANSI text it would have put on a
/// real terminal, for tools (e.g. `freeze`) that turn a terminal capture into
/// an image. Not meant for a real backend: it reopens one SGR escape per cell
/// whose style changed, which only a snapshot buffer needs.
#[cfg(any(test, feature = "test-util"))]
pub fn to_ansi(buf: &ratatui::buffer::Buffer) -> String {
    use ratatui::style::Modifier;
    use std::fmt::Write;

    fn sgr(color: Color, bg: bool) -> Option<String> {
        let base = if bg { 48 } else { 38 };
        let n = match color {
            Color::Reset => return None,
            Color::Black => 0,
            Color::Red => 1,
            Color::Green => 2,
            Color::Yellow => 3,
            Color::Blue => 4,
            Color::Magenta => 5,
            Color::Cyan => 6,
            Color::Gray => 7,
            Color::DarkGray => 8,
            Color::LightRed => 9,
            Color::LightGreen => 10,
            Color::LightYellow => 11,
            Color::LightBlue => 12,
            Color::LightMagenta => 13,
            Color::LightCyan => 14,
            Color::White => 15,
            Color::Indexed(i) => i,
            Color::Rgb(r, g, b) => return Some(format!("{base};2;{r};{g};{b}")),
        };
        Some(format!("{base};5;{n}"))
    }

    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.y + area.height {
        let mut style = None;
        for x in area.x..area.x + area.width {
            let cell = &buf[(x, y)];
            let this = (cell.fg, cell.bg, cell.modifier);
            if style != Some(this) {
                let mut codes = vec!["0".to_string()];
                codes.extend(sgr(cell.fg, false));
                codes.extend(sgr(cell.bg, true));
                if cell.modifier.contains(Modifier::BOLD) {
                    codes.push("1".into());
                }
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    codes.push("4".into());
                }
                if cell.modifier.contains(Modifier::CROSSED_OUT) {
                    codes.push("9".into());
                }
                write!(out, "\x1b[{}m", codes.join(";")).unwrap();
                style = Some(this);
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn wrapping_breaks_at_spaces_and_splits_long_words() {
        assert_eq!(super::wrap("loop in / loop out", 9), ["loop in /", "loop out"]);
        assert_eq!(super::wrap("abcdefgh ij", 3), ["abc", "def", "gh", "ij"]);
    }

    /// Every word of a keys page shows on some scroll position, side by
    /// side or stacked, and the tabs turn the pages both ways.
    #[test]
    fn the_help_loses_nothing_scrolls_and_turns() {
        use super::{Help, Page, draw_help};
        use ratatui::crossterm::event::KeyCode;
        let keys: &[(&str, &str)] = &[
            ("Space", "Play / Pause"),
            ("Shift+1 2 3", "Clear hot cue (REC mode only) and some more words to wrap around"),
            ("Ctrl+C", "Quit (cue memory is saved)"),
        ];
        let pages = [Page::Keys("Basics", &[("?", "Help")]), Page::Keys("Cues", keys)];
        for (w, h) in [(50, 8), (20, 10), (100, 30)] {
            let mut help = Help::default();
            help.toggle();
            help.key(KeyCode::Right, false, pages.len());
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut seen = String::new();
            for _ in 0..30 {
                t.draw(|f| draw_help(f, &help, &pages)).unwrap();
                seen += &screen(&t);
                seen.push(' ');
                help.key(KeyCode::Down, false, pages.len());
            }
            let fits = help.max.get() == 0;
            assert_eq!(fits, w == 100, "{w}×{h} scrolls only when it doesn't fit");
            assert_eq!(seen.contains("↑↓"), !fits, "{w}×{h}");
            assert!(seen.contains("Basics") && seen.contains("Cues"), "{w}×{h} names every page");
            for word in keys.iter().flat_map(|(k, v)| k.split(' ').chain(v.split(' '))) {
                assert!(seen.contains(word), "{w}×{h} lost {word:?}");
            }
            assert_eq!(help.top.get(), help.max.get(), "stops at the end");
        }

        let mut help = Help::default();
        help.toggle();
        help.key(KeyCode::BackTab, false, pages.len());
        assert_eq!(help.page(), 1, "back from the first page goes round to the last");
        help.key(KeyCode::Tab, false, pages.len());
        assert_eq!(help.page(), 0);
        help.key(KeyCode::Tab, true, pages.len());
        assert_eq!(help.page(), 1, "Shift+Tab as Tab with Shift goes back too");
        help.toggle();
        help.toggle();
        assert_eq!(help.page(), 0, "opens on the first page");
    }

    #[test]
    fn tabs_that_dont_fit_go_on_more_lines() {
        use super::{Page, tab_lines};
        let pages = [Page::Keys("Basics", &[]), Page::Keys("Cues", &[]), Page::Keys("Beat grid", &[])];
        let text = |w| tab_lines(&pages, 0, w).iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(text(40), [" Basics   Cues   Beat grid "]);
        assert_eq!(text(20), [" Basics   Cues ", " Beat grid "]);
    }

    #[test]
    fn hints_wrap_whole() {
        let text = |w| super::hint_lines(&["type to filter", "↑↓ select", "Esc close"], w)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>();
        assert_eq!(text(80), ["type to filter  ↑↓ select  Esc close"]);
        assert_eq!(text(25), ["type to filter  ↑↓ select", "Esc close"]);
        assert_eq!(text(10), ["type to", "filter", "↑↓ select", "Esc close"], "a hint wider than a line wraps");
    }

    fn screen(t: &Terminal<TestBackend>) -> String {
        t.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    /// About on its page: the DJ beside the text, above it, or left out.
    #[test]
    fn about_shows_the_author_code_and_license() {
        use super::{Help, Page, draw_help};
        let pages = [Page::Keys("Basics", &[("Space", "Play / Pause")]), Page::About(crate::about!())];
        let mut help = Help::default();
        help.toggle();
        help.key(ratatui::crossterm::event::KeyCode::Tab, false, pages.len());
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| draw_help(f, &help, &pages)).unwrap();
        for want in
            ["odj-core", "Ioan Bizău <odj@ibz.me>", "https://github.com/ibz/odj/blob/master/LICENSE", "sync --d1_"]
        {
            assert!(screen(&t).contains(want), "{want}");
        }
        assert!(!screen(&t).contains("WITHOUT WARRANTY"), "the license text is linked, not shown");

        // Narrower but tall, like an 87 × 53 terminal: the DJ goes on top.
        let mut t = Terminal::new(TestBackend::new(87, 53)).unwrap();
        t.draw(|f| draw_help(f, &help, &pages)).unwrap();
        assert!(screen(&t).contains("sync --d1_") && screen(&t).contains("Ioan Bizău"));

        // Too small for the DJ either way: the text alone.
        let mut t = Terminal::new(TestBackend::new(70, 20)).unwrap();
        t.draw(|f| draw_help(f, &help, &pages)).unwrap();
        assert!(screen(&t).contains("Ioan Bizău") && !screen(&t).contains("sync --d1_"));
    }
}
