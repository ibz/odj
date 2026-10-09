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
use odj_core::tui::{AMBER, DIM, centered};

use crate::app::{App, Editor, PAD_KEYS, PadStatus, Picker, Screen, Slot};
use crate::engine::{Mode, PADS, PadState, Snapshot, TAIL};

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
    } else if app.show_help {
        draw_help(f);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let used = app.slots.iter().filter(|s| s.kit.source.is_some()).count();
    let left = vec![
        Span::styled(" ODJ SAMPLER ", Style::new().fg(Color::Black).bg(AMBER).bold()),
        Span::styled(format!("  {used}/{PADS} pads"), Style::new().fg(DIM)),
    ];
    f.render_widget(Paragraph::new(Line::from(left)), area);

    let mut right = Vec::new();
    if !app.key_release {
        right.push(Span::styled("no key-release events: gate pads are timed  ", Style::new().fg(Color::Yellow)));
    }
    right.push(Span::styled("MEM ", Style::new().fg(DIM)));
    right.push(Span::styled(fmt_bytes(app.sample_bytes() as u64), Style::new().fg(AMBER).bold()));
    right.push(Span::styled(format!("   {} ", app.output_name), Style::new().fg(DIM)));
    f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), area);
}

fn draw_grid(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    let rows = Layout::vertical([Constraint::Ratio(1, 4); 4]).split(area);
    for (r, row) in rows.iter().enumerate() {
        let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(*row);
        for (c, cell) in cols.iter().enumerate() {
            let i = r * 4 + c;
            draw_pad(f, *cell, i, &app.slots[i], s.pads[i], i == app.selected);
        }
    }
}

