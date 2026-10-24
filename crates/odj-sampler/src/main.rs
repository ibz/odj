//! odj-sampler: plays the cues stored by odj-player from 16 pads. Picks
//! a real output at startup, same as any other app here — unless
//! odj-mixer is already running, in which case it routes through a
//! virtual sink for odj-mixer to pick up instead, no picker asked; see
//! `setup_through_mixer`'s own doc comment.

mod app;
mod engine;
mod kit;
mod kit_browser;
mod kits;
mod loader;
mod midi;
mod settings;
mod starter_kit;
mod ui;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use odj_core::{audio, tui};

use app::App;
use engine::Sampler;
use kit::Kit;

/// What `setup_direct`/`setup_through_mixer` have going by the time
/// everything else in `start` can proceed the same way regardless of
/// which one actually ran — same shape as odj-player's own `Setup`.
struct Setup {
    sampler: Arc<Mutex<Sampler>>,
    name: String,
    out_rate: u32,
    errors: audio::Errors,
    _stream: cpal::Stream,
    /// Only `Some` on the through-odj-mixer path — kept alive for as
    /// long as this runs; dropping it tears the virtual sink down.
    _sink: Option<audio::NullSink>,
}

fn main() -> Result<()> {
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        println!(
            "usage: odj-sampler\n\nPlays the cues stored by odj-player from 16 pads. Press ? inside for keys.\n\
             Asks which output to use at startup, same as any other app here — unless\n\
             odj-mixer is already running, in which case it routes through that instead, no\n\
             picker asked, and odj-mixer discovers it live, labeled \"Sampler\", to apply its\n\
             own Volume/EQ to.\n\
             Kits are folders under {}, each with its own samples; pick one (or create a new\n\
             one) at startup, and press K any time to switch. Optionally triggered by MIDI\n\
             Note On/Off too, picked at startup; the port and each pad's note are kept in the\n\
             settings file shared with odj-player, {}.",
            kits::kits_dir().display(),
            odj_core::memory::settings_file().display()
        );
        return Ok(());
    }
    let (mut terminal, key_release) = tui::init();
    let result = start(&mut terminal, key_release);
    tui::restore(key_release);
    result
}

fn start(terminal: &mut DefaultTerminal, key_release: bool) -> Result<()> {
    // If odj-mixer is running right now, route through a virtual sink for
    // it to capture and apply its own filters to instead of going
    // straight to a real device — see `setup_through_mixer`'s own doc
    // comment. A one-time check at startup, not re-checked afterward (if
    // odj-mixer quits or starts later mid-session, this doesn't switch
    // sides on its own) — same convention as odj-player's.
    let Some(setup) =
        (if odj_core::control::any_running("mixer") { setup_through_mixer() } else { setup_direct(terminal) })?
    else {
        return Ok(());
    };

    let mut midi_config = midi::MidiConfig::load();
    let midi_port = odj_core::midi::pick_midi_input(terminal, midi_config.port_id.as_deref())?;
    // Kept alive for the rest of `start`, closing automatically when it
    // returns; `None` (no port picked, or setup below didn't finish) just
    // means plain keyboard use.
    let midi_conn = match midi_port.as_deref().and_then(odj_core::midi::device_slot) {
        Some((name, slot)) => {
            let id = midi_port.unwrap();
            midi_config.port_id = Some(id.clone());
            // A device/slot we haven't seen before: walk the user through
            // pressing each pad so we learn its notes, rather than assuming
            // a layout. Unrecognized device, same `None` either way: the
            // port's gone, or the user cancelled partway through.
            let pad_notes = match midi_config.pad_notes_for(&name, slot) {
                Some(notes) => Some(notes),
                None => midi::learn_pad_notes(terminal, &id)?,
            };
            pad_notes.and_then(|notes| {
                midi_config.set_pad_notes_for(name, slot, notes);
                let _ = midi_config.save();
                midi::connect(&id, setup.sampler.clone(), notes)
            })
        }
        None => None,
    };

    let (kit_dir, kit) = match settings::load_active_kit() {
        Some(dir) => {
            let kit = Kit::load(&dir.join("kit.json"));
            (Some(dir), kit)
        }
        None => match kit_browser::pick_initial_kit(terminal)? {
            kit_browser::MaterializedKit::Saved(dir) => {
                let kit = Kit::load(&dir.join("kit.json"));
                let slug = dir.file_name().map(|n| n.to_string_lossy().into_owned());
                settings::set_active_kit(slug);
                (Some(dir), kit)
            }
            // Nothing written to disk yet — see `App::ensure_kit_dir`.
            kit_browser::MaterializedKit::Unsaved(kit) => (None, *kit),
        },
    };

    let name = format!("{}{}", setup.name, if midi_conn.is_some() { " · MIDI" } else { "" });
    let mut app = App::new(setup.sampler.clone(), setup.out_rate, kit_dir, kit, key_release, name);
    run(terminal, &mut app, &setup.errors)
}

