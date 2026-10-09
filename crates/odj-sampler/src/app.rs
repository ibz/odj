//! Keyboard handling and app state around the pads: the cue picker, the sample
//! editor, the kit and the memory figures.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use odj_core::grid::{BAR, BeatGrid};
use odj_core::library::fuzzy_match;
use odj_core::memory::{Cue, Memory, user_grid};
use odj_core::track::Track;

use crate::engine::{MAX_GAIN_DB, MIN_GAIN_DB, Mode, PADS, Props, Sample, Sampler, cut};
use crate::kit::{Kit, KitPad, PadSource};
use crate::loader::{Done, Failure, Job, Loader};

/// The pad keys, row by row: a 4×4 block on the left of the keyboard.
pub const PAD_KEYS: [char; PADS] = ['1', '2', '3', '4', 'q', 'w', 'e', 'r', 'a', 's', 'd', 'f', 'z', 'x', 'c', 'v'];
/// Without key-release events a gate pad plays this long after the last key event.
const HOLD_FALLBACK: Duration = Duration::from_millis(600);
const STATUS_TIME: Duration = Duration::from_secs(4);
const MEMORY_REFRESH: Duration = Duration::from_millis(500);
/// Length of a new sample from a cue without an end.
const DEFAULT_BARS: f64 = 4.0;
/// Length of a new sample when the track has no BPM, in seconds.
const DEFAULT_SECONDS: f64 = 2.0;
const MIN_SECONDS: f64 = 0.01;

#[derive(Clone, Debug, PartialEq)]
pub enum PadStatus {
    Empty,
    Loading,
    Ready,
    Missing,
    Changed,
    Failed(String),
}

pub struct Slot {
    pub kit: KitPad,
    pub status: PadStatus,
    /// Memory held by the pad's sample.
    pub bytes: usize,
    generation: u64,
}

/// A stored cue as the picker lists it.
pub struct CueEntry {
    pub track_id: String,
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub sample_rate: u32,
    /// The grid the user set in the player, if any.
    pub grid: Option<BeatGrid>,
    /// BPM tapped by an older player.
    pub tapped_bpm: Option<f64>,
    pub kind: String,
    pub cue: Cue,
    /// What the filter matches: "artist – title".
    pub name: String,
}

pub struct Picker {
    pub pad: usize,
    pub entries: Vec<CueEntry>,
    pub query: String,
    /// Indices into `entries`, best match first, with the matched character positions.
    pub visible: Vec<(usize, Vec<usize>)>,
    pub selected: usize,
}

impl Picker {
    fn new(pad: usize, entries: Vec<CueEntry>) -> Self {
        let mut p = Self { pad, entries, query: String::new(), visible: Vec::new(), selected: 0 };
        p.refilter();
        p
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize, Vec<usize>)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| fuzzy_match(&self.query, &e.name).map(|(s, pos)| (s, i, pos)))
            .collect();
        if !self.query.is_empty() {
            scored.sort_by_key(|(s, i, _)| (std::cmp::Reverse(*s), *i));
        }
        self.visible = scored.into_iter().map(|(_, i, pos)| (i, pos)).collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.visible.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }
}

/// Trimming a region of a track before it goes on a pad. It starts at the cue;
/// both ends move, one at a time.
pub struct Editor {
    pub pad: usize,
    pub source: PadSource,
    /// The user's grid from the player, which wins over the detected one.
    pub user_grid: Option<BeatGrid>,
    /// BPM tapped by an older player, on the detected phase.
    pub tapped_bpm: Option<f64>,
    pub track: Option<Arc<Track>>,
    loading: Option<mpsc::Receiver<anyhow::Result<Track>>>,
    /// The end isn't known until the track is in, unless it was set before.
    end_known: bool,
    /// The arrow and fine keys move the start rather than the end.
    pub editing_start: bool,
    pub previewing: bool,
    /// Memory held by the audition.
    pub preview_bytes: usize,
}

impl Editor {
    pub fn grid(&self) -> Option<BeatGrid> {
        let track = self.track.as_ref()?;
        user_grid(self.user_grid, self.tapped_bpm, track).or(track.grid)
    }

    /// The grid is the user's rather than detected.
    pub fn grid_edited(&self) -> bool {
        self.user_grid.is_some() || self.tapped_bpm.is_some()
    }

    /// Frames per beat at the track's rate.
    pub fn beat(&self) -> Option<f64> {
        self.grid().map(|g| g.period(self.source.sample_rate as f64))
    }

