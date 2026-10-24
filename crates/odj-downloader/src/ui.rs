//! Drawing: search box, results list and download queue.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use crate::app::{App, Focus};
use crate::queue::Status;

const AMBER: Color = Color::Rgb(255, 170, 30);
const DIM: Color = Color::DarkGray;
const GREEN: Color = Color::Rgb(60, 220, 100);
const RED: Color = Color::Rgb(230, 70, 70);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn key_hint(key: &str, action: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key} "), Style::new().fg(Color::Black).bg(AMBER).bold()),
        Span::styled(format!(" {action} "), Style::new().fg(AMBER).bold()),
    ])
}

pub fn draw(f: &mut Frame, app: &App) {
    let [search_area, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
    draw_search(f, search_area, app);

    let [results_area, queue_area] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
    draw_results(f, results_area, app);
    draw_queue(f, queue_area, app);
    draw_footer(f, footer, app);
}

fn focused_border(focused: bool) -> Style {
    Style::new().fg(if focused { AMBER } else { DIM })
}

fn draw_search(f: &mut Frame, area: Rect, app: &App) {
    let title = if app.searching { " Search (searching…) " } else { " Search " };
    let block = Block::bordered().title(title).border_style(focused_border(app.focus == Focus::Search));
    let line = Line::from(vec![Span::raw(app.query.as_str()), Span::styled("▏", Style::new().fg(AMBER))]);
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn duration_label(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s.is_finite() && s >= 0.0 => format!("{}:{:02}", s as u64 / 60, s as u64 % 60),
        _ => "--:--".to_string(),
    }
}

fn draw_results(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Results ").border_style(focused_border(app.focus == Focus::Results));
    if app.searching {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [_, spinner_row, hint_row, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        let frame = SPINNER[(app.spinner / 5) as usize % SPINNER.len()];
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
    let items: Vec<ListItem> = if app.results.is_empty() {
        vec![ListItem::new("(no results yet — type and press Enter)").fg(DIM)]
    } else {
        app.results
            .iter()
            .map(|r| {
                let channel = r.channel.as_deref().unwrap_or("?");
                let line = format!("{}  ·  {} · {}", r.title, channel, duration_label(r.duration));
                ListItem::new(line)
            })
            .collect()
    };
    let list = List::new(items).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!app.results.is_empty()).then_some(app.selected_result));
    f.render_stateful_widget(list, area, &mut state);
}

fn status_color(status: &Status) -> Color {
    match status {
        Status::Queued => DIM,
        Status::Downloading(_) => AMBER,
        Status::Done => GREEN,
        Status::Failed(_) => RED,
    }
}

fn status_label(status: &Status) -> String {
    match status {
        Status::Queued => "queued".to_string(),
        Status::Downloading(pct) if *pct > 0.0 => format!("downloading… {pct:.0}%"),
        Status::Downloading(_) => "downloading…".to_string(),
        Status::Done => "done".to_string(),
        Status::Failed(reason) => format!("failed: {reason}"),
    }
}

fn draw_queue(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Queue ").border_style(focused_border(app.focus == Focus::Queue));
    let items: Vec<ListItem> = if app.queue.is_empty() {
        vec![ListItem::new("(empty)").fg(DIM)]
    } else {
        app.queue
            .iter()
            .map(|item| {
                let line = Line::from(vec![
                    Span::raw(format!("{}  ", item.title)),
                    Span::styled(status_label(&item.status), Style::new().fg(status_color(&item.status))),
                ]);
                ListItem::new(line)
            })
            .collect()
    };
    let list = List::new(items).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!app.queue.is_empty()).then_some(app.selected_queue));
    f.render_stateful_widget(list, area, &mut state);
}

/// Contextual keys left, globals pinned right — mirrors odj-player's own
/// `draw_footer`, the formatting (and naming: "quit", not "exit") every
/// app's footer is meant to follow; see its own doc comment for the
/// convention itself. This binary deliberately doesn't depend on
/// odj-core (see `main.rs`'s own doc comment), so it keeps its own
/// small copies of `AMBER`/`DIM`/`key_hint` above rather than reusing
/// odj-core's — same convention, independent implementation. No `` ` ``
/// settings or `?` help here: this tool has neither.
fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    if let Some(status) = &app.status {
        let line = Line::styled(status.clone(), Style::new().fg(AMBER));
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let left = Line::from(match app.focus {
        Focus::Search if app.searching => key_hint("Esc", "cancel search").spans,
        Focus::Search => key_hint("Enter", "search").spans,
        Focus::Results => key_hint("Enter", "add to queue").spans,
        Focus::Queue => key_hint("x/Del", "remove queued").spans,
    });
    let esc_hint = if app.searching { "cancel" } else { "clear" };
    let right = Line::styled(format!("Tab focus  ↑↓ navigate  Esc {esc_hint}  Ctrl+C quit "), Style::new().fg(DIM));
    let rw = right.width() as u16;
    let [left_area, _, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(left), left_area);
    f.render_widget(Paragraph::new(right), right_area);
}
