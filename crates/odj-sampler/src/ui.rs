//! Drawing: the pad grid, the selected pad, the memory meter, the cue picker and
//! the sample editor.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Widget};

use odj_core::grid::{BAR, BeatGrid};
use odj_core::track::{Track, WAVE_RATE};
use odj_core::tui::{AMBER, DIM, Page, centered, draw_help, hinted_box, key_hint, tab_span, wrap};

use crate::app::{App, AudioRoute, Editor, KitBrowserMode, KitBrowserState, PAD_KEYS, PadStatus, Picker, SampleBrowser, Screen, Slot};
use crate::engine::{Mode, PadState, Snapshot, TAIL};
use crate::midi::DeviceRow;

const PLAYING: Color = Color::Rgb(60, 220, 100);
const PLAYING_BG: Color = Color::Rgb(20, 45, 25);
const REGION_BG: Color = Color::Rgb(40, 70, 25);
const PLAYHEAD: Color = Color::Rgb(210, 40, 40);
const ERROR: Color = Color::Rgb(255, 80, 80);
const SIDE_WIDTH: u16 = 40;

pub fn draw(f: &mut Frame, app: &App, s: &Snapshot) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(8), Constraint::Length(1)]).areas(f.area());
    draw_header(f, header, app);
    let [main, side] = Layout::horizontal([Constraint::Min(40), Constraint::Length(SIDE_WIDTH)]).areas(body);

    match &app.screen {
        Screen::Editor(ed) => draw_editor(f, main, ed, s, app.out_rate()),
        _ => draw_grid(f, main, app, s),
    }
    let [pad_info, meter] = Layout::vertical([Constraint::Min(10), Constraint::Length(8)]).areas(side);
    draw_pad_info(f, pad_info, app);
    draw_memory(f, meter, app);
    draw_footer(f, footer, app);

    if let Screen::Picker(p) = &app.screen {
        draw_picker(f, p);
    } else if let Screen::KitBrowser(state) = &app.screen {
        draw_kit_browser(f, state);
    } else if let Screen::SampleBrowser(b) = &app.screen {
        draw_sample_browser(f, b);
    } else if let Screen::NamingKit(name) = &app.screen {
        draw_naming_kit(f, name);
    } else if let Screen::Settings = &app.screen {
        let rows = app.midi_rows();
        draw_settings(f, &rows, app.settings_selected.min(rows.len().saturating_sub(1)), app.midi_device.as_deref());
    } else if app.help.is_open() {
        draw_help(f, &app.help, HELP);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let badge = Style::new().fg(Color::Black).bg(AMBER).bold();
    let mut left = vec![Span::styled(" ODJ SAMPLER ", badge)];
    match &app.output {
        AudioRoute::Mixer => {
            left.push(Span::styled(" → ", Style::new().fg(DIM)));
            left.push(Span::styled(" ODJ MIXER ", badge));
        }
        AudioRoute::Direct(name) => left.push(Span::styled(format!(" → {name}"), Style::new().fg(DIM))),
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);

    if !app.key_release {
        let right = Span::styled("no key-release events: gate pads are timed  ", Style::new().fg(Color::Yellow));
        f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), area);
    }
}

fn draw_grid(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    let rows = Layout::vertical([Constraint::Ratio(1, 4); 4]).split(area);
    for (r, row) in rows.iter().enumerate() {
        let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(*row);
        for (c, cell) in cols.iter().enumerate() {
            let i = r * 4 + c;
            draw_pad(f, *cell, i, &app.slots[i], s.pads[i], i == app.selected, app.out_rate());
        }
    }
}

/// Each digit as 5 rows of a 3-column dot pattern — same font as MIDI learn screen.
const PAD_DIGIT_FONT: [[&str; 5]; 10] = [
    ["###", "# #", "# #", "# #", "###"],
    ["  #", "  #", "  #", "  #", "  #"],
    ["###", "  #", "###", "#  ", "###"],
    ["###", "  #", "###", "  #", "###"],
    ["# #", "# #", "###", "  #", "  #"],
    ["###", "#  ", "###", "  #", "###"],
    ["###", "#  ", "###", "# #", "###"],
    ["###", "  #", "  #", "  #", "  #"],
    ["###", "# #", "###", "# #", "###"],
    ["###", "# #", "###", "  #", "###"],
];

