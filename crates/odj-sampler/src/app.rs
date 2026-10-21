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
use odj_core::track::{Track, decode};
use odj_core::tui::Help;

use crate::engine::{MAX_GAIN_DB, MIN_GAIN_DB, Mode, PADS, Props, Sample, Sampler, cut};
use crate::kit::{Kit, KitPad, PadSource};
use crate::kit_browser::{self, KitRow};
use crate::kits;
use crate::loader::{Done, Failure, Job, Loader};
use crate::settings;
use crate::starter_kit;

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

/// A row in the sample browser (see [`SampleBrowser`]).
pub enum BrowserEntry {
    /// Back to the list of kits — not shown while already there.
    Up,
    /// A sibling kit's own samples, one level up — only shown there.
    Kit(PathBuf, String),
    /// A `.wav` already in whichever directory is open.
    Sample(PathBuf),
    /// Always listed last: the original fuzzy cue picker, unchanged.
    BrowsePlayerCues,
}

impl BrowserEntry {
    pub fn label(&self) -> String {
        match self {
            BrowserEntry::Up => ".. (back to kits)".to_string(),
            BrowserEntry::Kit(_, name) => name.clone(),
            BrowserEntry::Sample(path) => {
                path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
            }
            BrowserEntry::BrowsePlayerCues => "Browse odj-player's cues…".to_string(),
        }
    }
}

/// What `Enter` on a pad opens: samples already in a kit, with a shortcut
/// to browse odj-player's cues instead (the old, now-nested, fuzzy
/// picker). Starts in the active kit's own `samples/`; `Up` reaches every
/// other kit's, one level up, at `kits_dir()`.
pub struct SampleBrowser {
    pub pad: usize,
    pub dir: PathBuf,
    pub entries: Vec<BrowserEntry>,
    pub selected: usize,
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

/// What [`App::copy_into_kit`] needs: `samples` (interleaved stereo at
/// `rate`) and the `start..end` region of it to keep, plus the metadata a
/// fresh `PadSource` needs once that's written and decoded.
struct NewSample<'a> {
    samples: &'a [f32],
    rate: u32,
    start: f64,
    end: f64,
    title: String,
    artist: Option<String>,
    cue: String,
}

pub enum Screen {
    Pads,
    Picker(Picker),
    Editor(Box<Editor>),
    KitBrowser(KitBrowserState),
    SampleBrowser(SampleBrowser),
    /// Naming the kit a change just landed on, so it can actually be
    /// saved — see `App::ensure_kit_dir`/`App::save_kit`. The typed name.
    NamingKit(String),
}

/// Switching the active kit, or creating a new one — see `kit_browser.rs`
/// for the row list this picks from (no network here, unlike the
/// mandatory bootstrap picker in `main.rs`: existing kits, Embedded, and
/// "create new" only).
pub struct KitBrowserState {
    pub rows: Vec<KitRow>,
    pub selected: usize,
    pub mode: KitBrowserMode,
}

