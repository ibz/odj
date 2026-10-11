//! Drawing: the player display in the terminal.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Widget};

use odj_core::track::{Track, WAVE_RATE, WaveBin};
use odj_core::tui::{AMBER, DIM, Page, centered, draw_help, hinted_box, key_line};

use crate::app::App;
use crate::engine::{CD_FRAMES, Lane, Snapshot, SyncView};

const CUE_COLOR: Color = Color::Rgb(255, 140, 0);
const HOT_COLOR: Color = Color::Rgb(60, 220, 100);
const PLAYHEAD: Color = Color::Rgb(210, 40, 40);
const MEMORY_COLOR: Color = Color::Rgb(255, 70, 70);
const LOOP_BG: Color = Color::Rgb(40, 70, 25);
const GRID_COLOR: Color = Color::Rgb(90, 200, 230);
/// Backgrounds of the columns a beat or a bar starts in.
const BEAT_BG: Color = Color::Rgb(38, 38, 44);
pub const BAR_BG: Color = Color::Rgb(75, 75, 88);
const SLIP_COLOR: Color = Color::Rgb(200, 110, 255);
/// The column of the slip ghost, where the track would be.
pub const GHOST_BG: Color = Color::Rgb(95, 50, 125);
const SYNC_COLOR: Color = Color::Rgb(255, 200, 40);
const MASTER_COLOR: Color = Color::Rgb(255, 120, 40);
/// Waveform background, and the colours of its bass, mids and highs.
const WAVE_BG: Color = Color::Rgb(14, 14, 18);
const LOW_WAVE: (u8, u8, u8) = (40, 110, 255);
const MID_WAVE: (u8, u8, u8) = (255, 160, 40);
const HIGH_WAVE: (u8, u8, u8) = (235, 240, 255);
const CLIP_WAVE: (u8, u8, u8) = (255, 40, 40);
const CENTRE_LINE: Color = Color::Rgb(55, 55, 68);
const BEAT_TICK: Color = Color::Rgb(70, 110, 125);
/// The wave keeps at least this many rows when the beat lanes take room.
const MIN_WAVE_ROWS: usize = 4;
/// Cells of the phase meter, across one beat.
const PHASE_CELLS: usize = 17;
/// Seconds of track shown in the zoomed waveform.
const ZOOM_SPAN: f64 = 8.0;