    pub fn len(&self) -> f64 {
        self.source.end - self.source.start
    }

    /// Moves the end by `frames`, keeping the region inside the track.
    fn move_end(&mut self, frames: f64) {
        self.set_len(self.len() + frames);
    }

    /// Moves the start by `frames`, keeping the end where it is.
    fn move_start(&mut self, frames: f64) {
        let min = MIN_SECONDS * self.source.sample_rate as f64;
        self.source.start = (self.source.start + frames).min(self.source.end - min).max(0.0);
    }

    /// Moves whichever end is being edited.
    fn move_edge(&mut self, frames: f64) {
        if self.editing_start { self.move_start(frames) } else { self.move_end(frames) }
    }

    /// Moves whichever end is being edited `beats` along the grid, onto beats;
    /// by seconds (a tenth each) without one.
    fn step_edge(&mut self, beats: i64) {
        let sr = self.source.sample_rate as f64;
        let Some(g) = self.grid() else { return self.move_edge(beats as f64 * 0.1 * sr) };
        let from = if self.editing_start { self.source.start } else { self.source.end };
        self.move_edge(g.step(from, beats, sr) - from);
    }

    fn set_len(&mut self, len: f64) {
        let Some(track) = &self.track else { return };
        let min = MIN_SECONDS * self.source.sample_rate as f64;
        let max = track.frames() as f64 - self.source.start;
        self.source.end = self.source.start + len.min(max).max(min);
    }

    fn cut(&self, out_rate: u32) -> Option<Sample> {
        let t = self.track.as_ref()?;
        Some(cut(&t.samples, t.sample_rate, self.source.start, self.source.end, out_rate))
    }
}

pub enum Screen {
    Pads,
    Picker(Picker),
    Editor(Box<Editor>),
}

/// Memory figures, refreshed every `MEMORY_REFRESH`.
#[derive(Clone, Copy, Default)]
pub struct MemoryUse {
    /// Resident memory of the whole process.
    pub process: Option<u64>,
    /// Physical memory of the machine.
    pub system: Option<u64>,
}

pub struct App {
    pub sampler: Arc<Mutex<Sampler>>,
    out_rate: u32,
    pub slots: [Slot; PADS],
    pub selected: usize,
    pub screen: Screen,
    pub show_help: bool,
    kit_file: PathBuf,
    loader: Loader,
    status: Option<(String, Instant)>,
    /// Whether the terminal reports key releases (kitty keyboard protocol).
    pub key_release: bool,
    pub output_name: String,
    /// Gate pads held without key-release events, and when they time out.
    holds: [Option<Instant>; PADS],
    pub memory: MemoryUse,
    memory_at: Option<Instant>,
    pub quit: bool,
}

impl App {
    pub fn new(
        sampler: Arc<Mutex<Sampler>>,
        out_rate: u32,
        kit_file: PathBuf,
        key_release: bool,
        output_name: String,
    ) -> Self {
        let kit = Kit::load(&kit_file);
        let mut app = Self {
            sampler,
            out_rate,
            slots: kit.pads.map(|kit| Slot { kit, status: PadStatus::Empty, bytes: 0, generation: 0 }),
            selected: 0,
            screen: Screen::Pads,
            show_help: false,
            kit_file,
            loader: Loader::new(out_rate),
            status: None,
            key_release,
            output_name,
            holds: [None; PADS],
            memory: MemoryUse { process: None, system: system_memory() },
            memory_at: None,
            quit: false,
        };
        for i in 0..PADS {
            let props = app.slots[i].kit.props;
            app.sampler().set_props(i, props);
            if app.slots[i].kit.source.is_some() {
                app.reload(i);
            }
        }
        app
    }

