//! Keyboard handling and app state around the deck.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use odj_core::library::{Browser, siblings};
use odj_core::link;
use odj_core::memory::{Memory, TrackMemory};
use odj_core::track::Track;
use odj_core::tui::Help;

use crate::engine::{Deck, JUMP_SIZES, MAX_LOOP_BEATS, MIN_LOOP_BEATS, MemoryCall};
use crate::video;

/// How far the playhead must move since the last request before asking for
/// another video frame. Matches the batch's own frame spacing: asking for
/// less than that wouldn't show a different frame anyway, and the worker
/// usually serves it from the already-decoded batch, not a fresh `ffmpeg`
/// spawn — see `video::FRAME_INTERVAL`.
const VIDEO_POS_EPSILON: f64 = video::FRAME_INTERVAL;

/// A loaded track's video preview, if it has a video track and `ffmpeg`
/// stays available for it.
pub struct VideoView {
    worker: video::VideoWorker,
    pub frame: Option<video::Frame>,
    last_requested: f64,
}

impl VideoView {
    fn new(source: Arc<dyn video::VideoSource>) -> Self {
        Self { worker: video::VideoWorker::start(source), frame: None, last_requested: f64::NEG_INFINITY }
    }
}

/// Without key-release events a hold lasts this long after the last key event.
const HOLD_FALLBACK: Duration = Duration::from_millis(500);
const STATUS_TIME: Duration = Duration::from_secs(4);
/// How long a first `X` on a loop memory stays armed, waiting for a second
/// one to confirm the delete.
const MEMORY_DELETE_CONFIRM: Duration = Duration::from_secs(2);
const BEND: f64 = 3.0;
const BEND_STRONG: f64 = 12.0;
const SEARCH: f64 = 4.0;
const SUPER_SEARCH: f64 = 20.0;
const BRAKE_TIMES: &[f64] = &[0.0, 0.25, 0.5, 1.0, 2.0, 4.0];
const START_TIMES: &[f64] = &[0.0, 0.1, 0.25, 0.5, 1.0];

/// A held key. `until` is set when we can't see the release and must time out.
struct Hold {
    strong: bool,
    until: Option<Instant>,
}

/// How many rows the settings screen has (auto-cue, music folders).
const SETTINGS_ROWS: usize = 2;

/// The settings screen: a list of global settings, one of which (music
/// folders) drills into its own list.
#[derive(Default)]
pub struct SettingsScreen {
    open: bool,
    row: usize,
    /// Set while drilled into the music-folders row, with the selected index.
    folder: Option<usize>,
    /// False when this is the quick switcher opened by going "up" from a
    /// music folder while browsing: picking is still allowed, but adding
    /// and removing folders is something only the real settings screen does.
    editable: bool,
    /// True only for the forced first-run flow (`open_to_music_dirs`): no
    /// folder configured yet anywhere, so there's nothing else to do. Not
    /// re-triggered by later emptying an already-set-up list by hand — that
    /// Esc should still work, since you're not stuck, you just chose to.
    onboarding: bool,
}

impl SettingsScreen {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn row(&self) -> usize {
        self.row
    }

    pub fn folder(&self) -> Option<usize> {
        self.folder
    }

    pub fn editable(&self) -> bool {
        self.editable
    }

    pub fn onboarding(&self) -> bool {
        self.onboarding
    }

    fn toggle(&mut self) {
        self.open = !self.open;
        self.row = 0;
        self.folder = None;
        self.editable = true;
        self.onboarding = false;
    }

    /// Opens straight into the (editable) music folders list, for first-run
    /// setup when nothing's configured yet, skipping the row menu.
    pub fn open_to_music_dirs(&mut self) {
        self.open = true;
        self.row = 1;
        self.folder = Some(0);
        self.editable = true;
        self.onboarding = true;
    }

    /// Opens the non-editable quick switcher, folder `i` selected — for
    /// picking among several saved folders without being able to add or
    /// remove any.
    pub fn open_to_switch(&mut self, i: usize) {
        self.open = true;
        self.row = 1;
        self.folder = Some(i);
        self.editable = false;
        self.onboarding = false;
    }

    /// Esc: out of the folder list first, else closes. The quick switcher
    /// has no row list to step back to, so it always closes outright.
    fn back(&mut self) {
        if !self.editable || self.folder.take().is_none() {
            self.open = false;
            self.folder = None;
        }
    }

    /// Jumping to a folder leaves the screen entirely, not just its list.
    fn close(&mut self) {
        self.open = false;
        self.folder = None;
    }
}

pub struct App {
    pub deck: Arc<Mutex<Deck>>,
    pub browser: Browser,
    memory: Memory,
    /// The loaded track's cue memory as it was on disk when loaded — see
    /// `remember`.
    memory_baseline: TrackMemory,
    loading: Option<(PathBuf, mpsc::Receiver<anyhow::Result<Track>>)>,
    status: Option<(String, Instant)>,
    pub help: Help,
    pub settings_screen: SettingsScreen,
    /// Browsing to pick a new music folder (from Settings' Tab): a plain
    /// file browser, so going up stays a filesystem "up" even at an
    /// existing music folder, rather than jumping to the quick switcher.
    picking_music_dir: bool,
    pub show_remaining: bool,
    /// Hot cue REC mode: the hot cue keys store instead of trigger.
    pub hot_rec: bool,
    /// Loaded track's number and the track count in its folder.
    pub track_number: Option<(usize, usize)>,
    /// Whether the terminal reports key releases (kitty keyboard protocol).
    pub key_release: bool,
    pub output_name: String,
    bend_down: Option<Hold>,
    bend_up: Option<Hold>,
    search_back: Option<Hold>,
    search_fwd: Option<Hold>,
    /// The loop roll key held down.
    roll: Option<Hold>,
    /// Recent taps, with the playhead at each while playing forward.
    taps: Vec<(Instant, Option<f64>)>,
    /// Set by a first `X` on a loop memory; a second one within
    /// `MEMORY_DELETE_CONFIRM` deletes it, so one stray press can't lose a
    /// promoted loop the way it can a plain cue.
    pending_memory_delete: Option<Instant>,
    pub quit: bool,
    pub video: Option<VideoView>,
}

impl App {
    pub fn new(deck: Arc<Mutex<Deck>>, browser: Browser, key_release: bool, output_name: String, memory: Memory) -> Self {
        {
            let mut d = deck.lock().unwrap_or_else(|e| e.into_inner());
            if JUMP_SIZES.contains(&memory.settings.jump_beats) {
                d.jump_beats = memory.settings.jump_beats;
            }
            d.loop_size = memory.settings.loop_beats.clamp(MIN_LOOP_BEATS, MAX_LOOP_BEATS);
        }
        Self {
            deck,
            browser,
            memory,
            memory_baseline: TrackMemory::default(),
            loading: None,
            status: None,
            help: Help::default(),
            settings_screen: SettingsScreen::default(),
            picking_music_dir: false,
            show_remaining: false,
            hot_rec: false,
            track_number: None,
            key_release,
            output_name,
            bend_down: None,
            bend_up: None,
            search_back: None,
            search_fwd: None,
            roll: None,
            taps: Vec::new(),
            pending_memory_delete: None,
            quit: false,
            video: None,
        }
    }