pub fn draw(f: &mut Frame, app: &App, s: &Snapshot) {
    let [header, status, zoom, overview, info, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(4),
        // Two rows each side of the centre, and one on top for the markers.
        Constraint::Length(7),
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
            Wave { track, s, start: now - ZOOM_SPAN / 2.0, end: now + ZOOM_SPAN / 2.0, dim_played: false, beats: s.show_grid, lanes: true },
            zoom_inner,
        );
        f.render_widget(
            Wave { track, s, start: 0.0, end: track.duration(), dim_played: true, beats: false, lanes: false },
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
    } else if app.help.is_open() {
        draw_help(f, &app.help, HELP);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    let mut left = vec![Span::styled(" ODJ ", Style::new().fg(Color::Black).bg(AMBER).bold()), Span::raw(" ")];
    if let Some(n) = s.sync.deck {
        left.push(Span::styled(format!("DECK {n}  "), Style::new().fg(AMBER).bold()));
    }
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
    } else if s.sync.waiting {
        ("◆ WAIT ", SYNC_COLOR)
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
    spans.push(Span::raw(" "));
    let slip_style = if s.slip { Style::new().fg(Color::Black).bg(SLIP_COLOR).bold() } else { Style::new().fg(DIM) };
    spans.push(Span::styled(" SLIP ", slip_style));

    let [first, second] = Layout::vertical([Constraint::Length(1); 2]).areas(inner);
    f.render_widget(Paragraph::new(Line::from(spans)), first);
    f.render_widget(Paragraph::new(sync_line(&s.sync, s.bpm.map(|b| b * (1.0 + s.tempo / 100.0)))), second);
}

/// SYNC and MASTER, the phase meter, and the other decks.
fn sync_line(sync: &SyncView, heard_bpm: Option<f64>) -> Line<'static> {
    let badge = |on: bool, color: Color| {
        if on { Style::new().fg(Color::Black).bg(color).bold() } else { Style::new().fg(DIM) }
    };
    let mut spans = Vec::new();
    if sync.deck.is_none() {
        spans.push(Span::styled(" SYNC ", Style::new().fg(DIM).add_modifier(Modifier::CROSSED_OUT)));
        return Line::from(spans);
    }
    let label = if sync.synced && sync.leader.is_some() && !sync.phase_lock { " SYNC BPM " } else { " SYNC " };
    spans.push(Span::styled(label, badge(sync.synced, SYNC_COLOR)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(" MASTER ", badge(sync.master, MASTER_COLOR)));
    match sync.leader {
        Some(n) => spans.push(Span::styled(format!(" → D{n}"), Style::new().fg(SYNC_COLOR).bold())),
        None if sync.synced && !sync.master => spans.push(Span::styled(" no master yet", Style::new().fg(DIM))),
        None => {}
    }

    spans.push(Span::raw("    "));
    spans.push(Span::styled("PHASE ", Style::new().fg(DIM)));
    match sync.phase {
        Some((deck, beats)) => {
            let ms = heard_bpm.map_or(0.0, |bpm| beats * 60_000.0 / bpm);
            let color = match ms.abs() {
                m if m < 5.0 => Color::Green,
                m if m < 20.0 => Color::Yellow,
                _ => Color::Red,
            };
            let at = (((beats + 0.5) * PHASE_CELLS as f64) as usize).min(PHASE_CELLS - 1);
            let meter: String = (0..PHASE_CELLS)
                .map(|i| match i {
                    _ if i == at => '●',
                    _ if i == PHASE_CELLS / 2 => '┃',
                    _ => '·',
                })
                .collect();
            spans.push(Span::styled(format!("▕{meter}▏"), Style::new().fg(color)));
            spans.push(Span::styled(format!(" {ms:+4.0} ms"), Style::new().fg(color).bold()));
            spans.push(Span::styled(format!(" vs D{deck}"), Style::new().fg(DIM)));
        }
        None => spans.push(Span::styled(format!("▕{}▏", "·".repeat(PHASE_CELLS)), Style::new().fg(DIM))),
    }

    for p in &sync.others {
        spans.push(Span::raw("    "));
        let style = if p.master { Style::new().fg(MASTER_COLOR).bold() } else { Style::new().fg(Color::Gray) };
        spans.push(Span::styled(format!("D{} ", p.deck), style));
        spans.push(Span::styled(if p.playing { "▶ " } else { "‖ " }, Style::new().fg(DIM)));
        let bpm = p.bpm.map_or("---.--".to_string(), |b| format!("{b:.2}"));
        spans.push(Span::styled(bpm, style));
        if p.master {
            spans.push(Span::styled(" M", style));
        }
        if p.synced && !p.master {
            spans.push(Span::styled(" S", Style::new().fg(SYNC_COLOR)));
        }
    }
    Line::from(spans)
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
    if let Some(beats) = s.roll {
        spans.push(Span::styled(format!(" ROLL {} ", fmt_beats(beats)), Style::new().fg(Color::Black).bg(SLIP_COLOR).bold()));
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
        _ => spans.push(Span::styled(format!(" L: {} beats", fmt_beats(s.loop_size)), Style::new().fg(DIM))),
    }
    spans.push(Span::raw("    "));
    spans.push(Span::styled("JUMP ", Style::new().fg(DIM)));
    spans.push(Span::raw(fmt_beats(s.jump_beats)));

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
        None => key_line(
            " Space play  C cue  1-3 hot cue  I/O/P loop  L loop  -/= jump  Z slip  F sync  Q quantize  ↑↓ tempo  ,/. jog  ←→ search  Tab browse  ? help  Ctrl+C quit",
            area.width,
        ),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_browser(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 70, 70);
    let b = &app.browser;
    let mut block = Block::bordered().title(format!(" {} ", b.dir.display())).border_style(Style::new().fg(AMBER));
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
    let hints = [
        "type to filter",
        "↑↓ PgUp PgDn Home End select",
        "Enter/→ open",
        "←/Backspace up",
        "Esc clear/close",
        "Tab close",
    ];
    let list_area = hinted_box(f, area, block, &hints);
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(b.selected));
    f.render_stateful_widget(list, list_area, &mut state);
}