    fn sampler(&self) -> MutexGuard<'_, Sampler> {
        self.sampler.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn status(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STATUS_TIME)
            .map(|(msg, _)| msg.as_str())
    }

    /// Memory held by all pad samples.
    pub fn sample_bytes(&self) -> usize {
        self.slots.iter().map(|s| s.bytes).sum()
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    // --- Kit -------------------------------------------------------------------

    fn save_kit(&mut self) {
        let kit = Kit::new(std::array::from_fn(|i| self.slots[i].kit.clone()));
        if let Err(e) = kit.save(&self.kit_file) {
            self.set_status(format!("Could not save the kit: {e}"));
        }
    }

    /// Cuts the pad's sample from its file again, in the background.
    fn reload(&mut self, i: usize) {
        let slot = &mut self.slots[i];
        let Some(source) = slot.kit.source.clone() else { return };
        slot.generation += 1;
        slot.status = PadStatus::Loading;
        self.loader.load(Job { pad: i, generation: slot.generation, source });
    }

    /// Puts a sample on a pad (or empties it) and invalidates pending loads for it.
    fn set_sample(&mut self, i: usize, sample: Option<Sample>) {
        let slot = &mut self.slots[i];
        slot.bytes = sample.as_ref().map_or(0, Sample::bytes);
        slot.status = if sample.is_some() { PadStatus::Ready } else { PadStatus::Empty };
        slot.generation += 1;
        let old = self.sampler().set_sample(i, sample);
        // Free the old audio outside the audio lock.
        drop(old);
    }

    fn finish_loads(&mut self) {
        while let Some(Done { pad, generation, result }) = self.loader.poll() {
            if self.slots[pad].generation != generation {
                continue;
            }
            match result {
                Ok(sample) => {
                    self.set_sample(pad, Some(sample));
                }
                Err(failure) => {
                    self.set_sample(pad, None);
                    let name = self.slots[pad].kit.source.as_ref().map(|s| s.name()).unwrap_or_default();
                    let (status, msg) = match failure {
                        Failure::Missing => (PadStatus::Missing, format!("Pad {}: {name} is not where it was", pad + 1)),
                        Failure::Changed => {
                            (PadStatus::Changed, format!("Pad {}: the file of {name} holds different audio now", pad + 1))
                        }
                        Failure::Error(e) => (PadStatus::Failed(e.clone()), format!("Pad {}: can't load {name}: {e}", pad + 1)),
                    };
                    self.slots[pad].status = status;
                    self.set_status(msg);
                }
            }
        }
    }

    fn clear_pad(&mut self, i: usize) {
        if self.slots[i].kit.source.take().is_none() {
            return;
        }
        self.set_sample(i, None);
        self.save_kit();
        self.set_status(format!("Pad {} cleared", i + 1));
    }

    fn update_props(&mut self, f: impl FnOnce(&mut Props)) {
        let i = self.selected;
        f(&mut self.slots[i].kit.props);
        let props = self.slots[i].kit.props;
        self.sampler().set_props(i, props);
        self.save_kit();
    }

    // --- Picking and editing ---------------------------------------------------

    fn open_picker(&mut self) {
        let entries = cue_entries();
        if entries.is_empty() {
            self.set_status("No cues yet. Store some in odj-player first.");
            return;
        }
        let mut picker = Picker::new(self.selected, entries);
        if let Some(src) = &self.slots[self.selected].kit.source {
            let current = picker.visible.iter().position(|(i, _)| {
                let e = &picker.entries[*i];
                e.track_id == src.track_id && e.cue.pos == src.start
            });
            picker.selected = current.unwrap_or(0);
        }
        self.screen = Screen::Picker(picker);
    }

    /// Takes the chosen cue: a loop goes straight on the pad, a bare cue to the editor.
    fn choose(&mut self, pad: usize, e: &CueEntry) {
        let source = PadSource {
            track_id: e.track_id.clone(),
            path: e.path.clone(),
            title: e.title.clone(),
            artist: e.artist.clone(),
            sample_rate: e.sample_rate,
            cue: e.kind.clone(),
            start: e.cue.pos,
            end: e.cue.loop_out.unwrap_or(e.cue.pos),
        };
        match e.cue.loop_out {
            Some(_) => {
                self.slots[pad].kit.source = Some(source);
                self.slots[pad].kit.props.looped = true;
                let props = self.slots[pad].kit.props;
                self.sampler().set_props(pad, props);
                self.reload(pad);
                self.save_kit();
                self.screen = Screen::Pads;
            }
            None => self.open_editor(pad, source, e.grid, e.tapped_bpm, false),
        }
    }

    fn open_editor(
        &mut self,
        pad: usize,
        source: PadSource,
        user_grid: Option<BeatGrid>,
        tapped_bpm: Option<f64>,
        end_known: bool,
    ) {
        let (tx, rx) = mpsc::channel();
        let path = source.path.clone();
        thread::spawn(move || {
            let _ = tx.send(Track::load(&path));
        });
        self.screen = Screen::Editor(Box::new(Editor {
            pad,
            source,
            user_grid,
            tapped_bpm,
            track: None,
            loading: Some(rx),
            end_known,
            editing_start: false,
            previewing: false,
            preview_bytes: 0,
        }));
    }

    /// Trims the selected pad's sample: back into the editor with its region.
    fn trim(&mut self) {
        let i = self.selected;
        let Some(source) = self.slots[i].kit.source.clone() else {
            self.set_status(format!("Pad {} is empty, press Enter to assign a cue", i + 1));
            return;
        };
        let mem = Memory::load().tracks().into_iter().find(|t| t.id == source.track_id).map(|t| t.memory);
        let (grid, bpm) = mem.map_or((None, None), |m| (m.grid, m.bpm));
        self.open_editor(i, source, grid, bpm, true);
    }

    fn finish_editor_load(&mut self) {
        let Screen::Editor(ed) = &mut self.screen else { return };
        let Some(rx) = &ed.loading else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("loader crashed")),
        };
        ed.loading = None;
        let err = match result {
            Ok(track) if !ed.source.path.is_file() => Some(format!("{} is gone", track.path.display())),
            Ok(track) if track.id != ed.source.track_id || track.sample_rate != ed.source.sample_rate => {
                Some(format!("{} holds different audio than when the cue was stored", track.path.display()))
            }
            Ok(track) => {
                ed.track = Some(Arc::new(track));
                if !ed.end_known {
                    let len = match ed.beat() {
                        Some(beat) => DEFAULT_BARS * 4.0 * beat,
                        None => DEFAULT_SECONDS * ed.source.sample_rate as f64,
                    };
                    ed.set_len(len);
                    ed.end_known = true;
                }
                None
            }
            Err(e) => Some(format!("Can't load {}: {e}", ed.source.path.display())),
        };
        if let Some(msg) = err {
            self.screen = Screen::Pads;
            self.set_status(msg);
        }
    }

    fn close_editor(&mut self, keep: bool) {
        let Screen::Editor(ed) = std::mem::replace(&mut self.screen, Screen::Pads) else { return };
        let old = self.sampler().set_preview(None);
        drop(old);
        if !keep {
            return;
        }
        let Some(sample) = ed.cut(self.out_rate) else { return };
        let pad = ed.pad;
        let secs = ed.source.seconds();
        self.slots[pad].kit.source = Some(ed.source);
        self.set_sample(pad, Some(sample));
        self.save_kit();
        self.set_status(format!("Pad {} set, {secs:.2} s", pad + 1));
    }

    /// Re-cuts the audition after the region changed.
    fn refresh_preview(&mut self) {
        let Screen::Editor(ed) = &mut self.screen else { return };
        if !ed.previewing {
            return;
        }
        let sample = ed.cut(self.out_rate);
        ed.preview_bytes = sample.as_ref().map_or(0, Sample::bytes);
        let old = self.sampler.lock().unwrap_or_else(|e| e.into_inner()).set_preview(sample);
        drop(old);
    }

    // --- Ticking ---------------------------------------------------------------

    pub fn tick(&mut self) {
        self.finish_loads();
        self.finish_editor_load();
        let now = Instant::now();
        for i in 0..PADS {
            if self.holds[i].is_some_and(|t| now >= t) {
                self.holds[i] = None;
                self.sampler().release(i);
            }
        }
        if self.memory_at.is_none_or(|t| now - t >= MEMORY_REFRESH) {
            self.memory.process = process_memory();
            self.memory_at = Some(now);
        }
    }

    // --- Keys --------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match &self.screen {
            Screen::Picker(_) if key.kind != KeyEventKind::Release => self.picker_key(&key),
            Screen::Editor(_) if key.kind != KeyEventKind::Release => {
                let (code, shift) = normalize(&key);
                self.editor_key(code, shift);
            }
            Screen::Pads => {
                let (code, shift) = normalize(&key);
                match key.kind {
                    KeyEventKind::Release => self.release(code),
                    KeyEventKind::Repeat => self.repeat(code),
                    KeyEventKind::Press => self.pads_key(code, shift),
                }
            }
            _ => {}
        }
    }

    fn release(&mut self, code: KeyCode) {
        if let Some(i) = pad_of(code) {
            self.sampler().release(i);
        }
    }

    /// With key-release events, repeats only mean the key is still down.
    fn repeat(&mut self, code: KeyCode) {
        if !self.key_release {
            self.pads_key(code, false);
        }
    }

    fn press_pad(&mut self, i: usize) {
        self.selected = i;
        if !self.sampler().press(i) {
            let msg = match self.slots[i].status {
                PadStatus::Loading => format!("Pad {} is still loading", i + 1),
                PadStatus::Empty => format!("Pad {} is empty, press Enter to assign a cue", i + 1),
                _ => format!("Pad {} has no audio, press Enter to assign a cue", i + 1),
            };
            self.set_status(msg);
            return;
        }
        if !self.key_release && self.slots[i].kit.props.mode == Mode::Gate {
            self.holds[i] = Some(Instant::now() + HOLD_FALLBACK);
        }
    }

    fn pads_key(&mut self, code: KeyCode, shift: bool) {
        if self.show_help {
            if matches!(code, KeyCode::Esc | KeyCode::Char('/')) {
                self.show_help = false;
            }
            return;
        }
        if let Some(i) = pad_of(code) {
            if shift {
                self.sampler().stop(i);
            } else if self.holds[i].is_some() {
                // A key repeat on a held gate pad (no key-release events).
                self.holds[i] = Some(Instant::now() + HOLD_FALLBACK);
            } else {
                self.press_pad(i);
            }
            return;
        }
        let i = self.selected;
        match code {
            KeyCode::Char('/') => self.show_help = true,
            KeyCode::Left => self.selected = i - i % 4 + (i % 4 + 3) % 4,
            KeyCode::Right => self.selected = i - i % 4 + (i % 4 + 1) % 4,
            KeyCode::Up => self.selected = (i + PADS - 4) % PADS,
            KeyCode::Down => self.selected = (i + 4) % PADS,
            KeyCode::Enter => self.open_picker(),
            KeyCode::Char('t') => self.trim(),
            KeyCode::Char('m') => self.update_props(|p| p.mode = p.mode.next()),
            KeyCode::Char('l') => self.update_props(|p| p.looped = !p.looped),
            KeyCode::Char('g') => self.update_props(|p| p.retrigger = !p.retrigger),
            KeyCode::Char('-') => self.update_props(|p| p.gain_db = (p.gain_db - 1.0).max(MIN_GAIN_DB)),
            KeyCode::Char('=') => self.update_props(|p| p.gain_db = (p.gain_db + 1.0).min(MAX_GAIN_DB)),
            KeyCode::Char('0') => self.update_props(|p| p.gain_db = 0.0),
            KeyCode::Delete | KeyCode::Backspace => self.clear_pad(i),
            KeyCode::Char(' ') => self.sampler().stop_all(),
            _ => {}
        }
    }

    /// Takes the raw key: typed characters go into the filter as they are.
    fn picker_key(&mut self, key: &KeyEvent) {
        let Screen::Picker(p) = &mut self.screen else { return };
        let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if typing => {
                p.query.push(c);
                p.selected = 0;
                p.refilter();
            }
            KeyCode::Backspace => {
                p.query.pop();
                p.refilter();
            }
            KeyCode::Up => p.move_by(-1),
            KeyCode::Down => p.move_by(1),
            KeyCode::PageUp => p.move_by(-10),
            KeyCode::PageDown => p.move_by(10),
            KeyCode::Home => p.selected = 0,
            KeyCode::End => p.move_by(isize::MAX / 2),
            KeyCode::Esc if !p.query.is_empty() => {
                p.query.clear();
                p.refilter();
            }
            KeyCode::Esc => self.screen = Screen::Pads,
            KeyCode::Enter => {
                let Screen::Picker(p) = std::mem::replace(&mut self.screen, Screen::Pads) else { return };
                let Some(&(i, _)) = p.visible.get(p.selected) else {
                    self.screen = Screen::Picker(p);
                    return;
                };
                self.choose(p.pad, &p.entries[i]);
            }
            _ => {}
        }
    }

    fn editor_key(&mut self, code: KeyCode, shift: bool) {
        let Screen::Editor(ed) = &mut self.screen else { return };
        match code {
            KeyCode::Esc => return self.close_editor(false),
            KeyCode::Enter if ed.track.is_some() => return self.close_editor(true),
            _ => {}
        }
        if ed.track.is_none() {
            return;
        }
        let sr = ed.source.sample_rate as f64;
        // Beats and bars on the grid, tenths and whole seconds without.
        let beats = if shift { BAR } else { 1 };
        match code {
            KeyCode::Tab => {
                ed.editing_start = !ed.editing_start;
                return;
            }
            KeyCode::Left if shift && ed.grid().is_none() => ed.move_edge(-sr),
            KeyCode::Right if shift && ed.grid().is_none() => ed.move_edge(sr),
            KeyCode::Left => ed.step_edge(-beats),
            KeyCode::Right => ed.step_edge(beats),
            KeyCode::Char(',') => ed.move_edge(-if shift { 0.001 } else { 0.01 } * sr),
            KeyCode::Char('.') => ed.move_edge(if shift { 0.001 } else { 0.01 } * sr),
            KeyCode::Char('[') => ed.set_len(ed.len() / 2.0),
            KeyCode::Char(']') => ed.set_len(ed.len() * 2.0),
            KeyCode::Char(' ') => {
                ed.previewing = !ed.previewing;
                if !ed.previewing {
                    ed.preview_bytes = 0;
                    let old = self.sampler().set_preview(None);
                    drop(old);
                    return;
                }
            }
            _ => return,
        }
        self.refresh_preview();
    }
}

