//! Optional MIDI input: Note On/Off trigger pads exactly like a key press,
//! so an external sequencer (hardware or software) can drive odj-sampler
//! instead of — or alongside — the keyboard. Config lives in the settings
//! file odj-sampler shares with odj-player (see
//! `odj_core::memory::Settings`'s `midi_port_id`/`midi_devices`).
//!
//! Controllers don't agree on how they number their pads (confirmed on real
//! hardware: some send a straight ascending range, others group and jump
//! around), so each pad's note is learned from the device itself rather
//! than assumed — see [`learn_pad_notes`] — and kept per device in
//! [`MidiConfig::devices`], keyed by a *stable* device name rather than
//! port id: a port id (e.g. an ALSA client:port number) is just a
//! connection-order number that can change across a replug, where the name
//! (once its own connection-order suffix is stripped — see
//! [`stable_name`]) is not. Two identical controllers plugged in at once
//! share that stable name, so each gets its own slot in that name's `Vec`
//! instead — see [`device_slot`].

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use odj_core::memory::{load_settings, save_settings};
use odj_core::tui::{AMBER, centered};

use crate::engine::{PADS, Sampler};

#[derive(Debug, Default, PartialEq)]
pub struct MidiConfig {
    pub port_id: Option<String>,
    /// The MIDI note each pad responds to, per device: `devices[name][slot][i]`
    /// for pad `i`, where `slot` distinguishes multiple simultaneously
    /// connected devices that report the same name (see [`device_slot`]).
    /// Learned once per slot (see [`learn_pad_notes`]) rather than assumed,
    /// since pad numbering varies across controllers.
    pub devices: HashMap<String, Vec<[u8; PADS]>>,
}

impl MidiConfig {
    /// This app's slice of the shared settings file, or the default (no
    /// port, no devices learned) when there is none yet.
    pub fn load() -> Self {
        let settings = load_settings();
        Self { port_id: settings.midi_port_id, devices: settings.midi_devices }
    }

    /// Saves this app's fields back into the shared settings file, leaving
    /// odj-player's and odj-sampler's own other fields (`auto_cue`, `kit`,
    /// …) exactly as they were.
    pub fn save(&self) -> std::io::Result<()> {
        let mut settings = load_settings();
        settings.midi_port_id = self.port_id.clone();
        settings.midi_devices = self.devices.clone();
        save_settings(&settings)
    }

    /// The learned notes for `name`'s `slot`-th connected instance, if any.
    pub fn pad_notes_for(&self, name: &str, slot: usize) -> Option<[u8; PADS]> {
        self.devices.get(name)?.get(slot).copied()
    }

    /// Remembers `notes` for `name`'s `slot`-th connected instance.
    pub fn set_pad_notes_for(&mut self, name: String, slot: usize, notes: [u8; PADS]) {
        let slots = self.devices.entry(name).or_default();
        if slots.len() <= slot {
            slots.resize(slot + 1, [0; PADS]);
        }
        slots[slot] = notes;
    }
}

/// The pad whose entry in `pad_notes` is `note`, if any.
pub fn note_to_pad(note: u8, pad_notes: &[u8; PADS]) -> Option<usize> {
    pad_notes.iter().position(|&n| n == note)
}

/// Available MIDI input ports as `(id, display name)` — the id is what
/// `connect` and `MidiConfig::port_id` use; it stays valid across runs
/// (backend-dependent), unlike a plain index into this list.
pub fn list_ports() -> Vec<(String, String)> {
    let Ok(input) = midir::MidiInput::new("odj-sampler") else { return Vec::new() };
    input.ports().iter().filter_map(|p| input.port_name(p).ok().map(|name| (p.id(), name))).collect()
}

/// `port_id`'s stable name, with its connection-order suffix removed —
/// confirmed by reading midir's ALSA backend source, `list_ports`'s display
/// name always ends in literally " {id}" (it's appended deliberately, to
/// keep same-named devices visually distinct in a list), so a straight
/// `strip_suffix` recovers the name as the device itself reports it, with
/// nothing connection-order-dependent left in it.
fn stable_name(id: &str, name: &str) -> String {
    name.strip_suffix(&format!(" {id}")).unwrap_or(name).to_string()
}

/// `port_id`'s stable name, and its slot: how many *other* currently
/// connected ports share that same stable name and sort before it. Two
/// identical controllers plugged in together get slots 0 and 1 under their
/// shared name, so each keeps its own entry in [`MidiConfig::devices`]; a
/// single controller keeps slot 0 across replugs even though its port id
/// (and the raw display name's suffix) changes. `None` if `port_id` isn't
/// currently connected.
pub fn device_slot(port_id: &str) -> Option<(String, usize)> {
    let ports = list_ports();
    let name = stable_name(port_id, &ports.iter().find(|(id, _)| id == port_id)?.1);
    let slot = ports
        .iter()
        .take_while(|(id, _)| id != port_id)
        .filter(|(id, n)| stable_name(id, n) == name)
        .count();
    Some((name, slot))
}

