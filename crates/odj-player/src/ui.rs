//! Drawing: the player display in the terminal.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Widget};

use odj_core::track::{Track, WAVE_RATE};
use odj_core::tui::{AMBER, DIM, centered};

use crate::app::App;
use crate::engine::{CD_FRAMES, Snapshot};

const CUE_COLOR: Color = Color::Rgb(255, 140, 0);
const HOT_COLOR: Color = Color::Rgb(60, 220, 100);
const PLAYHEAD: Color = Color::Rgb(210, 40, 40);
const MEMORY_COLOR: Color = Color::Rgb(255, 70, 70);
const LOOP_BG: Color = Color::Rgb(40, 70, 25);
const GRID_COLOR: Color = Color::Rgb(90, 200, 230);
/// Backgrounds of the columns a beat or a bar starts in.
const BEAT_BG: Color = Color::Rgb(38, 38, 44);
pub const BAR_BG: Color = Color::Rgb(75, 75, 88);
/// Seconds of track shown in the zoomed waveform.
const ZOOM_SPAN: f64 = 8.0;

pub fn draw(f: &mut Frame, app: &App, s: &Snapshot) {
    let [header, status, zoom, overview, info, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, header, app, s);
    draw_status(f, status, app, s);

    let zoom_block = Block::bordered().border_style(Style::new().fg(DIM));
    let overview_block = Block::bordered().border_style(Style::new().fg(DIM));
    let zoom_inner = zoom_block.inner(zoom);
    let overview_inner = overview_block.inner(overview);
    f.render_widget(zoom_block, zoom);
    f.render_widget(overview_block, overview);
    if let Some(track) = &s.track {
        let now = s.pos / track.sample_rate as f64;
        f.render_widget(
            Wave { track, s, start: now - ZOOM_SPAN / 2.0, end: now + ZOOM_SPAN / 2.0, dim_played: false, beats: s.show_grid },
            zoom_inner,
        );
        f.render_widget(
            Wave { track, s, start: 0.0, end: track.duration(), dim_played: true, beats: false },
            overview_inner,
        );
    } else {
        let msg = match app.loading() {
            Some(name) => format!("Loading {name}…"),
            None => "No track — press Tab to browse".to_string(),
        };
        f.render_widget(Paragraph::new(msg).alignment(Alignment::Center).fg(DIM), zoom_inner);
    }

    draw_info(f, info, app, s);
    draw_footer(f, footer, app);

    if app.browser.open {
        draw_browser(f, app);
    } else if app.show_help {
        draw_help(f);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    let mut left = vec![Span::styled(" ODJ ", Style::new().fg(Color::Black).bg(AMBER).bold()), Span::raw(" ")];
    if let Some((n, total)) = app.track_number {
        left.push(Span::styled("TRACK ", Style::new().fg(DIM)));
        left.push(Span::styled(format!("{n:02}"), Style::new().bold()));
        left.push(Span::styled(format!("/{total:02}  "), Style::new().fg(DIM)));
    }
    match (&s.track, app.loading()) {
        (_, Some(name)) => left.push(Span::styled(format!("Loading {name}…"), Style::new().fg(DIM))),
        (Some(t), None) => {
            left.push(Span::styled(t.title.clone(), Style::new().bold()));
            if let Some(artist) = &t.artist {
                left.push(Span::styled(format!(" — {artist}"), Style::new().fg(Color::Gray)));
            }
        }
        (None, None) => left.push(Span::styled("NO DISC", Style::new().fg(DIM))),
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);

    let mut right = Vec::new();
    if !app.key_release {
        right.push(Span::styled("no key-release events: holds are timed  ", Style::new().fg(Color::Yellow)));
    }
    right.push(Span::styled(format!("{} ", app.output_name), Style::new().fg(DIM)));
    f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), area);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    let block = Block::bordered().border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let (state, color) = if s.cue_preview {
        ("● CUE  ", CUE_COLOR)
    } else if s.playing && s.reverse {
        ("◀ REV  ", Color::Cyan)
    } else if s.playing {
        ("▶ PLAY ", Color::Green)
    } else if s.speed != 0.0 {
        ("▶ BRAKE", Color::Yellow)
    } else {
        ("‖ PAUSE", CUE_COLOR)
    };

    let mut spans = vec![Span::styled(format!(" {state} "), Style::new().fg(Color::Black).bg(color).bold()), Span::raw("  ")];

    if let Some(t) = &s.track {
        let elapsed = s.pos / t.sample_rate as f64;
        let remaining = (t.duration() - elapsed).max(0.0);
        // The remaining time flashes in the last 30 seconds.
        let warn = remaining < 30.0 && s.playing && (remaining * 2.0) as i64 % 2 == 0;
        let (label, value) = if app.show_remaining {
            ("REMAIN ", format!("-{}", display_time(remaining)))
        } else {
            ("TIME ", display_time(elapsed))
        };
        spans.push(Span::styled(label, Style::new().fg(DIM)));
        spans.push(Span::styled(value, Style::new().bold().fg(if warn { Color::Red } else { Color::White })));
        spans.push(Span::styled(" M:S:F", Style::new().fg(DIM)));
    } else {
        spans.push(Span::styled("TIME --:--:--", Style::new().fg(DIM)));
    }

    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        " A.CUE ",
        if app.auto_cue() { Style::new().fg(Color::Black).bg(CUE_COLOR).bold() } else { Style::new().fg(DIM) },
    ));
    spans.push(Span::raw("    "));
    spans.push(Span::styled("TEMPO ", Style::new().fg(DIM)));
    let tempo_style = if s.tempo == 0.0 { Style::new().fg(Color::Green) } else { Style::new().bold() };
    spans.push(Span::styled(format!("{:+.2}%", s.tempo), tempo_style));
    spans.push(Span::styled(format!(" ±{}", s.range.percent()).replace("±100", "WIDE"), Style::new().fg(DIM)));
    if s.bend != 0.0 {
        spans.push(Span::styled(format!(" JOG {:+.0}%", s.bend), Style::new().fg(Color::Cyan)));
    }

    spans.push(Span::raw("    "));
    let mt_style = match (s.master_tempo, s.master_tempo_available) {
        (true, _) => Style::new().fg(Color::Black).bg(Color::Red).bold(),
        (false, true) => Style::new().fg(DIM),
        (false, false) => Style::new().fg(DIM).add_modifier(Modifier::CROSSED_OUT),
    };
    spans.push(Span::styled(" MT ", mt_style));

    spans.push(Span::raw("    "));
    spans.push(Span::styled("BPM ", Style::new().fg(DIM)));
    match s.bpm {
        Some(bpm) => {
            let heard = bpm * (1.0 + s.tempo / 100.0);
            spans.push(Span::styled(format!("{heard:.1}"), Style::new().bold()));
            if s.grid_edited {
                spans.push(Span::styled(" GRID", Style::new().fg(GRID_COLOR)));
            }
        }
        None => spans.push(Span::styled("---.-", Style::new().fg(DIM))),
    }
    if let (Some(g), Some(t)) = (s.grid, &s.track)
        && s.show_grid
    {
        let (bar, beat) = g.bar_beat(s.pos, t.sample_rate as f64);
        spans.push(Span::styled("   BAR ", Style::new().fg(DIM)));
        spans.push(Span::styled(format!("{bar}.{beat} "), Style::new().bold()));
        for b in 1..=4 {
            let lit = Style::new().fg(if beat == 1 { Color::Red } else { GRID_COLOR });
            spans.push(if b == beat { Span::styled("■", lit) } else { Span::styled("□", Style::new().fg(DIM)) });
        }
    }
    spans.push(Span::raw("    "));
    let q_style = match (s.quantize, s.grid.is_some()) {
        (true, _) => Style::new().fg(Color::Black).bg(GRID_COLOR).bold(),
        (false, true) => Style::new().fg(DIM),
        (false, false) => Style::new().fg(DIM).add_modifier(Modifier::CROSSED_OUT),
    };
    spans.push(Span::styled(" QUANTIZE ", q_style));

    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn draw_info(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    let block = Block::bordered().border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let sr = s.track.as_ref().map_or(44_100, |t| t.sample_rate) as f64;

    let mut spans = vec![if app.hot_rec {
        Span::styled(" REC ", Style::new().fg(Color::Black).bg(Color::Red).bold())
    } else {
        Span::styled(" PLAY", Style::new().fg(DIM))
    }];
    // A hot cue waiting for the beat blinks with the beats.
    let blink = s.grid.is_some_and(|g| g.beat_at(s.pos, sr).rem_euclid(1.0) < 0.5);
    for (i, hot) in s.hot.iter().enumerate() {
        let letter = (b'A' + i as u8) as char;
        match hot {
            Some(h) => {
                let kind = if h.loop_out.is_some() { "⟲" } else { "" };
                let bg = if s.pending_hot == Some(i) && blink { AMBER } else { HOT_COLOR };
                spans.push(Span::styled(format!(" {letter} "), Style::new().fg(Color::Black).bg(bg).bold()));
                spans.push(Span::styled(format!(" {}{kind}  ", short_time(h.pos / sr)), Style::new().fg(HOT_COLOR)));
            }
            None => spans.push(Span::styled(format!(" {letter} ----  "), Style::new().fg(DIM))),
        }
    }

    spans.push(Span::raw("  "));
    let loop_style = if s.looping { Style::new().fg(Color::Black).bg(Color::Green).bold() } else { Style::new().fg(DIM) };
    spans.push(Span::styled(" LOOP ", loop_style));
    if s.loop_adjust {
        spans.push(Span::styled(" OUT ADJ ", Style::new().fg(Color::Black).bg(Color::Yellow).bold()));
    }
    if s.grid_adjust {
        spans.push(Span::styled(" GRID ADJ ", Style::new().fg(Color::Black).bg(GRID_COLOR).bold()));
    }
    match (s.loop_in, s.loop_out) {
        (Some(a), Some(b)) => {
            let len = match s.loop_beats {
                Some(n) => format!("{} beats", fmt_beats(n)),
                None => format!("{:.2}s", (b - a) / sr),
            };
            spans.push(Span::raw(format!(" {} → {}  {len}", short_time(a / sr), short_time(b / sr))));
        }
        (Some(a), None) => spans.push(Span::raw(format!(" IN {} …", short_time(a / sr)))),
        _ => spans.push(Span::styled(" --", Style::new().fg(DIM))),
    }

    spans.push(Span::raw("    "));
    spans.push(Span::styled("MEM ", Style::new().fg(DIM)));
    spans.push(Span::styled(s.memories.len().to_string(), Style::new().fg(MEMORY_COLOR)));
    spans.push(Span::raw("    "));
    spans.push(Span::styled("CUE ", Style::new().fg(DIM)));
    spans.push(Span::styled(short_time(s.cue / sr), Style::new().fg(CUE_COLOR)));
    spans.push(Span::raw("    "));
    spans.push(Span::styled(
        format!("START {:.2}s  BRAKE {:.2}s", s.start_time, s.brake_time),
        Style::new().fg(DIM),
    ));
    if s.reverse {
        spans.push(Span::styled("   REV", Style::new().fg(Color::Cyan).bold()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let line = match app.status() {
        Some(msg) => Line::from(Span::styled(format!(" {msg}"), Style::new().fg(AMBER))),
        None => Line::from(Span::styled(
            " Space play  C cue  1-3 hot cue  I/O/P loop  L auto loop  Q quantize  ↑↓ tempo  ,/. jog  ←→ search  Tab browse  ? help  Ctrl+C quit",
            Style::new().fg(DIM),
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_browser(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 70, 70);
    f.render_widget(Clear, area);
    let b = &app.browser;
    let mut block = Block::bordered()
        .title(format!(" {} ", b.dir.display()))
        .title_bottom(" type to filter  ↑↓ select  Enter/→ open  ←/Backspace up  Esc clear/close ")
        .border_style(Style::new().fg(AMBER));
    if !b.query.is_empty() {
        block = block.title(Line::from(format!(" filter: {}▏", b.query)).right_aligned().bold());
    }
    let items: Vec<ListItem> = if b.visible.is_empty() {
        let msg = if b.entries.is_empty() { "(no folders or audio files)" } else { "(no matches)" };
        vec![ListItem::new(msg).fg(DIM)]
    } else {
        b.visible
            .iter()
            .map(|(i, matched)| {
                let e = &b.entries[*i];
                let (icon, suffix, color) =
                    if e.is_dir { ("▸ ", "/", Color::Cyan) } else { ("♪ ", "", Color::Reset) };
                let mut spans = vec![Span::raw(icon)];
                spans.extend(e.name.chars().enumerate().map(|(k, c)| {
                    if matched.contains(&k) {
                        Span::styled(c.to_string(), Style::new().fg(AMBER).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
                    } else {
                        Span::raw(c.to_string())
                    }
                }));
                spans.push(Span::raw(suffix));
                ListItem::new(Line::from(spans)).fg(color)
            })
            .collect()
    };
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().fg(Color::Black).bg(AMBER))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(b.selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_help(f: &mut Frame) {
    let rows: &[(&str, &str)] = &[
        ("Space", "Play / Pause"),
        ("C", "Cue — playing: back to cue & pause · paused: set cue · hold on cue: preview"),
        ("Space while holding C", "Keep playing after releasing Cue"),
        ("1 2 3", "Hot cue A/B/C: jump & play (on the next beat when quantizing) · REC mode: store"),
        ("E", "Hot cue REC / PLAY mode"),
        ("Shift+1 2 3", "Clear hot cue (REC mode only)"),
        ("I / O / P", "Loop in / Loop out / Reloop-Exit"),
        ("O while looping", "Loop out adjust: , / . move the out point, O to finish"),
        ("L", "Auto beat loop (4 beats)"),
        ("W / J / K / X", "Memory: store cue or loop / call previous / call next / delete"),
        ("[ / ]", "Halve / double loop"),
        ("↑ / ↓", "Tempo fader (Shift: ×10)"),
        ("0 / G", "Reset tempo / cycle range ±6 ±10 ±16 WIDE"),
        ("M", "Master Tempo (key lock)"),
        (", / .", "Jog: hold to slow/speed up · paused: 1 frame (Shift: strong / 1 beat)"),
        ("Q", "Quantize this track: cues, loops and jumps go on the beat"),
        ("A / Shift+A", "Tap BPM (playing: the beats go on the taps) / back to the detected grid"),
        ("D", "Make the playhead beat 1 of a bar"),
        ("Y", "Grid adjust: , / . shift the beats 1 ms (Shift 10) · ↑/↓ BPM ±0.01 (Shift 0.1)"),
        ("← / →", "Search (hold) · Shift: super fast search"),
        ("B / N", "Previous / next track in folder"),
        ("R", "Reverse"),
        ("T / Shift+T", "Elapsed / remaining time · Auto Cue on/off"),
        ("S / V", "Cycle start / brake time"),
        ("Tab", "Browse files; type to filter the folder"),
        ("Ctrl+C", "Quit (cue memory is saved)"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k:>22}  "), Style::new().fg(AMBER).bold()),
                Span::raw(*v),
            ])
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

/// A waveform over a time window, with markers, loop region and playhead.
struct Wave<'a> {
    track: &'a Track,
    s: &'a Snapshot,
    start: f64,
    end: f64,
    dim_played: bool,
    /// Shade the columns where beats and bars start.
    beats: bool,
}

impl Widget for Wave<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        if area.width == 0 || area.height == 0 || self.end <= self.start {
            return;
        }
        let (w, h) = (area.width as usize, area.height as usize);
        let sr = self.track.sample_rate as f64;
        let per_col = (self.end - self.start) / w as f64;
        let now = self.s.pos / sr;
        let bins = &self.track.wave;
        let col_of = |sec: f64| -> Option<u16> {
            let c = ((sec - self.start) / per_col).floor();
            (c >= 0.0 && c < w as f64).then_some(c as u16)
        };
        let loop_range = match (self.s.loop_in, self.s.loop_out) {
            (Some(a), Some(b)) => Some((a / sr, b / sr)),
            _ => None,
        };
        let grid = self.s.grid.filter(|g| self.beats && g.period(sr) / sr > 2.0 * per_col);
        // Whether a beat starts in [t0, t1), and if it is a downbeat.
        let tick = |t0: f64, t1: f64| -> Option<bool> {
            let g = grid?;
            let n = g.beat_at(t0 * sr, sr).ceil();
            (g.beat_pos(n, sr) < t1 * sr).then_some(n.rem_euclid(4.0) == 0.0)
        };

        for x in 0..w {
            let t0 = self.start + x as f64 * per_col;
            let t1 = t0 + per_col;
            let in_loop = loop_range.is_some_and(|(a, b)| t1 > a && t0 < b);
            let b0 = (t0 * WAVE_RATE).max(0.0) as usize;
            let b1 = ((t1 * WAVE_RATE).ceil().max(0.0) as usize).min(bins.len()).max(b0 + 1);
            let bin = if t1 <= 0.0 || b0 >= bins.len() {
                None
            } else {
                Some(bins[b0..b1].iter().fold((0.0f32, 0.0f32), |(p, l), b| (p.max(b.peak), l.max(b.low))))
            };
            let (eighths, color) = match bin {
                Some((peak, low)) => {
                    let level = peak.min(1.0).powf(0.6);
                    let bass = (low / peak.max(1e-6)).clamp(0.0, 1.0);
                    let mut c = lerp((235, 240, 255), (30, 100, 255), bass);
                    if self.dim_played && t1 <= now {
                        c = lerp(c, (0, 0, 0), 0.6);
                    }
                    ((level * (h * 8) as f32).round() as usize, Color::Rgb(c.0, c.1, c.2))
                }
                None => (0, DIM),
            };
            for row in 0..h {
                let from_bottom = h - 1 - row;
                let fill = eighths.saturating_sub(from_bottom * 8).min(8);
                if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + row as u16)) {
                    cell.set_char(BARS[fill]).set_fg(color);
                    let bg = match (in_loop, tick(t0, t1)) {
                        (true, Some(_)) if self.s.looping => Some(Color::Rgb(70, 115, 45)),
                        (true, _) if self.s.looping => Some(LOOP_BG),
                        (true, _) => Some(Color::Rgb(30, 40, 25)),
                        (false, Some(true)) => Some(BAR_BG),
                        (false, Some(false)) => Some(BEAT_BG),
                        (false, None) => None,
                    };
                    if let Some(bg) = bg {
                        cell.set_bg(bg);
                    }
                }
            }
        }

        let mut marker = |sec: f64, ch: char, color: Color| {
            if let Some(x) = col_of(sec)
                && let Some(cell) = buf.cell_mut((area.x + x, area.y))
            {
                cell.set_char(ch).set_style(Style::new().fg(color).add_modifier(Modifier::BOLD));
            }
        };
        if let Some((a, b)) = loop_range {
            marker(a, '[', Color::Green);
            marker(b, ']', Color::Green);
        }
        for (i, hot) in self.s.hot.iter().enumerate() {
            if let Some(hc) = hot {
                marker(hc.pos / sr, (b'A' + i as u8) as char, HOT_COLOR);
            }
        }
        for m in &self.s.memories {
            marker(m.pos / sr, '▾', MEMORY_COLOR);
        }
        marker(self.s.cue / sr, '▼', CUE_COLOR);

        if let Some(x) = col_of(now) {
            for row in 0..area.height {
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + row)) {
                    cell.set_bg(PLAYHEAD);
                }
            }
        }
    }
}

fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Display time: minutes, seconds and 1/75 s frames.
fn display_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    let m = (secs / 60.0).floor();
    let s = (secs % 60.0).floor();
    let frames = (secs.fract() * CD_FRAMES).floor();
    format!("{m:02}:{s:02}:{frames:02}")
}

fn short_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    format!("{}:{:04.1}", (secs / 60.0).floor(), secs % 60.0)
}

fn fmt_beats(n: f64) -> String {
    if (n - n.round()).abs() < 0.01 {
        format!("{}", n.round())
    } else if n < 1.0 {
        format!("1/{}", (1.0 / n).round())
    } else {
        format!("{n:.2}")
    }
}