    fn deck(&self) -> MutexGuard<'_, Deck> {
        self.deck.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn status(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STATUS_TIME)
            .map(|(msg, _)| msg.as_str())
    }

    /// Keeps the beatjump and beatloop sizes for next time.
    fn save_sizes(&mut self) {
        let (jump, size) = {
            let deck = self.deck();
            (deck.jump_beats, deck.loop_size)
        };
        let s = &mut self.memory.settings;
        if (s.jump_beats, s.loop_beats) == (jump, size) {
            return;
        }
        (s.jump_beats, s.loop_beats) = (jump, size);
        if let Err(e) = self.memory.save_settings() {
            self.set_status(format!("Could not save settings: {e}"));
        }
    }

    pub fn auto_cue(&self) -> bool {
        self.memory.settings.auto_cue
    }

    pub fn music_dirs(&self) -> &[PathBuf] {
        &self.memory.settings.music_dirs
    }

    fn toggle_auto_cue(&mut self) {
        self.memory.settings.auto_cue = !self.memory.settings.auto_cue;
        let on = self.memory.settings.auto_cue;
        self.set_status(format!("Auto Cue {} (applies from the next load)", if on { "on" } else { "off" }));
        if let Err(e) = self.memory.save_settings() {
            self.set_status(format!("Could not save settings: {e}"));
        }
    }

    /// Adds the browser's current folder to the music folders list, so it's
    /// one of the places the browser can open or jump to. A no-op if it's
    /// already there or inside one already there — its subfolders are
    /// already browseable through that one. Adding a folder that's an
    /// ancestor of existing ones replaces them, for the same reason.
    fn add_music_dir(&mut self) {
        let dir = self.browser.dir.clone();
        if let Some(existing) = self.memory.settings.music_dirs.iter().find(|d| dir.starts_with(d)).cloned() {
            self.set_status(format!("{} is already inside {}", dir.display(), existing.display()));
            return;
        }
        let dirs = &mut self.memory.settings.music_dirs;
        let before = dirs.len();
        dirs.retain(|d| !d.starts_with(&dir));
        let replaced = before - dirs.len();
        dirs.push(dir.clone());
        match self.memory.save_settings() {
            Ok(()) => {
                let msg = if before == 0 {
                    format!("Using {} as your music folder", dir.display())
                } else if replaced == 0 {
                    format!("Added {} to music folders", dir.display())
                } else {
                    let s = if replaced == 1 { "" } else { "s" };
                    format!("Added {} to music folders, replacing {replaced} subfolder{s}", dir.display())
                };
                self.set_status(msg);
                // Picked a folder to add, rather than adding one in passing
                // while just browsing: that's done now, so leave both screens.
                if self.picking_music_dir {
                    self.browser.close();
                    self.settings_screen.close();
                    self.picking_music_dir = false;
                }
            }
            Err(e) => self.set_status(format!("Could not save settings: {e}")),
        }
    }

    /// Removes the music folder at `i`, keeping the selection in range.
    fn remove_music_dir(&mut self, i: usize) {
        if i >= self.memory.settings.music_dirs.len() {
            return;
        }
        let removed = self.memory.settings.music_dirs.remove(i);
        match self.memory.save_settings() {
            Ok(()) => self.set_status(format!("Removed {}", removed.display())),
            Err(e) => self.set_status(format!("Could not save settings: {e}")),
        }
        let last = self.memory.settings.music_dirs.len().saturating_sub(1);
        self.settings_screen.folder = Some(i.min(last));
    }

    /// Opens the browser at the music folder at `i`, leaving settings.
    fn jump_to_music_dir(&mut self, i: usize) {
        let Some(dir) = self.memory.settings.music_dirs.get(i).cloned() else { return };
        self.browser.enter(dir, None);
        self.browser.open = true;
        self.picking_music_dir = false;
        self.settings_screen.close();
    }

    /// Going up from one of the music folders goes to the folder list
    /// instead of its filesystem parent; elsewhere, and while picking a new
    /// music folder to add, it's a normal "up" even at an existing one —
    /// you're looking for a different folder, not browsing to play.
    fn up_or_to_music_dirs(&mut self) {
        if self.picking_music_dir {
            self.browser.parent();
            return;
        }
        let at = self.memory.settings.music_dirs.iter().position(|d| *d == self.browser.dir);
        match at {
            Some(i) => {
                self.browser.close();
                self.settings_screen.open_to_switch(i);
            }
            None => self.browser.parent(),
        }
    }

    /// While the settings screen is open it's modal: ↑↓ to move or drill,
    /// Enter/X to act, Esc falls through to close it, anything else is
    /// swallowed rather than reaching the deck underneath.
    fn settings_key(&mut self, code: KeyCode) -> bool {
        if !self.settings_screen.open {
            return false;
        }
        match (self.settings_screen.folder, code) {
            // Forced first-run setup, nothing picked yet: there's nothing
            // else to do, so no backing out (Ctrl+C still quits the app).
            // Not triggered by later emptying an already-set-up list by
            // hand — that's a deliberate choice, not being stuck.
            (Some(_), KeyCode::Esc) if self.settings_screen.onboarding => {}
            (_, KeyCode::Esc) => return false,
            (Some(i), KeyCode::Up) => self.settings_screen.folder = Some(i.saturating_sub(1)),
            (Some(i), KeyCode::Down) => {
                let last = self.memory.settings.music_dirs.len().saturating_sub(1);
                self.settings_screen.folder = Some((i + 1).min(last));
            }
            (Some(i), KeyCode::Char('x') | KeyCode::Delete) if self.settings_screen.editable => {
                self.remove_music_dir(i);
            }
            // The quick switcher (not editable) is for picking a folder to
            // browse; the real settings screen is for managing the list, so
            // it doesn't also double as a way to jump into one.
            (Some(i), KeyCode::Enter | KeyCode::Right) if !self.settings_screen.editable => {
                self.jump_to_music_dir(i);
            }
            // Settings stays open underneath; a successful Ctrl+U while
            // picking closes both and returns to normal use (add_music_dir).
            // Picking, so going up doesn't bounce off an existing folder.
            (Some(_), KeyCode::Enter | KeyCode::Right) if self.settings_screen.editable => {
                self.browser.open = true;
                self.picking_music_dir = true;
            }
            (None, KeyCode::Up) => self.settings_screen.row = self.settings_screen.row.saturating_sub(1),
            (None, KeyCode::Down) => self.settings_screen.row = (self.settings_screen.row + 1).min(SETTINGS_ROWS - 1),
            (None, KeyCode::Enter) if self.settings_screen.row == 0 => self.toggle_auto_cue(),
            (None, KeyCode::Enter) => self.settings_screen.folder = Some(0),
            _ => {}
        }
        true
    }

    pub fn loading(&self) -> Option<String> {
        self.loading
            .as_ref()
            .map(|(p, _)| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
    }

    // --- Tracks ----------------------------------------------------------------

    /// Starts loading a track in the background; it replaces the current
    /// one, playing or not, once decoded.
    pub fn load(&mut self, path: PathBuf) {
        // Absolute paths keep folder lookups and cue memory keys stable.
        let path = path.canonicalize().unwrap_or(path);
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        thread::spawn(move || {
            let _ = tx.send(Track::load(&p));
        });
        self.loading = Some((path, rx));
    }

    /// Stores the loaded track's cues so they come back next time. Merges
    /// with whatever's on disk rather than overwriting it outright, so
    /// another odj instance with the same track open doesn't have its own
    /// saved changes silently discarded — see `Memory::save_track_merged`.
    pub fn remember(&mut self) {
        let (track, mem) = {
            let deck = self.deck();
            let Some(track) = deck.track.as_ref() else { return };
            (track.clone(), deck.memory())
        };
        if let Err(e) = self.memory.save_track_merged(&track, &self.memory_baseline, &mem) {
            self.set_status(format!("Could not save cue memory: {e}"));
        }
    }

    fn finish_loading(&mut self) {
        let Some((path, rx)) = &self.loading else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("loader crashed")),
        };
        let path = path.clone();
        self.loading = None;
        match result {
            Ok(track) => {
                self.remember();
                let mem = self.memory.get(&track);
                // This track's on-disk state at the moment of loading —
                // `remember()` diffs against it to merge rather than
                // overwrite; see `Memory::save_track_merged`.
                self.memory_baseline = mem.clone();
                let msg = match track.bpm() {
                    Some(bpm) => format!("Loaded {} ({bpm:.1} BPM)", track.title),
                    None => format!("Loaded {} (no BPM found)", track.title),
                };
                let folder = siblings(&track.path);
                self.track_number = folder
                    .iter()
                    .position(|p| *p == track.path)
                    .map(|i| (i + 1, folder.len()));
                let auto_cue = self.memory.settings.auto_cue;
                self.video = track
                    .has_video
                    .then(|| VideoView::new(Arc::new(video::FfmpegSource::new(track.path.clone()))));
                let old = self.deck().load(Arc::new(track), &mem, auto_cue);
                // Free the previous track's samples outside the audio lock.
                drop(old);
                self.set_status(msg);
            }
            Err(e) => self.set_status(format!("Can't load {}: {e}", path.display())),
        }
    }

    fn skip_track(&mut self, delta: isize) {
        // Count from a track still loading so quick repeated presses keep moving.
        let current = match &self.loading {
            Some((path, _)) => Some(path.clone()),
            None => self.deck().track.as_ref().map(|t| t.path.clone()),
        };
        let Some(current) = current else {
            self.browser.open = true;
            self.picking_music_dir = false;
            return;
        };
        let list = siblings(&current);
        let Some(i) = list.iter().position(|p| *p == current) else { return };
        let j = i as isize + delta;
        if j < 0 || j as usize >= list.len() {
            self.set_status(if delta < 0 { "First track" } else { "Last track" });
            return;
        }
        self.load(list[j as usize].clone());
    }

    // --- Holds -----------------------------------------------------------------

    fn hold(&self, strong: bool) -> Option<Hold> {
        let until = (!self.key_release).then(|| Instant::now() + HOLD_FALLBACK);
        Some(Hold { strong, until })
    }

    fn apply_holds(&mut self) {
        let now = Instant::now();
        for hold in [
            &mut self.bend_down,
            &mut self.bend_up,
            &mut self.search_back,
            &mut self.search_fwd,
        ] {
            if hold.as_ref().and_then(|h| h.until).is_some_and(|t| now >= t) {
                *hold = None;
            }
        }
        if self.roll.take_if(|h| h.until.is_some_and(|t| now >= t)).is_some() {
            self.deck().roll_up();
        }
        let amount = |h: &Option<Hold>| match h {
            Some(h) if h.strong => BEND_STRONG,
            Some(_) => BEND,
            None => 0.0,
        };
        let speed = |h: &Option<Hold>| match h {
            Some(h) if h.strong => SUPER_SEARCH,
            Some(_) => SEARCH,
            None => 0.0,
        };
        let bend = amount(&self.bend_up) - amount(&self.bend_down);
        let search = speed(&self.search_fwd) - speed(&self.search_back);
        let mut deck = self.deck();
        deck.bend = bend;
        deck.search = search;
    }

    pub fn tick(&mut self) {
        self.finish_loading();
        self.apply_holds();
        self.update_video();
    }

    /// Drains any frame the video worker decoded, and requests one at the
    /// current playhead if it's moved enough since the last request.
    fn update_video(&mut self) {
        let Some(video) = &mut self.video else { return };
        let mut unavailable = false;
        while let Ok(event) = video.worker.events.try_recv() {
            match event {
                video::Event::Frame(frame) => video.frame = Some(frame),
                video::Event::Unavailable => {
                    unavailable = true;
                    break;
                }
            }
        }
        if unavailable {
            self.video = None;
            return;
        }

        let Some(pos_secs) = ({
            let deck = self.deck();
            deck.track.as_ref().map(|t| deck.position() / t.sample_rate as f64)
        }) else {
            return;
        };
        let video = self.video.as_mut().expect("checked Some above");
        // Once a frame has arrived, throttle by how far the playhead has
        // moved. Until then, keep asking every tick — e.g. right after
        // load, or while every grab is failing — so repeated failures
        // actually reach the worker's give-up threshold instead of
        // waiting forever on a position that may never move again (a
        // paused track whose first grab failed would otherwise keep
        // `self.video` around showing nothing, forever).
        if video.frame.is_none() || (pos_secs - video.last_requested).abs() > VIDEO_POS_EPSILON {
            video.worker.request(pos_secs);
            video.last_requested = pos_secs;
        }
    }

    /// Taps the tempo. While playing, the taps also place the beats, the last
    /// tap on one; while paused only the BPM changes.
    fn tap(&mut self) {
        let now = Instant::now();
        let pos = {
            let deck = self.deck();
            (deck.is_playing() && !deck.is_reverse()).then(|| deck.position())
        };
        if self.taps.last().is_some_and(|&(t, p)| now - t > Duration::from_secs(2) || p.is_some() != pos.is_some()) {
            self.taps.clear();
        }
        self.taps.push((now, pos));
        if self.taps.len() > 8 {
            self.taps.remove(0);
        }
        if self.taps.len() < 4 {
            self.set_status(format!("Tap… {}", self.taps.len()));
            return;
        }
        let positions: Option<Vec<f64>> = self.taps.iter().map(|&(_, p)| p).collect();
        let bpm = match positions {
            Some(positions) => self.deck().set_tapped_beats(&positions),
            None => {
                let span = (now - self.taps[0].0).as_secs_f64();
                let bpm = 60.0 * (self.taps.len() - 1) as f64 / span;
                self.deck().set_tapped_bpm(bpm);
                Some(bpm)
            }
        };
        match bpm {
            Some(bpm) if pos.is_some() => self.set_status(format!("Tapped {bpm:.1} BPM, beats on the taps")),
            Some(bpm) => self.set_status(format!("Tapped {bpm:.1} BPM")),
            None => self.set_status("Tap again"),
        }
    }

    // --- Keys --------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        let (code, shift) = normalize(&key);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.kind == KeyEventKind::Release {
            self.handle_release(code);
        } else {
            let repeat = key.kind == KeyEventKind::Repeat;
            if ctrl && code == KeyCode::Char('c') {
                self.quit = true;
            } else if self.browser.open {
                self.browser_key(&key);
            } else {
                self.deck_key(code, shift, repeat);
            }
        }
        self.apply_holds();
    }

    fn handle_release(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('c') => self.deck().cue_up(),
            KeyCode::Char(',') => self.bend_down = None,
            KeyCode::Char('.') => self.bend_up = None,
            KeyCode::Left => self.search_back = None,
            KeyCode::Right => self.search_fwd = None,
            KeyCode::Char(c @ '1'..='3') => self.deck().hot_cue_up((c as u8 - b'1') as usize),
            KeyCode::Char(';') if self.roll.is_some() => {
                self.roll = None;
                self.deck().roll_up();
            }
            _ => {}
        }
    }

    /// Takes the raw key: typed characters go into the filter as they are.
    fn browser_key(&mut self, key: &KeyEvent) {
        let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            // Not Ctrl+H: on terminals without the kitty protocol that's
            // indistinguishable from plain Backspace, which is also bound
            // here (delete filter char / go up), so it would silently do
            // the wrong thing instead of adding the folder.
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.add_music_dir(),
            KeyCode::Char(c) if typing => self.browser.type_char(c),
            KeyCode::Up => self.browser.move_by(-1),
            KeyCode::Down => self.browser.move_by(1),
            KeyCode::PageUp => self.browser.move_by(-10),
            KeyCode::PageDown => self.browser.move_by(10),
            KeyCode::Home => self.browser.selected = 0,
            KeyCode::End => self.browser.move_by(isize::MAX / 2),
            KeyCode::Backspace => {
                if !self.browser.backspace() {
                    self.up_or_to_music_dirs();
                }
            }
            KeyCode::Left => self.up_or_to_music_dirs(),
            KeyCode::Enter | KeyCode::Right => {
                if let Some(path) = self.browser.activate() {
                    self.load(path);
                    self.browser.close();
                }
            }
            KeyCode::Esc => {
                if !self.browser.clear_query() {
                    self.browser.close();
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.browser.close(),
            _ => {}
        }
    }

    /// `X`: deletes the memory at the current position — works whether you
    /// got there by playing, scrubbing, or calling the memory itself
    /// (which only moves `cue` while paused; `pos` always moves). A plain
    /// cue goes immediately, as before; a loop needs a second `X` within
    /// `MEMORY_DELETE_CONFIRM` to confirm, since losing a promoted loop is
    /// pricier than losing a bare cue point.
    fn delete_memory_key(&mut self) {
        let candidate = self.deck().memory_at_pos();
        match candidate {
            None => {
                self.pending_memory_delete = None;
                self.set_status("No memory here");
            }
            Some(cue) if cue.loop_out.is_none() => {
                self.pending_memory_delete = None;
                self.deck().delete_memory();
                self.remember();
                self.set_status("Memory deleted");
            }
            Some(_) => {
                let armed = self.pending_memory_delete.is_some_and(|at| at.elapsed() < MEMORY_DELETE_CONFIRM);
                if armed {
                    self.pending_memory_delete = None;
                    self.deck().delete_memory();
                    self.remember();
                    self.set_status("Loop memory deleted");
                } else {
                    self.pending_memory_delete = Some(Instant::now());
                    self.set_status("Press X again to delete this loop memory");
                }
            }
        }
    }

    fn deck_key(&mut self, code: KeyCode, shift: bool, repeat: bool) {
        // Keys that toggle or trigger ignore auto-repeat; steppers accept it.
        let once = !repeat;
        // Anything other than a confirming second `X` drops the arm, so it
        // can't go off later from an unrelated press.
        if code != KeyCode::Char('x') {
            self.pending_memory_delete = None;
        }
        // Over the deck, the help takes the keys that turn and scroll it.
        if self.help.is_open() && self.help.key(code, shift, crate::ui::HELP.len()) {
            return;
        }
        // Settings is otherwise fully modal — everything but Esc is
        // swallowed rather than reaching the deck underneath, see
        // `settings_key` — so its own open/close key is checked first,
        // ahead of that gate: without this, closing the help straight into
        // Settings would be swallowed by the help, same as any other key,
        // and only Esc could back out.
        if once && code == KeyCode::Char('`') {
            self.help.close();
            self.settings_screen.toggle();
            return;
        }
        if self.settings_key(code) {
            return;
        }
        match code {
            KeyCode::Esc => {
                self.help.close();
                self.settings_screen.back();
                if self.deck().is_adjusting_grid() {
                    self.deck().toggle_grid_adjust();
                }
            }
            KeyCode::Char('/') if once => self.help.toggle(),
            KeyCode::Tab if once => {
                self.browser.open = true;
                self.picking_music_dir = false;
            }

            KeyCode::Char(' ') if once => self.deck().play_pause(),
            KeyCode::Char('c') if once => {
                let can_hold = self.key_release;
                self.deck().cue_down(can_hold);
            }
            KeyCode::Char(c @ '1'..='3') if once => {
                let i = (c as u8 - b'1') as usize;
                let letter = (b'A' + i as u8) as char;
                if !self.hot_rec {
                    if shift {
                        self.set_status("Hot cues can only be cleared in REC mode (E)");
                    } else if !self.deck().hot_cue(i, self.key_release) {
                        self.set_status(format!("Hot cue {letter} is empty, press E for REC mode to store it"));
                    }
                } else if shift {
                    self.deck().clear_hot_cue(i);
                    self.remember();
                    self.set_status(format!("Hot cue {letter} cleared"));
                } else {
                    self.deck().set_hot_cue(i);
                    self.remember();
                    self.set_status(format!("Hot cue {letter} stored"));
                }
            }
            KeyCode::Char('e') if once => {
                self.hot_rec = !self.hot_rec;
                self.set_status(if self.hot_rec { "Hot cue REC mode" } else { "Hot cue PLAY mode" });
            }

            KeyCode::Char('w') if once => {
                let stored = self.deck().store_memory();
                if stored.is_some() {
                    self.remember();
                }
                match stored {
                    Some(n) => self.set_status(format!("Memory {n} stored")),
                    None => self.set_status("Already in memory"),
                }
            }
            KeyCode::Char(c @ ('j' | 'k')) if once => {
                let which = if c == 'j' { MemoryCall::Left } else { MemoryCall::Right };
                let called = self.deck().call_memory(which);
                match called {
                    Some(n) => self.set_status(format!("Memory {n} called")),
                    None if which == MemoryCall::Left => self.set_status("No memory point before the playhead"),
                    None => self.set_status("No memory point after the playhead"),
                }
            }
            KeyCode::Char('x') if once => self.delete_memory_key(),
            KeyCode::Char('i') if once => self.deck().loop_in(),
            KeyCode::Char('o') if once => {
                let adjusting = {
                    let mut deck = self.deck();
                    deck.loop_out();
                    deck.is_adjusting_loop()
                };
                if adjusting {
                    self.set_status("Loop out adjust: , / . move the out point (Shift: ×10), O to finish");
                }
            }
            KeyCode::Char('p') if once && shift => {
                if !self.deck().reloop_draft() {
                    self.set_status("No draft loop since the last track (I/O or L)");
                }
            }
            KeyCode::Char('p') if once => self.deck().reloop_exit(),
            KeyCode::Char('l') if once && shift => {
                let (stored, had_draft) = {
                    let mut deck = self.deck();
                    let had_draft = deck.snapshot().draft_loop.is_some();
                    (deck.promote_draft_loop(), had_draft)
                };
                if stored.is_some() {
                    self.remember();
                }
                match stored {
                    Some(n) => self.set_status(format!("Memory {n} stored")),
                    None if had_draft => self.set_status("Already in memory"),
                    None => self.set_status("No draft loop to promote"),
                }
            }
            KeyCode::Char('l') if once => {
                if !self.deck().beat_loop() {
                    self.set_status("Beat loops need a beat grid (tap the BPM with A)");
                }
            }
            KeyCode::Char(c @ ('[' | ']')) => {
                self.deck().resize_loop(if c == '[' { 0.5 } else { 2.0 }, shift);
                self.save_sizes();
            }
            KeyCode::Char(c @ ('-' | '=')) if shift => {
                self.deck().resize_jump(c == '=');
                self.save_sizes();
            }
            KeyCode::Char(c @ ('-' | '=')) => {
                if !self.deck().beat_jump(if c == '-' { -1.0 } else { 1.0 }) {
                    self.set_status("Beatjump needs a beat grid (tap the BPM with A)");
                }
            }
            KeyCode::Char(';') if once || !self.key_release => {
                let held = self.roll.is_some();
                self.roll = self.hold(false);
                if !held {
                    let (ok, size) = {
                        let mut deck = self.deck();
                        let size = deck.loop_size;
                        (deck.roll_down(size), size)
                    };
                    if !ok {
                        self.roll = None;
                        self.set_status("Loop roll needs a beat grid (tap the BPM with A)");
                    } else if size > 1.0 {
                        self.set_status("Rolling; [ / ] change the size while held");
                    }
                }
            }
            KeyCode::Char('z') if once => {
                let on = self.deck().toggle_slip();
                self.set_status(if on { "Slip on" } else { "Slip off" });
            }

            KeyCode::Up | KeyCode::Down if self.deck().is_adjusting_grid() => {
                let sign = if code == KeyCode::Up { 1.0 } else { -1.0 };
                self.deck().nudge_grid_bpm(sign * if shift { 0.1 } else { 0.01 });
            }
            KeyCode::Up | KeyCode::Down => {
                let sign = if code == KeyCode::Up { 1.0 } else { -1.0 };
                let moved = self.deck().move_tempo(sign * if shift { 10.0 } else { 1.0 });
                if !moved {
                    self.synced_tempo();
                }
            }
            KeyCode::Char('0') if once => {
                let reset = self.deck().reset_tempo();
                if !reset {
                    self.synced_tempo();
                }
            }
            KeyCode::Char('f') if once && shift => {
                let claimed = self.deck().claim_master(link::now());
                if claimed {
                    self.set_status("Master: synced decks follow this one");
                } else {
                    self.set_status("The master needs a beat grid (tap the BPM with A)");
                }
            }
            KeyCode::Char('f') if once => {
                let on = self.deck().toggle_sync();
                match on {
                    Some(true) => self.set_status("Sync on"),
                    Some(false) => self.set_status("Sync off, the tempo stays"),
                    None => self.set_status("Sync needs a beat grid (tap the BPM with A)"),
                }
            }
            KeyCode::Char('g') if once => self.deck().cycle_range(),
            KeyCode::Char('h') if once => {
                let (on, latency) = {
                    let mut deck = self.deck();
                    (deck.toggle_master_tempo(), deck.stretch_latency_ms())
                };
                match (on, latency) {
                    (true, Some(ms)) => self.set_status(format!("Master Tempo on (+{ms:.0} ms)")),
                    (false, None) => self.set_status("Master Tempo unavailable (no librubberband)"),
                    _ => self.set_status("Master Tempo off"),
                }
            }

            KeyCode::Char(c @ (',' | '.')) => {
                let sign = if c == ',' { -1.0 } else { 1.0 };
                let (playing, adjusting, adjusting_grid) = {
                    let deck = self.deck();
                    (deck.is_playing(), deck.is_adjusting_loop(), deck.is_adjusting_grid())
                };
                if adjusting_grid {
                    self.deck().shift_grid(sign * if shift { 10.0 } else { 1.0 });
                } else if adjusting {
                    self.deck().adjust_loop_out(sign * if shift { 10.0 } else { 1.0 });
                } else if !playing {
                    let mut deck = self.deck();
                    if shift {
                        deck.step_beats_paused(sign as i64);
                    } else {
                        deck.step_paused(sign);
                    }
                } else if once || !self.key_release {
                    let hold = self.hold(shift);
                    if c == ',' { self.bend_down = hold } else { self.bend_up = hold }
                }
            }
            KeyCode::Left if once || !self.key_release => self.search_back = self.hold(shift),
            KeyCode::Right if once || !self.key_release => self.search_fwd = self.hold(shift),
            KeyCode::Char('b') if once => self.skip_track(-1),
            KeyCode::Char('n') if once => self.skip_track(1),

            KeyCode::Char('r') if once => self.deck().toggle_reverse(),
            KeyCode::Char('t') if once && shift => self.toggle_auto_cue(),
            KeyCode::Char('t') if once => self.show_remaining = !self.show_remaining,
            KeyCode::Char('a') if once && shift => {
                self.deck().reset_grid();
                self.taps.clear();
                self.set_status("Beat grid reset to detected");
            }
            KeyCode::Char('a') if once => self.tap(),
            KeyCode::Char('q') if once && self.deck().is_synced() => {
                self.set_status("Quantize stays on while synced, so the deck stays in phase");
            }
            KeyCode::Char('q') if once => {
                let on = self.deck().toggle_quantize();
                match on {
                    Some(true) => self.set_status("Quantize on for this track"),
                    Some(false) => self.set_status("Quantize off for this track"),
                    None => self.set_status("Quantize needs a beat grid (tap the BPM with A)"),
                }
            }
            KeyCode::Char('d') if once => {
                if self.deck().set_downbeat() {
                    self.set_status("Beat 1 of a bar is here now");
                } else {
                    self.set_status("No beat grid (tap the BPM with A)");
                }
            }
            KeyCode::Char('y') if once => {
                let on = {
                    let mut deck = self.deck();
                    deck.toggle_grid_adjust().then(|| deck.is_adjusting_grid())
                };
                match on {
                    Some(true) => self.set_status("Grid adjust: , / . shift the beats 1 ms (Shift: 10 ms), ↑/↓ BPM ±0.01 (Shift: ±0.1), Y to finish"),
                    Some(false) => self.set_status("Grid adjust done"),
                    None => self.set_status("No beat grid (tap the BPM with A)"),
                }
            }
            KeyCode::Char('v') if once => {
                let mut deck = self.deck();
                deck.brake_time = next_preset(BRAKE_TIMES, deck.brake_time);
            }
            KeyCode::Char('s') if once => {
                let mut deck = self.deck();
                deck.start_time = next_preset(START_TIMES, deck.start_time);
            }
            _ => {}
        }
    }
}

