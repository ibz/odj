//! Optional MIDI input: Note On/Off trigger pads exactly like a key press,
//! so an external sequencer (hardware or software) can drive odj-sampler
//! instead of — or alongside — the keyboard. Everything about devices,
//! learning and Settings' MIDI tab is shared with odj-player in
//! `odj_core::pad_midi`; this is just odj-sampler's own part: where its
//! config lives in the shared settings file
//! (`Settings.sampler_midi_last`/`sampler_midi_controllers`) and what a
//! pad press does. Nothing is picked at startup: a remembered device is
//! restored automatically when it's plugged in, and Settings (`` ` ``)
//! can switch to any other present device — or learn a new one — at any
//! time.

use std::sync::{Arc, Mutex};

use odj_core::memory::{load_settings, save_settings};
use odj_core::pad_midi::{PadConfig, PadHandler, PadMidi};

use crate::engine::{PADS, Sampler};

// odj-sampler's pads are exactly the shared controller's pads.
const _: () = assert!(PADS == odj_core::pad_midi::PADS);

/// This app's slice of the shared settings file, or the default (no
/// device, nothing learned) when there is none yet.
pub fn load() -> PadConfig {
    let settings = load_settings();
    PadConfig { last: settings.sampler_midi_last, controllers: settings.sampler_midi_controllers }
}

/// Saves this app's fields back into the shared settings file, leaving
/// every other app's own fields exactly as they were.
pub fn save(config: &PadConfig) -> std::io::Result<()> {
    let mut settings = load_settings();
    settings.sampler_midi_last = config.last.clone();
    settings.sampler_midi_controllers = config.controllers.clone();
    save_settings(&settings)
}

/// A pad press is exactly a key press on that pad.
fn handler(sampler: Arc<Mutex<Sampler>>) -> PadHandler {
    Arc::new(move |pad, down| {
        let mut sampler = sampler.lock().unwrap_or_else(|e| e.into_inner());
        if down {
            sampler.press(pad);
        } else {
            sampler.release(pad);
        }
    })
}

/// odj-sampler's pad controller, starting from `config` — not connected
/// to anything until [`PadMidi::restore`] or Settings does.
pub fn pad_midi(sampler: Arc<Mutex<Sampler>>, config: PadConfig) -> PadMidi {
    PadMidi::new(config, handler(sampler), Box::new(save))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits;

    fn default_pad_notes() -> [u8; PADS] {
        std::array::from_fn(|i| 36 + i as u8)
    }

    /// Isolates both settings-file env vars together — the config now
    /// reads/writes through the shared settings file, which lives under
    /// `XDG_CONFIG_HOME` with an `XDG_DATA_HOME` fallback (see
    /// `load_settings`), so a test touching one without the other risks
    /// leaking into the real file.
    fn with_temp_dirs<T>(f: impl FnOnce() -> T) -> T {
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-midi-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only, same pattern used throughout this crate's tests.
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
    fn config_round_trips() {
        with_temp_dirs(|| {
            let mut config = PadConfig { last: Some("Some Pad".into()), ..Default::default() };
            config.set_pad_notes_for("Some Pad".to_string(), default_pad_notes());
            save(&config).unwrap();
            assert_eq!(load(), config);
        });
    }

    #[test]
    fn config_round_trip_preserves_the_rest_of_the_shared_settings() {
        with_temp_dirs(|| {
            let mut settings = load_settings();
            settings.auto_cue = false;
            settings.music_dirs = vec!["/music".into()];
            save_settings(&settings).unwrap();

            let config = PadConfig { last: Some("Some Pad".into()), ..Default::default() };
            save(&config).unwrap();

            let after = load_settings();
            assert_eq!(after.sampler_midi_last, Some("Some Pad".into()), "odj-sampler's own field changed");
            assert!(!after.auto_cue, "odj-player's fields survived the save");
            assert_eq!(after.music_dirs, vec![std::path::PathBuf::from("/music")]);
        });
    }

    #[test]
    fn missing_settings_is_the_default_config() {
        with_temp_dirs(|| assert_eq!(load(), PadConfig::default()));
    }

    #[test]
    #[ignore = "needs a real ALSA sequencer; exercises list_ports/connect against a virtual port"]
    fn real_midi_note_triggers_a_pad() {
        use crate::engine::{Mode, Props, Sample};
        use midir::os::unix::VirtualOutput;
        use odj_core::midi::list_ports;
        use std::time::{Duration, Instant};

        let sampler = Arc::new(Mutex::new(Sampler::new()));
        {
            let mut s = sampler.lock().unwrap();
            s.set_sample(0, Some(Sample::new(vec![0.1; (100 + 256) * 2], 100)));
            s.set_props(0, Props { mode: Mode::Gate, looped: true, retrigger: false, gain_db: 0.0 });
        }

        let mut out = midir::MidiOutput::new("odj-sampler-test-src")
            .unwrap()
            .create_virtual("odj-sampler-test-src")
            .unwrap();

        let ports = list_ports();
        let (id, _) = ports.iter().find(|(_, name)| name.contains("odj-sampler-test-src")).expect("virtual port visible");
        let _conn = odj_core::pad_midi::connect(id, handler(sampler.clone()), default_pad_notes()).expect("connects");

        out.send(&[0x90, 36, 100]).unwrap(); // Note On, pad 0
        let deadline = Instant::now() + Duration::from_secs(2);
        while !sampler.lock().unwrap().snapshot().pads[0].playing {
            assert!(Instant::now() < deadline, "pad never started playing");
            std::thread::sleep(Duration::from_millis(5));
        }

        out.send(&[0x80, 36, 0]).unwrap(); // Note Off: Gate mode starts the fade-out
        std::thread::sleep(Duration::from_millis(50)); // let the callback thread process it
        // `active` only clears once `render` (normally the audio thread's
        // job) has pumped through the fade-out — nothing else calls it here.
        let mut buf = vec![0.0f32; 4096];
        for _ in 0..4 {
            sampler.lock().unwrap().render(&mut buf);
        }
        assert!(!sampler.lock().unwrap().snapshot().pads[0].playing, "pad stopped after the fade-out");
    }
}