/// Opens `port_id`, wiring Note On (velocity > 0) to `press` and Note Off
/// (or Note On with velocity 0, the running-status convention) to
/// `release` — the same calls a key press already makes. `None` if the
/// port no longer exists or can't be opened; the returned connection must
/// be kept alive (held somewhere) for as long as MIDI input should work —
/// dropping it closes the connection.
pub fn connect(
    port_id: &str,
    sampler: Arc<Mutex<Sampler>>,
    pad_notes: [u8; PADS],
) -> Option<midir::MidiInputConnection<()>> {
    let mut input = midir::MidiInput::new("odj-sampler").ok()?;
    input.ignore(midir::Ignore::ActiveSense | midir::Ignore::Time | midir::Ignore::Sysex);
    let port = input.find_port_by_id(port_id)?;
    input
        .connect(
            &port,
            "odj-sampler-in",
            move |_stamp, message, &mut ()| {
                let &[status, note, velocity, ..] = message else { return };
                let Some(pad) = note_to_pad(note, &pad_notes) else { return };
                let mut sampler = sampler.lock().unwrap_or_else(|e| e.into_inner());
                match status & 0xF0 {
                    0x90 if velocity > 0 => {
                        sampler.press(pad);
                    }
                    0x80 | 0x90 => sampler.release(pad),
                    _ => {}
                }
            },
            (),
        )
        .ok()
}

