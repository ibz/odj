//! odj-mixer: picks one main output at startup, then continuously
//! discovers remote-controlled odj-player decks and a running
//! odj-sampler, connecting every one of them to it automatically, live
//! — a hardware sound-card input instead waits until it's added from
//! Settings — and lets you control each channel's Volume and 3-band EQ
//! by hand or by MIDI knob (mapped from Settings too).

mod devices;
mod log;
mod midi;
mod routing;
mod ui;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use ratatui::DefaultTerminal;

use odj_core::audio;
use odj_core::filter::{MAX_DB, MIN_DB};
use odj_core::tui;

use devices::{Devices, Kind};
use midi::{Learning, Targets};
use routing::{Param, Routing};

fn main() -> Result<()> {
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        println!(
            "usage: odj-mixer\n\n\
             Asks for one main output at startup (mandatory — nothing would have anywhere to\n\
             play without it). From then on, continuously detects running odj-player decks\n\
             (as \"Deck n\") and a running odj-sampler (as \"Sampler\"), connecting every one\n\
             of them to that output automatically, showing a channel strip — High, Mid, Low,\n\
             Volume — for each. ←/→ picks the\n\
             channel and ↑/↓ picks its control — neither changes a value, just navigates;\n\
             +/- adjusts the selected one instead (Shift: ×4).\n\
             ` opens Settings: one row per detected input, deck and the sampler. Space adds\n\
             the selected input to the mix or removes it (a no-op on a deck's or the\n\
             sampler's row — always connected automatically); an added input is remembered\n\
             and reconnected automatically on the next run, same as those are. Enter drills\n\
             into any row's own settings — for now just its 4 MIDI mappings, Volume/Low/Mid/\n\
             High, where Enter maps a knob to the selected one and X clears it, Esc backs\n\
             out. Esc/` closes Settings.\n\
             Nothing on the live screen itself changes a mapping; that's Settings-only.\n\
             Picks a MIDI controller at startup too (optional, skip with Esc/N) and, if you\n\
             did, walks you through mapping a knob to each deck's (and the sampler's, if\n\
             it's running) Volume/Low/Mid/High that isn't mapped yet. Press ? for the full\n\
             list of keys.\n\
             Needs PipeWire: routing between two cards needs its shared graph clock, which\n\
             plain ALSA doesn't have.\n\
             Every error shown on screen is also logged, with when it happened, to {}.",
            log::log_file().display()
        );
        return Ok(());
    }
    let (mut terminal, key_release) = tui::init();
    let result = start(&mut terminal);
    tui::restore(key_release);
    result
}

fn start(terminal: &mut DefaultTerminal) -> Result<()> {
    let host = audio::host();
    if !audio::is_pipewire(&host) {
        bail!(
            "odj-mixer needs PipeWire running — routing between two sound cards relies on \
             its shared graph clock to keep them in sync; plain ALSA has no way to do that \
             between two independent hardware clocks."
        );
    }
    drop(host); // only needed for this check — everything else opens its own

    let devices_in = Devices::start(Kind::Input);
    let devices_deck = Devices::start(Kind::Deck);
    let devices_out = Devices::start(Kind::Output);

    let Some(main_output) = ui::pick_main_output(terminal, &devices_out)? else { return Ok(()) };
    let routing: Arc<Mutex<Routing>> = Arc::new(Mutex::new(Routing::default()));
    routing.lock().unwrap_or_else(|e| e.into_inner()).set_main_output(Some(main_output));

    // Marks this process as "a mixer is running" for any odj-player deck
    // or odj-sampler that checks — see `odj_core::control`'s own module
    // doc comment. Kept alive for the rest of `start`; dropping it (process exit)
    // removes the marker. No protocol, nothing to hold onto beyond this.
    let _presence = odj_core::control::Control::open("mixer").ok();

    let mut settings = odj_core::memory::load_settings();
    let midi_port = odj_core::midi::pick_midi_input(terminal, settings.mixer_midi_port_id.as_deref())?;
    // Only remembered when one's actually picked — skipping (Esc/N) leaves
    // whatever was saved before alone, same convention odj-player's old
    // MIDI setup used.
    if midi_port.is_some() {
        settings.mixer_midi_port_id = midi_port.clone();
        let _ = odj_core::memory::save_settings(&settings);
    }
    let targets =
        Arc::new(Targets::new(settings.mixer_midi_knobs.iter().map(|(target, &cc)| (cc, target.clone())).collect()));

    let learning: Arc<Learning> = Arc::new(Mutex::new(None));
    let midi_conn = midi_port.as_deref().and_then(|id| {
        let routing = routing.clone();
        midi::connect(id, targets.clone(), learning.clone(), move |target, t| {
            dispatch(target, t, &routing);
        })
    });

    let midi_connected = midi_conn.is_some();
    if midi_connected {
        // Already-running decks need a moment to be scanned before
        // deciding what's left unmapped — the common case is starting
        // odj-mixer with decks already up, wanting to map them right
        // away rather than discovering `L` on their own.
        ui::wait_for_decks(terminal, Duration::from_millis(300))?;
        ui::learn_unmapped(terminal, &devices_deck, &settings.mixer_midi_knobs, &targets, &learning)?;
    }

    let result = ui::run(
        terminal,
        &devices_in,
        &devices_deck,
        &devices_out,
        &routing,
        &targets,
        &learning,
        midi_connected,
        settings.mixer_active_inputs,
    );
    drop(midi_conn);
    result
}

/// Applies one MIDI knob's absolute position (`t`, `0.0..=1.0`) to
/// whichever input/parameter `target` names (scaled into
/// `MIN_DB..=MAX_DB` for EQ, same convention odj-player's old knob wiring
/// used) — a no-op if that input isn't routed to anything right now.
fn dispatch(target: &str, t: f32, routing: &Mutex<Routing>) {
    let Some(target) = midi::parse_target(target) else { return };
    let value = if target.param == Param::Volume { t } else { MIN_DB + t * (MAX_DB - MIN_DB) };
    routing.lock().unwrap_or_else(|e| e.into_inner()).set_param(&target.input, target.param, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use odj_core::audio::Route;

    #[test]
    fn dispatch_ignores_a_target_thats_not_routed_anywhere() {
        let routing = Mutex::new(Routing::default());
        // No panic, no route created out of thin air.
        dispatch("input:card#s0-1:volume", 0.3, &routing);
        assert!(routing.lock().unwrap().output_of(&("card".into(), Route::Stereo(0, 1))).is_none());
    }

    #[test]
    fn dispatch_ignores_garbage() {
        let routing = Mutex::new(Routing::default());
        dispatch("nonsense", 0.3, &routing); // doesn't panic
    }
}
