//! Keyboard handling and app state around the deck.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::engine::Deck;
use crate::library::{Browser, siblings};
use crate::memory::Memory;
use crate::track::Track;

/// Without key-release events a hold lasts this long after the last key event.
const HOLD_FALLBACK: Duration = Duration::from_millis(500);
const STATUS_TIME: Duration = Duration::from_secs(4);
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

pub struct App {
    pub deck: Arc<Mutex<Deck>>,
    pub browser: Browser,
    memory: Memory,
    loading: Option<(PathBuf, mpsc::Receiver<anyhow::Result<Track>>)>,
    status: Option<(String, Instant)>,
    pub show_help: bool,
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
    taps: Vec<Instant>,
    pub quit: bool,
}

impl App {
    pub fn new(
        deck: Arc<Mutex<Deck>>,
        browser: Browser,
        key_release: bool,
        output_name: String,
    ) -> Self {
        Self {
            deck,
            browser,
            memory: Memory::load(),
            loading: None,
            status: None,
            show_help: false,
            show_remaining: false,
            hot_rec: false,
            track_number: None,
            key_release,
            output_name,
            bend_down: None,
            bend_up: None,
            search_back: None,
            search_fwd: None,
            taps: Vec::new(),
            quit: false,
        }
    }

    fn deck(&self) -> MutexGuard<'_, Deck> {
        self.deck.lock().unwrap_or_else(|e| e.into_inner())
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

