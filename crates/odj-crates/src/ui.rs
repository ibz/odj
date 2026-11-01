//! Drawing: the crate list, a crate's own track list, and (nested under
//! one) the search/queue download screen or the add-existing browser.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use odj_core::crates::Availability;

use crate::app::{App, Focus, ResultMark, Screen};
use crate::queue::Status;

const AMBER: Color = Color::Rgb(255, 170, 30);
const DIM: Color = Color::DarkGray;
const GREEN: Color = Color::Rgb(60, 220, 100);
const RED: Color = Color::Rgb(230, 70, 70);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Each key highlighted (bold, set apart from the rest), what it does dim —
/// the same convention odj-player's and odj-sampler's own footers use.
fn hints_line(parts: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, d)) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(k.to_string(), Style::new().fg(Color::Gray).bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(d.to_string(), Style::new().fg(DIM)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn list_block(title: &str) -> Block<'static> {
    Block::bordered().title(format!(" {title} ")).border_style(Style::new().fg(AMBER))
}

pub fn draw(f: &mut Frame, app: &App) {
    match &app.screen {
        Screen::Crates(_) => draw_crates(f, app),
        Screen::NamingCrate(_) => draw_naming_crate(f, app),
        Screen::Crate(_) => draw_crate_detail(f, app),
        Screen::Downloading(_) => draw_downloading(f, app),
        Screen::AddingExisting(_) => draw_adding_existing(f, app),
    }
}

fn body_and_footer(area: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area)
}

fn draw_crates(f: &mut Frame, app: &App) {
    let Screen::Crates(list) = &app.screen else { return };
    let [body, footer] = body_and_footer(f.area());
    let items: Vec<ListItem> = if list.rows.is_empty() {
        vec![ListItem::new("(no crates yet — press N to create one)").fg(DIM)]
    } else {
        list.rows.iter().map(|c| ListItem::new(c.name.clone())).collect()
    };
    let l = List::new(items).block(list_block("Crates")).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!list.rows.is_empty()).then_some(list.selected));
    f.render_stateful_widget(l, body, &mut state);
    draw_footer(f, footer, app, &[("Enter", "open"), ("N", "new crate")]);
}

fn draw_naming_crate(f: &mut Frame, app: &App) {
    let Screen::NamingCrate(name) = &app.screen else { return };
    let [body, footer] = body_and_footer(f.area());
    let line = Line::from(vec![Span::raw(name.as_str()), Span::styled("▏", Style::new().fg(AMBER))]);
    f.render_widget(Paragraph::new(line).block(list_block("New crate's name")), body);
    draw_footer(f, footer, app, &[("Enter", "create"), ("Esc", "cancel")]);
}

fn draw_crate_detail(f: &mut Frame, app: &App) {
    let Screen::Crate(c) = &app.screen else { return };
    let [body, footer] = body_and_footer(f.area());
    let items: Vec<ListItem> = if c.tracks().is_empty() {
        vec![ListItem::new("(no tracks yet — D downloads, A adds an existing file)").fg(DIM)]
    } else {
        c.tracks()
            .iter()
            .map(|t| {
                let (marker, color) = match c.availability(&t.id) {
                    Some(Availability::Changed) => (" (changed)", Color::Yellow),
                    Some(Availability::Missing) => (" (missing)", RED),
                    Some(Availability::Available) | None => ("", Color::Reset),
                };
                let armed = c.armed() == Some(t.id.as_str());
                let confirm = if armed { " — press again to remove" } else { "" };
                let artist = t.artist.as_deref().map(|a| format!(" — {a}")).unwrap_or_default();
                let youtube = if t.youtube_url.is_some() { " — on YouTube" } else { "" };
                ListItem::new(format!("{}{artist}{marker}{youtube}{confirm}", t.title)).fg(color)
            })
            .collect()
    };
    let title = format!("Crate: {}", c.name);
    let l = List::new(items).block(list_block(&title)).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!c.tracks().is_empty()).then_some(c.selected));
    f.render_stateful_widget(l, body, &mut state);
    draw_footer(
        f,
        footer,
        app,
        &[("D", "download"), ("A", "add existing"), ("X/Del", "remove"), ("Esc", "back")],
    );
}