/// Every stored cue in the library, by artist and title, then position.
fn cue_entries() -> Vec<CueEntry> {
    let mut tracks = Memory::load().tracks();
    tracks.sort_by_cached_key(|t| (t.artist.clone().unwrap_or_default().to_lowercase(), t.title.to_lowercase()));
    let mut out = Vec::new();
    for t in tracks {
        let mem = &t.memory;
        let mut cues: Vec<(String, Cue)> = Vec::new();
        for (slot, hot) in mem.hot.iter().enumerate() {
            if let Some(cue) = hot {
                cues.push((format!("hot {}", (b'A' + slot as u8) as char), *cue));
            }
        }
        for (i, cue) in mem.memories.iter().enumerate() {
            cues.push((format!("memory {}", i + 1), *cue));
        }
        if let Some(pos) = mem.loop_in {
            cues.push(("last loop".into(), Cue { pos, loop_out: mem.loop_out }));
        }
        let name = match &t.artist {
            Some(a) => format!("{a} – {}", t.title),
            None => t.title.clone(),
        };
        for (kind, cue) in cues {
            out.push(CueEntry {
                track_id: t.id.clone(),
                path: t.path.clone(),
                title: t.title.clone(),
                artist: t.artist.clone(),
                sample_rate: t.sample_rate,
                grid: mem.grid,
                tapped_bpm: mem.bpm,
                kind,
                cue,
                name: name.clone(),
            });
        }
    }
    out
}

