//! Optional MIDI input for the Mixer: 4 knobs (Volume, Low, Mid, High —
//! the same order as `MixerScreen`'s own rows in `app.rs`) driven by
//! Control Change messages, mirroring odj-sampler's note-pad learning but
//! for knobs instead of pads. Port listing/picking and the stable device
//! identity used to key [`KnobConfig::devices`] live in `odj_core::midi`,
//! shared with odj-sampler.
//!
//! Saved into the settings file odj-player shares with odj-sampler (see
//! `odj_core::memory::Settings`'s `midi_port_id`/`midi_knobs`), split by
//! *deck* — `0` for `--deck 1`, `1` for `--deck 2` — since one physical
//! controller's 8 knobs serve two separate odj-player processes. Nothing
//! here reads or sets that deck index itself; the caller (`main.rs`)
//! already knows it from the `--deck` flag.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Alignment;
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Paragraph};

use odj_core::filter::{MAX_DB, MIN_DB};
use odj_core::memory::{load_settings, save_settings};
use odj_core::tui::{AMBER, centered};

use crate::engine::Deck;

/// Volume, Low, Mid, High — matches `MixerScreen`'s own row order, so a
/// learned knob's position lines straight up with the overlay.
pub const KNOBS: usize = 4;
const LABELS: [&str; KNOBS] = ["Volume", "Low", "Mid", "High"];

#[derive(Debug, Default, PartialEq)]
pub struct KnobConfig {
    pub port_id: Option<String>,
    /// Each knob's learned CC, per device, slot and deck (`0` = `--deck 1`,
    /// `1` = `--deck 2`) — see the module doc comment.
    pub devices: HashMap<String, Vec<[Option<[u8; KNOBS]>; 2]>>,
}

impl KnobConfig {
    /// This app's slice of the shared settings file, or the default (no
    /// port, nothing learned) when there is none yet.
    pub fn load() -> Self {
        let settings = load_settings();
        Self { port_id: settings.midi_port_id, devices: settings.midi_knobs }
    }

    /// Saves this app's fields back into the shared settings file, leaving
    /// odj-player's and odj-sampler's own other fields (`auto_cue`, `kit`,
    /// …) exactly as they were.
    pub fn save(&self) -> std::io::Result<()> {
        let mut settings = load_settings();
        settings.midi_port_id = self.port_id.clone();
        settings.midi_knobs = self.devices.clone();
        save_settings(&settings)
    }

    /// The learned CCs for `name`'s `slot`-th connected instance, `deck`'s
    /// half (`0` or `1`) — `None` until that half has actually been
    /// learned (which may be after the other half, learned by the sibling
    /// process for the other deck).
    pub fn knobs_for(&self, name: &str, slot: usize, deck: usize) -> Option<[u8; KNOBS]> {
        self.devices.get(name)?.get(slot)?[deck]
    }

    /// Remembers `ccs` for `name`'s `slot`-th connected instance, `deck`'s
    /// half — leaving the other deck's half (learned separately, by that
    /// other process) untouched.
    pub fn set_knobs_for(&mut self, name: String, slot: usize, deck: usize, ccs: [u8; KNOBS]) {
        let slots = self.devices.entry(name).or_default();
        if slots.len() <= slot {
            slots.resize(slot + 1, [None, None]);
        }
        slots[slot][deck] = Some(ccs);
    }
}