impl App {
    fn synced_tempo(&mut self) {
        let leader = self.deck().follows();
        if let Some(n) = leader {
            self.set_status(format!("Synced to DECK {n}, F to unsync"));
        }
    }
}

fn next_preset(presets: &[f64], current: f64) -> f64 {
    let i = presets.iter().position(|&p| p == current).unwrap_or(0);
    presets[(i + 1) % presets.len()]
}

/// Maps shifted symbols back to their base key (US layout) so a key's
/// press and release match whether or not the terminal applied Shift.
fn normalize(key: &KeyEvent) -> (KeyCode, bool) {
    let mut shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let code = match key.code {
        KeyCode::Char(c) => {
            let base = match c {
                '<' => ',',
                '>' => '.',
                '!' => '1',
                '@' => '2',
                '#' => '3',
                '{' => '[',
                '}' => ']',
                '?' => '/',
                ')' => '0',
                '_' => '-',
                '+' => '=',
                ':' => ';',
                c => c.to_ascii_lowercase(),
            };
            shift |= base != c;
            KeyCode::Char(base)
        }
        KeyCode::BackTab => KeyCode::Tab,
        other => other,
    };
    (code, shift)
}

#[cfg(test)]
mod tests {
    use super::*;
    use odj_core::track::test_util::click_track;
    use ratatui::crossterm::event::KeyEventState;