/// The help's pages: the keys for playing first, the rest by topic.
pub const HELP: &[Page] = &[
    Page::Keys(
        "Basics",
        &[
            ("Space", "Play / Pause"),
            ("C", "Cue"),
            ("1 2 3", "Hot cue A/B/C"),
            ("L", "Beat loop / exit the loop"),
            ("← / →", "Search (Shift: super fast)"),
            ("↑ / ↓", "Tempo (Shift: ×10)"),
            (", / .", "Jog: hold to slow down / speed up 3% (Shift: 12%)"),
            ("R", "Reverse"),
            ("F", "Sync to the master deck"),
            ("B / N", "Previous / next track in folder"),
            ("Tab", "File browser"),
        ],
    ),
    Page::Keys(
        "Cues",
        &[
            ("C", "Cue — playing: back to cue & pause · paused: set cue · hold on cue: preview"),
            ("Space while holding C", "Keep playing after releasing Cue"),
            ("1 2 3", "Hot cue A/B/C: jump & play (on the next beat when quantizing) · REC mode: store the cue, or the loop while looping"),
            ("1 2 3 held in slip", "Play the hot cue while held, then back to where the track would be"),
            ("E", "Hot cue REC / PLAY mode"),
            ("Shift+1 2 3", "Clear hot cue (REC mode only)"),
            ("W / J / K / X", "Memory: store cue or loop / call previous / call next / delete"),
            ("Shift+J", "Back to the start of the memory region you're in"),
            ("T", "Elapsed / remaining time"),
            ("Shift+T", "Auto Cue on/off"),
        ],
    ),
    Page::Keys(
        "Loops",
        &[
            ("I / O / P", "Loop in / Loop out / Reloop-Exit"),
            ("O while looping", "Loop out adjust: , / . move the out point (Shift: ×10), O to finish"),
            ("R while looping", "Play the loop backwards, R again for forwards"),
            ("L", "Beat loop of the loop size / exit the loop"),
            ("[ / ]", "Halve / double the roll or loop, else the next beat loop's size"),
            ("; (hold)", "Loop roll of the loop size, [ / ] change it; back to where the track would be"),
            ("- / =", "Beatjump back / forward (moves the loop while looping)"),
            ("Shift+- / =", "Beatjump size 1 … 64 beats"),
            ("Z", "Slip: loops, reverse, held hot cues and pause carry on underneath"),
        ],
    ),
    Page::Keys(
        "Tempo",
        &[
            ("↑ / ↓", "Tempo fader (Shift: ×10)"),
            ("0 / G", "Reset tempo / cycle range ±6 ±10 ±16 WIDE"),
            ("M", "Master Tempo (key lock)"),
            ("F", "Sync to the master deck's tempo and beats"),
            ("Shift+F", "Make this deck the master"),
            (", / .", "Jog: hold to slow down / speed up 3% (Shift: 12%) · paused: 1 frame (Shift: 1 beat, 1 s without a BPM)"),
            ("S / V", "Cycle start / brake time"),
        ],
    ),
    Page::Keys(
        "Beat grid",
        &[
            ("Q", "Quantize this track: cues, loops and jumps go on the beat (always on while synced)"),
            ("A", "Tap BPM (playing: the beats go on the taps)"),
            ("Shift+A", "Back to the detected grid"),
            ("D", "Make the playhead beat 1 of a bar"),
            ("Y", "Grid adjust: , / . shift the beats 1 ms (Shift: 10 ms) · ↑/↓ BPM ±0.01 (Shift: ±0.1) · Y or Esc to finish"),
        ],
    ),
    Page::About(odj_core::about!()),
];