fn draw_pad(f: &mut Frame, area: Rect, i: usize, slot: &Slot, state: PadState, selected: bool) {
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
        .title(format!(" {key} "))
        .title(Line::from(format!(" {} ", i + 1)).right_aligned())
        .border_style(border);
    if state.playing {
        block = block.style(Style::new().bg(PLAYING_BG));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(src) = &slot.kit.source else {
        f.render_widget(Paragraph::new("—").fg(DIM).alignment(Alignment::Center), inner);
        return;
    };
    let props = slot.kit.props;
    let mut lines = vec![
        Line::from(Span::styled(src.title.clone(), Style::new().bold())),
        Line::from(Span::styled(format!("{} · {:.2} s", src.cue, src.seconds()), Style::new().fg(Color::Gray))),
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
    f.render_widget(Paragraph::new(lines), inner);

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
        Screen::Pads => app.selected,
    };
    let slot = &app.slots[i];
    let block = Block::bordered()
        .title(format!(" Pad {} · key {} ", i + 1, PAD_KEYS[i].to_ascii_uppercase()))
        .border_style(Style::new().fg(DIM));
    let label = |k: &str| Span::styled(format!("{k:<10}"), Style::new().fg(DIM));
    let mut lines = Vec::new();
    match &slot.kit.source {
        Some(src) => {
            lines.push(Line::from(Span::styled(src.title.clone(), Style::new().bold())));
            if let Some(a) = &src.artist {
                lines.push(Line::from(Span::styled(a.clone(), Style::new().fg(Color::Gray))));
            }
            lines.push(Line::from(vec![label("Cue"), Span::raw(format!("{} at {}", src.cue, time(src.start / src.sample_rate as f64)))]));
            lines.push(Line::from(vec![label("Length"), Span::raw(format!("{:.3} s", src.seconds()))]));
            let state = match &slot.status {
                PadStatus::Ready => Span::raw(fmt_bytes(slot.bytes as u64)),
                PadStatus::Loading => Span::styled("loading…", Style::new().fg(DIM)),
                PadStatus::Missing => Span::styled("file missing", Style::new().fg(ERROR)),
                PadStatus::Changed => Span::styled("file changed", Style::new().fg(ERROR)),
                PadStatus::Failed(e) => Span::styled(e.clone(), Style::new().fg(ERROR)),
                PadStatus::Empty => Span::raw("—"),
            };
            lines.push(Line::from(vec![label("Memory"), state]));
            lines.push(Line::from(Span::styled(src.path.display().to_string(), Style::new().fg(DIM))));
        }
        None => lines.push(Line::from(Span::styled("Empty — Enter to assign a cue", Style::new().fg(DIM)))),
    }
    let p = slot.kit.props;
    let on_off = |b: bool| if b { Span::styled("on", Style::new().fg(PLAYING).bold()) } else { Span::styled("off", Style::new().fg(DIM)) };
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![label("Mode  M"), Span::styled(p.mode.label(), Style::new().fg(AMBER).bold())]));
    lines.push(Line::from(vec![label("Loop  L"), on_off(p.looped)]));
    lines.push(Line::from(vec![label("Retrig G"), on_off(p.retrigger)]));
    lines.push(Line::from(vec![label("Gain -/="), Span::raw(format!("{:+.0} dB", p.gain_db))]));
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
    let hints = match &app.screen {
        Screen::Pads => {
            " 1-4 Q-R A-F Z-V play (Shift stop)  arrows select  Enter assign  T trim  M L G -/= props  Del clear  Space stop all  ? help  Ctrl+C quit"
        }
        Screen::Picker(_) => " type to filter  ↑↓ select  Enter assign  Esc clear/close",
        Screen::Editor(_) => {
            " Tab start/end  ←/→ beat (Shift: bar)  ,/. ±10 ms (Shift: 1 ms)  [ ] halve/double  Space preview  Enter keep  Esc cancel"
        }
    };
    let line = match app.status() {
        Some(msg) => Line::from(Span::styled(format!(" {msg}"), Style::new().fg(AMBER))),
        None => Line::from(Span::styled(hints, Style::new().fg(DIM))),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_picker(f: &mut Frame, p: &Picker) {
    let area = centered(f.area(), 80, 80);
    f.render_widget(Clear, area);
    let mut block = Block::bordered()
        .title(format!(" Assign a cue to pad {} ", p.pad + 1))
        .title_bottom(" type to filter  ↑↓ select  Enter assign (a cue without an end opens the editor)  Esc clear/close ")
        .border_style(Style::new().fg(AMBER));
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
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().fg(Color::Black).bg(AMBER))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(p.selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_editor(f: &mut Frame, area: Rect, ed: &Editor, s: &Snapshot, out_rate: u32) {
    let src = &ed.source;
    let block = Block::bordered()
        .title(format!(" Sample editor · pad {} · {} · {} ", ed.pad + 1, src.name(), src.cue))
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
    let (start, end) = (src.start / sr, src.end / sr);
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

fn draw_help(f: &mut Frame) {
    let rows: &[(&str, &str)] = &[
        ("1 2 3 4 / Q W E R", "Pads 1–8 (press to play, also selects)"),
        ("A S D F / Z X C V", "Pads 9–16"),
        ("Shift+pad", "Stop that pad"),
        ("Space", "Stop all pads"),
        ("← → ↑ ↓", "Select a pad"),
        ("Enter", "Assign a stored cue to the selected pad"),
        ("T", "Trim: open the selected pad in the sample editor"),
        ("M", "Mode: one-shot · gate (plays while held) · toggle"),
        ("L", "Loop on/off"),
        ("G", "Retrigger: a one-shot press while playing restarts or is ignored"),
        ("- / = / 0", "Gain down / up / 0 dB"),
        ("Delete", "Clear the selected pad"),
        ("Editor Tab", "Switch between moving the start and the end"),
        ("Editor ← / →", "Move it to the next beat of the grid (Shift: 4 beats); seconds without a BPM"),
        ("Editor , / .", "Move it ±10 ms (Shift: ±1 ms)"),
        ("Editor [ / ]", "Halve / double the length"),
        ("Editor Space", "Preview (loops)"),
        ("Editor Enter / Esc", "Put it on the pad / cancel"),
        ("Ctrl+C", "Quit (the kit is saved as you go)"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![Span::styled(format!("{k:>20}  "), Style::new().fg(AMBER).bold()), Span::raw(*v)])
        })
        .collect();
    let height = lines.len() as u16 + 2;
    let area = centered(f.area(), 90, 100);
    let area = Rect { y: area.y + area.height.saturating_sub(height) / 2, height: height.min(area.height), ..area };
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered().title(" Keys ").title_bottom(" ? or Esc to close ").border_style(Style::new().fg(AMBER)),
        ),
        area,
    );
}

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