/// `n`'s digits (the pad number, 1–16) as 5 rows.
fn big_number(n: usize) -> [String; 5] {
    let digits: Vec<usize> = n.to_string().chars().filter_map(|c| c.to_digit(10)).map(|d| d as usize).collect();
    std::array::from_fn(|row| {
        digits
            .iter()
            .map(|&d| PAD_DIGIT_FONT[d][row].chars().map(|c| if c == '#' { "██" } else { "  " }).collect::<String>())
            .collect::<Vec<_>>()
            .join("  ")
    })
}

fn draw_pad(f: &mut Frame, area: Rect, i: usize, slot: &Slot, state: PadState, selected: bool, out_rate: u32) {
    let key = PAD_KEYS[i].to_ascii_uppercase();
    let border = if selected {
        Style::new().fg(AMBER).bold()
    } else if state.playing {
        Style::new().fg(PLAYING)
    } else if slot.kit.source.is_some() {
        Style::new().fg(Color::Gray)
    } else {
        Style::new().fg(DIM)
    };
    let mut block = Block::bordered()
        .title(Span::styled(format!(" {key} "), Style::new().fg(Color::Gray).bold()))
        .border_style(border);
    if state.playing {
        block = block.style(Style::new().bg(PLAYING_BG));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height >= 6 {
        let big = big_number(i + 1);
        let color = if slot.kit.source.is_some() { AMBER } else { DIM };
        for (r, rowstr) in big.iter().enumerate() {
            f.render_widget(
                Paragraph::new(Line::styled(rowstr.clone(), Style::new().fg(color))).alignment(Alignment::Center),
                Rect { x: inner.x, y: inner.y + r as u16, width: inner.width, height: 1 },
            );
        }
    }

    let Some(src) = &slot.kit.source else {
        if inner.height < 6 {
            f.render_widget(Paragraph::new("—").fg(DIM).alignment(Alignment::Center), inner);
        }
        return;
    };
    let props = slot.kit.props;
    let mut lines = vec![
        Line::from(Span::styled(src.title.clone(), Style::new().bold())),
        Line::from(Span::styled(format!("{:.2} s", slot.frames as f64 / out_rate as f64), Style::new().fg(Color::Gray))),
    ];
    let status = match &slot.status {
        PadStatus::Ready | PadStatus::Empty => None,
        PadStatus::Loading => Some(Span::styled("loading…", Style::new().fg(DIM))),
        PadStatus::Missing => Some(Span::styled("MISSING", Style::new().fg(ERROR).bold())),
        PadStatus::Changed => Some(Span::styled("CHANGED", Style::new().fg(ERROR).bold())),
        PadStatus::Failed(_) => Some(Span::styled("ERROR", Style::new().fg(ERROR).bold())),
    };
    let mut flags = vec![Span::styled(
        match props.mode {
            Mode::OneShot => "1-SHOT",
            Mode::Gate => "GATE",
            Mode::Toggle => "TOGGLE",
        },
        Style::new().fg(AMBER),
    )];
    if props.looped {
        flags.push(Span::styled(" ⟲", Style::new().fg(PLAYING).bold()));
    }
    if props.gain_db != 0.0 {
        flags.push(Span::styled(format!(" {:+.0}dB", props.gain_db), Style::new().fg(Color::Gray)));
    }
    flags.push(Span::raw("  "));
    flags.push(status.unwrap_or_else(|| Span::styled(fmt_bytes(slot.bytes as u64), Style::new().fg(DIM))));
    lines.push(Line::from(flags));
    let text_area = if inner.height >= 6 {
        Rect { x: inner.x, y: inner.y + 5, width: inner.width, height: inner.height.saturating_sub(5) }
    } else {
        inner
    };
    f.render_widget(Paragraph::new(lines), text_area);

    // Progress along the bottom row while playing.
    if state.playing && inner.height >= 4 {
        let y = inner.y + inner.height - 1;
        let filled = (state.progress * inner.width as f32).round() as u16;
        let bar = Rect { x: inner.x, y, width: filled.min(inner.width), height: 1 };
        f.render_widget(Paragraph::new("█".repeat(bar.width as usize)).fg(PLAYING), bar);
    }
}

fn draw_pad_info(f: &mut Frame, area: Rect, app: &App) {
    let i = match &app.screen {
        Screen::Editor(ed) => ed.pad,
        Screen::Picker(p) => p.pad,
        Screen::SampleBrowser(b) => b.pad,
        Screen::Pads | Screen::KitBrowser(_) | Screen::NamingKit(_) | Screen::Settings => app.selected,
    };
    let slot = &app.slots[i];
    let block = Block::bordered()
        .title(Line::from(vec![
            Span::styled(" ", Style::new().fg(DIM)),
            Span::styled(PAD_KEYS[i].to_ascii_uppercase().to_string(), Style::new().fg(Color::Gray).bold()),
            Span::styled(format!(" Pad {} ", i + 1), Style::new().fg(DIM)),
        ]))
        .border_style(Style::new().fg(DIM));
    let label = |k: &str| Span::styled(format!("{k:<10}"), Style::new().fg(DIM));
    let key = |k: &str| Span::styled(format!("{k:<10}"), Style::new().fg(Color::Gray));
    let ctrl = |k: &str, name: &str, value: Span<'static>| {
        Line::from(vec![key(k), Span::styled(format!("{name:<8}"), Style::new().fg(DIM)), value])
    };
    let mut lines = Vec::new();
    match &slot.kit.source {
        Some(src) => {
            let name_text = match &src.artist {
                Some(a) => format!("{} ({a})", src.title),
                None => src.title.clone(),
            };
            let mut name = vec![Span::styled(src.title.clone(), Style::new().bold())];
            if let Some(a) = &src.artist {
                name.push(Span::styled(format!(" ({a})"), Style::new().fg(Color::Gray)));
            }
            lines.push(Line::from(name));
            // Pads to 3 rows, however many the name itself wraps to, so
            // "Length" below always lands on the same row.
            let rows = wrap(&name_text, area.width.saturating_sub(2).max(1) as usize).len().clamp(1, 3);
            lines.extend((rows..3).map(|_| Line::raw("")));
            lines.push(Line::from(vec![label("Length"), Span::raw(format!("{:.3} s", slot.frames as f64 / app.out_rate() as f64))]));
            let state = match &slot.status {
                PadStatus::Ready => Span::raw(fmt_bytes(slot.bytes as u64)),
                PadStatus::Loading => Span::styled("loading…", Style::new().fg(DIM)),
                PadStatus::Missing => Span::styled("file missing", Style::new().fg(ERROR)),
                PadStatus::Changed => Span::styled("file changed", Style::new().fg(ERROR)),
                PadStatus::Failed(e) => Span::styled(e.clone(), Style::new().fg(ERROR)),
                PadStatus::Empty => Span::raw("—"),
            };
            lines.push(Line::from(vec![label("Memory"), state]));
        }
        None => {
            lines.push(ctrl("Enter", "Assign", Span::raw("")));
            f.render_widget(Paragraph::new(lines).block(block).wrap(ratatui::widgets::Wrap { trim: false }), area);
            return;
        }
    }
    let p = slot.kit.props;
    lines.push(Line::raw(""));
    lines.push(ctrl("M", "Mode", Span::styled(p.mode.label(), Style::new().fg(AMBER).bold())));
    lines.push(ctrl(
        "L",
        "Loop",
        if p.looped { Span::styled("on", Style::new().fg(PLAYING).bold()) } else { Span::styled("off", Style::new().fg(DIM)) },
    ));
    lines.push(ctrl(
        "G",
        "Retrig",
        if p.retrigger { Span::styled("on", Style::new().fg(PLAYING).bold()) } else { Span::styled("off", Style::new().fg(DIM)) },
    ));
    lines.push(ctrl("-/=", "Gain", Span::raw(format!("{:+.0} dB", p.gain_db))));
    lines.push(Line::raw(""));
    lines.push(ctrl("T", "Trim", Span::raw("")));
    lines.push(ctrl("Enter", "Assign", Span::raw("")));
    lines.push(ctrl("Del", "Clear", Span::raw("")));
    f.render_widget(Paragraph::new(lines).block(block).wrap(ratatui::widgets::Wrap { trim: false }), area);
}