/// Opens `port_id`, wiring each knob in `ccs` (by position: Volume, Low,
/// Mid, High) to the matching `Deck` setter — a knob sets its parameter
/// *absolutely* from the Control Change value (`0..=127`), unlike the
/// keyboard's relative nudge, since a knob has a physical position. `None`
/// if the port no longer exists or can't be opened; the returned
/// connection must be kept alive for as long as MIDI input should work —
/// dropping it closes the connection.
pub fn connect(port_id: &str, deck: Arc<Mutex<Deck>>, ccs: [u8; KNOBS]) -> Option<midir::MidiInputConnection<()>> {
    let mut input = midir::MidiInput::new("odj").ok()?;
    input.ignore(midir::Ignore::ActiveSense | midir::Ignore::Time | midir::Ignore::Sysex);
    let port = input.find_port_by_id(port_id)?;
    input
        .connect(
            &port,
            "odj-player-in",
            move |_stamp, message, &mut ()| {
                let &[status, cc, value, ..] = message else { return };
                if status & 0xF0 != 0xB0 {
                    return;
                }
                let Some(knob) = ccs.iter().position(|&c| c == cc) else { return };
                let t = value as f32 / 127.0;
                let mut d = deck.lock().unwrap_or_else(|e| e.into_inner());
                match knob {
                    0 => d.set_volume(t),
                    1 => d.set_low_db(MIN_DB + t * (MAX_DB - MIN_DB)),
                    2 => d.set_mid_db(MIN_DB + t * (MAX_DB - MIN_DB)),
                    _ => d.set_high_db(MIN_DB + t * (MAX_DB - MIN_DB)),
                }
            },
            (),
        )
        .ok()
}

/// Opens `port_id` purely to listen: every Control Change's controller
/// number is sent down the returned channel. Used by [`learn_knobs`] to
/// watch for a knob turning without needing a `Deck` or a fixed CC mapping
/// yet. `None` if the port can't be opened; the connection closes when the
/// returned `MidiInputConnection` is dropped, same as `connect`.
fn listen_cc(port_id: &str) -> Option<(midir::MidiInputConnection<()>, mpsc::Receiver<u8>)> {
    let mut input = midir::MidiInput::new("odj").ok()?;
    input.ignore(midir::Ignore::ActiveSense | midir::Ignore::Time | midir::Ignore::Sysex);
    let port = input.find_port_by_id(port_id)?;
    let (tx, rx) = mpsc::channel();
    input
        .connect(
            &port,
            "odj-player-learn",
            move |_stamp, message, &mut ()| {
                let &[status, cc, ..] = message else { return };
                if status & 0xF0 == 0xB0 {
                    let _ = tx.send(cc);
                }
            },
            (),
        )
        .ok()
        .map(|conn| (conn, rx))
}