/// A waveform over a time window, with markers, loop region and playhead.
struct Wave<'a> {
    track: &'a Track,
    s: &'a Snapshot,
    start: f64,
    end: f64,
    dim_played: bool,
    /// Show the beats and bars.
    beats: bool,
    /// A row for the markers on top, and rows under the wave for this deck's
    /// beats and the other decks'; clipping in red.
    lanes: bool,
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
        // The beat that starts in [t0, t1), if any.
        let tick = |t0: f64, t1: f64| -> Option<f64> {
            let g = grid?;
            let n = g.beat_at(t0 * sr, sr).ceil();
            (g.beat_pos(n, sr) < t1 * sr).then_some(n)
        };
        let lanes = self.lanes && h >= 2 + MIN_WAVE_ROWS;
        let shown = if lanes { shown_lanes(&self.s.sync.lanes, h - 2 - MIN_WAVE_ROWS) } else { Vec::new() };
        // Without the beat ruler, beats are shaded through the wave; grid
        // adjust shades them too, to line them up with the hits.
        let shade = self.beats && (!lanes || self.s.grid_adjust);

        // The wave is mirrored around a centre line, `half` rows each way; with
        // an odd height the spare row goes on top, under the markers.
        let (wave_top, wave_h) = if lanes { (1, h - 2 - shown.len()) } else { (0, h) };
        let half = wave_h / 2;
        let centre = wave_top + wave_h - half;
        let depth = (half * 8) as f32;
        let scale = self.track.wave_scale;
        let eighths = |level: f32| ((level / scale).min(1.0) * depth).round() as usize;
        let now_col = col_of(now);
        let ghost_col = self.s.ghost.and_then(|g| col_of(g / sr));

        for x in 0..w {
            let t0 = self.start + x as f64 * per_col;
            let t1 = t0 + per_col;
            let in_loop = loop_range.is_some_and(|(a, b)| t1 > a && t0 < b);
            let b0 = (t0 * WAVE_RATE).max(0.0) as usize;
            let b1 = ((t1 * WAVE_RATE).ceil().max(0.0) as usize).min(bins.len()).max(b0 + 1);
            let bin = (t1 > 0.0 && b0 < bins.len()).then(|| {
                bins[b0..b1].iter().fold(WaveBin::default(), |a, b| WaveBin {
                    peak: a.peak.max(b.peak),
                    low: a.low.max(b.low),
                    mid: a.mid.max(b.mid),
                    high: a.high.max(b.high),
                    clipped: a.clipped || b.clipped,
                })
            });
            let playhead = now_col == Some(x as u16);
            let bg = if playhead {
                PLAYHEAD
            } else if ghost_col == Some(x as u16) {
                GHOST_BG
            } else {
                match (in_loop, tick(t0, t1).filter(|_| shade).map(|n| n.rem_euclid(4.0) == 0.0)) {
                    (true, Some(_)) if self.s.looping => Color::Rgb(70, 115, 45),
                    (true, _) if self.s.looping => LOOP_BG,
                    (true, _) => Color::Rgb(30, 40, 25),
                    (false, Some(true)) => BAR_BG,
                    (false, Some(false)) => BEAT_BG,
                    (false, None) => WAVE_BG,
                }
            };
            // Bands from the centre out, each drawn over the next: bass, mids,
            // then the whole signal, whose rim shows the highs; clipping turns
            // the outer half row red.
            let shade = |c: (u8, u8, u8)| {
                let c = if playhead { lerp(c, (255, 90, 90), 0.4) } else { c };
                let played = if self.dim_played { 0.6 } else { 0.35 };
                let c = if t1 <= now { lerp(c, (0, 0, 0), played) } else { c };
                Color::Rgb(c.0, c.1, c.2)
            };
            let layers = bin.map(|b| {
                let full = eighths(b.peak);
                let tip = if b.clipped && self.lanes { full.saturating_sub(4) } else { full };
                [
                    (eighths(b.low).min(tip), shade(LOW_WAVE)),
                    (eighths(b.mid).min(tip), shade(MID_WAVE)),
                    (tip, shade(HIGH_WAVE)),
                    (full, shade(CLIP_WAVE)),
                ]
            });
            // The colour at `d` eighths out from the centre, and where it ends.
            let at = |d: usize| layers.and_then(|l| l.into_iter().find(|&(e, _)| e > d));

            for row in 0..h {
                let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + row as u16)) else {
                    continue;
                };
                cell.set_char(' ').set_bg(bg);
                if row < centre - half || row >= centre + half {
                    continue;
                }
                let k = if row < centre { centre - 1 - row } else { row - centre };
                let Some((end, inner)) = at(k * 8) else {
                    if row == centre - 1 && bin.is_some() {
                        cell.set_char('▁').set_fg(CENTRE_LINE);
                    }
                    continue;
                };
                let fill = (end - k * 8).min(8);
                let outer = if fill == 8 { inner } else { at(end).map_or(bg, |(_, c)| c) };
                if row < centre {
                    cell.set_char(BARS[fill]).set_fg(inner).set_bg(outer);
                } else {
                    // Growing down: the background colour fills from the bottom.
                    cell.set_char(BARS[8 - fill]).set_fg(outer).set_bg(inner);
                }
            }
        }

        if lanes {
            self.draw_beats(area, buf, per_col, &tick, &shown);
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
    }
}