fn draw_memory(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Memory ").border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let samples = app.sample_bytes() as u64;
    let preview = match &app.screen {
        Screen::Editor(ed) => ed.preview_bytes as u64,
        _ => 0,
    };
    let row = |k: &str, v: String, style: Style| {
        Line::from(vec![Span::styled(format!("{k:<10}"), Style::new().fg(DIM)), Span::styled(v, style)])
    };
    let mut lines = vec![row("Samples", fmt_bytes(samples), Style::new().fg(AMBER).bold())];
    if preview > 0 {
        lines.push(row("Preview", fmt_bytes(preview), Style::new()));
    }
    let process = app.memory.process;
    lines.push(row("Process", process.map_or("?".into(), fmt_bytes), Style::new()));
    if let Some(system) = app.memory.system {
        lines.push(row("System", fmt_bytes(system), Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(lines), inner);

    // Samples and the rest of the process, against the machine's memory.
    if let (Some(process), Some(system)) = (process, app.memory.system)
        && inner.height >= 5
    {
        let w = inner.width.saturating_sub(7) as f64;
        let cols = |bytes: u64| ((bytes as f64 / system as f64) * w).ceil().min(w) as usize;
        let (a, b) = (cols(samples + preview), cols(process.max(samples + preview)));
        let bar = Line::from(vec![
            Span::styled("█".repeat(a), Style::new().fg(AMBER)),
            Span::styled("█".repeat(b - a), Style::new().fg(Color::Gray)),
            Span::styled("░".repeat(w as usize - b), Style::new().fg(DIM)),
            Span::styled(format!(" {:>4.1}%", process as f64 / system as f64 * 100.0), Style::new().fg(DIM)),
        ]);
        let y = inner.y + inner.height - 1;
        f.render_widget(Paragraph::new(bar), Rect { y, height: 1, ..inner });
    }
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    if let Some(msg) = app.status() {
        let line = Line::from(Span::styled(format!(" {msg}"), Style::new().fg(AMBER)));
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let hints: &[(&str, &str)] = match &app.screen {
        Screen::Pads => &[("↑↓←→", "select"), ("Shift+pad", "stop"), ("Space", "stop all")],
        Screen::Picker(_) => {
            &[("type", "to filter"), ("↑↓ PgUp PgDn Home End", "select"), ("Enter", "assign"), ("Esc", "clear/close")]
        }
        Screen::Editor(_) => &[
            ("Tab", "start/end"),
            ("←/→", "beat (Shift: bar)"),
            (",/.", "±10 ms (Shift: 1 ms)"),
            ("[ ]", "halve/double"),
            ("Space", "preview"),
            ("Enter", "keep"),
            ("Esc", "cancel"),
        ],
        Screen::KitBrowser(_) => &[("↑↓", "select"), ("Enter", "choose/create"), ("Del", "delete"), ("Esc", "cancel")],
        Screen::SampleBrowser(_) => &[("↑↓", "select"), ("Enter", "choose/open"), ("Esc", "cancel")],
        Screen::NamingKit(_) => &[("Enter", "save as this name"), ("Esc", "leave unsaved for now")],
        Screen::Settings => &[],
    };
    if !matches!(app.screen, Screen::Pads) {
        f.render_widget(Paragraph::new(hints_line(hints)), area);
        return;
    }
    // Contextual keys left, globals pinned right — mirrors odj-player's
    // `draw_footer`, the formatting every app's footer is meant to
    // follow; see its own doc comment for the convention itself.
    let left = hints_line(hints);
    let right = hints_line(&[("K", "kits"), ("`", "settings"), ("?", "help"), ("Ctrl+C", "quit")]);
    let rw = right.width() as u16;
    let [left_area, _, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(left), left_area);
    f.render_widget(Paragraph::new(right), right_area);
}

/// Each key highlighted (bold, set apart from the rest), what it does dim —
/// the same convention [`draw_pad_info`]'s shortcuts use.
fn hints_line(parts: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
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

/// Settings (`` ` ``): one tab, MIDI, with the same list odj-mixer's own
/// Settings shows and the same `Enter` use / `X` forget convention. The
/// single tab is still drawn as a tab — not just a title — so it reads as
/// the same kind of screen as odj-mixer's multi-tab one, and there's room
/// for more here later without changing the shape.
fn draw_settings(f: &mut Frame, rows: &[DeviceRow], selected: usize, current: Option<&str>) {
    let area = centered(f.area(), 70, 70);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Settings ")
        .title_bottom(key_hint("Esc", "Close", AMBER).left_aligned())
        .title_bottom(hints_line(&[("Enter", "use"), ("X", "forget mappings")]).centered())
        .border_style(Style::new().fg(AMBER));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [tabs_area, _, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    f.render_widget(Paragraph::new(Line::from(tab_span("MIDI", true))), tabs_area);

    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new("(no MIDI device used or connected yet)").fg(DIM)]
    } else {
        let width = body.width.saturating_sub(2) as usize;
        rows.iter()
            .map(|r| {
                // Same 3-icon convention as odj-mixer's MIDI tab: a
                // distinct shape for a device known only from its saved
                // mappings, separate from "present but not the active one".
                let is_current = current == Some(r.name.as_str());
                let (icon, icon_color) = match (r.present, is_current) {
                    (false, _) => ("✕", DIM),
                    (true, true) => ("●", AMBER),
                    (true, false) => ("○", DIM),
                };
                let mappings = format!("{} mapping{}", r.mappings, if r.mappings == 1 { "" } else { "s" });
                let pad = width.saturating_sub(r.name.chars().count() + mappings.chars().count() + 2).max(1);
                ListItem::new(Line::from(vec![
                    Span::raw(r.name.clone()),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(mappings, Style::new().fg(DIM)),
                    Span::raw(" "),
                    Span::styled(icon, Style::new().fg(icon_color)),
                ]))
            })
            .collect()
    };
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, body, &mut state);
}

fn draw_picker(f: &mut Frame, p: &Picker) {
    let area = centered(f.area(), 80, 80);
    let mut block =
        Block::bordered().title(format!(" Assign a cue to pad {} ", p.pad + 1)).border_style(Style::new().fg(AMBER));
    if !p.query.is_empty() {
        block = block.title(Line::from(format!(" filter: {}▏", p.query)).right_aligned().bold());
    }
    let items: Vec<ListItem> = if p.visible.is_empty() {
        vec![ListItem::new("(no matches)").fg(DIM)]
    } else {
        p.visible
            .iter()
            .map(|(i, matched)| {
                let e = &p.entries[*i];
                let sr = e.sample_rate as f64;
                let mut spans: Vec<Span> = e
                    .name
                    .chars()
                    .enumerate()
                    .map(|(k, c)| {
                        if matched.contains(&k) {
                            Span::styled(c.to_string(), Style::new().fg(AMBER).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
                        } else {
                            Span::raw(c.to_string())
                        }
                    })
                    .collect();
                spans.push(Span::styled(format!("   {:<10}", e.kind), Style::new().fg(Color::Gray)));
                spans.push(Span::raw(time(e.cue.pos / sr)));
                match e.cue.loop_out {
                    Some(out) => spans.push(Span::styled(
                        format!("   loop {:.2} s", (out - e.cue.pos) / sr),
                        Style::new().fg(PLAYING),
                    )),
                    None => spans.push(Span::styled("   cue → edit", Style::new().fg(DIM))),
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };
    let hints = [
        "type to filter",
        "↑↓ PgUp PgDn Home End select",
        "Enter assign (a cue without an end opens the editor)",
        "Esc clear/close",
    ];
    let list_area = hinted_box(f, area, block, &hints);
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(p.selected));
    f.render_stateful_widget(list, list_area, &mut state);
}

fn draw_kit_browser(f: &mut Frame, browser: &KitBrowserState) {
    // Wide and tall: a narrower box left jagged fragments of the pad grid's
    // own row borders peeking out on both sides (they don't share row
    // boundaries with a centered popup), which reads as broken rather than
    // as a background showing through a deliberately smaller dialog.
    let area = centered(f.area(), 92, 90);
    match &browser.mode {
        KitBrowserMode::Naming(name) => {
            let block = Block::bordered().title(" Name the new kit ").border_style(Style::new().fg(AMBER));
            let hints = ["Enter create", "Esc back"];
            let inner = hinted_box(f, area, block, &hints);
            f.render_widget(Paragraph::new(format!("{name}▏")), inner);
        }
        KitBrowserMode::ConfirmingDelete(i) => draw_delete_warning(f, area, &browser.rows[*i].label()),
        KitBrowserMode::List => {
            let block = Block::bordered().title(" Pick a kit ").border_style(Style::new().fg(AMBER));
            let hints = ["↑↓ select", "Enter choose/create", "Del/Backspace delete", "Esc cancel"];
            let inner = hinted_box(f, area, block, &hints);
            let [list_area, detail_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
            let items: Vec<ListItem> = browser
                .rows
                .iter()
                .map(|r| {
                    let item = ListItem::new(r.label());
                    match r.color() {
                        Some(c) => item.style(Style::new().fg(c)),
                        None => item,
                    }
                })
                .collect();
            let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
            let mut state = ListState::default().with_selected(Some(browser.selected));
            f.render_stateful_widget(list, list_area, &mut state);
            let detail = browser.rows[browser.selected].detail();
            f.render_widget(Paragraph::new(detail).fg(DIM), detail_area);
        }
    }
}

/// A hard-to-miss, all-red confirmation before a kit (its whole directory,
/// every sample in it) is deleted with no way to undo it.
fn draw_delete_warning(f: &mut Frame, area: Rect, kit_name: &str) {
    let block = Block::bordered()
        .title(Span::styled(" DELETE KIT ", Style::new().fg(Color::Black).bg(Color::Red).bold()))
        .border_style(Style::new().fg(Color::Red));
    let hints = ["Enter: DELETE permanently", "any other key: cancel"];
    let inner = hinted_box(f, area, block, &hints);
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
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center), inner);
}

fn draw_sample_browser(f: &mut Frame, browser: &SampleBrowser) {
    // Wide and tall, same reasoning as draw_kit_browser: minimizes the pad
    // grid fringe left visible around the edges.
    let area = centered(f.area(), 92, 90);
    let block = Block::bordered()
        .title(format!(" Assign a sample to pad {} ", browser.pad + 1))
        .border_style(Style::new().fg(AMBER));
    let hints = ["↑↓ select", "Enter choose/open", "Esc cancel"];
    let list_area = hinted_box(f, area, block, &hints);
    let items: Vec<ListItem> = browser.entries.iter().map(|e| ListItem::new(e.label())).collect();
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(browser.selected));
    f.render_stateful_widget(list, list_area, &mut state);
}

fn draw_naming_kit(f: &mut Frame, name: &str) {
    // Wide (same reasoning as draw_kit_browser), but short — it's only
    // ever three lines, so a tall box would mostly be empty space.
    let area = centered(f.area(), 92, 25);
    let block = Block::bordered().title(" Name this kit ").border_style(Style::new().fg(AMBER));
    let hints = ["Enter save", "Esc leave unsaved for now"];
    let inner = hinted_box(f, area, block, &hints);
    let text = vec![
        Line::from(format!("{name}▏")),
        Line::from(""),
        Line::from(Span::styled("This kit hasn't been saved yet.", Style::new().fg(DIM))),
    ];
    f.render_widget(Paragraph::new(text), inner);
}

fn draw_editor(f: &mut Frame, area: Rect, ed: &Editor, s: &Snapshot, out_rate: u32) {
    let src = &ed.source;
    let block = Block::bordered()
        .title(format!(" Sample editor · pad {} · {} ", ed.pad + 1, src.name()))
        .border_style(Style::new().fg(AMBER));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(track) = &ed.track else {
        let msg = format!("Loading {}…", src.path.display());
        f.render_widget(Paragraph::new(msg).alignment(Alignment::Center).fg(DIM), inner);
        return;
    };
    let [info, ruler, wave] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Min(3)]).areas(inner);

    let sr = src.sample_rate as f64;
    let (start, end) = (ed.start / sr, ed.end / sr);
    let secs = end - start;
    let bytes = ((secs * out_rate as f64).round() as u64 + TAIL as u64) * 8;
    let dim = |t: &str| Span::styled(t.to_string(), Style::new().fg(DIM));
    let mut length = vec![dim("LENGTH "), Span::styled(format!("{secs:.3} s"), Style::new().bold())];
    if let Some(beat) = ed.beat() {
        let beats = ed.len() / beat;
        length.push(Span::styled(format!("  {} beats", fmt_beats(beats)), Style::new().fg(AMBER).bold()));
        if (beats / 4.0 - (beats / 4.0).round()).abs() < 0.01 {
            length.push(Span::styled(format!(" = {} bars", (beats / 4.0).round()), Style::new().fg(AMBER)));
        }
    }
    length.push(dim("   MEM "));
    length.push(Span::raw(fmt_bytes(bytes)));
    if ed.previewing {
        length.push(Span::styled("   ▶ PREVIEW", Style::new().fg(PLAYING).bold()));
    }
    let bpm = match ed.grid() {
        Some(g) if ed.grid_edited() => format!("{:.2} (your grid)", g.bpm),
        Some(g) => format!("{:.2}", g.bpm),
        None => "none, steps are seconds".into(),
    };
    // The edge the keys move is highlighted.
    let edge = |name: &str, t: f64, active: bool, color: Color| {
        if active {
            vec![
                Span::styled(format!(" {name} "), Style::new().fg(Color::Black).bg(color).bold()),
                Span::styled(format!(" {}", time(t)), Style::new().fg(color).bold()),
            ]
        } else {
            vec![dim(&format!(" {name}  ")), Span::raw(time(t))]
        }
    };
    let mut edges = edge("START", start, ed.editing_start, PLAYING);
    edges.push(Span::raw("   "));
    edges.extend(edge("END", end, !ed.editing_start, AMBER));
    edges.extend([dim("   BPM "), Span::raw(bpm), dim("   Tab: move the other end")]);
    let lines = vec![Line::from(length), Line::from(edges)];
    f.render_widget(Paragraph::new(lines), info);

    // Show the region with some track around it.
    let span = (secs * 1.3).max(1.0);
    let view = (start - span * 0.1, start + span * 0.9);
    let playhead = s.preview.filter(|p| ed.previewing && p.playing).map(|p| start + p.progress as f64 * secs);
    let grid = ed.grid().map(|g| BeatGrid { anchor: g.anchor / sr, ..g });
    f.render_widget(Ruler { view, start, end, grid }, ruler);
    f.render_widget(Wave { track, view, start, end, playhead }, wave);
}

/// Beat and bar ticks of the grid, with the region's start and end.
struct Ruler {
    view: (f64, f64),
    start: f64,
    end: f64,
    /// In seconds: the anchor is in seconds and the rate is 1.
    grid: Option<BeatGrid>,
}

impl Widget for Ruler {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 {
            return;
        }
        let per_col = (self.view.1 - self.view.0) / area.width as f64;
        let col = |t: f64| {
            let c = ((t - self.view.0) / per_col).floor();
            (c >= 0.0 && c < area.width as f64).then_some(area.x + c as u16)
        };
        let mut put = |t: f64, ch: char, style: Style| {
            if let Some(x) = col(t)
                && let Some(cell) = buf.cell_mut((x, area.y))
            {
                cell.set_char(ch).set_style(style);
            }
        };
        if let Some(g) = self.grid.filter(|g| g.period(1.0) / per_col >= 1.0) {
            let mut n = g.beat_at(self.view.0, 1.0).ceil();
            while g.beat_pos(n, 1.0) < self.view.1 {
                let bar = n.rem_euclid(BAR as f64) == 0.0;
                let ch = if bar { '|' } else { '·' };
                let style = if bar { Style::new().fg(Color::Gray) } else { Style::new().fg(DIM) };
                put(g.beat_pos(n, 1.0), ch, style);
                n += 1.0;
            }
        }
        put(self.start, '[', Style::new().fg(PLAYING).bold());
        put(self.end, ']', Style::new().fg(AMBER).bold());
    }
}