    pub fn auto_cue(&self) -> bool {
        self.memory.settings.auto_cue
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

    /// Stores the loaded track's cues so they come back next time.
    pub fn remember(&mut self) {
        let (path, mem) = {
            let deck = self.deck();
            let Some(track) = deck.track.as_ref() else { return };
            (track.path.clone(), deck.memory())
        };
        self.memory.set(&path, mem);
        if let Err(e) = self.memory.save() {
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
                let mem = self.memory.get(&track.path);
                let msg = match track.bpm {
                    Some(bpm) => format!("Loaded {} ({bpm:.1} BPM)", track.title),
                    None => format!("Loaded {} (no BPM found)", track.title),
                };
                let folder = siblings(&track.path);
                self.track_number = folder
                    .iter()
                    .position(|p| *p == track.path)
                    .map(|i| (i + 1, folder.len()));
                let auto_cue = self.memory.settings.auto_cue;
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
    }

    fn tap(&mut self) {
        let now = Instant::now();
        if self.taps.last().is_some_and(|&t| now - t > Duration::from_secs(2)) {
            self.taps.clear();
        }
        self.taps.push(now);
        if self.taps.len() > 8 {
            self.taps.remove(0);
        }
        if self.taps.len() < 4 {
            self.set_status(format!("Tap… {}", self.taps.len()));
            return;
        }
        let span = (now - self.taps[0]).as_secs_f64();
        let bpm = 60.0 * (self.taps.len() - 1) as f64 / span;
        self.deck().set_tapped_bpm(bpm);
        self.set_status(format!("Tapped {bpm:.1} BPM"));
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
                self.browser_key(code);
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
            _ => {}
        }
    }

    fn browser_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.browser.move_by(-1),
            KeyCode::Down => self.browser.move_by(1),
            KeyCode::PageUp => self.browser.move_by(-10),
            KeyCode::PageDown => self.browser.move_by(10),
            KeyCode::Home => self.browser.selected = 0,
            KeyCode::End => self.browser.move_by(isize::MAX / 2),
            KeyCode::Left | KeyCode::Backspace => self.browser.parent(),
            KeyCode::Enter | KeyCode::Right => {
                if let Some(path) = self.browser.activate() {
                    self.load(path);
                    self.browser.open = false;
                }
            }
            KeyCode::Esc | KeyCode::Tab => self.browser.open = false,
            _ => {}
        }
    }

    fn deck_key(&mut self, code: KeyCode, shift: bool, repeat: bool) {
        // Keys that toggle or trigger ignore auto-repeat; steppers accept it.
        let once = !repeat;
        match code {
            KeyCode::Char('q') if shift => self.quit = true,
            KeyCode::Char('q') => self.set_status("Press Shift+Q to quit"),
            KeyCode::Esc => self.show_help = false,
            KeyCode::Char('/') if once => self.show_help = !self.show_help,
            KeyCode::Tab if once => self.browser.open = true,

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
                    } else if !self.deck().hot_cue(i) {
                        self.set_status(format!("Hot cue {letter} is empty, press E for REC mode to store it"));
                    }
                } else if shift {
                    self.deck().clear_hot_cue(i);
                    self.set_status(format!("Hot cue {letter} cleared"));
                } else {
                    self.deck().set_hot_cue(i);
                    self.set_status(format!("Hot cue {letter} stored"));
                }
            }
            KeyCode::Char('e') if once => {
                self.hot_rec = !self.hot_rec;
                self.set_status(if self.hot_rec { "Hot cue REC mode" } else { "Hot cue PLAY mode" });
            }

            KeyCode::Char('w') if once => {
                let stored = self.deck().store_memory();
                match stored {
                    Some(n) => self.set_status(format!("Memory {n} stored")),
                    None => self.set_status("Already in memory"),
                }
            }
            KeyCode::Char(c @ ('j' | 'k')) if once => {
                let called = self.deck().call_memory(if c == 'j' { -1 } else { 1 });
                match called {
                    Some(n) => self.set_status(format!("Memory {n} called")),
                    None => self.set_status("No more memory points this way"),
                }
            }
            KeyCode::Char('x') if once => {
                let deleted = self.deck().delete_memory();
                self.set_status(if deleted { "Memory deleted" } else { "No memory at the cue point" });
            }
            KeyCode::Char('i') if once => self.deck().loop_in(),
            KeyCode::Char('o') if once => {
                let adjusting = {
                    let mut deck = self.deck();
                    deck.loop_out();
                    deck.is_adjusting_loop()
                };
                if adjusting {
                    self.set_status("Loop out adjust: , / . move the out point (Shift ×10), O to finish");
                }
            }
            KeyCode::Char('p') if once => self.deck().reloop_exit(),
            KeyCode::Char('l') if once => {
                if !self.deck().auto_loop(4.0) {
                    self.set_status("Auto loop needs a BPM (tap one with A)");
                }
            }
            KeyCode::Char('[') => self.deck().scale_loop(0.5),
            KeyCode::Char(']') => self.deck().scale_loop(2.0),

            KeyCode::Up => self.deck().move_tempo(if shift { 10.0 } else { 1.0 }),
            KeyCode::Down => self.deck().move_tempo(if shift { -10.0 } else { -1.0 }),
            KeyCode::Char('0') if once => self.deck().reset_tempo(),
            KeyCode::Char('g') if once => self.deck().cycle_range(),
            KeyCode::Char('m') if once => {
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
                let (playing, adjusting) = {
                    let deck = self.deck();
                    (deck.is_playing(), deck.is_adjusting_loop())
                };
                if adjusting {
                    self.deck().adjust_loop_out(sign * if shift { 10.0 } else { 1.0 });
                } else if !playing {
                    let mut deck = self.deck();
                    if shift {
                        deck.step_beats_paused(sign);
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
            KeyCode::Char('t') if once && shift => {
                self.memory.settings.auto_cue = !self.memory.settings.auto_cue;
                let on = self.memory.settings.auto_cue;
                self.set_status(format!(
                    "Auto Cue {} (applies from the next load)",
                    if on { "on" } else { "off" }
                ));
                if let Err(e) = self.memory.save() {
                    self.set_status(format!("Could not save settings: {e}"));
                }
            }
            KeyCode::Char('t') if once => self.show_remaining = !self.show_remaining,
            KeyCode::Char('a') if once && shift => {
                self.deck().clear_tapped_bpm();
                self.taps.clear();
                self.set_status("BPM reset to detected");
            }
            KeyCode::Char('a') if once => self.tap(),
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
    use crate::memory::TrackMemory;
    use crate::track::tests::click_track;
    use ratatui::crossterm::event::KeyEventState;

    fn app() -> App {
        let dir = std::env::temp_dir().join(format!("odj-test-{}", std::process::id()));
        // Keep tests away from the real cue memory.
        unsafe { std::env::set_var("XDG_DATA_HOME", &dir) };
        let track = Track::from_samples("t.wav".into(), "t".into(), None, 48_000, click_track(120.0, 10.0, 48_000));
        let deck = Arc::new(Mutex::new(Deck::new(48_000)));
        deck.lock().unwrap().load(Arc::new(track), &TrackMemory::default(), false);
        App::new(deck, Browser::new(dir), true, String::new())
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
}