fn pad_of(code: KeyCode) -> Option<usize> {
    match code {
        KeyCode::Char(c) => PAD_KEYS.iter().position(|&k| k == c),
        _ => None,
    }
}

/// Maps shifted symbols back to their base key (US layout) so a key's
/// press and release match whether or not the terminal applied Shift.
fn normalize(key: &KeyEvent) -> (KeyCode, bool) {
    let mut shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let code = match key.code {
        KeyCode::Char(c) => {
            let base = match c {
                '!' => '1',
                '@' => '2',
                '#' => '3',
                '$' => '4',
                ')' => '0',
                '<' => ',',
                '>' => '.',
                '{' => '[',
                '}' => ']',
                '?' => '/',
                '_' => '-',
                '+' => '=',
                c => c.to_ascii_lowercase(),
            };
            shift |= base != c;
            KeyCode::Char(base)
        }
        other => other,
    };
    (code, shift)
}

/// Resident memory of this process, from /proc.
fn process_memory() -> Option<u64> {
    proc_kb("/proc/self/status", "VmRSS:")
}

fn system_memory() -> Option<u64> {
    proc_kb("/proc/meminfo", "MemTotal:")
}

fn proc_kb(file: &str, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(file).ok()?;
    let line = text.lines().find(|l| l.starts_with(field))?;
    let kb: u64 = line[field.len()..].trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;

    fn app(name: &str) -> App {
        let dir = std::env::temp_dir().join(format!("odj-sampler-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sampler = Arc::new(Mutex::new(Sampler::new()));
        App::new(sampler, 48_000, dir.join("kit.json"), true, String::new())
    }

    fn key(app: &mut App, c: char, kind: KeyEventKind) {
        let shift = c.is_ascii_uppercase() || "!@#$".contains(c);
        let mods = if shift { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
        app.handle_key(KeyEvent { code: KeyCode::Char(c), modifiers: mods, kind, state: KeyEventState::NONE });
    }

    fn source() -> PadSource {
        PadSource {
            track_id: "ab".into(),
            path: "/nonexistent.wav".into(),
            title: "T".into(),
            artist: None,
            sample_rate: 48_000,
            cue: "hot A".into(),
            start: 0.0,
            end: 4800.0,
        }
    }

    #[test]
    fn pad_keys_select_and_play() {
        let mut a = app("play");
        a.slots[6].kit.source = Some(source());
        a.set_sample(6, Some(Sample::new(vec![0.1; 9600], 4800)));
        key(&mut a, 'e', KeyEventKind::Press);
        assert_eq!(a.selected, 6);
        assert!(a.sampler().snapshot().pads[6].playing);
        key(&mut a, 'E', KeyEventKind::Press); // Shift: stop
        let mut out = vec![0.0; 2048];
        a.sampler().render(&mut out);
        assert!(!a.sampler().snapshot().pads[6].playing);
    }

    #[test]
    fn props_are_applied_and_saved() {
        let mut a = app("props");
        a.selected = 2;
        key(&mut a, 'm', KeyEventKind::Press);
        key(&mut a, 'l', KeyEventKind::Press);
        key(&mut a, '-', KeyEventKind::Press);
        assert_eq!(a.sampler().pad(2).props.mode, Mode::Gate);
        assert!(a.sampler().pad(2).props.looped);
        let kit = Kit::load(&a.kit_file);
        assert_eq!(kit.pads[2].props.gain_db, -1.0);
        assert_eq!(kit.pads[2].props.mode, Mode::Gate);
    }

    #[test]
    fn missing_files_are_reported_and_kept_in_the_kit() {
        let dir = std::env::temp_dir().join(format!("odj-sampler-test-{}-missing", std::process::id()));
        let file = dir.join("kit.json");
        let mut kit = Kit::default();
        kit.pads[0].source = Some(source());
        kit.save(&file).unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, file.clone(), true, String::new());
        assert_eq!(a.slots[0].status, PadStatus::Loading);
        let t = Instant::now();
        while a.slots[0].status == PadStatus::Loading && t.elapsed() < Duration::from_secs(5) {
            a.tick();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(a.slots[0].status, PadStatus::Missing);
        assert!(Kit::load(&file).pads[0].source.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn memory_counts_pad_samples() {
        let mut a = app("memory");
        a.set_sample(0, Some(Sample::new(vec![0.0; 2000], 1000)));
        a.set_sample(3, Some(Sample::new(vec![0.0; 500], 250)));
        assert_eq!(a.sample_bytes(), 2500 * 4);
        a.slots[3].kit.source = Some(source());
        a.clear_pad(3);
        assert_eq!(a.sample_bytes(), 2000 * 4);
    }

    #[test]
    fn editor_moves_the_end_by_beats_and_stays_in_the_track() {
        let track = Track::from_samples("t".into(), "t".into(), None, 48_000, vec![0.0; 48_000 * 20 * 2]);
        let mut ed = Editor {
            pad: 0,
            source: PadSource { start: 48_000.0, end: 48_000.0, ..source() },
            user_grid: Some(BeatGrid { bpm: 120.0, anchor: 0.0 }),
            tapped_bpm: None,
            track: Some(Arc::new(track)),
            loading: None,
            end_known: false,
            editing_start: false,
            previewing: false,
            preview_bytes: 0,
        };
        ed.set_len(DEFAULT_BARS * 4.0 * ed.beat().unwrap());
        assert_eq!(ed.len(), 8.0 * 48_000.0);
        ed.move_end(-ed.beat().unwrap());
        assert_eq!(ed.len(), 7.5 * 48_000.0);
        ed.set_len(1e12);
        assert_eq!(ed.source.end, 20.0 * 48_000.0);
        ed.set_len(0.0);
        assert_eq!(ed.len(), MIN_SECONDS * 48_000.0);
    }

    #[test]
    fn editor_moves_the_start_and_keeps_the_end() {
        let track = Track::from_samples("t".into(), "t".into(), None, 48_000, vec![0.0; 48_000 * 20 * 2]);
        let mut ed = Editor {
            pad: 0,
            source: PadSource { start: 48_000.0, end: 96_000.0, ..source() },
            user_grid: Some(BeatGrid { bpm: 120.0, anchor: 0.0 }),
            tapped_bpm: None,
            track: Some(Arc::new(track)),
            loading: None,
            end_known: true,
            editing_start: true,
            previewing: false,
            preview_bytes: 0,
        };
        ed.move_edge(-ed.beat().unwrap());
        assert_eq!((ed.source.start, ed.source.end), (24_000.0, 96_000.0));
        ed.move_edge(-1e9);
        assert_eq!(ed.source.start, 0.0);
        ed.move_edge(1e9);
        assert_eq!(ed.source.start, 96_000.0 - MIN_SECONDS * 48_000.0);
        // Halving and doubling keep the start.
        ed.editing_start = false;
        ed.source.start = 24_000.0;
        ed.set_len(ed.len() / 2.0);
        assert_eq!((ed.source.start, ed.source.end), (24_000.0, 60_000.0));
    }

    #[test]
    fn editor_steps_land_on_the_grid() {
        let track = Track::from_samples("t".into(), "t".into(), None, 48_000, vec![0.0; 48_000 * 20 * 2]);
        // 120 BPM, a beat every 24 000 frames, from frame 1 000.
        let mut ed = Editor {
            pad: 0,
            source: PadSource { start: 49_000.0, end: 60_000.0, ..source() },
            user_grid: Some(BeatGrid { bpm: 120.0, anchor: 1_000.0 }),
            tapped_bpm: None,
            track: Some(Arc::new(track)),
            loading: None,
            end_known: true,
            editing_start: false,
            previewing: false,
            preview_bytes: 0,
        };
        ed.step_edge(1);
        assert_eq!(ed.source.end, 73_000.0, "off the grid, the first step goes to the next beat");
        ed.step_edge(BAR);
        assert_eq!(ed.source.end, 169_000.0);
        ed.step_edge(-1);
        assert_eq!(ed.source.end, 145_000.0);
        ed.editing_start = true;
        ed.step_edge(-1);
        assert_eq!(ed.source.start, 25_000.0);
        ed.user_grid = None;
        ed.step_edge(1);
        assert_eq!(ed.source.start, 29_800.0, "a tenth of a second without a grid");
    }

    /// Draws every screen at a roomy and a cramped size; shown with --nocapture.
    #[test]
    fn draws_every_screen() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use odj_core::track::test_util::click_track;

        let mut a = app("draw");
        a.slots[0].kit.source = Some(source());
        a.set_sample(0, Some(Sample::new(vec![0.1; 96_000], 48_000)));
        a.slots[5].kit.source = Some(PadSource { title: "A rather long title for a pad".into(), ..source() });
        a.slots[5].kit.props.looped = true;
        a.slots[5].status = PadStatus::Missing;
        a.sampler().press(0);
        a.tick();
        let entry = |kind: &str, cue: Cue| CueEntry {
            track_id: "ab".into(),
            path: "/music/t.flac".into(),
            title: "Title".into(),
            artist: Some("Artist".into()),
            sample_rate: 48_000,
            grid: None,
            tapped_bpm: None,
            kind: kind.into(),
            cue,
            name: "Artist – Title".into(),
        };
        let picker = Picker::new(
            3,
            vec![entry("hot A", Cue { pos: 48_000.0, loop_out: None }), entry("last loop", Cue { pos: 96_000.0, loop_out: Some(192_000.0) })],
        );
        let track = Track::from_samples("t".into(), "t".into(), None, 48_000, click_track(120.0, 20.0, 48_000));
        let mut ed = Editor {
            pad: 3,
            source: PadSource { start: 48_000.0, end: 48_000.0, ..source() },
            user_grid: None,
            tapped_bpm: None,
            track: Some(Arc::new(track)),
            loading: None,
            end_known: false,
            editing_start: false,
            previewing: true,
            preview_bytes: 0,
        };
        ed.set_len(16.0 * ed.beat().unwrap());

        let screens = [Screen::Pads, Screen::Picker(picker), Screen::Editor(Box::new(ed))];
        for screen in screens {
            a.screen = screen;
            for (w, h) in [(140, 40), (50, 12)] {
                let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                let snapshot = a.sampler().snapshot();
                t.draw(|f| crate::ui::draw(f, &a, &snapshot)).unwrap();
                if w > 100 {
                    println!("{}", t.backend());
                }
            }
        }
        a.show_help = true;
        a.screen = Screen::Pads;
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let snapshot = a.sampler().snapshot();
        t.draw(|f| crate::ui::draw(f, &a, &snapshot)).unwrap();
    }
}