/// The track's waveform over `view`, with the region shaded and the preview playhead.
struct Wave<'a> {
    track: &'a Track,
    view: (f64, f64),
    start: f64,
    end: f64,
    playhead: Option<f64>,
}

impl Widget for Wave<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (w, h) = (area.width as usize, area.height as usize);
        let per_col = (self.view.1 - self.view.0) / w as f64;
        let bins = &self.track.wave;
        for x in 0..w {
            let t0 = self.view.0 + x as f64 * per_col;
            let t1 = t0 + per_col;
            let inside = t1 > self.start && t0 < self.end;
            let b0 = (t0 * WAVE_RATE).max(0.0) as usize;
            let b1 = ((t1 * WAVE_RATE).ceil().max(0.0) as usize).min(bins.len()).max(b0 + 1);
            let (eighths, color) = if t1 <= 0.0 || b0 >= bins.len() {
                (0, DIM)
            } else {
                let (peak, low) = bins[b0..b1].iter().fold((0.0f32, 0.0f32), |(p, l), b| (p.max(b.peak), l.max(b.low)));
                let level = peak.min(1.0).powf(0.6);
                let bass = (low / peak.max(1e-6)).clamp(0.0, 1.0);
                let mut c = lerp((235, 240, 255), (30, 100, 255), bass);
                if !inside {
                    c = lerp(c, (0, 0, 0), 0.6);
                }
                ((level * (h * 8) as f32).round() as usize, Color::Rgb(c.0, c.1, c.2))
            };
            for row in 0..h {
                let fill = eighths.saturating_sub((h - 1 - row) * 8).min(8);
                if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + row as u16)) {
                    cell.set_char(BARS[fill]).set_fg(color);
                    if inside {
                        cell.set_bg(REGION_BG);
                    }
                }
            }
        }
        if let Some(t) = self.playhead {
            let c = ((t - self.view.0) / per_col).floor();
            if c >= 0.0 && c < w as f64 {
                for row in 0..area.height {
                    if let Some(cell) = buf.cell_mut((area.x + c as u16, area.y + row)) {
                        cell.set_bg(PLAYHEAD);
                    }
                }
            }
        }
    }
}

