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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use odj_core::{audio, audio_link, tui};

use app::{App, AudioRoute};
use engine::Sampler;
use kit::Kit;

/// Whichever way this sampler's audio actually leaves the process right
/// now — kept alive for as long as it runs; dropping it tears down
/// whatever it holds (a real device's stream, or the virtual sink/
/// socket connection behind a mixer-routed one).
enum Playback {
    Direct { _stream: cpal::Stream },
    MixerPipeWire { _stream: cpal::Stream, _sink: audio::NullSink },
    MixerSocket { _sink: audio_link::SocketSink },
}

/// What `setup_direct`/`setup_through_mixer` have going by the time
/// everything else in `start` can proceed the same way regardless of
/// which one actually ran.
struct Setup {
    sampler: Arc<Mutex<Sampler>>,
    output: AudioRoute,
    out_rate: u32,
    errors: audio::Errors,
    _playback: Playback,
}

fn main() -> Result<()> {
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        println!(
            "usage: odj-sampler\n\nPlays the cues stored by odj-player from 16 pads. Press ? inside for keys.\n\
             Asks which output to use at startup, same as any other app here — unless\n\
             odj-mixer is already running, in which case it routes through that instead, no\n\
             picker asked, and odj-mixer discovers it live, labeled \"Sampler\", to apply its\n\
             own Volume/EQ to.\n\
             Kits are folders under {}, each with its own samples, plus built-in kits;\n\
             pick one (or create a new one) at startup when none is remembered, and press\n\
             K any time to switch. With a crate active (set from odj-player's Settings),\n\
             kits live inside it instead — picked up live, no restart needed either side.\n\
             Optionally triggered by MIDI\n\
             Note On/Off too: a previously learned device reconnects automatically when it's\n\
             plugged in, and ` opens Settings to choose another present device (learning its\n\
             pads on the spot) or forget one. The device and each pad's note are kept in the\n\
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

    // No MIDI picker at startup: a remembered device is restored if it's
    // plugged in and already learned, and Settings (`` ` ``) can switch to
    // any other present device — learning its pads on the spot — at any
    // time. The connection is kept alive by `App` for the rest of `start`.
    let mut midi_config = midi::MidiConfig::load();
    let midi_restored = midi::restore(&mut midi_config, setup.sampler.clone());

    let (kit_dir, kit) = match settings::load_active_kit() {
        Some(settings::ActiveKit::Saved(dir)) => {
            let kit = Kit::load(&dir.join("kit.json"));
            (Some(dir), kit)
        }
        Some(settings::ActiveKit::Embedded(id)) => match kit_browser::embedded_kit(&id) {
            Some(kit) => (None, kit),
            // That built-in kit no longer exists in this version: fall back
            // to the picker rather than starting with nothing.
            None => pick_and_remember_kit(terminal)?,
        },
        None => pick_and_remember_kit(terminal)?,
    };

    let mut app = App::new(setup.sampler.clone(), setup.out_rate, kit_dir, kit, key_release, setup.output);
    app.set_midi(midi_config, midi_restored);
    run(terminal, &mut app, &setup.errors)
}

/// Runs the one-time kit picker and remembers what it returned for the
/// next start. Embedded picks are remembered by id without creating a
/// kit directory; saved/new ones by slug.
fn pick_and_remember_kit(terminal: &mut DefaultTerminal) -> Result<(Option<PathBuf>, Kit)> {
    match kit_browser::pick_initial_kit(terminal)? {
        kit_browser::MaterializedKit::Saved(dir) => {
            let kit = Kit::load(&dir.join("kit.json"));
            settings::set_active_kit(Some(&dir));
            Ok((Some(dir), kit))
        }
        // Nothing written to disk yet — see `App::ensure_kit_dir`.
        kit_browser::MaterializedKit::Embedded { id, kit } => {
            settings::set_active_embedded_kit(&id);
            Ok((None, *kit))
        }
    }
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
    let route = AudioRoute::Direct(format!("{} · {} Hz", output.name, output.sample_rate()));
    Ok(Some(Setup {
        sampler,
        output: route,
        out_rate: output.sample_rate(),
        errors: output.errors,
        _playback: Playback::Direct { _stream: stream },
    }))
}

/// Instead of asking for a real output, routes through odj-mixer instead
/// — a virtual PipeWire sink (`audio::NullSink`) if it's running,
/// falling back to connecting straight to odj-mixer's own
/// `audio_link::Listener` over a Unix socket otherwise (e.g. on a
/// platform with no PipeWire at all). Named [`audio::sampler_sink_name`]
/// (no picker, no number either way — only one odj-sampler instance is
/// ever expected at a time, unlike a deck); odj-mixer discovers it the
/// same way it discovers a deck's own virtual sink, labeled "Sampler",
/// and mixes in whatever it captures. The sample rate doesn't matter the
/// way it does for a real device on the PipeWire path — it resamples
/// everything to its own shared clock regardless (the same property
/// that lets odj-mixer route between physically different cards at
/// all) — but does matter on the socket path, which has no such
/// resampling and requires it to match odj-mixer's own output rate.
fn setup_through_mixer() -> Result<Option<Setup>> {
    const RATE: u32 = 48_000;
    let sampler = Arc::new(Mutex::new(Sampler::new()));
    let sink_name = audio::sampler_sink_name();

    // `host()` itself already tries a real PipeWire connection and falls
    // back to plain ALSA on its own if that fails (see `audio::is_pipewire`'s
    // own doc comment) — checking `is_pipewire` on a freshly-made `Host`
    // is the one reliable way to tell; cpal's own `is_available()` alone
    // just checks whether a socket file exists on disk, which can be
    // stale (confirmed: still `true` moments after actually stopping
    // PipeWire).
    if audio::is_pipewire(&audio::host()) {
        let sink = audio::NullSink::create(sink_name, "Sampler")?;
        // Fresh: `host()` caches its own enumeration, so this must postdate creating the sink.
        let host = audio::host();
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
            output: AudioRoute::Mixer,
            out_rate: RATE,
            errors: output.errors,
            _playback: Playback::MixerPipeWire { _stream: stream, _sink: sink },
        }))
    } else {
        let sink = audio_link::SocketSink::create(sink_name, RATE, sampler.clone())
            .context("connecting to odj-mixer (it's running without PipeWire — using its direct socket link)")?;
        let errors = sink.errors.clone();
        Ok(Some(Setup {
            sampler,
            output: AudioRoute::Mixer,
            out_rate: RATE,
            errors,
            _playback: Playback::MixerSocket { _sink: sink },
        }))
    }
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
        // Settings picked a device whose pads aren't learned yet: walk
        // through them here, where drawing a temporary screen is possible,
        // then let `App` connect it and remember the result.
        if let Some(req) = app.take_midi_learn() {
            let notes = midi::learn_pad_notes(terminal, &req.port_id)?;
            app.finish_midi_learn(req, notes);
        }
    }
    Ok(())
}