/// The usual path: pick a real output and play straight to it, same as
/// always. `Ok(None)` if the user backs out of the output picker.
fn setup_direct(terminal: &mut DefaultTerminal) -> Result<Option<Setup>> {
    let host = audio::host();
    let Some(output) = tui::pick_output(terminal, &host)? else { return Ok(None) };
    let sampler = Arc::new(Mutex::new(Sampler::new()));
    let stream = output.start(sampler.clone())?;
    if let Err(e) = audio::verify_route(&host, &output) {
        drop(stream); // tear down rather than play on from the wrong device
        return Err(e);
    }
    let name = format!("{} · {} Hz", output.name, output.sample_rate());
    Ok(Some(Setup {
        sampler,
        name,
        out_rate: output.sample_rate(),
        errors: output.errors,
        _stream: stream,
        _sink: None,
    }))
}

/// Instead of asking for a real output, creates a virtual sink (see
/// `audio::NullSink`) named [`audio::sampler_sink_name`] (no picker, no
/// number — only one odj-sampler instance is ever expected at a time,
/// unlike a deck) and plays into that; odj-mixer discovers it the same
/// way it discovers a deck's own virtual sink, labeled "Sampler", and
/// mixes in whatever it captures. The sample rate doesn't matter here
/// the way it does for a real device — PipeWire resamples everything to
/// its own shared clock regardless (the same property that lets
/// odj-mixer route between physically different cards at all).
fn setup_through_mixer() -> Result<Option<Setup>> {
    const RATE: u32 = 48_000;
    let sampler = Arc::new(Mutex::new(Sampler::new()));

    let sink_name = audio::sampler_sink_name();
    let sink = audio::NullSink::create(sink_name, "Sampler")?;

    let host = audio::host(); // fresh: `host()` caches its own enumeration, so this must postdate creating the sink
    let outputs = audio::output_cards(&host)?;
    let choice = audio::choices(&outputs)
        .into_iter()
        .find(|c| outputs[c.card].id() == sink_name)
        .ok_or_else(|| anyhow!("the virtual sink didn't register with PipeWire in time"))?;
    let output = audio::Output::open(&host, &outputs, &choice)?;
    let stream = output.start(sampler.clone())?;
    if let Err(e) = audio::verify_route(&host, &output) {
        drop(stream);
        return Err(e);
    }
    Ok(Some(Setup {
        sampler,
        name: "Sampler".to_string(),
        out_rate: RATE,
        errors: output.errors,
        _stream: stream,
        _sink: Some(sink),
    }))
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, errors: &audio::Errors) -> Result<()> {
    while !app.quit {
        if let Some(e) = errors.take() {
            app.set_status(e);
        }
        app.tick();
        let snapshot = app.sampler.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
        terminal.draw(|f| ui::draw(f, app, &snapshot))?;
        if event::poll(Duration::from_millis(16))? {
            loop {
                if let Event::Key(key) = event::read()? {
                    app.handle_key(key);
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}
