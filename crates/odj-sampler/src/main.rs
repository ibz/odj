//! odj-sampler: plays the cues stored by odj-player from 16 pads.

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

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use odj_core::{audio, tui};

use app::App;
use engine::Sampler;
use kit::Kit;

fn main() -> Result<()> {
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        println!(
            "usage: odj-sampler\n\nPlays the cues stored by odj-player from 16 pads. Press ? inside for keys.\n\
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
    let host = audio::host();
    let Some(output) = tui::pick_output(terminal, &host)? else {
        return Ok(());
    };
    let sampler = Arc::new(Mutex::new(Sampler::new()));
    let stream = output.start(sampler.clone())?;
    if let Err(e) = audio::verify_route(&host, &output) {
        drop(stream); // tear down rather than play on from the wrong device
        return Err(e);
    }

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
                midi::connect(&id, sampler.clone(), notes)
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

    let name =
        format!("{} · {} Hz{}", output.name, output.sample_rate(), if midi_conn.is_some() { " · MIDI" } else { "" });
    let mut app = App::new(sampler, output.sample_rate(), kit_dir, kit, key_release, name);
    run(terminal, &mut app, &output.errors)
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
