//! MIDI pad controllers, shared by odj-sampler (pads trigger samples) and
//! odj-player (pads call memory points): which device is in use, each
//! pad's learned note per device, learning them (all 16 in a row, or one
//! pad at a time), and the Settings MIDI tab that manages all of it — a
//! device list (`Enter` use, `C` configure, `X` forget) and, drilled in, a
//! per-pad list (`Enter` learn, `X` clear). Each app supplies only what a
//! pad press does ([`PadHandler`]) and where its config is saved
//! ([`SaveFn`]); port listing and stable device identity are
//! [`crate::midi`]'s.
//!
//! Controllers don't agree on how they number their pads (confirmed on real
//! hardware: some send a straight ascending range, others group and jump
//! around), so each pad's note is learned from the device itself rather
//! than assumed — see [`learn_pad_notes`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::tui::{AMBER, DIM, centered};

/// How many pads a controller has here — the classic 4×4 grid.
pub const PADS: usize = 16;

/// What a pad press does: called on the MIDI thread with the pad index
/// and `true` for Note On, `false` for Note Off.
pub type PadHandler = Arc<dyn Fn(usize, bool) + Send + Sync>;

/// Persists a [`PadConfig`] into the app's own fields of the shared
/// settings file.
pub type SaveFn = Box<dyn Fn(&PadConfig) -> std::io::Result<()> + Send>;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PadConfig {
    /// The stable name of the MIDI input in use (see
    /// `crate::midi::stable_name`), resolved to a current port id when
    /// restoring rather than persisting a connection-order id.
    pub last: Option<String>,
    /// The MIDI note each pad responds to, per device: `controllers[name][pad]`.
    pub controllers: HashMap<String, HashMap<usize, u8>>,
}

impl PadConfig {
    /// The learned notes for `name`, `0` for an unlearned pad — `None` if
    /// nothing at all is learned on it.
    pub fn pad_notes_for(&self, name: &str) -> Option<[u8; PADS]> {
        let map = self.controllers.get(name)?;
        let mut notes = [0u8; PADS];
        for (pad, note) in map {
            if *pad < PADS {
                notes[*pad] = *note;
            }
        }
        Some(notes)
    }

    /// Remembers `notes` for `name` (`0` meaning unlearned).
    pub fn set_pad_notes_for(&mut self, name: String, notes: [u8; PADS]) {
        let map = notes.iter().enumerate().filter(|&(_, &n)| n != 0).map(|(pad, &n)| (pad, n)).collect();
        self.controllers.insert(name, map);
    }
}

/// One row of Settings' MIDI tab: the union of every device with learned
/// pad notes (whether or not it's plugged in right now) and every device
/// currently plugged in that has none yet — so a learned device still
/// shows up even unplugged, with the option to forget it, while a brand
/// new one can be picked the moment it's seen. `mappings` is the number of
/// pads learned on it (0 for a device that's merely present).
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    pub name: String,
    pub present: bool,
    pub mappings: usize,
}

/// The rows Settings' MIDI tab lists — same shape and ordering as
/// odj-mixer's own controller list (by stable name, known and present
/// unioned together).
pub fn device_rows(config: &PadConfig) -> Vec<DeviceRow> {
    let ports = crate::midi::list_ports();
    let mut names: Vec<String> = config.controllers.keys().cloned().collect();
    for (id, label) in &ports {
        let name = crate::midi::stable_name(id, label);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let present = ports.iter().any(|(id, label)| crate::midi::stable_name(id, label) == name);
            let mappings = config.controllers.get(&name).map_or(0, HashMap::len);
            DeviceRow { name, present, mappings }
        })
        .collect()
}

/// Pads that still need learning — set by Settings and picked up by the
/// app's main loop ([`run_pending_learns`]), which owns the terminal the
/// learn screen needs.
#[derive(Debug, Clone, PartialEq)]
pub struct LearnRequest {
    pub name: String,
    pub port_id: String,
    /// `Some(pad)` to learn just that one pad (configure, `Enter` — see
    /// [`learn_one_pad`]); `None` for the whole 16-pad walkthrough a
    /// never-learned device gets.
    pub pad: Option<usize>,
}