/// Opens `port_id` purely to listen: every Note On (velocity > 0) is sent
/// down the returned channel. Used by [`learn_pad_notes`] to watch for pad
/// presses without needing a `Sampler` or a fixed note mapping yet. `None`
/// if the port can't be opened; the connection closes when the returned
/// `MidiInputConnection` is dropped, same as `connect`.
fn listen_notes(port_id: &str) -> Option<(midir::MidiInputConnection<()>, mpsc::Receiver<u8>)> {
    let mut input = midir::MidiInput::new("odj-sampler").ok()?;
    input.ignore(midir::Ignore::ActiveSense | midir::Ignore::Time | midir::Ignore::Sysex);
    let port = input.find_port_by_id(port_id)?;
    let (tx, rx) = mpsc::channel();
    input
        .connect(
            &port,
            "odj-sampler-learn",
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

/// Walks the user through pressing each of the 16 pads in order, recording
/// whichever note each press sends — so a newly-picked device works
/// regardless of how it numbers its pads. `Ok(None)` if the port can't be
/// opened, or the user cancels with Esc/Ctrl+C (nothing is saved either
/// way; the caller decides what to do, same as `pick_midi_input` skipping).
pub fn learn_pad_notes(terminal: &mut DefaultTerminal, port_id: &str) -> Result<Option<[u8; PADS]>> {
    let Some((_conn, rx)) = listen_notes(port_id) else { return Ok(None) };
    let mut notes = [0u8; PADS];
    let mut pad = 0;
    while pad < PADS {
        terminal.draw(|f| draw_learn_screen(f, pad))?;
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && (key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            return Ok(None);
        }
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(note) => {
                notes[pad] = note;
                pad += 1;
                // Debounce: a pad can send more than one Note On per press
                // (velocity-sensitive bounce); give it a moment to settle
                // and drop whatever else arrives before moving on.
                std::thread::sleep(Duration::from_millis(150));
                while rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
    Ok(Some(notes))
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

fn draw_learn_screen(f: &mut Frame, pad: usize) {
    let area = centered(f.area(), 60, 60);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Configure pads ")
        .title_bottom(" Esc cancel ")
        .border_style(Style::new().fg(AMBER));
    let mut text = big_number(pad + 1).join("\n");
    text.push_str(&format!("\n\npress pad {} of {}", pad + 1, PADS));
    let paragraph = Paragraph::new(text).alignment(Alignment::Center).style(Style::new().fg(AMBER)).block(block);
    f.render_widget(paragraph, area);
}

/// Lists available MIDI inputs and lets the user pick one, or skip —
/// unlike the (mandatory) audio output picker, there's always a
/// reasonable "no MIDI" outcome, so this returns `Ok(None)` rather than
/// ending the program: no ports found, or the user backed out.
pub fn pick_midi_input(terminal: &mut DefaultTerminal, config: &MidiConfig) -> Result<Option<String>> {
    let ports = list_ports();
    if ports.is_empty() {
        return Ok(None);
    }
    let mut selected =
        config.port_id.as_ref().and_then(|id| ports.iter().position(|(pid, _)| pid == id)).unwrap_or(0);
    loop {
        terminal.draw(|f| draw_midi_picker(f, &ports, selected))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(ports.len() - 1),
            KeyCode::Enter => return Ok(Some(ports[selected].0.clone())),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Esc | KeyCode::Char('q' | 'Q' | 'n' | 'N') => return Ok(None),
            _ => {}
        }
    }
}

fn draw_midi_picker(f: &mut Frame, ports: &[(String, String)], selected: usize) {
    let area = centered(f.area(), 70, 50);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" MIDI input (optional) ")
        .title_bottom(" ↑↓ select  Enter use  Esc/N skip  Ctrl+C quit ")
        .border_style(Style::new().fg(AMBER));
    let items: Vec<ListItem> = ports.iter().map(|(_, name)| ListItem::new(name.as_str())).collect();
    let list = List::new(items).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits;

    fn default_pad_notes() -> [u8; PADS] {
        std::array::from_fn(|i| 36 + i as u8)
    }

    /// Isolates both settings-file env vars together — `MidiConfig` now
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

        // Real hardware (an M-VAVE SMC-PAD) sends pads in groups of 4,
        // descending by 4 each group — not a straight ascending range —
        // confirming the table handles an arbitrary order correctly.
        let quirky = [96, 97, 98, 99, 92, 93, 94, 95, 88, 89, 90, 91, 84, 85, 86, 87];
        assert_eq!(note_to_pad(88, &quirky), Some(8), "pad 9 sends note 88, not base + 8");
        assert_eq!(note_to_pad(85, &quirky), Some(13));
        assert_eq!(note_to_pad(100, &quirky), None);
    }

    #[test]
    fn config_round_trips() {
        with_temp_dirs(|| {
            let mut devices = HashMap::new();
            devices.insert("Some Pad".to_string(), vec![default_pad_notes()]);
            let config = MidiConfig { port_id: Some("24:0".into()), devices };
            config.save().unwrap();
            assert_eq!(MidiConfig::load(), config);
        });
    }

    #[test]
    fn config_round_trip_preserves_the_rest_of_the_shared_settings() {
        with_temp_dirs(|| {
            let mut settings = load_settings();
            settings.auto_cue = false;
            settings.music_dirs = vec!["/music".into()];
            save_settings(&settings).unwrap();

            let config = MidiConfig { port_id: Some("24:0".into()), devices: HashMap::new() };
            config.save().unwrap();

            let after = load_settings();
            assert_eq!(after.midi_port_id, Some("24:0".into()), "odj-sampler's own field changed");
            assert!(!after.auto_cue, "odj-player's fields survived the save");
            assert_eq!(after.music_dirs, vec![std::path::PathBuf::from("/music")]);
        });
    }

    #[test]
    fn missing_settings_is_the_default_config() {
        with_temp_dirs(|| assert_eq!(MidiConfig::load(), MidiConfig::default()));
    }

    #[test]
    fn pad_notes_for_slot_is_independent_per_slot() {
        let mut config = MidiConfig::default();
        assert_eq!(config.pad_notes_for("Twin Pad", 0), None);
        assert_eq!(config.pad_notes_for("Twin Pad", 1), None);

        let notes_a = default_pad_notes();
        let notes_b = [1; PADS];
        config.set_pad_notes_for("Twin Pad".into(), 0, notes_a);
        config.set_pad_notes_for("Twin Pad".into(), 1, notes_b);

        assert_eq!(config.pad_notes_for("Twin Pad", 0), Some(notes_a));
        assert_eq!(config.pad_notes_for("Twin Pad", 1), Some(notes_b));
        assert_eq!(config.pad_notes_for("Other Pad", 0), None, "a different name is a different device");
    }

    #[test]
    #[ignore = "needs a real ALSA sequencer; exercises list_ports/connect against a virtual port"]
    fn real_midi_note_triggers_a_pad() {
        use crate::engine::{Mode, Props, Sample};
        use midir::os::unix::VirtualOutput;
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
        let _conn = connect(id, sampler.clone(), default_pad_notes()).expect("connects");

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

    #[test]
    #[ignore = "needs a real ALSA sequencer; exercises list_ports against two virtual ports"]
    fn device_slot_tells_identical_names_apart() {
        use midir::os::unix::VirtualOutput;

        // Two virtual ports sharing a name — including the client name,
        // which `port_name` folds in too — standing in for two identical
        // physical controllers plugged in at once (same make and model
        // reports the same client and port name both times).
        let _a = midir::MidiOutput::new("odj-sampler-test-twin")
            .unwrap()
            .create_virtual("odj-sampler-test-twin")
            .unwrap();
        let _b = midir::MidiOutput::new("odj-sampler-test-twin")
            .unwrap()
            .create_virtual("odj-sampler-test-twin")
            .unwrap();

        let ports = list_ports();
        let mut twins = ports.iter().filter(|(_, name)| name.contains("odj-sampler-test-twin"));
        let (id_a, _) = twins.next().expect("first virtual twin visible");
        let (id_b, _) = twins.next().expect("second virtual twin visible");

        let (name_a, slot_a) = device_slot(id_a).expect("a connected port resolves");
        let (name_b, slot_b) = device_slot(id_b).expect("a connected port resolves");
        assert_eq!(name_a, name_b, "both report the same name");
        assert_ne!(slot_a, slot_b, "but each gets its own slot under that name");
    }
}