impl Wave<'_> {
    /// The bottom rows: this deck's beats, with bar numbers, and right under
    /// them a row for each other deck's, so beats in phase line up.
    fn draw_beats(
        &self,
        area: Rect,
        buf: &mut Buffer,
        per_col: f64,
        tick: &dyn Fn(f64, f64) -> Option<f64>,
        lanes: &[Lane],
    ) {
        let ruler = area.y + area.height - 1 - lanes.len() as u16;
        let mut put = |x: u16, y: u16, ch: char, style: Style| {
            if x < area.width
                && let Some(cell) = buf.cell_mut((area.x + x, y))
            {
                cell.set_char(ch).set_style(style);
            }
        };
        for x in 0..area.width {
            let t0 = self.start + x as f64 * per_col;
            let t1 = t0 + per_col;
            if let Some(n) = tick(t0, t1) {
                if n.rem_euclid(4.0) == 0.0 {
                    let bar = (n / 4.0) as i64 + 1;
                    // Phrases start every 16 bars.
                    let color = if (bar - 1).rem_euclid(16) == 0 { AMBER } else { GRID_COLOR };
                    put(x, ruler, '┃', Style::new().fg(color).bold());
                    // Bars before the grid's first downbeat go unnumbered.
                    let number = if bar >= 1 { bar.to_string() } else { String::new() };
                    for (i, ch) in number.chars().enumerate() {
                        put(x + 1 + i as u16, ruler, ch, Style::new().fg(color));
                    }
                } else {
                    put(x, ruler, '│', Style::new().fg(BEAT_TICK));
                }
            }
        }
        for (i, lane) in lanes.iter().enumerate() {
            let row = ruler + 1 + i as u16;
            let color = lane_color(lane);
            if let Some(b) = lane.beats.filter(|b| b.period > 2.0 * per_col) {
                for x in 0..area.width {
                    let t0 = self.start + x as f64 * per_col;
                    let k = (b.beat + (t0 - b.at) / b.period).ceil();
                    if b.at + (k - b.beat) * b.period < t0 + per_col {
                        let ch = if k.rem_euclid(4.0) == 0.0 { '┃' } else { '│' };
                        put(x, row, ch, Style::new().fg(color));
                    }
                }
            }
            let label = format!("D{}{}", lane.deck, if lane.playing { "" } else { " ‖" });
            let style = if lane.beats.is_some() { Style::new().fg(color).bold() } else { Style::new().fg(DIM) };
            for (x, ch) in label.chars().enumerate() {
                put(x as u16, row, ch, style);
            }
        }
    }
}

/// The master in the sync colour, the other decks each in their own.
fn lane_color(lane: &Lane) -> Color {
    if lane.master {
        return SYNC_COLOR;
    }
    match lane.deck {
        1 => Color::Rgb(255, 120, 170),
        2 => Color::Rgb(110, 220, 150),
        3 => Color::Rgb(170, 150, 255),
        _ => Color::Rgb(120, 200, 255),
    }
}

/// The lanes that fit in `room` rows, by deck number; the master's stays.
fn shown_lanes(lanes: &[Lane], room: usize) -> Vec<Lane> {
    let (master, rest): (Vec<Lane>, Vec<Lane>) = lanes.iter().partition(|l| l.master);
    let mut shown: Vec<Lane> = master.into_iter().chain(rest).take(room).collect();
    shown.sort_by_key(|l| l.deck);
    shown
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