fn focused_border(focused: bool) -> Style {
    Style::new().fg(if focused { AMBER } else { DIM })
}

fn draw_downloading(f: &mut Frame, app: &App) {
    let Screen::Downloading(d) = &app.screen else { return };
    let [search_area, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());

    let title = if d.searching { " Search (searching…) " } else { " Search " };
    let block = Block::bordered().title(title).border_style(focused_border(d.focus == Focus::Search));
    let line = Line::from(vec![Span::raw(d.query.as_str()), Span::styled("▏", Style::new().fg(AMBER))]);
    f.render_widget(Paragraph::new(line).block(block), search_area);

    let [results_area, queue_area] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
    draw_results(f, results_area, app);
    draw_queue(f, queue_area, app);

    let hints: &[(&str, &str)] = match d.focus {
        _ if d.searching => &[("Esc", "cancel search")],
        Focus::Search => &[("Enter", "search")],
        Focus::Results => &[("Enter", "add to queue")],
        Focus::Queue => &[("x/Del", "remove queued")],
    };
    let mut all = hints.to_vec();
    all.push(("Tab", "focus"));
    if !d.downloading() {
        all.push(("Esc", "back"));
    }
    draw_footer(f, footer, app, &all);
}

fn duration_label(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s.is_finite() && s >= 0.0 => format!("{}:{:02}", s as u64 / 60, s as u64 % 60),
        _ => "--:--".to_string(),
    }
}

fn draw_results(f: &mut Frame, area: Rect, app: &App) {
    let Screen::Downloading(d) = &app.screen else { return };
    let block = Block::bordered().title(" Results ").border_style(focused_border(d.focus == Focus::Results));
    if d.searching {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [_, spinner_row, hint_row, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        let frame = SPINNER[(d.spinner / 5) as usize % SPINNER.len()];
        f.render_widget(
            Paragraph::new(Line::styled(format!("{frame} Searching…"), Style::new().fg(AMBER)))
                .alignment(Alignment::Center),
            spinner_row,
        );
        f.render_widget(
            Paragraph::new(Line::styled("Esc to cancel search", Style::new().fg(DIM))).alignment(Alignment::Center),
            hint_row,
        );
        return;
    }
    let items: Vec<ListItem> = if d.results.is_empty() {
        vec![ListItem::new("(no results yet — type and press Enter)").fg(DIM)]
    } else {
        d.results
            .iter()
            .map(|r| {
                let channel = r.channel.as_deref().unwrap_or("?");
                let line = format!("{}  ·  {} · {}", r.title, channel, duration_label(r.duration));
                match d.mark_for(r) {
                    ResultMark::InCrate => ListItem::new(format!("✓ {line}  (in crate)")).fg(GREEN),
                    ResultMark::AlreadyDownloaded(_) => ListItem::new(format!("⬇ {line}  (already downloaded)")).fg(AMBER),
                    ResultMark::None => ListItem::new(line),
                }
            })
            .collect()
    };
    let list = List::new(items).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!d.results.is_empty()).then_some(d.selected_result));
    f.render_stateful_widget(list, area, &mut state);
}

/// A block-character bar (`█` filled, `░` empty) plus the percentage —
/// same convention as odj-sampler's own memory-use bar — in place of the
/// word "downloading", which says nothing about how far along it is.
const PROGRESS_BAR_WIDTH: usize = 14;

fn progress_bar(pct: f32) -> Vec<Span<'static>> {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * PROGRESS_BAR_WIDTH as f32).round() as usize;
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(AMBER)),
        Span::styled("░".repeat(PROGRESS_BAR_WIDTH - filled), Style::new().fg(DIM)),
        Span::styled(format!(" {pct:>3.0}%"), Style::new().fg(AMBER)),
    ]
}