    fn app() -> App {
        let dir = std::env::temp_dir().join(format!("odj-test-{}", std::process::id()));
        // Keep tests away from the real cue memory.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let track = Track::from_samples("t.wav".into(), "t".into(), None, 48_000, click_track(120.0, 10.0, 48_000));
        let deck = Arc::new(Mutex::new(Deck::new(48_000)));
        let mem = TrackMemory { quantize: Some(false), ..TrackMemory::default() };
        deck.lock().unwrap().load(Arc::new(track), &mem, false);
        App::new(deck, Browser::new(dir), true, String::new(), Memory::load())
    }

    fn key(app: &mut App, code: KeyCode, shift: bool, kind: KeyEventKind) {
        let mods = if shift { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
        app.handle_key(KeyEvent { code, modifiers: mods, kind, state: KeyEventState::NONE });
    }

    fn press(app: &mut App, code: KeyCode, shift: bool) {
        key(app, code, shift, KeyEventKind::Press);
    }

    #[test]
    fn hot_cues_store_only_in_rec_mode() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'), false);
        assert!(app.deck().snapshot().hot[0].is_none());
        press(&mut app, KeyCode::Char('e'), false);
        press(&mut app, KeyCode::Char('1'), false);
        assert!(app.deck().snapshot().hot[0].is_some());
        press(&mut app, KeyCode::Char('e'), false);
        press(&mut app, KeyCode::Char('!'), false); // Shift+1 in PLAY mode: protected
        assert!(app.deck().snapshot().hot[0].is_some());
        press(&mut app, KeyCode::Char('e'), false);
        press(&mut app, KeyCode::Char('1'), true);
        assert!(app.deck().snapshot().hot[0].is_none());
    }

    #[test]
    fn loading_works_while_playing() {
        let mut app = app();
        press(&mut app, KeyCode::Char(' '), false);
        app.load(PathBuf::from("other.mp3"));
        assert!(app.loading.is_some());
    }

    #[test]
    fn shift_l_promotes_the_last_loop_even_after_leaving_it() {
        let mut app = app();
        press(&mut app, KeyCode::Char('i'), false);
        app.deck().step_paused(75.0); // 1 s
        press(&mut app, KeyCode::Char('o'), false);
        press(&mut app, KeyCode::Char('p'), false); // reloop_exit: leave it
        assert!(!app.deck().snapshot().looping);

        press(&mut app, KeyCode::Char('l'), true);
        let memories = app.deck().snapshot().memories;
        assert_eq!(memories.len(), 1);
        assert!(memories[0].loop_out.is_some());

        press(&mut app, KeyCode::Char('l'), true); // same loop again: duplicate
        assert_eq!(app.deck().snapshot().memories.len(), 1);
    }