/// The pad whose entry in `pad_notes` is `note`, if any.
pub fn note_to_pad(note: u8, pad_notes: &[u8; PADS]) -> Option<usize> {
    // 0 is the "unlearned" sentinel (see `PadConfig::set_pad_notes_for`),
    // never a real mapping — otherwise a device with only some pads
    // learned would fire its first unlearned one on note 0.
    if note == 0 {
        return None;
    }
    pad_notes.iter().position(|&n| n == note)
}

/// Opens `port_id`, calling `handler(pad, true)` on Note On (velocity > 0)
/// and `handler(pad, false)` on Note Off (or Note On with velocity 0, the
/// running-status convention). `None` if the port no longer exists or
/// can't be opened; dropping the returned connection closes it.
pub fn connect(port_id: &str, handler: PadHandler, pad_notes: [u8; PADS]) -> Option<midir::MidiInputConnection<()>> {
    let mut input = midir::MidiInput::new("odj").ok()?;
    input.ignore(midir::Ignore::ActiveSense | midir::Ignore::Time | midir::Ignore::Sysex);
    let port = input.find_port_by_id(port_id)?;
    input
        .connect(
            &port,
            "odj-pads-in",
            move |_stamp, message, &mut ()| {
                let &[status, note, velocity, ..] = message else { return };
                let Some(pad) = note_to_pad(note, &pad_notes) else { return };
                match status & 0xF0 {
                    0x90 if velocity > 0 => handler(pad, true),
                    0x80 | 0x90 => handler(pad, false),
                    _ => {}
                }
            },
            (),
        )
        .ok()
}

/// Opens `port_id` purely to listen: every Note On (velocity > 0) is sent
/// down the returned channel, for learning. `None` if the port can't be
/// opened; the connection closes when the returned one is dropped.
fn listen_notes(port_id: &str) -> Option<(midir::MidiInputConnection<()>, mpsc::Receiver<u8>)> {
    let mut input = midir::MidiInput::new("odj").ok()?;
    input.ignore(midir::Ignore::ActiveSense | midir::Ignore::Time | midir::Ignore::Sysex);
    let port = input.find_port_by_id(port_id)?;
    let (tx, rx) = mpsc::channel();
    input
        .connect(
            &port,
            "odj-pads-learn",
            move |_stamp, message, &mut ()| {
                let &[status, note, velocity, ..] = message else { return };
                if status & 0xF0 == 0x90 && velocity > 0 {
                    let _ = tx.send(note);
                }
            },
            (),
        )
        .ok()
        .map(|conn| (conn, rx))
}