/// Walks the user through turning each of the 4 Mixer knobs in order
/// (Volume, Low, Mid, High), recording whichever CC each one sends — so a
/// newly-picked device works regardless of how its knobs are numbered.
/// `Ok(None)` if the port can't be opened, or the user cancels with
/// Esc/Ctrl+C (nothing is saved either way; the caller decides what to do,
/// same as `odj_core::midi::pick_midi_input` skipping).
pub fn learn_knobs(terminal: &mut DefaultTerminal, port_id: &str) -> Result<Option<[u8; KNOBS]>> {
    let Some((_conn, rx)) = listen_cc(port_id) else { return Ok(None) };
    let mut ccs = [0u8; KNOBS];
    let mut knob = 0;
    while knob < KNOBS {
        terminal.draw(|f| draw_learn_screen(f, LABELS[knob]))?;
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && (key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            return Ok(None);
        }
        match rx.recv_timeout(Duration::from_millis(30)) {
            // Still the knob just learned (easy to overshoot and keep
            // turning past the moment it was captured): ignore it rather
            // than registering it again for this one — only a CC that
            // doesn't match any knob already learned advances.
            Ok(cc) if ccs[..knob].contains(&cc) => {}
            Ok(cc) => {
                ccs[knob] = cc;
                knob += 1;
                // Debounce: a turning knob sends a burst of CCs; give it a
                // moment to settle and drop whatever else arrives before
                // moving on to the next one.
                std::thread::sleep(Duration::from_millis(150));
                while rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
    Ok(Some(ccs))
}

fn draw_learn_screen(f: &mut Frame, label: &str) {
    let area = centered(f.area(), 60, 40);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Configure Mixer knobs ")
        .title_bottom(" Esc cancel ")
        .border_style(Style::new().fg(AMBER));
    let text = format!("turn the {label} knob");
    let paragraph = Paragraph::new(text).alignment(Alignment::Center).style(Style::new().fg(AMBER)).block(block);
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ccs(a: u8, b: u8, c: u8, d: u8) -> [u8; KNOBS] {
        [a, b, c, d]
    }

    /// Isolates both settings-file env vars together, same pattern as
    /// odj-sampler's `midi.rs`/`settings.rs` tests.
    fn with_temp_dirs<T>(f: impl FnOnce() -> T) -> T {
        let dir = std::env::temp_dir()
            .join(format!("odj-player-midi-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only; no other test in this crate touches these vars
        // concurrently (odj-player has no equivalent of odj-sampler's
        // ENV_LOCK-guarded tests sharing them).
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            std::env::set_var("XDG_DATA_HOME", &dir);
        }
        let result = f();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("XDG_DATA_HOME");
        }
        std::fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn missing_settings_is_the_default_config() {
        with_temp_dirs(|| assert_eq!(KnobConfig::load(), KnobConfig::default()));
    }

    #[test]
    fn knobs_for_deck_is_independent_per_slot_and_deck() {
        let mut config = KnobConfig::default();
        assert_eq!(config.knobs_for("Knob Box", 0, 0), None);
        assert_eq!(config.knobs_for("Knob Box", 0, 1), None);

        let deck1 = ccs(10, 11, 12, 13);
        config.set_knobs_for("Knob Box".into(), 0, 0, deck1);
        // Deck 2's half of the same device/slot is still unlearned, even
        // though the Vec now has an entry for this slot.
        assert_eq!(config.knobs_for("Knob Box", 0, 0), Some(deck1));
        assert_eq!(config.knobs_for("Knob Box", 0, 1), None, "deck 2's half isn't learned yet");

        let deck2 = ccs(20, 21, 22, 23);
        config.set_knobs_for("Knob Box".into(), 0, 1, deck2);
        assert_eq!(config.knobs_for("Knob Box", 0, 0), Some(deck1), "deck 1's half survives deck 2 learning its own");
        assert_eq!(config.knobs_for("Knob Box", 0, 1), Some(deck2));

        assert_eq!(config.knobs_for("Other Box", 0, 0), None, "a different name is a different device");
    }

    #[test]
    fn config_round_trip_preserves_the_rest_of_the_shared_settings() {
        with_temp_dirs(|| {
            let mut settings = load_settings();
            settings.auto_cue = false;
            settings.music_dirs = vec![PathBuf::from("/music")];
            save_settings(&settings).unwrap();

            let mut config = KnobConfig { port_id: Some("24:0".into()), devices: HashMap::new() };
            config.set_knobs_for("Knob Box".into(), 0, 0, ccs(1, 2, 3, 4));
            config.save().unwrap();

            let after = load_settings();
            assert_eq!(after.midi_port_id, Some("24:0".into()), "odj-player's own field changed");
            assert_eq!(after.midi_knobs.get("Knob Box").and_then(|s| s[0][0]), Some(ccs(1, 2, 3, 4)));
            assert!(!after.auto_cue, "the settings odj-player already owned survived the save");
            assert_eq!(after.music_dirs, vec![PathBuf::from("/music")]);

            let reloaded = KnobConfig::load();
            assert_eq!(reloaded, config);
        });
    }

    #[test]
    #[ignore = "needs a real ALSA sequencer; exercises connect against a virtual port"]
    fn real_cc_moves_the_deck_volume() {
        use midir::os::unix::VirtualOutput;
        use std::time::{Duration, Instant};

        let deck = Arc::new(Mutex::new(Deck::new(48_000)));
        let mut out =
            midir::MidiOutput::new("odj-player-test-src").unwrap().create_virtual("odj-player-test-src").unwrap();

        let ports = odj_core::midi::list_ports();
        let (id, _) = ports.iter().find(|(_, name)| name.contains("odj-player-test-src")).expect("virtual port visible");
        let _conn = connect(id, deck.clone(), ccs(20, 21, 22, 23)).expect("connects");

        out.send(&[0xB0, 20, 0]).unwrap(); // Volume knob, CC 20, all the way down
        let deadline = Instant::now() + Duration::from_secs(2);
        while deck.lock().unwrap().volume() != 0.0 {
            assert!(Instant::now() < deadline, "volume never reached 0");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