/// The help's pages: playing the pads first, then setting them up.
pub const HELP: &[Page] = &[
    Page::Keys(
        "Basics",
        &[
            ("1 2 3 4 / Q W E R", "Pads 1–8 (press to play, also selects)"),
            ("A S D F / Z X C V", "Pads 9–16"),
            ("Shift+pad", "Stop that pad"),
            ("Space", "Stop all pads"),
            ("← → ↑ ↓", "Select a pad"),
            ("Enter", "Browse samples to assign to the selected pad"),
            ("K", "Browse kits: switch, or create a new one"),
        ],
    ),
    Page::Keys(
        "Pads",
        &[
            ("Enter", "Browse samples to assign to the selected pad"),
            ("K", "Browse kits: switch, or create a new one"),
            ("T", "Trim: open the selected pad in the sample editor"),
            ("M", "Mode: one-shot · gate (plays while held) · toggle"),
            ("L", "Loop on/off"),
            ("G", "Retrigger: a one-shot press while playing restarts or is ignored"),
            ("- / = / 0", "Gain down / up / 0 dB"),
            ("Delete / Backspace", "Clear the selected pad"),
        ],
    ),
    Page::Keys(
        "Sample browser",
        &[
            ("↑ ↓", "Select a row"),
            ("Enter", "Assign the sample, open the kit, or browse odj-player's cues"),
            ("Esc", "Close without assigning"),
        ],
    ),
    Page::Keys(
        "Kit browser",
        &[
            ("↑ ↓", "Select a row"),
            ("Enter", "Switch to that kit, or start naming a new one"),
            ("Delete / Backspace", "Delete the selected kit (asks to confirm first, in red)"),
            ("(typing)", "Name a new kit; Enter creates it, Esc cancels"),
            ("Esc", "Close without switching"),
        ],
    ),
    Page::Keys(
        "Name this kit",
        &[
            ("(typing)", "A kit changed for the first time has no name yet"),
            ("Enter", "Save it under this name, creating its folder"),
            ("Esc", "Leave it unsaved for now; asked again next change"),
        ],
    ),
    Page::Keys(
        "Sample editor",
        &[
            ("Tab", "Switch between moving the start and the end"),
            ("← / →", "Move it a beat along the grid (Shift: a bar); without a grid 0.1 s (Shift: 1 s)"),
            (", / .", "Move it ±10 ms (Shift: ±1 ms)"),
            ("[ / ]", "Halve / double the length"),
            ("Space", "Preview (loops)"),
            ("Enter / Esc", "Put it on the pad / cancel"),
        ],
    ),
    Page::About(odj_core::about!()),
];

fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn time(secs: f64) -> String {
    let secs = secs.max(0.0);
    format!("{}:{:06.3}", (secs / 60.0) as u32, secs % 60.0)
}

fn fmt_beats(n: f64) -> String {
    if (n - n.round()).abs() < 0.01 { format!("{}", n.round()) } else { format!("{n:.2}") }
}

pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}