/// A pad can send more than one Note On per press (velocity-sensitive
/// bounce); after capturing one, learning waits this long on the same
/// connection so the rest land there rather than on the next pad's.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Walks the user through pressing each of the 16 pads in order, recording
/// whichever note each press sends — so a newly-picked device works
/// regardless of how it numbers its pads. `Esc` stops early rather than
/// requiring all 16: whatever's already been pressed is kept (unpressed
/// pads stay at the `0` "unlearned" sentinel), returning `Ok(None)` only
/// if nothing was learned yet either. `Ctrl+C` always discards the lot
/// instead, same as `Ok(None)` for a port that can't be opened.
pub fn learn_pad_notes(terminal: &mut DefaultTerminal, port_id: &str) -> Result<Option<[u8; PADS]>> {
    let Some((_conn, rx)) = listen_notes(port_id) else { return Ok(None) };
    let mut notes = [0u8; PADS];
    let mut pad = 0;
    while pad < PADS {
        terminal.draw(|f| draw_learn_screen(f, pad, false))?;
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            if key.code == KeyCode::Esc {
                return Ok((pad > 0).then_some(notes));
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(None);
            }
        }
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(note) => {
                notes[pad] = note;
                pad += 1;
                std::thread::sleep(DEBOUNCE);
                while rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
    Ok(Some(notes))
}

/// Learns just `pad`'s note: waits for one Note On from `port_id` and
/// returns it — Settings' per-pad configure, as opposed to
/// [`learn_pad_notes`]' whole walkthrough. `Ok(None)` if `Esc`/`Ctrl+C`
/// cancels, or the port can't be opened.
pub fn learn_one_pad(terminal: &mut DefaultTerminal, port_id: &str, pad: usize) -> Result<Option<u8>> {
    let Some((_conn, rx)) = listen_notes(port_id) else { return Ok(None) };
    loop {
        terminal.draw(|f| draw_learn_screen(f, pad, true))?;
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && (key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            return Ok(None);
        }
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(note) => {
                std::thread::sleep(DEBOUNCE);
                return Ok(Some(note));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

/// Runs every learn [`PadMidi`] has queued, one after another without
/// returning to the app's own screen in between — learning one pad can
/// queue the next (see [`PadMidi::finish_pad_learn`]), which must follow
/// straight on rather than flashing the app's normal screen for a frame.
/// Call from the main loop, which owns the terminal.
pub fn run_pending_learns(terminal: &mut DefaultTerminal, midi: &mut PadMidi) -> Result<()> {
    while let Some(req) = midi.take_learn() {
        if let Some(pad) = req.pad {
            let note = learn_one_pad(terminal, &req.port_id, pad)?;
            midi.finish_pad_learn(req, pad, note);
        } else {
            let notes = learn_pad_notes(terminal, &req.port_id)?;
            midi.finish_learn(req, notes);
        }
    }
    Ok(())
}

/// Each digit as 5 rows of a 3-column dot pattern — a plain bitmap font,
/// doubled to 6 columns per digit in [`big_number`] so it reads as "huge"
/// rather than just chunky in a terminal, where cells are taller than wide.
const DIGIT_FONT: [[&str; 5]; 10] = [
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

/// `n` rendered as 5 lines of ASCII-art digits, side by side.
fn big_number(n: usize) -> [String; 5] {
    let digits: Vec<usize> = n.to_string().chars().map(|c| c.to_digit(10).unwrap() as usize).collect();
    std::array::from_fn(|row| {
        digits
            .iter()
            .map(|&d| DIGIT_FONT[d][row].chars().map(|c| if c == '#' { "██" } else { "  " }).collect::<String>())
            .collect::<Vec<_>>()
            .join("  ")
    })
}

/// `single`: learning just this one pad ([`learn_one_pad`]) rather than
/// walking through all of them — no "of 16" progress, and nothing
/// already learned to "finish here" with.
fn draw_learn_screen(f: &mut Frame, pad: usize, single: bool) {
    let area = centered(f.area(), 60, 60);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Configure pads ")
        .title_bottom(if pad > 0 && !single { " Esc finish here  Ctrl+C discard all " } else { " Esc/Ctrl+C cancel " })
        .border_style(Style::new().fg(AMBER));
    let mut text = big_number(pad + 1).join("\n");
    if single {
        text.push_str(&format!("\n\npress pad {}", pad + 1));
    } else {
        text.push_str(&format!("\n\npress pad {} of {}", pad + 1, PADS));
    }
    let paragraph = Paragraph::new(text).alignment(Alignment::Center).style(Style::new().fg(AMBER)).block(block);
    f.render_widget(paragraph, area);
}

/// One app's pad controller: its config, the live connection, and the
/// Settings MIDI tab's own state — the device list's selection and, while
/// drilled into a device's pads (`C`), which device and pad.
pub struct PadMidi {
    pub config: PadConfig,
    conn: Option<midir::MidiInputConnection<()>>,
    /// The device in use right now, by stable name.
    pub device: Option<String>,
    learn: Option<LearnRequest>,
    pub selected: usize,
    /// `Some(device name)` while drilled into that device's per-pad
    /// mappings, with [`Self::pad_selected`] the pad under the cursor.
    pub pad_config: Option<String>,
    pub pad_selected: usize,
    status: Option<String>,
    handler: PadHandler,
    save: SaveFn,
}

impl PadMidi {
    pub fn new(config: PadConfig, handler: PadHandler, save: SaveFn) -> Self {
        Self {
            config,
            conn: None,
            device: None,
            learn: None,
            selected: 0,
            pad_config: None,
            pad_selected: 0,
            status: None,
            handler,
            save,
        }
    }

    /// Reconnects the remembered device, no picker: `config.last` by
    /// stable name if it's plugged in and has pads learned. With
    /// `or_only_known`, failing that, the one device with learned pads
    /// that's currently present (only when exactly one is) — fine for an
    /// app with one instance, but not for one with several running at
    /// once, which would all grab the same device.
    pub fn restore(&mut self, or_only_known: bool) {
        if let Some(name) = self.config.last.clone()
            && let Some(port_id) = crate::midi::port_id_for(&name)
            && let Some(notes) = self.config.pad_notes_for(&name)
            && let Some(conn) = connect(&port_id, self.handler.clone(), notes)
        {
            self.conn = Some(conn);
            self.device = Some(name);
            return;
        }
        if !or_only_known {
            return;
        }
        let known_present: Vec<(String, String, [u8; PADS])> = crate::midi::list_ports()
            .into_iter()
            .filter_map(|(id, label)| {
                let name = crate::midi::stable_name(&id, &label);
                let notes = self.config.pad_notes_for(&name)?;
                Some((id, name, notes))
            })
            .collect();
        let [(id, name, notes)] = known_present.as_slice() else { return };
        if let Some(conn) = connect(id, self.handler.clone(), *notes) {
            self.conn = Some(conn);
            self.device = Some(name.clone());
            self.config.last = Some(name.clone());
            let _ = (self.save)(&self.config);
        }
    }

    pub fn rows(&self) -> Vec<DeviceRow> {
        device_rows(&self.config)
    }

    /// A status message from the last call, for the app to show.
    pub fn take_status(&mut self) -> Option<String> {
        self.status.take()
    }

    /// Pads waiting to be learned — see [`run_pending_learns`].
    pub fn take_learn(&mut self) -> Option<LearnRequest> {
        self.learn.take()
    }

    /// Applies a completed 16-pad walkthrough: remembers the notes (or,
    /// if the user cancelled before learning any, leaves everything as it
    /// was) and connects the device.
    pub fn finish_learn(&mut self, req: LearnRequest, notes: Option<[u8; PADS]>) {
        if let Some(notes) = notes {
            self.config.set_pad_notes_for(req.name.clone(), notes);
            self.connect_device(&req.port_id, &req.name, notes);
        }
    }

    /// Applies one pad learned from configure. `None` (cancelled) changes
    /// nothing. On success, if the very next pad isn't learned yet
    /// either, moves the cursor there and queues learning it too — so
    /// filling in a run of unmapped pads is just pressing them in turn,
    /// `Esc` to stop.
    pub fn finish_pad_learn(&mut self, req: LearnRequest, pad: usize, note: Option<u8>) {
        let Some(note) = note else { return };
        self.set_pad_note(&req.name, pad, Some(note));
        let next = pad + 1;
        if next < PADS && self.pad_notes_of(&req.name)[next].is_none() {
            self.pad_selected = next;
            self.learn = Some(LearnRequest { pad: Some(next), ..req });
        }
    }

    /// `name`'s learned note per pad, `None` for an unlearned one.
    pub fn pad_notes_of(&self, name: &str) -> [Option<u8>; PADS] {
        let map = self.config.controllers.get(name);
        std::array::from_fn(|pad| map.and_then(|m| m.get(&pad).copied()))
    }

    /// Handles a key on the Settings MIDI tab. `false` means it wasn't
    /// for this tab — `Esc`/`` ` `` on the device list, which the app
    /// treats as closing Settings — and everything else is consumed.
    pub fn key(&mut self, code: KeyCode) -> bool {
        if let Some(name) = self.pad_config.clone() {
            self.pad_config_key(name, code);
            return true;
        }
        let rows = self.rows();
        match code {
            KeyCode::Esc | KeyCode::Char('`') => return false,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Enter if rows.get(self.selected).is_some_and(|r| r.present) => {
                let name = rows[self.selected].name.clone();
                self.use_device(&name);
            }
            KeyCode::Char('x' | 'X') | KeyCode::Delete | KeyCode::Backspace
                if rows.get(self.selected).is_some_and(|r| r.mappings > 0) =>
            {
                let name = rows[self.selected].name.clone();
                self.forget_device(&name);
            }
            KeyCode::Char('c' | 'C') if !rows.is_empty() => {
                self.pad_config = Some(rows[self.selected.min(rows.len() - 1)].name.clone());
                self.pad_selected = 0;
            }
            _ => {}
        }
        self.clamp_selection();
        true
    }

    /// Keeps the device list's selection on a row that exists — call on
    /// opening Settings, since devices come and go.
    pub fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
    }

    fn pad_config_key(&mut self, name: String, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('`') => self.pad_config = None,
            KeyCode::Up => self.pad_selected = self.pad_selected.saturating_sub(1),
            KeyCode::Down => self.pad_selected = (self.pad_selected + 1).min(PADS - 1),
            KeyCode::Enter => match crate::midi::port_id_for(&name) {
                Some(port_id) => self.learn = Some(LearnRequest { name, port_id, pad: Some(self.pad_selected) }),
                None => self.status = Some(format!("MIDI device {name} is not connected")),
            },
            KeyCode::Char('x' | 'X') | KeyCode::Delete | KeyCode::Backspace => self.set_pad_note(&name, self.pad_selected, None),
            _ => {}
        }
    }

    /// Switches to the present device `name`: connects it straight away
    /// if any of its pads were learned before, otherwise queues the
    /// 16-pad walkthrough first.
    fn use_device(&mut self, name: &str) {
        let Some(port_id) = crate::midi::port_id_for(name) else {
            self.status = Some(format!("MIDI device {name} is not connected"));
            return;
        };
        match self.config.pad_notes_for(name) {
            Some(notes) if self.device.as_deref() != Some(name) => self.connect_device(&port_id, name, notes),
            Some(_) => {}
            None => self.learn = Some(LearnRequest { name: name.to_string(), port_id, pad: None }),
        }
    }

    /// (Re)opens `port_id` with `notes` and remembers it as the device to
    /// restore next time. Drops the previous connection first — a
    /// connection is tied to one specific port and can't be redirected.
    fn connect_device(&mut self, port_id: &str, name: &str, notes: [u8; PADS]) {
        self.conn = None;
        self.device = None;
        match connect(port_id, self.handler.clone(), notes) {
            Some(conn) => {
                self.conn = Some(conn);
                self.device = Some(name.to_string());
                self.config.last = Some(name.to_string());
                let _ = (self.save)(&self.config);
                self.status = Some(format!("MIDI: {name}"));
            }
            None => self.status = Some(format!("Could not connect MIDI device {name}")),
        }
    }

    /// Forgets `name`'s learned pad notes and, if it's the one in use,
    /// stops listening to it too — the learned notes are the whole
    /// mapping, so a "forgotten" device still triggering pads would be
    /// misleading.
    fn forget_device(&mut self, name: &str) {
        if self.config.controllers.remove(name).is_none() {
            return;
        }
        if self.device.as_deref() == Some(name) {
            self.conn = None;
            self.device = None;
        }
        if self.config.last.as_deref() == Some(name) {
            self.config.last = None;
        }
        let _ = (self.save)(&self.config);
        self.status = Some(format!("Forgot MIDI mappings for {name}"));
    }

    /// Sets (or, with `None`, clears) one pad's note on `name`, saving it
    /// and, if `name` is the device in use, reconnecting it so the change
    /// applies straight away. A note moved onto `pad` is taken off any
    /// other pad that had it — one note only ever triggers one pad. A
    /// device left with nothing learned is dropped entirely, and stops
    /// being used.
    fn set_pad_note(&mut self, name: &str, pad: usize, note: Option<u8>) {
        let map = self.config.controllers.entry(name.to_string()).or_default();
        match note {
            Some(note) => {
                map.retain(|_, n| *n != note);
                map.insert(pad, note);
            }
            None => {
                map.remove(&pad);
            }
        }
        if map.is_empty() {
            self.config.controllers.remove(name);
        }
        let _ = (self.save)(&self.config);
        if self.device.as_deref() != Some(name) {
            return;
        }
        match (crate::midi::port_id_for(name), self.config.pad_notes_for(name)) {
            (Some(port_id), Some(notes)) => self.connect_device(&port_id, name, notes),
            _ => {
                self.conn = None;
                self.device = None;
            }
        }
    }

    /// The Settings MIDI tab's own key hints, for the app's border.
    pub fn hints(&self) -> &'static [(&'static str, &'static str)] {
        if self.pad_config.is_some() {
            &[("Enter", "learn"), ("X", "clear")]
        } else {
            &[("Enter", "use"), ("C", "configure"), ("X", "forget mappings")]
        }
    }

    /// Draws the Settings MIDI tab's body into `area`: the device list,
    /// or, drilled in, the per-pad list. `pad_label` names a pad's
    /// purpose next to its number (e.g. odj-player's "→ memory 3"), or
    /// returns nothing.
    pub fn draw(&self, f: &mut Frame, area: Rect, pad_label: impl Fn(usize) -> Option<String>) {
        let (items, selected): (Vec<ListItem>, usize) = match &self.pad_config {
            Some(name) => {
                let mut items = vec![ListItem::new(Line::styled(name.clone(), Style::new().fg(AMBER).bold()))];
                items.extend(self.pad_notes_of(name).iter().enumerate().map(|(pad, note)| {
                    let note = note.map_or(Span::styled("—", Style::new().fg(DIM)), |n| Span::raw(format!("note {n}")));
                    let mut spans = vec![Span::raw(format!("Pad {:<4}", pad + 1)), note];
                    if let Some(label) = pad_label(pad) {
                        spans.push(Span::styled(format!("   {label}"), Style::new().fg(DIM)));
                    }
                    ListItem::new(Line::from(spans))
                }));
                (items, self.pad_selected + 1)
            }
            None => {
                let rows = self.rows();
                let items = if rows.is_empty() {
                    vec![ListItem::new("(no MIDI device used or connected yet)").fg(DIM)]
                } else {
                    let width = area.width.saturating_sub(2) as usize;
                    rows.iter()
                        .map(|r| {
                            // Same 3-icon convention as odj-mixer's MIDI
                            // tab: a distinct shape for a device known only
                            // from its saved mappings, separate from
                            // "present but not the one in use".
                            let is_current = self.device.as_deref() == Some(r.name.as_str());
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
                (items, self.selected.min(rows.len().saturating_sub(1)))
            }
        };
        let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
        let mut state = ListState::default().with_selected(Some(selected));
        f.render_stateful_widget(list, area, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn default_pad_notes() -> [u8; PADS] {
        std::array::from_fn(|i| 36 + i as u8)
    }

    /// A `PadMidi` whose pad presses and saves are just recorded.
    fn midi() -> (PadMidi, Arc<Mutex<Vec<PadConfig>>>) {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let s = saved.clone();
        let midi = PadMidi::new(
            PadConfig::default(),
            Arc::new(|_, _| {}),
            Box::new(move |c| {
                s.lock().unwrap().push(c.clone());
                Ok(())
            }),
        );
        (midi, saved)
    }

    #[test]
    fn big_number_renders_each_digit_five_rows_tall() {
        let one = big_number(1);
        assert_eq!(one.len(), 5);
        assert!(one.iter().all(|row| row.ends_with("██")), "digit 1 is a single right-hand stroke on every row");
        let six = big_number(6);
        let sixteen = big_number(16);
        for row in 0..5 {
            assert_eq!(sixteen[row], format!("{}  {}", one[row], six[row]), "multi-digit numbers are just digits side by side");
        }
    }

    #[test]
    fn note_to_pad_looks_up_the_table() {
        let notes = default_pad_notes(); // 36..=51
        assert_eq!(note_to_pad(35, &notes), None, "below the table");
        assert_eq!(note_to_pad(36, &notes), Some(0));
        assert_eq!(note_to_pad(51, &notes), Some(15), "the last pad");
        assert_eq!(note_to_pad(52, &notes), None, "above the table");
        let mut partial = [0u8; PADS];
        partial[3] = 40;
        assert_eq!(note_to_pad(0, &partial), None, "0 marks an unlearned pad, never a mapping");
        assert_eq!(note_to_pad(40, &partial), Some(3));

        // Real hardware (an M-VAVE SMC-PAD) sends pads in groups of 4,
        // descending by 4 each group — not a straight ascending range.
        let quirky = [96, 97, 98, 99, 92, 93, 94, 95, 88, 89, 90, 91, 84, 85, 86, 87];
        assert_eq!(note_to_pad(88, &quirky), Some(8), "pad 9 sends note 88, not base + 8");
        assert_eq!(note_to_pad(85, &quirky), Some(13));
        assert_eq!(note_to_pad(100, &quirky), None);
    }

    #[test]
    fn device_rows_lists_a_known_device_with_its_learned_pad_count() {
        let mut config = PadConfig::default();
        let mut notes = default_pad_notes();
        notes[7] = 0; // left unlearned after an early Esc
        config.set_pad_notes_for("zzz-not-a-real-pad".into(), notes);
        let row = device_rows(&config).into_iter().find(|r| r.name == "zzz-not-a-real-pad").expect("known row listed");
        assert_eq!(row.mappings, PADS - 1, "the 0 sentinel isn't a learned pad");
        assert!(!row.present, "nothing real is named this");
    }

    #[test]
    fn pad_notes_for_is_per_device() {
        let mut config = PadConfig::default();
        assert_eq!(config.pad_notes_for("Pad"), None);
        config.set_pad_notes_for("Pad".into(), default_pad_notes());
        assert_eq!(config.pad_notes_for("Pad"), Some(default_pad_notes()));
        assert_eq!(config.pad_notes_for("Other Pad"), None, "a different name is a different device");
    }

    #[test]
    fn configure_lists_and_clears_single_pads() {
        let (mut m, saved) = midi();
        let mut notes = [0u8; PADS];
        notes[0] = 36;
        notes[1] = 37;
        m.config.set_pad_notes_for("zzz-not-a-real-pad".into(), notes);
        m.selected = m.rows().iter().position(|r| r.name == "zzz-not-a-real-pad").expect("known row listed");

        assert!(m.key(KeyCode::Char('c')));
        assert_eq!(m.pad_config.as_deref(), Some("zzz-not-a-real-pad"));
        assert_eq!(m.pad_notes_of("zzz-not-a-real-pad")[..3], [Some(36), Some(37), None]);

        m.key(KeyCode::Char('x')); // clears pad 1 only
        assert_eq!(m.pad_notes_of("zzz-not-a-real-pad")[..2], [None, Some(37)]);
        assert!(!saved.lock().unwrap().is_empty(), "saved");

        // Enter on a device that isn't plugged in can't learn anything.
        m.key(KeyCode::Enter);
        assert!(m.take_learn().is_none());
        assert!(m.take_status().is_some_and(|s| s.contains("not connected")));

        m.pad_selected = 1;
        m.key(KeyCode::Char('x')); // last one gone: device forgotten
        assert!(m.config.controllers.is_empty());

        assert!(m.key(KeyCode::Esc), "Esc backs out of configure");
        assert!(m.pad_config.is_none());
        assert!(!m.key(KeyCode::Esc), "Esc on the device list is the app's to close Settings");
    }

    #[test]
    fn learning_a_pad_chains_onto_the_next_one_only_while_its_unmapped() {
        let (mut m, _) = midi();
        let mut notes = [0u8; PADS];
        notes[2] = 50; // pad 3 already learned
        m.config.set_pad_notes_for("zzz-not-a-real-pad".into(), notes);
        m.pad_config = Some("zzz-not-a-real-pad".into());
        let req = |pad| LearnRequest { name: "zzz-not-a-real-pad".into(), port_id: "p".into(), pad: Some(pad) };

        m.finish_pad_learn(req(0), 0, Some(36));
        assert_eq!(m.pad_selected, 1);
        assert_eq!(m.take_learn().and_then(|r| r.pad), Some(1), "pad 2 is unmapped: keep going");

        m.finish_pad_learn(req(1), 1, Some(37));
        assert!(m.take_learn().is_none(), "pad 3 is already mapped: stop");

        m.finish_pad_learn(req(5), 5, None);
        assert!(m.take_learn().is_none(), "cancelled (Esc): stop");
        assert_eq!(m.pad_notes_of("zzz-not-a-real-pad")[..3], [Some(36), Some(37), Some(50)]);
    }

    #[test]
    fn a_note_moved_onto_a_pad_leaves_the_pad_that_had_it() {
        let (mut m, _) = midi();
        m.config.set_pad_notes_for("zzz-not-a-real-pad".into(), default_pad_notes());
        let req = LearnRequest { name: "zzz-not-a-real-pad".into(), port_id: "p".into(), pad: Some(0) };
        m.finish_pad_learn(req, 0, Some(40)); // pad 5's note
        let notes = m.pad_notes_of("zzz-not-a-real-pad");
        assert_eq!(notes[0], Some(40));
        assert_eq!(notes[4], None);
    }

    #[test]
    fn forget_drops_a_devices_learned_pads() {
        let (mut m, _) = midi();
        m.config.set_pad_notes_for("zzz-not-a-real-pad".into(), [36; PADS]);
        m.selected = m.rows().iter().position(|r| r.name == "zzz-not-a-real-pad").expect("known row listed");
        m.key(KeyCode::Char('x'));
        assert!(m.config.controllers.is_empty(), "the learned pad notes are gone");
        assert!(m.take_status().is_some_and(|s| s.contains("Forgot MIDI mappings for zzz-not-a-real-pad")));
    }
}