    #[test]
    fn shift_l_without_a_loop_does_nothing() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l'), true);
        assert!(app.deck().snapshot().memories.is_empty());
    }

    #[test]
    fn shift_p_jumps_back_to_the_draft_loop() {
        let mut app = app();
        press(&mut app, KeyCode::Char('i'), false);
        app.deck().step_paused(75.0); // 1 s
        press(&mut app, KeyCode::Char('o'), false); // the draft loop: 0..1s
        press(&mut app, KeyCode::Char('p'), false); // leave it
        app.deck().step_paused(150.0); // wander off elsewhere
        assert!(!app.deck().snapshot().looping);

        press(&mut app, KeyCode::Char('p'), true); // Shift+P
        let s = app.deck().snapshot();
        assert_eq!((s.loop_in, s.loop_out), (Some(0.0), Some(48_000.0)));
        assert_eq!(app.deck().position(), 0.0);
    }

    #[test]
    fn shift_p_without_a_draft_loop_shows_a_status() {
        let mut app = app();
        press(&mut app, KeyCode::Char('p'), true);
        assert!(app.status().unwrap().contains("No draft loop"));
    }

    #[test]
    fn shift_search_is_super_fast() {
        let mut app = app();
        press(&mut app, KeyCode::Right, false);
        assert_eq!(app.deck().search, SEARCH);
        key(&mut app, KeyCode::Right, false, KeyEventKind::Release);
        assert_eq!(app.deck().search, 0.0);
        press(&mut app, KeyCode::Left, true);
        assert_eq!(app.deck().search, -SUPER_SEARCH);
        key(&mut app, KeyCode::Left, true, KeyEventKind::Release);
        assert_eq!(app.deck().search, 0.0);
    }

    #[test]
    fn shift_bracket_resizes_the_loop_from_the_left() {
        let mut app = app();
        app.deck().step_paused(150.0); // 2 s in, so there's room to extend left
        press(&mut app, KeyCode::Char('i'), false);
        app.deck().step_paused(75.0); // +1 s
        press(&mut app, KeyCode::Char('o'), false);
        let (start, end) = {
            let s = app.deck().snapshot();
            (s.loop_in.unwrap(), s.loop_out.unwrap())
        };

        press(&mut app, KeyCode::Char(']'), true); // Shift+]: double from the left
        let s = app.deck().snapshot();
        assert_eq!(s.loop_out, Some(end), "the out point doesn't move");
        assert_eq!(s.loop_in, Some(start - (end - start)));
    }

    #[test]
    fn w_saves_to_disk_immediately_not_just_on_track_switch_or_quit() {
        let dir = std::env::temp_dir().join(format!("odj-save-on-change-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let id = "deadbeef00000000000000000000beef".to_string();
        let mut loaded = Track::from_samples("t.wav".into(), "t".into(), None, 48_000, click_track(120.0, 10.0, 48_000));
        loaded.id = id.clone();
        let deck = Arc::new(Mutex::new(Deck::new(48_000)));
        let mem = TrackMemory { quantize: Some(false), ..TrackMemory::default() };
        deck.lock().unwrap().load(Arc::new(loaded), &mem, false);
        let mut app = App::new(deck, Browser::new(dir), true, String::new(), Memory::load());

        press(&mut app, KeyCode::Char('w'), false); // store a memory

        // Read back with a *separate* `Memory` pointed at the same files —
        // not this App's own in-process state — to prove it actually hit
        // disk rather than just sitting in RAM until a later checkpoint.
        let mut probe = Track::from_samples("t.wav".into(), "t".into(), None, 48_000, vec![0.0; 2]);
        probe.id = id;
        let on_disk = Memory::load().get(&probe);
        assert_eq!(on_disk.memories.len(), 1, "W should save immediately, not wait for a track switch or quit");
    }

    #[test]
    fn x_deletes_a_plain_cue_immediately_no_confirmation_needed() {
        let mut app = app();
        app.deck().store_memory(); // a plain cue at pos 0
        press(&mut app, KeyCode::Char('x'), false);
        assert!(app.deck().snapshot().memories.is_empty());
        assert!(app.pending_memory_delete.is_none());
    }

    #[test]
    fn x_arms_a_loop_memory_and_a_second_x_confirms_it() {
        let mut app = app();
        press(&mut app, KeyCode::Char('i'), false); // also sets the cue here
        app.deck().step_paused(75.0); // 1 s
        press(&mut app, KeyCode::Char('o'), false);
        app.deck().promote_draft_loop();
        assert_eq!(app.deck().snapshot().memories.len(), 1);

        press(&mut app, KeyCode::Char('x'), false); // first X: arms, doesn't delete
        assert_eq!(app.deck().snapshot().memories.len(), 1, "not deleted yet");
        assert!(app.pending_memory_delete.is_some());

        press(&mut app, KeyCode::Char('x'), false); // second X: confirms
        assert!(app.deck().snapshot().memories.is_empty());
    }

    #[test]
    fn any_other_key_cancels_a_pending_loop_delete() {
        let mut app = app();
        press(&mut app, KeyCode::Char('i'), false);
        app.deck().step_paused(75.0);
        press(&mut app, KeyCode::Char('o'), false);
        app.deck().promote_draft_loop();

        press(&mut app, KeyCode::Char('x'), false); // arm
        press(&mut app, KeyCode::Char(' '), false); // unrelated key: cancels the arm
        assert!(app.pending_memory_delete.is_none());

        press(&mut app, KeyCode::Char('x'), false); // only re-arms, doesn't confirm
        assert_eq!(app.deck().snapshot().memories.len(), 1, "still there — the earlier arm was cancelled");
    }

    #[test]
    fn x_deletes_a_memory_you_just_jumped_to_while_playing() {
        let mut app = app();
        app.deck().store_memory(); // a memory at pos 0
        app.deck().step_paused(150.0); // 2 s in, still paused
        press(&mut app, KeyCode::Char('c'), false); // sets a new cue here, away from the memory
        press(&mut app, KeyCode::Char(' '), false); // start playing

        press(&mut app, KeyCode::Char('j'), false); // J: jump back to it, still playing
        let s = app.deck().snapshot();
        assert!((s.pos - 0.0).abs() < 1.0, "jumped to the memory");
        assert!((s.cue - 2.0 * 48_000.0).abs() < 1.0, "cue stayed put — it doesn't follow a call while playing");

        press(&mut app, KeyCode::Char('x'), false);
        assert!(app.deck().snapshot().memories.is_empty(), "X finds it via pos even though cue never moved");
    }

    #[test]
    fn jog_keys_adjust_loop_out_in_adjust_mode() {
        let mut app = app();
        press(&mut app, KeyCode::Char('i'), false);
        app.deck().step_paused(75.0);
        press(&mut app, KeyCode::Char('o'), false);
        press(&mut app, KeyCode::Char('o'), false);
        assert!(app.deck().is_adjusting_loop());
        press(&mut app, KeyCode::Char('.'), true);
        assert_eq!(app.deck().snapshot().loop_out, Some(48_000.0 + 6_400.0));
    }

    #[test]
    fn grid_keys() {
        let mut app = app();
        press(&mut app, KeyCode::Char('q'), false);
        assert!(app.deck().quantize());
        app.deck().step_paused(10.0);
        press(&mut app, KeyCode::Char('d'), false);
        let g = app.deck().grid().unwrap();
        assert_eq!(g.anchor, 6_400.0);

        press(&mut app, KeyCode::Char('y'), false);
        press(&mut app, KeyCode::Char('.'), true);
        press(&mut app, KeyCode::Up, false);
        let adjusted = app.deck().grid().unwrap();
        assert_eq!(adjusted.bpm, g.bpm + 0.01);
        assert_eq!(app.deck().snapshot().tempo, 0.0, "the tempo fader stays");
        press(&mut app, KeyCode::Esc, false);
        assert!(!app.deck().is_adjusting_grid());

        press(&mut app, KeyCode::Char('a'), true);
        assert_eq!(app.deck().memory().grid, None);
    }

    #[test]
    fn help_opens_on_the_basics_and_turns_to_the_other_pages() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        let screen = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(160, 50)).unwrap();
            let snapshot = app.deck().snapshot();
            t.draw(|f| crate::ui::draw(f, app, &snapshot)).unwrap();
            t.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>()
        };
        press(&mut app, KeyCode::Char('?'), true);
        assert!(app.help.is_open());
        let text = screen(&app);
        assert!(text.contains(" Esc  Close ") && text.contains(" Tab  Next page "));
        assert!(text.contains(" Basics ") && text.contains(" Beat grid ") && text.contains(" About "));
        assert!(!text.contains("Make the playhead beat 1"), "the grid keys are on their own page");

        press(&mut app, KeyCode::Tab, false);
        assert_eq!(app.help.page(), 1);
        assert!(!app.browser.open, "Tab turns the page instead of browsing");
        press(&mut app, KeyCode::Tab, true);
        assert_eq!(app.help.page(), 0, "Shift+Tab goes back");
        press(&mut app, KeyCode::Left, false);
        assert_eq!(app.help.page(), crate::ui::HELP.len() - 1);
        assert!(screen(&app).contains("odj@ibz.me"), "the last page is About");

        press(&mut app, KeyCode::Char('?'), true);
        assert!(!app.help.is_open());
        press(&mut app, KeyCode::Char('?'), true);
        assert_eq!(app.help.page(), 0, "opens on the basics again");
        press(&mut app, KeyCode::Esc, false);
        assert!(!app.help.is_open());
    }

    /// The README's Keys section mirrors the help: the same pages, each with
    /// the same keys.
    #[test]
    fn the_readme_has_every_help_key() {
        use odj_core::tui::Page;
        let readme = include_str!("../../../README.md");
        let start = readme.find("### odj-player\n").expect("a section for odj-player");
        let section = &readme[start..];
        let section = &section[..section[4..].find("\n### ").map_or(section.len(), |i| i + 4)];
        for page in crate::ui::HELP {
            let Page::Keys(title, keys) = page else { continue };
            let page = section.split("#### ").find(|p| p.starts_with(&format!("{title}\n"))).expect(title);
            for (k, v) in *keys {
                // A key that's itself a backtick needs the longer-delimiter
                // markdown escape (`` ` ``), not a plain `k` code span.
                let md_key = if k.contains('`') { format!("`` {k} ``") } else { format!("`{k}`") };
                assert!(page.contains(&format!("| {md_key} | {v} |")), "{title}: {k}");
            }
        }
    }

    /// Every key of the old single page is on one of the pages.
    #[test]
    fn the_help_pages_have_every_key() {
        use odj_core::tui::Page;
        let keys: Vec<&str> = crate::ui::HELP
            .iter()
            .flat_map(|p| match p {
                Page::Keys(_, keys) => keys.iter().map(|(k, _)| *k).collect(),
                Page::About(_) => Vec::new(),
            })
            .collect();
        for k in [
            "Space", "C", "Space while holding C", "1 2 3", "E", "Shift+1 2 3", "I / O / P", "O while looping", "L",
            "- / =", "Shift+- / =", "; (hold)", "Z", "W / J / K / X", "[ / ]", "↑ / ↓", "0 / G",
            ", / .", "Q", "F", "Shift+F", "A", "Shift+A", "D", "Y", "← / →", "B / N", "R", "T", "Shift+T", "S / V",
            "Tab",
        ] {
            assert!(keys.contains(&k), "{k}");
        }
    }

    /// Draws the deck with the grid showing; shown with --nocapture.
    #[test]
    fn draws_the_grid() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        press(&mut app, KeyCode::Char('q'), false);
        press(&mut app, KeyCode::Char('e'), false);
        app.deck().step_paused(200.0);
        press(&mut app, KeyCode::Char('1'), false);
        press(&mut app, KeyCode::Char('e'), false);
        app.deck().step_paused(-100.0);
        press(&mut app, KeyCode::Char('l'), false);
        press(&mut app, KeyCode::Char('y'), false);
        press(&mut app, KeyCode::Char(' '), false);
        app.deck().render(&mut [0.0; 2 * 9_000]);
        press(&mut app, KeyCode::Char('1'), false);
        assert_eq!(app.deck().snapshot().pending_hot, Some(0));
        press(&mut app, KeyCode::Char('z'), false);
        app.deck().loop_size = 0.25;
        press(&mut app, KeyCode::Char(';'), false);
        app.deck().render(&mut [0.0; 2 * 30_000]);
        for (w, h) in [(160, 24), (60, 12)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            let snapshot = app.deck().snapshot();
            t.draw(|f| crate::ui::draw(f, &app, &snapshot)).unwrap();
            if w > 100 {
                println!("{}", t.backend());
                let cells = &t.backend().buffer().content;
                assert!(cells.iter().any(|c| c.bg == crate::ui::BAR_BG), "bar lines are drawn");
                assert!(cells.iter().any(|c| c.bg == crate::ui::GHOST_BG), "the ghost is drawn");
                let text: String = cells.iter().map(|c| c.symbol()).collect();
                for label in ["ROLL 1/4", "SLIP", "JUMP 4", "┃"] {
                    assert!(text.contains(label), "{label}");
                }
            }
        }
    }

    #[test]
    fn jump_roll_and_slip_keys() {
        let mut app = app();
        // The tests share a settings file; start from the defaults.
        (app.deck().jump_beats, app.deck().loop_size) = (4.0, 4.0);
        press(&mut app, KeyCode::Char('+'), true);
        assert_eq!(app.deck().jump_beats, 8.0);
        assert_eq!(app.memory.settings.jump_beats, 8.0, "kept for next time");
        press(&mut app, KeyCode::Char('_'), true);
        press(&mut app, KeyCode::Char('='), false);
        assert_eq!(app.deck().position(), 4.0 * 24_000.0);

        press(&mut app, KeyCode::Char(' '), false);
        press(&mut app, KeyCode::Char(';'), false);
        assert_eq!(app.deck().snapshot().roll, Some(4.0));
        press(&mut app, KeyCode::Char('['), false);
        assert_eq!(app.deck().snapshot().roll, Some(2.0), "[ halves the roll while held");
        key(&mut app, KeyCode::Char(';'), false, KeyEventKind::Release);
        assert_eq!(app.deck().snapshot().roll, None);
        assert_eq!(app.memory.settings.loop_beats, 2.0);

        press(&mut app, KeyCode::Char('z'), false);
        assert!(app.deck().snapshot().slip);
    }

    #[test]
    fn rolls_time_out_without_key_releases() {
        let mut app = app();
        app.key_release = false;
        press(&mut app, KeyCode::Char(' '), false);
        app.deck().loop_size = 4.0;
        press(&mut app, KeyCode::Char(';'), false);
        assert_eq!(app.deck().snapshot().roll, Some(4.0));
        // Auto-repeat keeps it going.
        key(&mut app, KeyCode::Char(';'), false, KeyEventKind::Repeat);
        assert!(app.deck().snapshot().roll.is_some());
        app.roll.as_mut().unwrap().until = Some(Instant::now());
        app.tick();
        assert_eq!(app.deck().snapshot().roll, None);
    }

    fn press_ctrl(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent { code, modifiers: KeyModifiers::CONTROL, kind: KeyEventKind::Press, state: KeyEventState::NONE });
    }

    #[test]
    fn h_opens_settings_and_esc_closes_it() {
        let mut app = app();
        press(&mut app, KeyCode::Char('`'), false);
        assert!(app.settings_screen.is_open());
        press(&mut app, KeyCode::Esc, false);
        assert!(!app.settings_screen.is_open());
    }

    #[test]
    fn h_toggles_master_tempo() {
        let mut app = app();
        press(&mut app, KeyCode::Char('h'), false);
        assert!(app.deck().snapshot().master_tempo, "H now does what M used to");
    }

    #[test]
    fn open_to_music_dirs_skips_straight_to_the_list() {
        let mut app = app();
        app.settings_screen.open_to_music_dirs();
        assert!(app.settings_screen.is_open());
        assert!(app.settings_screen.editable());
        assert_eq!(app.settings_screen.folder(), Some(0));

        // Enter, X and Ctrl+U all behave exactly as in the normal editable list.
        app.memory.settings.music_dirs.clear();
        let dir = app.browser.dir.clone();
        press(&mut app, KeyCode::Enter, false);
        press_ctrl(&mut app, KeyCode::Char('u'));
        assert_eq!(app.music_dirs(), [dir]);
        assert!(!app.settings_screen.is_open(), "picking a folder concludes setup");
    }

    #[test]
    fn open_to_switch_picks_among_folders_without_editing_them() {
        let mut app = app();
        let dirs = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        app.memory.settings.music_dirs = dirs.clone();
        app.settings_screen.open_to_switch(1);
        assert!(app.settings_screen.is_open());
        assert!(!app.settings_screen.editable(), "a quick pick, not the real settings screen");
        assert!(!app.settings_screen.onboarding());
        assert_eq!(app.settings_screen.folder(), Some(1));

        // X does nothing here — it's not editable — only Enter jumps.
        press(&mut app, KeyCode::Char('x'), false);
        assert_eq!(app.music_dirs(), dirs, "nothing removed");
        press(&mut app, KeyCode::Enter, false);
        assert!(!app.settings_screen.is_open());
        assert!(app.browser.open);
    }

    #[test]
    fn opening_settings_closes_help() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'), false);
        assert!(app.help.is_open());
        press(&mut app, KeyCode::Char('`'), false);
        assert!(app.settings_screen.is_open());
        assert!(!app.help.is_open());
    }

    #[test]
    fn settings_is_modal_and_toggles_auto_cue() {
        let mut app = app();
        let was_playing = app.deck().snapshot().playing;
        press(&mut app, KeyCode::Char('`'), false);
        // Space is swallowed while settings is open, not sent to the deck.
        press(&mut app, KeyCode::Char(' '), false);
        assert_eq!(app.deck().snapshot().playing, was_playing);

        let auto_cue = app.auto_cue();
        press(&mut app, KeyCode::Enter, false); // row 0: auto-cue
        assert_eq!(app.auto_cue(), !auto_cue);
    }

    #[test]
    fn browser_hints_are_in_three_groups() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        // The tests share a settings file; this is the normal (not
        // onboarding) layout, so it needs a music folder already configured
        // — just not this one, or the Ctrl+U badge this test locates the
        // hints row by would be hidden as "already added".
        app.memory.settings.music_dirs = vec![app.browser.dir.join("elsewhere")];
        press(&mut app, KeyCode::Tab, false); // open the browser

        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let snapshot = app.deck().snapshot();
        t.draw(|f| crate::ui::draw(f, &app, &snapshot)).unwrap();
        let buf = t.backend().buffer();

        // The hints row is the one just above the box's bottom border, which
        // carries the Ctrl+U badge.
        let hints_y = (0..buf.area.height).find(|&y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("Ctrl+U")
        });
        let hints_y = hints_y.expect("the hints row is on screen") - 1;
        let row: String = (0..buf.area.width).map(|x| buf[(x, hints_y)].symbol()).collect();

        // The box is narrower than the terminal, so the row also carries
        // whatever's behind it on either side — check group order, not
        // exact position.
        let enter_at = row.find("Enter/").expect("Enter/\u{2192} open is shown");
        let esc_at = row.find("Esc").expect("Esc is shown");
        let filter_at = row.find("type to filter").expect("the filter hint is shown");
        let nav_at = row.find("navigate").expect("the navigate hint is shown");
        assert!(enter_at < esc_at, "Enter is left of Esc: {row:?}");
        assert!(esc_at < filter_at, "Esc is left of the filter hint: {row:?}");
        assert!(filter_at < nav_at, "the filter hint is left of navigate: {row:?}");
        assert!(!row.contains("Tab"), "Tab isn't worth a mention: {row:?}");
    }

    #[test]
    fn onboarding_browser_hints_drop_enter_and_fold_into_navigate() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        // The tests share a settings file; onboarding is specifically the
        // no-music-folders-at-all case.
        app.memory.settings.music_dirs.clear();
        press(&mut app, KeyCode::Tab, false); // open the browser

        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let snapshot = app.deck().snapshot();
        t.draw(|f| crate::ui::draw(f, &app, &snapshot)).unwrap();
        let buf = t.backend().buffer();

        // Locate the hints row the same way: just above the Ctrl+U badge.
        let hints_y = (0..buf.area.height)
            .find(|&y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().contains("Ctrl+U"))
            .expect("the hints row is on screen")
            - 1;
        let row: String = (0..buf.area.width).map(|x| buf[(x, hints_y)].symbol()).collect();

        assert!(!row.contains("Enter"), "Enter/open isn't shown during onboarding: {row:?}");
        assert!(row.contains('→'), "the right arrow moved into navigate: {row:?}");
        let esc_at = row.find("Esc").expect("Esc is shown");
        let filter_at = row.find("type to filter").expect("the filter hint is shown");
        let nav_at = row.find("navigate").expect("the navigate hint is shown");
        assert!(esc_at < filter_at, "Esc is left of the filter hint: {row:?}");
        assert!(filter_at < nav_at, "the filter hint is left of navigate: {row:?}");
    }

    #[test]
    fn ctrl_h_adds_to_music_folders_and_settings_lists_them() {
        let mut app = app();
        // The tests share a settings file; start from empty.
        app.memory.settings.music_dirs.clear();
        let dir = app.browser.dir.clone();
        press(&mut app, KeyCode::Tab, false); // open the browser
        press_ctrl(&mut app, KeyCode::Char('u'));
        assert_eq!(app.music_dirs(), [dir.clone()]);
        assert!(app.status().is_some_and(|s| s.contains("Using")), "the first folder gets its own message");

        // Adding the same folder again is a no-op.
        press_ctrl(&mut app, KeyCode::Char('u'));
        assert_eq!(app.music_dirs().len(), 1);

        press(&mut app, KeyCode::Tab, false); // close the browser
        press(&mut app, KeyCode::Char('`'), false); // open settings
        press(&mut app, KeyCode::Down, false); // row 1: music folders
        press(&mut app, KeyCode::Enter, false); // drill in
        assert_eq!(app.settings_screen.folder(), Some(0));

        press(&mut app, KeyCode::Char('x'), false); // remove it
        assert!(app.music_dirs().is_empty());
        // Emptied by hand in a normal session (not the forced first-run
        // flow), so Esc still works — same two-level back-then-close as ever.
        press(&mut app, KeyCode::Esc, false);
        assert!(app.settings_screen.is_open());
        assert_eq!(app.settings_screen.folder(), None);
        press(&mut app, KeyCode::Esc, false);
        assert!(!app.settings_screen.is_open());
    }

    #[test]
    fn footer_highlights_tab_when_theres_no_track() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let dir = std::env::temp_dir().join(format!("odj-test-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }

        // Wide enough that the full key list isn't clipped before "Space play".
        let render = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(200, 40)).unwrap();
            let snapshot = app.deck().snapshot();
            t.draw(|f| crate::ui::draw(f, app, &snapshot)).unwrap();
            t.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>()
        };

        let empty = App::new(Arc::new(Mutex::new(Deck::new(48_000))), Browser::new(dir), true, String::new(), Memory::load());
        let text = render(&empty);
        assert!(text.contains("Tab") && text.contains("browse"), "Tab is explained");
        assert!(!text.contains("Space play"), "deck keys are hidden: there's nothing to play");

        let loaded = app();
        let text = render(&loaded);
        assert!(text.contains("Space play"), "deck keys are back once a track is loaded");
    }

    #[test]
    fn footer_globals_stay_visible_and_pinned_right_even_when_narrow() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let app = app(); // a track is loaded
        // Narrow enough that the old single-string fallback would have
        // dropped `` entirely, keeping only "? help  Ctrl+C quit".
        let mut t = Terminal::new(TestBackend::new(80, 40)).unwrap();
        let snapshot = app.deck().snapshot();
        t.draw(|f| crate::ui::draw(f, &app, &snapshot)).unwrap();
        let buf = t.backend().buffer();
        let footer_row: String = (0..buf.area.width).map(|x| buf[(x, buf.area.height - 1)].symbol()).collect();

        assert!(footer_row.contains('`'), "settings is shown even when narrow: {footer_row:?}");
        assert!(footer_row.trim_end().ends_with("Ctrl+C quit"), "globals are pinned right: {footer_row:?}");
    }

    #[test]
    fn footer_pins_tab_left_and_globals_right_when_no_track() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let dir = std::env::temp_dir().join(format!("odj-test-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let empty = App::new(Arc::new(Mutex::new(Deck::new(48_000))), Browser::new(dir), true, String::new(), Memory::load());

        let mut t = Terminal::new(TestBackend::new(200, 40)).unwrap();
        let snapshot = empty.deck().snapshot();
        t.draw(|f| crate::ui::draw(f, &empty, &snapshot)).unwrap();
        let buf = t.backend().buffer();
        let footer_row: String = (0..buf.area.width).map(|x| buf[(x, buf.area.height - 1)].symbol()).collect();

        assert!(footer_row.trim_start().starts_with("Tab"), "Tab is pinned to the left: {footer_row:?}");
        assert!(footer_row.trim_end().ends_with("Ctrl+C quit"), "globals are pinned to the right: {footer_row:?}");
    }

    #[test]
    fn no_track_message_renders_as_big_pixel_text() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let dir = std::env::temp_dir().join(format!("odj-test-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let empty = App::new(Arc::new(Mutex::new(Deck::new(48_000))), Browser::new(dir), true, String::new(), Memory::load());

        let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
        let snapshot = empty.deck().snapshot();
        t.draw(|f| crate::ui::draw(f, &empty, &snapshot)).unwrap();
        let cells = &t.backend().buffer().content;

        // Rendered as block glyphs, not the old one-line plain-text message.
        assert!(cells.iter().any(|c| c.symbol() == "█"), "the big-text glyphs are drawn");
    }

    #[test]
    fn add_badge_hidden_when_current_folder_already_added() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.browser.open = true;
        let dir = app.browser.dir.clone();
        let other = dir.join("elsewhere");

        let render = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
            let snapshot = app.deck().snapshot();
            t.draw(|f| crate::ui::draw(f, app, &snapshot)).unwrap();
            t.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>()
        };

        app.memory.settings.music_dirs = vec![other];
        assert!(render(&app).contains("Add to music folders"), "a different folder is configured: badge shown");

        app.memory.settings.music_dirs = vec![dir];
        assert!(!render(&app).contains("Add to music folders"), "this folder is already configured: badge hidden");
    }

    #[test]
    fn empty_music_dirs_only_hints_what_actually_works() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.memory.settings.music_dirs.clear();
        app.settings_screen.open_to_music_dirs();

        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let snapshot = app.deck().snapshot();
        t.draw(|f| crate::ui::draw(f, &app, &snapshot)).unwrap();
        let text: String = t.backend().buffer().content.iter().map(|c| c.symbol()).collect();

        assert!(text.contains("Enter to browse"), "the one real action is explained");
        assert!(!text.contains("Ctrl+U"), "nothing to add yet, so don't mention it here");
        assert!(!text.contains("select"), "nothing to select with an empty list");
        assert!(!text.contains("remove"), "nothing to remove with an empty list");
        assert!(!text.contains("Esc"), "no backing out of setup with no folder yet either");
    }

    #[test]
    fn cant_escape_the_music_folders_list_with_none_configured() {
        let mut app = app();
        app.memory.settings.music_dirs.clear();
        app.settings_screen.open_to_music_dirs();

        press(&mut app, KeyCode::Esc, false);
        assert!(app.settings_screen.is_open(), "Esc does nothing: still empty, still gated");
        assert_eq!(app.settings_screen.folder(), Some(0));
    }

    #[test]
    fn the_row_menu_above_it_is_not_gated() {
        let mut app = app();
        app.memory.settings.music_dirs.clear();
        press(&mut app, KeyCode::Char('`'), false); // the row menu, not drilled in
        assert_eq!(app.settings_screen.folder(), None);
        press(&mut app, KeyCode::Esc, false);
        assert!(!app.settings_screen.is_open(), "no folders yet isn't a reason to trap you here too");
    }

    #[test]
    fn deleting_your_way_to_empty_mid_session_is_not_a_trap() {
        let mut app = app();
        let dir = app.browser.dir.clone();
        // The tests share a settings file; start with one already configured,
        // as if from a previous session — this is not first-run.
        app.memory.settings.music_dirs = vec![dir];

        press(&mut app, KeyCode::Char('`'), false); // open settings normally
        press(&mut app, KeyCode::Down, false); // row 1: music folders
        press(&mut app, KeyCode::Enter, false); // drill in
        assert!(!app.settings_screen.onboarding());
        press(&mut app, KeyCode::Char('x'), false); // delete the only folder
        assert!(app.music_dirs().is_empty());

        press(&mut app, KeyCode::Esc, false);
        assert!(app.settings_screen.is_open(), "steps back to the row menu first");
        assert_eq!(app.settings_screen.folder(), None);
        press(&mut app, KeyCode::Esc, false);
        assert!(!app.settings_screen.is_open(), "and a second Esc actually leaves");
    }

    #[test]
    fn adding_a_subfolder_of_an_added_folder_is_rejected() {
        let mut app = app();
        let parent = app.browser.dir.clone();
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        // The tests share a settings file; start from just the parent.
        app.memory.settings.music_dirs = vec![parent.clone()];

        app.browser.open = true;
        app.browser.enter(child, None);
        press_ctrl(&mut app, KeyCode::Char('u'));

        assert_eq!(app.music_dirs(), [parent], "the subfolder wasn't added");
        assert!(app.status().is_some_and(|s| s.contains("already inside")));
    }

    #[test]
    fn adding_a_folder_replaces_its_now_redundant_subfolders() {
        let mut app = app();
        let parent = app.browser.dir.clone();
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        // The tests share a settings file; start from just the child.
        app.memory.settings.music_dirs = vec![child];

        app.browser.open = true;
        app.browser.enter(parent.clone(), None);
        press_ctrl(&mut app, KeyCode::Char('u'));

        assert_eq!(app.music_dirs(), [parent], "the child was replaced, not duplicated");
        assert!(app.status().is_some_and(|s| s.contains("replacing 1 subfolder")));
    }

    #[test]
    fn adding_a_folder_from_inside_settings_concludes_setup() {
        let mut app = app();
        // The tests share a settings file; start from empty.
        app.memory.settings.music_dirs.clear();
        let dir = app.browser.dir.clone();
        press(&mut app, KeyCode::Char('`'), false); // open settings
        press(&mut app, KeyCode::Down, false); // row 1: music folders
        press(&mut app, KeyCode::Enter, false); // drill in, empty so far
        assert!(app.music_dirs().is_empty());

        press(&mut app, KeyCode::Enter, false); // Enter, from inside settings, opens the browser
        assert!(app.browser.open);
        assert!(app.settings_screen.is_open(), "settings stays open underneath");
        press_ctrl(&mut app, KeyCode::Char('u')); // pick the folder we're browsing

        assert!(!app.browser.open, "picking it closes the browser...");
        assert!(!app.settings_screen.is_open(), "...and settings too: setup is done");
        assert_eq!(app.music_dirs(), [dir]);
    }

    #[test]
    fn backing_out_without_picking_resumes_settings() {
        let mut app = app();
        app.memory.settings.music_dirs.clear();
        press(&mut app, KeyCode::Char('`'), false); // open settings
        press(&mut app, KeyCode::Down, false); // row 1: music folders
        press(&mut app, KeyCode::Enter, false); // drill in, empty so far
        press(&mut app, KeyCode::Enter, false); // opens the browser
        assert!(app.browser.open);

        press(&mut app, KeyCode::Esc, false); // changed my mind, no Ctrl+U

        assert!(!app.browser.open);
        assert!(app.settings_screen.is_open(), "back in settings automatically");
        assert_eq!(app.settings_screen.folder(), Some(0));
        assert!(app.music_dirs().is_empty(), "nothing was added");
    }

    #[test]
    fn enter_in_real_settings_opens_browser_to_add_another() {
        let mut app = app();
        let other = app.browser.dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        // The tests share a settings file; start from just this one folder.
        app.memory.settings.music_dirs = vec![other.clone()];

        press(&mut app, KeyCode::Char('`'), false); // open settings
        press(&mut app, KeyCode::Down, false); // row 1: music folders
        press(&mut app, KeyCode::Enter, false); // drill in
        press(&mut app, KeyCode::Enter, false); // Enter always offers to add another

        assert!(app.browser.open, "existing entries or not, Enter is how you add one");
        assert_eq!(app.music_dirs(), [other], "nothing changed yet, just browsing");
    }

    #[test]
    fn picking_a_new_folder_goes_up_normally_past_an_existing_one() {
        let mut app = app();
        let dir = app.browser.dir.clone();
        let parent = dir.parent().unwrap().to_path_buf();
        // The tests share a settings file; the browser's own folder is "already added".
        app.memory.settings.music_dirs = vec![dir.clone()];

        press(&mut app, KeyCode::Char('`'), false); // open settings
        press(&mut app, KeyCode::Down, false); // row 1: music folders
        press(&mut app, KeyCode::Enter, false); // drill in
        press(&mut app, KeyCode::Enter, false); // Enter: browse to pick a new folder
        assert_eq!(app.browser.dir, dir, "opens where the browser already was");

        press(&mut app, KeyCode::Left, false); // going up while picking...
        assert!(app.browser.open, "...stays a plain browser...");
        assert_eq!(app.browser.dir, parent, "...even past an already-added folder");
    }

    #[test]
    fn backspace_at_a_music_dir_goes_to_the_folder_list() {
        let mut app = app();
        let other = app.browser.dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        // The tests share a settings file; start from just this one folder.
        app.memory.settings.music_dirs = vec![other.clone()];

        app.browser.open = true;
        app.browser.enter(other, None);
        press(&mut app, KeyCode::Left, false);

        assert!(!app.browser.open);
        assert!(app.settings_screen.is_open());
        assert_eq!(app.settings_screen.folder(), Some(0));
        assert!(!app.settings_screen.editable(), "a quick switcher, not settings management");
    }

    #[test]
    fn the_quick_switcher_cant_add_or_remove_folders() {
        let mut app = app();
        let other = app.browser.dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        // The tests share a settings file; start from just this one folder.
        app.memory.settings.music_dirs = vec![other.clone()];

        app.browser.open = true;
        app.browser.enter(other, None);
        press(&mut app, KeyCode::Left, false); // up into the (non-editable) quick switcher

        press(&mut app, KeyCode::Char('x'), false);
        assert_eq!(app.music_dirs().len(), 1, "x doesn't remove it");
        press(&mut app, KeyCode::Tab, false);
        assert!(!app.browser.open, "Tab doesn't open the browser to add another");

        // The real settings screen, opened deliberately, still can.
        press(&mut app, KeyCode::Esc, false);
        press(&mut app, KeyCode::Char('`'), false);
        press(&mut app, KeyCode::Down, false);
        press(&mut app, KeyCode::Enter, false);
        assert!(app.settings_screen.editable());
        press(&mut app, KeyCode::Char('x'), false);
        assert!(app.music_dirs().is_empty());
    }

    #[test]
    fn backspace_elsewhere_still_goes_to_the_filesystem_parent() {
        let mut app = app();
        // The tests share a settings file; make sure this folder isn't one.
        app.memory.settings.music_dirs.clear();
        let parent = app.browser.dir.clone();
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();

        app.browser.open = true;
        app.browser.enter(child, None);
        press(&mut app, KeyCode::Left, false);

        assert!(app.browser.open, "a plain parent, not the folder list");
        assert_eq!(app.browser.dir, parent);
    }

    /// Writes ANSI dumps of a few screens under `target/screenshots/`, for a
    /// CI step to rasterize into the README/website images. Entirely
    /// synthetic: no audio file or device involved, so it needs neither a
    /// sample track nor CI audio hardware. Ignored by default since the
    /// output is meant to be generated on demand, not on every test run.
    ///
    /// `ODJ_SCREENSHOT_DIR` overrides the default location, so the CI step
    /// that rasterizes these doesn't have to guess it from a relative path.
    fn screenshot_dir() -> PathBuf {
        let dir = std::env::var_os("ODJ_SCREENSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/screenshots")));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dump_screenshot(name: &str, app: &App, cols: u16, rows: u16) {
        use odj_core::tui::to_ansi;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let snapshot = app.deck().snapshot();
        let mut t = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        t.draw(|f| crate::ui::draw(f, app, &snapshot)).unwrap();
        std::fs::write(screenshot_dir().join(format!("{name}.ans")), to_ansi(t.backend().buffer())).unwrap();
    }

    fn fictional_track(artist: &str, title: &str, bpm: f64, seconds: f64) -> Track {
        let path = PathBuf::from(format!("{artist} - {title}.mp3"));
        Track::from_samples(path, title.into(), Some(artist.into()), 48_000, click_track(bpm, seconds, 48_000))
    }

    #[test]
    #[ignore = "writes target/screenshots/*.ans; run with `cargo test -- --ignored screenshot`"]
    fn screenshot_performing() {
        let dir = std::env::temp_dir().join(format!("odj-shot-performing-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let deck = Arc::new(Mutex::new(Deck::new(48_000)));
        let mem = TrackMemory { quantize: Some(true), ..TrackMemory::default() };
        let track = fictional_track("Nova Satellite", "Night Drive", 128.0, 60.0);
        deck.lock().unwrap().load(Arc::new(track), &mem, false);
        let mut app = App::new(deck, Browser::new(dir), true, "Built-in Audio · 48000 Hz".into(), Memory::load());
        press(&mut app, KeyCode::Char(' '), false); // play
        app.deck().render(&mut vec![0.0; 2 * 48_000 * 20]); // 20s in, so the waveform isn't at the very start
        press(&mut app, KeyCode::Char('e'), false); // hot cue REC mode
        press(&mut app, KeyCode::Char('1'), false); // store hot cue A here
        press(&mut app, KeyCode::Char('e'), false); // back to PLAY mode
        press(&mut app, KeyCode::Char('l'), false); // 4-beat loop
        dump_screenshot("performing", &app, 120, 40);
    }

    #[test]
    #[ignore = "writes target/screenshots/*.ans; run with `cargo test -- --ignored screenshot`"]
    fn screenshot_browser() {
        let dir = std::env::temp_dir().join(format!("odj-shot-browser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        for name in [
            "Nova Satellite - Night Drive.mp3",
            "Glass Horizon - Afterglow.flac",
            "Echo Static - Low Light.wav",
            "Paper Tiger - Red Room.mp3",
            "Kilo Hearts - Sundown.mp3",
        ] {
            std::fs::write(dir.join(name), []).unwrap();
        }
        let deck = Arc::new(Mutex::new(Deck::new(48_000)));
        let mut browser = Browser::new(dir);
        browser.open = true;
        let app = App::new(deck, browser, true, "Built-in Audio · 48000 Hz".into(), Memory::load());
        dump_screenshot("browser", &app, 120, 40);
    }

    #[test]
    #[ignore = "writes target/screenshots/*.ans; run with `cargo test -- --ignored screenshot`"]
    fn screenshot_settings() {
        use odj_core::memory::{Settings, write_json};

        let dir = std::env::temp_dir().join(format!("odj-shot-settings-{}", std::process::id()));
        let config = dir.join("config");
        std::fs::create_dir_all(config.join("odj")).unwrap();
        let settings = Settings {
            music_dirs: vec!["/home/dj/Music".into(), "/mnt/usb/Tracks".into(), "/home/dj/Downloads/sets".into()],
            ..Settings::default()
        };
        write_json(&config.join("odj/settings.json"), &settings).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &config);
            std::env::set_var("XDG_CONFIG_HOME", &config);
        }
        let deck = Arc::new(Mutex::new(Deck::new(48_000)));
        let mut app = App::new(deck, Browser::new(dir), true, "Built-in Audio · 48000 Hz".into(), Memory::load());
        press(&mut app, KeyCode::Char('`'), false); // open settings
        press(&mut app, KeyCode::Down, false); // row 1: music folders
        press(&mut app, KeyCode::Enter, false); // drill in
        press(&mut app, KeyCode::Down, false); // select the second folder
        dump_screenshot("settings", &app, 120, 40);
    }

    /// Runs `tick()` until `done` is true or `timeout` elapses, so tests
    /// don't race the background video-grab thread.
    fn wait_for(app: &mut App, timeout: Duration, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + timeout;
        while !done(app) {
            assert!(Instant::now() < deadline, "condition never became true");
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn video_shows_a_frame_then_throttles_by_playhead_movement() {
        use std::sync::atomic::Ordering;
        let mut app = app();
        let fake = Arc::new(video::test_util::FakeSource::working());
        app.video = Some(VideoView::new(fake.clone()));

        wait_for(&mut app, Duration::from_secs(2), |a| a.video.as_ref().is_some_and(|v| v.frame.is_some()));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);

        // Position hasn't moved: ticking more shouldn't ask again.
        for _ in 0..5 {
            app.tick();
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);

        // ~0.2s at 48kHz — past the throttle threshold.
        app.deck().step_paused(15.0);
        wait_for(&mut app, Duration::from_secs(2), |_| fake.calls.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn video_gives_up_after_repeated_failures() {
        let mut app = app();
        app.video = Some(VideoView::new(Arc::new(video::test_util::FakeSource::failing())));
        wait_for(&mut app, Duration::from_secs(2), |a| a.video.is_none());
    }

    #[test]
    fn audio_only_track_never_starts_a_video_worker() {
        // `app()` loads a track built with `Track::from_samples`, which
        // always has `has_video: false` — `finish_loading` is the only
        // place that starts one, and it's not exercised by the test
        // helper's direct `deck.load(...)`, so this just confirms the
        // default stays `None` rather than something accidentally set.
        let app = app();
        assert!(app.video.is_none());
    }
}