pub enum KitBrowserMode {
    List,
    /// Typing a name for a new kit.
    Naming(String),
    /// About to delete `rows[_]` — a row index, not acted on until a plain
    /// `Enter` confirms it; anything else backs out, since this deletes
    /// the kit's whole directory, every sample included, with no undo.
    ConfirmingDelete(usize),
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
    pub help: Help,
    /// This kit's own directory under `kits/` — `kit.json` plus the
    /// `samples/` library it owns (see `kits.rs`). `None` for a kit that
    /// hasn't been named yet — nothing is written to disk until it is
    /// (see [`Self::save_kit`], [`Self::ensure_kit_dir`]).
    kit_dir: Option<PathBuf>,
    kit_name: String,
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
        kit_dir: Option<PathBuf>,
        kit: Kit,
        key_release: bool,
        output_name: String,
    ) -> Self {
        let mut app = Self {
            sampler,
            out_rate,
            slots: std::array::from_fn(|_| Slot { kit: KitPad::default(), status: PadStatus::Empty, bytes: 0, generation: 0 }),
            selected: 0,
            screen: Screen::Pads,
            help: Help::default(),
            kit_dir: None,
            kit_name: String::new(),
            loader: Loader::new(out_rate),
            status: None,
            key_release,
            output_name,
            holds: [None; PADS],
            memory: MemoryUse { process: None, system: system_memory() },
            memory_at: None,
            quit: false,
        };
        app.load_kit(kit_dir, kit);
        app
    }

    /// Replaces every pad with `kit`'s — shared by the initial load above
    /// and [`Self::switch_kit`]/[`Self::switch_to_unsaved_kit`]'s later
    /// ones. `dir` is `None` for a kit with nothing on disk yet (see
    /// [`Self::ensure_kit_dir`]).
    fn load_kit(&mut self, dir: Option<PathBuf>, kit: Kit) {
        self.kit_dir = dir;
        self.kit_name = kit.name;
        self.slots = kit.pads.map(|kit| Slot { kit, status: PadStatus::Empty, bytes: 0, generation: 0 });
        for i in 0..PADS {
            let props = self.slots[i].kit.props;
            self.sampler().set_props(i, props);
            if self.slots[i].kit.source.is_some() {
                self.reload(i);
            } else {
                self.set_sample(i, None);
            }
        }
    }

    /// Switches to a different, already-saved kit: stops every pad, loads
    /// it, and remembers it as the active one for next time.
    fn switch_kit(&mut self, dir: PathBuf) {
        self.sampler().stop_all();
        let kit = Kit::load(&dir.join("kit.json"));
        self.load_kit(Some(dir), kit);
        let slug = self.kit_dir.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned());
        settings::set_active_kit(slug);
    }

    /// Switches to `kit`, but with no directory of its own yet — nothing
    /// is written to disk until the first real change asks for a name
    /// (see [`Self::save_kit`], [`Self::ensure_kit_dir`]) and creates one.
    /// Used both for a fresh, nameless kit (`Kit::default()`) and for the
    /// embedded kit (already named, its pads already pointing at the
    /// binary's bundled assets — see `kit_browser::MaterializedKit`).
    fn switch_to_unsaved_kit(&mut self, kit: Kit) {
        self.sampler().stop_all();
        self.load_kit(None, kit);
        settings::set_active_kit(None);
    }

    fn sampler(&self) -> MutexGuard<'_, Sampler> {
        self.sampler.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_status(&mut self, msg: impl Into<String>) {
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

    fn kit_file(&self) -> Option<PathBuf> {
        self.kit_dir.as_ref().map(|d| d.join("kit.json"))
    }

    /// Where this kit's own sample library lives — every `.wav` ever
    /// assigned from odj-player's cues, or copied in from another kit,
    /// stays here even after being unassigned from a pad.
    fn samples_dir(&self) -> Option<PathBuf> {
        self.kit_dir.as_ref().map(|d| d.join("samples"))
    }

    /// `true` once this kit has a real directory to write samples into.
    /// Otherwise (a freshly-switched-to kit that's never been named)
    /// starts the naming prompt and returns `false`, so the caller should
    /// stop rather than try to write anywhere — there's nowhere yet.
    fn ensure_kit_dir(&mut self) -> bool {
        if self.kit_dir.is_some() {
            return true;
        }
        self.screen = Screen::NamingKit(self.kit_name.clone());
        false
    }

    /// No-op — deferred, see [`Self::ensure_kit_dir`] — until the kit has
    /// a name: the mutation already happened in memory regardless, it
    /// just can't be written anywhere yet.
    fn save_kit(&mut self) {
        let Some(kit_file) = self.kit_file() else {
            self.screen = Screen::NamingKit(self.kit_name.clone());
            return;
        };
        let kit = Kit::new(self.kit_name.clone(), std::array::from_fn(|i| self.slots[i].kit.clone()));
        if let Err(e) = kit.save(&kit_file) {
            self.set_status(format!("Could not save the kit: {e}"));
        }
    }

    /// Cuts `new.start..new.end` out of `new.samples` (interleaved stereo,
    /// native rate — no resampling, see `engine::cut`'s `rate == out_rate`
    /// branch) and writes it into this kit's `samples/` dir, returning a
    /// `PadSource` pointing at that local copy. A byte-identical cut
    /// already in the library (by its decoded id) is reused rather than
    /// duplicated.
    fn copy_into_kit(&mut self, new: NewSample) -> Option<PadSource> {
        if !self.ensure_kit_dir() {
            return None;
        }
        let dir = self.samples_dir().expect("ensure_kit_dir just confirmed one");
        if std::fs::create_dir_all(&dir).is_err() {
            self.set_status("Could not create the kit's samples folder");
            return None;
        }
        let cut_sample = cut(new.samples, new.rate, new.start, new.end, new.rate);
        let tmp = dir.join(format!(".tmp-{}", std::process::id()));
        if odj_core::wav::write_wav(&tmp, &cut_sample.data, 2, new.rate).is_err() {
            self.set_status("Could not write the sample into the kit");
            return None;
        }
        let decoded = match decode(&tmp) {
            Ok(d) => d,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.set_status(format!("Could not save the sample: {e}"));
                return None;
            }
        };
        let final_path = dir.join(format!("{}-{}.wav", kits::slugify(&new.title), &decoded.id[..8]));
        if final_path.is_file() {
            // The exact same audio is already in the library.
            let _ = std::fs::remove_file(&tmp);
        } else if std::fs::rename(&tmp, &final_path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            self.set_status("Could not save the sample into the kit");
            return None;
        }
        Some(PadSource {
            track_id: decoded.id,
            path: final_path,
            title: new.title,
            artist: new.artist,
            sample_rate: decoded.sample_rate,
            cue: new.cue,
            start: 0.0,
            end: (decoded.samples.len() / 2) as f64,
        })
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

    // --- Kit browser -------------------------------------------------------------

    fn open_kit_browser(&mut self) {
        let rows = kit_browser::build_rows(false);
        self.screen = Screen::KitBrowser(KitBrowserState { rows, selected: 0, mode: KitBrowserMode::List });
    }

    fn kit_browser_key(&mut self, key: &KeyEvent) {
        enum Action {
            None,
            StartNaming,
            Switch(kit_browser::MaterializedKit),
            Cancel,
            Delete(usize),
        }
        let Screen::KitBrowser(state) = &mut self.screen else { return };
        let action = match &mut state.mode {
            KitBrowserMode::Naming(name) => {
                let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                match key.code {
                    KeyCode::Char(c) if typing => {
                        name.push(c);
                        Action::None
                    }
                    KeyCode::Backspace => {
                        name.pop();
                        Action::None
                    }
                    KeyCode::Enter if !name.trim().is_empty() => {
                        match kit_browser::materialize_local(&KitRow::CreateNew, Some(name.trim())) {
                            Some(m) => Action::Switch(m),
                            None => Action::None,
                        }
                    }
                    KeyCode::Esc => {
                        state.mode = KitBrowserMode::List;
                        Action::None
                    }
                    _ => Action::None,
                }
            }
            // Only a plain Enter confirms; anything else — including Esc,
            // arrows, a stray key — backs out, so a deletion this
            // permanent never happens by reflex.
            KitBrowserMode::ConfirmingDelete(i) => match key.code {
                KeyCode::Enter => Action::Delete(*i),
                _ => {
                    state.mode = KitBrowserMode::List;
                    Action::None
                }
            },
            KitBrowserMode::List => match key.code {
                KeyCode::Up => {
                    state.selected = state.selected.saturating_sub(1);
                    Action::None
                }
                KeyCode::Down => {
                    state.selected = (state.selected + 1).min(state.rows.len().saturating_sub(1));
                    Action::None
                }
                KeyCode::Enter if matches!(state.rows[state.selected], KitRow::CreateNew) => Action::StartNaming,
                KeyCode::Enter => match kit_browser::materialize_local(&state.rows[state.selected], None) {
                    Some(m) => Action::Switch(m),
                    None => Action::None,
                },
                KeyCode::Delete | KeyCode::Backspace if matches!(state.rows[state.selected], KitRow::Existing(..)) => {
                    state.mode = KitBrowserMode::ConfirmingDelete(state.selected);
                    Action::None
                }
                KeyCode::Esc => Action::Cancel,
                _ => Action::None,
            },
        };
        match action {
            Action::None => {}
            Action::StartNaming => {
                let Screen::KitBrowser(state) = &mut self.screen else { return };
                state.mode = KitBrowserMode::Naming(String::new());
            }
            Action::Switch(kit_browser::MaterializedKit::Saved(dir)) => {
                self.switch_kit(dir);
                self.screen = Screen::Pads;
            }
            Action::Switch(kit_browser::MaterializedKit::Unsaved(kit)) => {
                self.switch_to_unsaved_kit(*kit);
                self.screen = Screen::Pads;
            }
            Action::Cancel => self.screen = Screen::Pads,
            Action::Delete(i) => {
                let (dir, name) = {
                    let Screen::KitBrowser(state) = &self.screen else { return };
                    let KitRow::Existing(dir, name) = &state.rows[i] else { return };
                    (dir.clone(), name.clone())
                };
                let is_active = self.kit_dir.as_deref() == Some(dir.as_path());
                if std::fs::remove_dir_all(&dir).is_err() {
                    self.set_status(format!("Could not delete \"{name}\""));
                } else if is_active {
                    // The kit you were using is gone too: fall back to
                    // another one that still exists, or — if that was the
                    // last kit — a fresh, unnamed kit (nothing's written
                    // to disk for it until it's actually changed).
                    match kits::list_kits().into_iter().next() {
                        Some((fallback_dir, fallback_name)) => {
                            self.switch_kit(fallback_dir);
                            self.set_status(format!("Deleted \"{name}\" — switched to \"{fallback_name}\""));
                        }
                        None => {
                            self.switch_to_unsaved_kit(Kit::default());
                            self.set_status(format!("Deleted \"{name}\" — switched to a new, unnamed kit"));
                        }
                    }
                } else {
                    self.set_status(format!("Deleted \"{name}\""));
                }
                let Screen::KitBrowser(state) = &mut self.screen else { return };
                state.rows = kit_browser::build_rows(false);
                state.selected = state.selected.min(state.rows.len().saturating_sub(1));
                state.mode = KitBrowserMode::List;
            }
        }
    }

    /// Naming an unnamed kit's first real change — see `ensure_kit_dir`.
    /// The change that triggered this already happened in `self.slots`;
    /// naming it here just creates the directory and finally saves it.
    fn naming_kit_key(&mut self, key: &KeyEvent) {
        let Screen::NamingKit(name) = &mut self.screen else { return };
        let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if typing => name.push(c),
            KeyCode::Backspace => {
                name.pop();
            }
            KeyCode::Enter if !name.trim().is_empty() => {
                let name = name.trim().to_string();
                match kits::create_kit(&name) {
                    Ok(dir) => {
                        self.kit_dir = Some(dir);
                        self.kit_name = name;
                        self.screen = Screen::Pads;
                        self.save_kit();
                        let slug = self.kit_dir.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned());
                        settings::set_active_kit(slug);
                    }
                    Err(e) => self.set_status(format!("Could not create \"{name}\": {e}")),
                }
            }
            // The change itself stays (it's already in `self.slots`), just
            // un-saved — asked again next time something changes.
            KeyCode::Esc => self.screen = Screen::Pads,
            _ => {}
        }
    }

    // --- Sample browser ----------------------------------------------------------

    fn open_sample_browser(&mut self) {
        // An unnamed, not-yet-saved kit has no samples/ of its own to
        // start in — go straight to the list of other kits instead.
        let dir = self.samples_dir().unwrap_or_else(kits::kits_dir);
        let entries = build_browser_entries(&dir);
        self.screen = Screen::SampleBrowser(SampleBrowser { pad: self.selected, dir, entries, selected: 0 });
    }

    fn sample_browser_key(&mut self, key: &KeyEvent) {
        enum Action {
            None,
            Navigate(PathBuf),
            BrowseCues,
            Assign(PathBuf),
            Cancel,
        }
        let Screen::SampleBrowser(browser) = &mut self.screen else { return };
        let pad = browser.pad;
        let action = match key.code {
            KeyCode::Up => {
                browser.selected = browser.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Down => {
                browser.selected = (browser.selected + 1).min(browser.entries.len().saturating_sub(1));
                Action::None
            }
            KeyCode::Enter => match &browser.entries[browser.selected] {
                BrowserEntry::Up => Action::Navigate(kits::kits_dir()),
                BrowserEntry::Kit(dir, _) => Action::Navigate(dir.join("samples")),
                BrowserEntry::BrowsePlayerCues => Action::BrowseCues,
                BrowserEntry::Sample(path) => Action::Assign(path.clone()),
            },
            KeyCode::Esc => Action::Cancel,
            _ => Action::None,
        };
        match action {
            Action::None => {}
            Action::Navigate(dir) => {
                let Screen::SampleBrowser(browser) = &mut self.screen else { return };
                browser.entries = build_browser_entries(&dir);
                browser.dir = dir;
                browser.selected = 0;
            }
            Action::BrowseCues => self.open_picker(),
            Action::Assign(path) => self.assign_library_sample(pad, &path),
            Action::Cancel => self.screen = Screen::Pads,
        }
    }

    /// Copies `path` (a sample owned by some other kit) into this one,
    /// reusing a byte-identical copy already here (by its decoded id)
    /// rather than duplicating it. `None` on any read/write failure.
    fn copy_sample_into_kit(&mut self, path: &std::path::Path) -> Option<PathBuf> {
        if !self.ensure_kit_dir() {
            return None;
        }
        let dir = self.samples_dir().expect("ensure_kit_dir just confirmed one");
        if std::fs::create_dir_all(&dir).is_err() {
            self.set_status("Could not create the kit's samples folder");
            return None;
        }
        let decoded = match decode(path) {
            Ok(d) => d,
            Err(e) => {
                self.set_status(format!("Could not read {}: {e}", path.display()));
                return None;
            }
        };
        let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = dir.join(format!("{}-{}.wav", kits::slugify(&title), &decoded.id[..8]));
        if !dest.is_file() && std::fs::copy(path, &dest).is_err() {
            self.set_status("Could not copy the sample into this kit");
            return None;
        }
        Some(dest)
    }

    /// Assigns an already-finished library sample straight to a pad — no
    /// editor step, unlike a fresh odj-player cue: it's already a complete
    /// one-shot/loop someone (maybe this kit, maybe another) finished.
    fn assign_library_sample(&mut self, pad: usize, path: &std::path::Path) {
        let local_path = if path.parent() == self.samples_dir().as_deref() {
            path.to_path_buf()
        } else {
            let Some(copied) = self.copy_sample_into_kit(path) else { return };
            copied
        };
        let decoded = match decode(&local_path) {
            Ok(d) => d,
            Err(e) => {
                self.set_status(format!("Could not read {}: {e}", local_path.display()));
                return;
            }
        };
        let title = local_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.slots[pad].kit.source = Some(PadSource {
            track_id: decoded.id,
            path: local_path,
            title,
            artist: None,
            sample_rate: decoded.sample_rate,
            cue: "kit".into(),
            start: 0.0,
            end: (decoded.samples.len() / 2) as f64,
        });
        self.reload(pad);
        self.save_kit();
        self.screen = Screen::Pads;
        self.set_status(format!("Pad {} set", pad + 1));
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
                let Ok(decoded) = decode(&source.path) else {
                    self.set_status(format!("Could not read {}", source.path.display()));
                    return;
                };
                let Some(copied) = self.copy_into_kit(NewSample {
                    samples: &decoded.samples,
                    rate: decoded.sample_rate,
                    start: source.start,
                    end: source.end,
                    title: source.title,
                    artist: source.artist,
                    cue: source.cue,
                }) else {
                    return;
                };
                self.slots[pad].kit.source = Some(copied);
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
        let mut source = ed.source;
        // Already a file this (or re-trimming, the same) kit owns: just the
        // region changed, not the audio — no need to copy it again. A pad
        // picked fresh from odj-player's cues (or a legacy, pre-migration
        // pad) still points outside the kit, so gets copied in now.
        if source.path.parent() != self.samples_dir().as_deref()
            && let Some(track) = &ed.track
        {
            let Some(copied) = self.copy_into_kit(NewSample {
                samples: &track.samples,
                rate: track.sample_rate,
                start: source.start,
                end: source.end,
                title: source.title.clone(),
                artist: source.artist.clone(),
                cue: source.cue.clone(),
            }) else {
                return;
            };
            source = copied;
        }
        let secs = source.seconds();
        self.slots[pad].kit.source = Some(source);
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
            Screen::KitBrowser(_) if key.kind != KeyEventKind::Release => self.kit_browser_key(&key),
            Screen::SampleBrowser(_) if key.kind != KeyEventKind::Release => self.sample_browser_key(&key),
            Screen::NamingKit(_) if key.kind != KeyEventKind::Release => self.naming_kit_key(&key),
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
        // The help covers the pads; they don't play under it.
        if self.help.is_open() {
            match code {
                KeyCode::Esc | KeyCode::Char('/') => self.help.close(),
                _ => {
                    self.help.key(code, shift, crate::ui::HELP.len());
                }
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
            KeyCode::Char('/') => self.help.toggle(),
            KeyCode::Left => self.selected = i - i % 4 + (i % 4 + 3) % 4,
            KeyCode::Right => self.selected = i - i % 4 + (i % 4 + 1) % 4,
            KeyCode::Up => self.selected = (i + PADS - 4) % PADS,
            KeyCode::Down => self.selected = (i + 4) % PADS,
            KeyCode::Enter => self.open_sample_browser(),
            KeyCode::Char('t') => self.trim(),
            KeyCode::Char('m') => self.update_props(|p| p.mode = p.mode.next()),
            KeyCode::Char('l') => self.update_props(|p| p.looped = !p.looped),
            KeyCode::Char('g') => self.update_props(|p| p.retrigger = !p.retrigger),
            KeyCode::Char('-') => self.update_props(|p| p.gain_db = (p.gain_db - 1.0).max(MIN_GAIN_DB)),
            KeyCode::Char('=') => self.update_props(|p| p.gain_db = (p.gain_db + 1.0).min(MAX_GAIN_DB)),
            KeyCode::Char('0') => self.update_props(|p| p.gain_db = 0.0),
            KeyCode::Delete | KeyCode::Backspace => self.clear_pad(i),
            KeyCode::Char(' ') => self.sampler().stop_all(),
            KeyCode::Char('k') => self.open_kit_browser(),
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
/// `.wav` files directly in `dir`, as `Sample` rows, sorted by name — or,
/// at `kits_dir()` itself, every kit's directory instead (never raw
/// filesystem entries like `kit.json`). `BrowsePlayerCues` is always last.
fn build_browser_entries(dir: &std::path::Path) -> Vec<BrowserEntry> {
    let mut entries = Vec::new();
    if dir == kits::kits_dir() {
        entries.extend(kits::list_kits().into_iter().map(|(d, name)| BrowserEntry::Kit(d, name)));
        // Always reachable here too, even though it's never a real, listed
        // kit on its own (see `kit_browser::embedded_cache_dir`) — picking
        // a sample from it still copies into the active kit as usual.
        entries.push(BrowserEntry::Kit(kit_browser::embedded_cache_dir(), kit_browser::EMBEDDED_NAME.to_string()));
    } else {
        entries.push(BrowserEntry::Up);
        // The embedded kit's files are only written on first visit, same
        // as `materialize_embedded` already does for picking it from the
        // kit browser (it's a cheap no-op once they're already there).
        if dir == kit_browser::embedded_cache_dir().join("samples") {
            starter_kit::materialize_embedded(dir);
        }
        if let Ok(read) = std::fs::read_dir(dir) {
            let mut samples: Vec<PathBuf> = read
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "wav"))
                .collect();
            samples.sort();
            entries.extend(samples.into_iter().map(BrowserEntry::Sample));
        }
    }
    entries.push(BrowserEntry::BrowsePlayerCues);
    entries
}

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
        App::new(sampler, 48_000, Some(dir.clone()), Kit::load(&dir.join("kit.json")), true, String::new())
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
    fn copy_into_kit_writes_a_local_wav_and_dedupes_identical_cuts() {
        let mut a = app("copy-into-kit");
        let samples = vec![0.2; 2_000];

        let new_sample = || NewSample {
            samples: &samples,
            rate: 8_000,
            start: 0.0,
            end: 500.0,
            title: "Kick".into(),
            artist: Some("Me".into()),
            cue: "hot A".into(),
        };
        let first = a.copy_into_kit(new_sample()).expect("decodes and writes");
        assert_eq!(first.path.parent(), a.samples_dir().as_deref());
        assert_ne!(first.path, PathBuf::from("/nonexistent.wav"));
        assert_eq!(first.title, "Kick");
        assert_eq!(first.artist, Some("Me".into()));
        assert_eq!(first.cue, "hot A");
        assert_eq!(first.start, 0.0);

        let second = a.copy_into_kit(new_sample()).expect("decodes and writes");
        assert_eq!(first.path, second.path, "identical audio reuses the same file, not a duplicate");
        let files: Vec<_> = std::fs::read_dir(a.samples_dir().unwrap()).unwrap().collect();
        assert_eq!(files.len(), 1, "no duplicate written to disk");
    }

    #[test]
    fn kit_browser_switches_between_kits() {
        use ratatui::crossterm::event::KeyEventState;

        // See `kits::ENV_LOCK`: serializes against other tests that also
        // point `XDG_DATA_HOME`/`XDG_CONFIG_HOME` (process-global) at a
        // scratch directory.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-kit-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only, same pattern odj-player's tests already use.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }

        let kit_a = kits::create_kit("Kit A").unwrap();
        let kit_b = kits::create_kit("Kit B").unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(kit_a.clone()), Kit::load(&kit_a.join("kit.json")), true, String::new());
        a.slots[3].kit.source = Some(source());
        a.save_kit();
        let kit_a_json_before = std::fs::read_to_string(kit_a.join("kit.json")).unwrap();

        key(&mut a, 'k', KeyEventKind::Press);
        assert!(matches!(a.screen, Screen::KitBrowser(_)));
        let press = |code| KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE };
        // Rows sort existing kits by name: "Kit A" (the current one, row 0), "Kit B" (row 1).
        a.handle_key(press(KeyCode::Down));
        a.handle_key(press(KeyCode::Enter));

        assert!(matches!(a.screen, Screen::Pads));
        assert_eq!(a.kit_dir, Some(kit_b.clone()));
        assert_eq!(a.kit_name, "Kit B");
        assert!(a.slots[3].kit.source.is_none(), "the other kit starts with its own, empty pads");

        let active = odj_core::memory::load_settings().kit.unwrap();
        assert_eq!(kits::kits_dir().join(active), kit_b);
        assert_eq!(std::fs::read_to_string(kit_a.join("kit.json")).unwrap(), kit_a_json_before, "switching away doesn't touch the other kit's file");

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn kit_browser_delete_needs_confirmation() {
        use ratatui::crossterm::event::KeyEventState;

        // See `kits::ENV_LOCK`.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-kit-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        } // SAFETY: see other tests above.

        let kit_a = kits::create_kit("Kit A").unwrap();
        let kit_b = kits::create_kit("Kit B").unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(kit_a.clone()), Kit::load(&kit_a.join("kit.json")), true, String::new());
        let press = |code| KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE };

        // Rows: "Kit A" (active, row 0), "Kit B" (row 1), Embedded, Create new.
        a.handle_key(press(KeyCode::Char('k')));
        a.handle_key(press(KeyCode::Delete));
        let Screen::KitBrowser(state) = &a.screen else { panic!("expected the kit browser") };
        assert!(matches!(state.mode, KitBrowserMode::ConfirmingDelete(0)), "Delete asks first, it doesn't act yet");

        // Anything but Enter backs out without deleting.
        a.handle_key(press(KeyCode::Esc));
        let Screen::KitBrowser(state) = &a.screen else { panic!("expected the kit browser") };
        assert!(matches!(state.mode, KitBrowserMode::List));
        assert!(kit_a.join("kit.json").is_file(), "cancelling the confirmation keeps the kit");

        // Kit B isn't active, so it just goes.
        a.handle_key(press(KeyCode::Down));
        a.handle_key(press(KeyCode::Delete));
        let Screen::KitBrowser(state) = &a.screen else { panic!("expected the kit browser") };
        assert!(matches!(state.mode, KitBrowserMode::ConfirmingDelete(1)));
        a.handle_key(press(KeyCode::Enter));
        assert!(!kit_b.exists(), "confirmed: the whole directory is gone");
        assert!(matches!(a.screen, Screen::KitBrowser(_)), "stays in the browser, doesn't bounce to Pads");

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deleting_the_active_kit_falls_back_to_another_kit() {
        use ratatui::crossterm::event::KeyEventState;

        // See `kits::ENV_LOCK`.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-kit-delete-active-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        } // SAFETY: see other tests above.

        let kit_a = kits::create_kit("Kit A").unwrap();
        let kit_b = kits::create_kit("Kit B").unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(kit_a.clone()), Kit::load(&kit_a.join("kit.json")), true, String::new());
        a.slots[0].kit.source = Some(source());
        a.set_sample(0, Some(Sample::new(vec![0.1; 9600], 4800)));
        a.save_kit();
        let press = |code| KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE };

        // Rows: "Kit A" (active, row 0), "Kit B" (row 1).
        a.handle_key(press(KeyCode::Char('k')));
        a.handle_key(press(KeyCode::Delete));
        a.handle_key(press(KeyCode::Enter));

        assert!(!kit_a.exists(), "actually removed, not just emptied");
        assert_eq!(a.kit_dir, Some(kit_b), "fell back to the other kit");
        assert_eq!(a.kit_name, "Kit B");
        assert_eq!(a.status(), Some("Deleted \"Kit A\" — switched to \"Kit B\""));
        assert_eq!(odj_core::memory::load_settings().kit.unwrap(), "kit-b");

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deleting_the_only_kit_switches_to_an_unnamed_kit() {
        use ratatui::crossterm::event::KeyEventState;

        // See `kits::ENV_LOCK`.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-kit-delete-only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        } // SAFETY: see other tests above.

        let kit_a = kits::create_kit("Kit A").unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(kit_a.clone()), Kit::load(&kit_a.join("kit.json")), true, String::new());
        let press = |code| KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE };

        // Only "Kit A" exists, so row 0 (active) is selected by default.
        a.handle_key(press(KeyCode::Char('k')));
        a.handle_key(press(KeyCode::Delete));
        a.handle_key(press(KeyCode::Enter));

        assert!(!kit_a.exists(), "actually removed, not just emptied");
        assert_eq!(a.kit_dir, None, "nothing written to disk for it yet");
        assert_eq!(a.kit_name, "");
        assert!(a.slots.iter().all(|s| s.kit.source.is_none()));
        assert_eq!(a.status(), Some("Deleted \"Kit A\" — switched to a new, unnamed kit"));
        assert!(kits::list_kits().is_empty(), "no kit directory was created just from switching to it");
        assert!(odj_core::memory::load_settings().kit.is_none());

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn changing_an_unnamed_kit_asks_for_a_name_before_saving_it() {
        // See `kits::ENV_LOCK`.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-kit-naming-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        } // SAFETY: see other tests above.

        let starter = kits::create_kit("Starter").unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(starter.clone()), Kit::load(&starter.join("kit.json")), true, String::new());
        a.switch_to_unsaved_kit(Kit::default());
        assert_eq!(a.kit_dir, None);

        // A prop change is the simplest "something changed" — no file to write, just a save.
        a.selected = 2;
        key(&mut a, 'm', KeyEventKind::Press);
        assert!(matches!(a.screen, Screen::NamingKit(_)), "asks for a name before it can save");
        assert_eq!(a.sampler().pad(2).props.mode, Mode::Gate, "the change itself already happened, in memory");

        use ratatui::crossterm::event::KeyEventState;
        for c in "Trap".chars() {
            key(&mut a, c, KeyEventKind::Press);
        }
        a.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });

        assert!(matches!(a.screen, Screen::Pads));
        assert_eq!(a.kit_name, "Trap");
        let dir = a.kit_dir.clone().expect("named, so now it has a directory");
        assert!(dir.join("kit.json").is_file());
        let kit = Kit::load(&dir.join("kit.json"));
        assert_eq!(kit.name, "Trap");
        assert_eq!(kit.pads[2].props.mode, Mode::Gate, "the change that triggered the prompt was kept, not lost");
        assert_eq!(odj_core::memory::load_settings().kit.unwrap(), dir.file_name().unwrap().to_string_lossy());

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn picking_the_embedded_kit_writes_nothing_until_changed() {
        use ratatui::crossterm::event::KeyEventState;

        // See `kits::ENV_LOCK`.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-kit-embedded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        } // SAFETY: see other tests above.

        let starter = kits::create_kit("Starter").unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(starter), Kit::default(), true, String::new());

        let Some(kit_browser::MaterializedKit::Unsaved(kit)) = kit_browser::materialize_local(&KitRow::Embedded, None) else {
            panic!("picking Embedded is never a saved kit");
        };
        assert_eq!(kit.name, "Acoustic (Embedded)");
        assert!(kit.pads[0].source.is_some(), "the embedded pads are already filled");
        a.switch_to_unsaved_kit(*kit);

        assert_eq!(a.kit_dir, None, "nothing kit-shaped on disk yet");
        assert_eq!(a.kit_name, "Acoustic (Embedded)");
        assert_eq!(kits::list_kits().len(), 1, "only Starter — not a real, listed kit until it's changed");

        // Press pad 1: playing it doesn't count as "changing" the kit.
        key(&mut a, '1', KeyEventKind::Press);
        assert_eq!(a.kit_dir, None, "merely playing a pad doesn't materialize it either");
        assert_eq!(kits::list_kits().len(), 1);

        // Now actually change something — same deferred-naming flow as any other unsaved kit.
        a.selected = 0;
        key(&mut a, 'l', KeyEventKind::Press); // toggle looping
        let Screen::NamingKit(name) = &a.screen else { panic!("expected the naming prompt") };
        assert_eq!(name, "Acoustic (Embedded)", "pre-filled with the name it already has");

        a.handle_key(KeyEvent { code: KeyCode::Enter, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE });

        assert!(matches!(a.screen, Screen::Pads));
        let saved_dir = a.kit_dir.clone().expect("now it has a directory");
        assert_eq!(Kit::load(&saved_dir.join("kit.json")).name, "Acoustic (Embedded)");
        assert_eq!(kits::list_kits().len(), 2, "Starter, plus this one now that it's real");

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sample_browser_navigates_up_to_other_kits_and_copies_a_pick() {
        use ratatui::crossterm::event::KeyEventState;

        // See `kits::ENV_LOCK`.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-sample-browser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        } // SAFETY: see other tests above.

        let kit_a = kits::create_kit("Kit A").unwrap();
        let kit_b = kits::create_kit("Kit B").unwrap();
        odj_core::wav::write_wav(&kit_b.join("samples").join("snare.wav"), &vec![0.1; 400], 1, 8_000).unwrap();

        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(kit_a.clone()), Kit::load(&kit_a.join("kit.json")), true, String::new());
        let press = |code| KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE };

        a.handle_key(press(KeyCode::Enter)); // open the sample browser on pad 0
        // Kit A's own samples/ is empty, so entries are [Up, BrowsePlayerCues]; selected starts on Up.
        a.handle_key(press(KeyCode::Enter));
        let Screen::SampleBrowser(browser) = &a.screen else { panic!("expected the sample browser") };
        assert_eq!(browser.dir, kits::kits_dir(), "Up lands on the list of kits, not a raw filesystem dir");
        assert!(
            browser.entries.iter().any(|e| matches!(e, BrowserEntry::Kit(d, name) if d == &kit_b && name == "Kit B")),
            "the other kit is listed here, not raw filesystem noise"
        );

        a.handle_key(press(KeyCode::Down)); // Kit A, Kit B, Browse cues — select Kit B
        a.handle_key(press(KeyCode::Enter)); // descend into Kit B's samples/
        let Screen::SampleBrowser(browser) = &a.screen else { panic!("expected the sample browser") };
        assert_eq!(browser.dir, kit_b.join("samples"));

        a.handle_key(press(KeyCode::Down)); // Up, snare.wav, Browse cues — select snare.wav
        a.handle_key(press(KeyCode::Enter)); // assign it

        assert!(matches!(a.screen, Screen::Pads));
        let source = a.slots[0].kit.source.as_ref().expect("the pad got a source");
        assert_eq!(source.path.parent(), a.samples_dir().as_deref(), "copied into Kit A, not left pointing at Kit B");
        assert!(source.path.is_file());

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sample_browser_reaches_the_embedded_kit_even_though_its_never_a_real_one() {
        use ratatui::crossterm::event::KeyEventState;

        // See `kits::ENV_LOCK`.
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-sample-browser-embedded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        } // SAFETY: see other tests above.

        let kit_a = kits::create_kit("Kit A").unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(kit_a), Kit::default(), true, String::new());
        let press = |code| KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE };

        a.handle_key(press(KeyCode::Enter)); // open the sample browser on pad 0
        a.handle_key(press(KeyCode::Enter)); // Up -> the list of kits
        let Screen::SampleBrowser(browser) = &a.screen else { panic!("expected the sample browser") };
        assert_eq!(browser.dir, kits::kits_dir());
        let embedded_at = browser.entries.iter().position(|e| {
            matches!(e, BrowserEntry::Kit(d, name) if d == &kit_browser::embedded_cache_dir() && name == kit_browser::EMBEDDED_NAME)
        });
        assert!(embedded_at.is_some(), "the embedded kit is reachable here even with no real kit directory");
        assert_eq!(kits::list_kits().len(), 1, "...and still isn't a real, listed kit — just Kit A");

        for _ in 0..embedded_at.unwrap() {
            a.handle_key(press(KeyCode::Down));
        }
        a.handle_key(press(KeyCode::Enter)); // descend into it — materializes the assets on first visit
        let Screen::SampleBrowser(browser) = &a.screen else { panic!("expected the sample browser") };
        assert_eq!(browser.dir, kit_browser::embedded_cache_dir().join("samples"));
        assert!(
            browser.entries.iter().any(|e| matches!(e, BrowserEntry::Sample(p) if p.extension().is_some_and(|e| e == "wav"))),
            "the embedded .wav files are listed, not empty"
        );

        a.handle_key(press(KeyCode::Down)); // Up, <first sample>, ... — select the first sample
        a.handle_key(press(KeyCode::Enter)); // assign it

        assert!(matches!(a.screen, Screen::Pads));
        let source = a.slots[0].kit.source.as_ref().expect("the pad got a source");
        assert_eq!(source.path.parent(), a.samples_dir().as_deref(), "copied into Kit A, not left pointing at the cache");
        assert!(source.path.is_file());

        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        };
        std::fs::remove_dir_all(dir).unwrap();
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
        let kit = Kit::load(&a.kit_file().unwrap());
        assert_eq!(kit.pads[2].props.gain_db, -1.0);
        assert_eq!(kit.pads[2].props.mode, Mode::Gate);
    }

    #[test]
    fn missing_files_are_reported_and_kept_in_the_kit() {
        let dir = std::env::temp_dir().join(format!("odj-sampler-test-{}-missing", std::process::id()));
        let mut kit = Kit::default();
        kit.pads[0].source = Some(source());
        kit.save(&dir.join("kit.json")).unwrap();
        let mut a = App::new(Arc::new(Mutex::new(Sampler::new())), 48_000, Some(dir.clone()), Kit::load(&dir.join("kit.json")), true, String::new());
        assert_eq!(a.slots[0].status, PadStatus::Loading);
        let t = Instant::now();
        while a.slots[0].status == PadStatus::Loading && t.elapsed() < Duration::from_secs(5) {
            a.tick();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(a.slots[0].status, PadStatus::Missing);
        let kit_file = a.kit_file().unwrap();
        assert!(Kit::load(&kit_file).pads[0].source.is_some());
        std::fs::remove_dir_all(kit_file.parent().unwrap()).unwrap();
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

    #[test]
    fn help_opens_on_the_basics_and_turns_to_the_other_pages() {
        let mut a = app("help");
        key(&mut a, '?', KeyEventKind::Press);
        assert!(a.help.is_open());
        key(&mut a, 'q', KeyEventKind::Press);
        assert!(a.help.is_open(), "pads don't play under the help");
        a.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!((a.help.page(), a.selected), (1, 0), "→ turns the page, not the pad");
        a.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        a.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(a.help.page(), crate::ui::HELP.len() - 1, "round to About");
        key(&mut a, '?', KeyEventKind::Press);
        assert!(!a.help.is_open());
        key(&mut a, '?', KeyEventKind::Press);
        assert_eq!(a.help.page(), 0);
        a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!a.help.is_open());
    }

    /// The README's Keys section mirrors the help: the same pages, each with
    /// the same keys.
    #[test]
    fn the_readme_has_every_help_key() {
        use odj_core::tui::Page;
        let readme = include_str!("../../../README.md").replace('`', "");
        let start = readme.find("### odj-sampler\n").expect("a section for odj-sampler");
        let section = &readme[start..];
        let section = &section[..section[4..].find("\n### ").map_or(section.len(), |i| i + 4)];
        for page in crate::ui::HELP {
            let Page::Keys(title, keys) = page else { continue };
            let page = section.split("#### ").find(|p| p.starts_with(&format!("{title}\n"))).expect(title);
            for (k, v) in *keys {
                assert!(page.contains(&format!("| {k} | {v} |")), "{title}: {k}");
            }
        }
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

        let kit_browser = Screen::KitBrowser(KitBrowserState {
            rows: vec![kit_browser::KitRow::Embedded, kit_browser::KitRow::CreateNew],
            selected: 0,
            mode: KitBrowserMode::List,
        });
        let kit_browser_naming = Screen::KitBrowser(KitBrowserState {
            rows: vec![kit_browser::KitRow::Embedded, kit_browser::KitRow::CreateNew],
            selected: 1,
            mode: KitBrowserMode::Naming("My New Kit".into()),
        });
        let kit_browser_deleting = Screen::KitBrowser(KitBrowserState {
            rows: vec![kit_browser::KitRow::Existing(PathBuf::from("/tmp/kits/trap"), "Trap".into())],
            selected: 0,
            mode: KitBrowserMode::ConfirmingDelete(0),
        });
        let sample_browser = Screen::SampleBrowser(SampleBrowser {
            pad: 3,
            dir: PathBuf::from("/tmp"),
            entries: vec![BrowserEntry::Up, BrowserEntry::Sample(PathBuf::from("/tmp/kick.wav")), BrowserEntry::BrowsePlayerCues],
            selected: 1,
        });
        let screens = [
            Screen::Pads,
            Screen::Picker(picker),
            Screen::Editor(Box::new(ed)),
            kit_browser,
            kit_browser_naming,
            kit_browser_deleting,
            sample_browser,
            Screen::NamingKit("Trap".into()),
        ];
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
        a.screen = Screen::Pads;
        a.help.toggle();
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let snapshot = a.sampler().snapshot();
        t.draw(|f| crate::ui::draw(f, &a, &snapshot)).unwrap();
    }
}
