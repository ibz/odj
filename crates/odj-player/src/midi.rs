//! Optional MIDI pads: each pad calls the memory point with its own number
//! (pad 1 → memory 1, in track order), straight there from wherever the
//! playhead is — the same jump `J`/`K` make, minus the stepping. Devices,
//! learning and Settings' MIDI tab are shared with odj-sampler in
//! `odj_core::pad_midi`; this is just odj-player's own part: where its
//! config lives in the shared settings file and what a pad press does.
//!
//! Several decks can run at once, so the device in use is remembered per
//! deck (`Settings.player_midi_last`), and restoring never falls back to
//! "the one known device that's plugged in" — that would hand one
//! controller to every deck. Learned pad notes are per device, shared by
//! every deck (`Settings.player_midi_controllers`).

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use odj_core::memory::{load_settings, save_settings};
use odj_core::pad_midi::{PadConfig, PadHandler, PadMidi};

use crate::engine::Deck;

/// Deck `deck`'s slice of the shared settings file.
pub fn load(deck: u8) -> PadConfig {
    let settings = load_settings();
    PadConfig { last: settings.player_midi_last.get(&deck).cloned(), controllers: settings.player_midi_controllers }
}

/// Saves deck `deck`'s device and the shared learned notes back, leaving
/// every other deck's device (and every other app's fields) as they were.
pub fn save(deck: u8, config: &PadConfig) -> std::io::Result<()> {
    let mut settings = load_settings();
    match &config.last {
        Some(name) => settings.player_midi_last.insert(deck, name.clone()),
        None => settings.player_midi_last.remove(&deck),
    };
    settings.player_midi_controllers = config.controllers.clone();
    save_settings(&settings)
}

/// A pad press calls its memory point; the outcome goes down `status`
/// for the app to show (this runs on the MIDI thread, not the app's).
fn handler(deck: Arc<Mutex<Deck>>, status: Sender<String>) -> PadHandler {
    let status = Mutex::new(status);
    Arc::new(move |pad, down| {
        if !down {
            return;
        }
        let called = deck.lock().unwrap_or_else(|e| e.into_inner()).call_memory_number(pad);
        let msg = match called {
            Some(n) => format!("Memory {n} called"),
            None => format!("No memory {} on this track", pad + 1),
        };
        let _ = status.lock().unwrap_or_else(|e| e.into_inner()).send(msg);
    })
}

/// Deck `deck`'s pad controller, starting from `config`, plus where its
/// pad presses report to. `deck` is `None` before the deck number is
/// known (or in tests): nothing is saved then.
pub fn pad_midi(deck_handle: Arc<Mutex<Deck>>, deck: Option<u8>, config: PadConfig) -> (PadMidi, Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let save = Box::new(move |c: &PadConfig| match deck {
        Some(n) => save(n, c),
        None => Ok(()),
    });
    (PadMidi::new(config, handler(deck_handle, tx), save), rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_deck_keeps_its_own_device_but_shares_learned_notes() {
        // Same shared test settings dir as `app.rs`'s own tests (which
        // never unset these either, since tests run concurrently) — deck
        // numbers and device names nothing else uses keep this apart.
        let dir = std::env::temp_dir().join(format!("odj-test-{}", std::process::id()));
        // SAFETY: test-only, same pattern as `app.rs`'s `app()`.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let mut one = load(201);
        one.last = Some("zzz Pad A".into());
        one.set_pad_notes_for("zzz Pad A".into(), [36; odj_core::pad_midi::PADS]);
        save(201, &one).unwrap();
        let mut two = load(202);
        assert_eq!(two.last, None, "deck 202 didn't pick a device");
        assert!(two.controllers.contains_key("zzz Pad A"), "but sees what deck 201 learned");
        two.last = Some("zzz Pad B".into());
        save(202, &two).unwrap();
        assert_eq!(load(201).last.as_deref(), Some("zzz Pad A"), "deck 202's pick left deck 201's alone");
        assert_eq!(load(202).last.as_deref(), Some("zzz Pad B"));
    }
}