fn draw_queue(f: &mut Frame, area: Rect, app: &App) {
    let Screen::Downloading(d) = &app.screen else { return };
    let block = Block::bordered().title(" Queue ").border_style(focused_border(d.focus == Focus::Queue));
    let items: Vec<ListItem> = if d.queue.is_empty() {
        vec![ListItem::new("(empty)").fg(DIM)]
    } else {
        d.queue
            .iter()
            .map(|item| {
                let mut spans = vec![Span::raw(format!("{}  ", item.title))];
                match &item.status {
                    Status::Downloading(pct) => spans.extend(progress_bar(*pct)),
                    Status::Queued => spans.push(Span::styled("queued", Style::new().fg(DIM))),
                    Status::Done(_) => spans.push(Span::styled("done", Style::new().fg(GREEN))),
                    Status::Failed(reason) => spans.push(Span::styled(format!("failed: {reason}"), Style::new().fg(RED))),
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };
    let list = List::new(items).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!d.queue.is_empty()).then_some(d.selected_queue));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_adding_existing(f: &mut Frame, app: &App) {
    let Screen::AddingExisting(a) = &app.screen else { return };
    let [body, footer] = body_and_footer(f.area());
    if let Some(i) = a.picking_folder {
        let items: Vec<ListItem> = a.music_dirs.iter().map(|d| ListItem::new(d.display().to_string())).collect();
        let l = List::new(items).block(list_block("Which music folder?")).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
        let mut state = ListState::default().with_selected(Some(i));
        f.render_stateful_widget(l, body, &mut state);
        draw_footer(f, footer, app, &[("Enter", "open"), ("Esc", "back")]);
        return;
    }
    let Some(browser) = &a.browser else { return };
    let title = format!("Add from {}", browser.dir.display());
    let items: Vec<ListItem> = if browser.visible.is_empty() {
        vec![ListItem::new("(empty)").fg(DIM)]
    } else {
        browser
            .visible
            .iter()
            .map(|&(i, _)| {
                let entry = &browser.entries[i];
                let marker = if a.selected().contains(&entry.path) { "[x] " } else { "[ ] " };
                let label = if entry.is_dir { format!("{marker}{}/", entry.name) } else { format!("{marker}{}", entry.name) };
                ListItem::new(label)
            })
            .collect()
    };
    let l = List::new(items).block(list_block(&title)).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!browser.visible.is_empty()).then_some(browser.selected));
    f.render_stateful_widget(l, body, &mut state);
    if a.adding() {
        draw_adding_progress(f, f.area(), a.adding_progress());
        draw_footer(f, footer, app, &[]);
        return;
    }
    draw_footer(f, footer, app, &[("type", "filter"), ("Space", "mark"), ("a", "add marked/highlighted"), ("Enter", "drill in"), ("Esc", "up/back")]);
}

/// A centered overlay while a batch add runs in the background — same
/// convention as odj-sampler's starter-kit download progress screen.
fn draw_adding_progress(f: &mut Frame, area: Rect, (current, total): (usize, usize)) {
    use odj_core::tui::centered;
    use ratatui::widgets::Clear;

    let area = centered(area, 60, 20);
    f.render_widget(Clear, area);
    let block = Block::bordered().title(" Adding tracks ").border_style(Style::new().fg(AMBER));
    let text = if total == 0 { "Starting…".to_string() } else { format!("{current} of {total} tracks") };
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center).style(Style::new().fg(AMBER)).block(block), area);
}

/// Contextual keys left, globals pinned right — mirrors odj-player's own
/// `draw_footer`, the formatting (and naming: "quit", not "exit") every
/// app's footer is meant to follow; see its own doc comment for the
/// convention itself.
fn draw_footer(f: &mut Frame, area: Rect, app: &App, hints: &[(&str, &str)]) {
    if let Some(status) = app.status() {
        let line = Line::styled(status.to_string(), Style::new().fg(AMBER));
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let left = hints_line(hints);
    let right = hints_line(&[("Ctrl+C", "quit")]);
    let rw = right.width() as u16;
    let [left_area, _, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(left), left_area);
    f.render_widget(Paragraph::new(right), right_area);
}
