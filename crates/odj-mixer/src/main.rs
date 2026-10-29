//! odj-mixer: continuously discovers remote-controlled odj-player decks
//! and a running odj-sampler, connecting every one of them to its main
//! output automatically, live — a hardware sound-card input instead
//! waits until it's added from Settings, which also holds its own
//! Outputs tab for picking or changing that main output any time
//! (restored from the previous session at startup, else the internal
//! speakers by default — no mandatory picker any more) — and lets you
//! control each channel's Volume and 3-band EQ by hand or by MIDI knob.
//! A MIDI controller it's already seen before needs no picker either
//! (Settings' own MIDI tab can always switch to a different one, known
//! or not, live); see [`start`]'s own doc comment for exactly how that
//! one's chosen.

mod devices;
mod log;
mod midi;
mod recorder;
mod routing;
mod ui;

use std::collections::HashMap;
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
             Continuously detects running odj-player decks (as \"Deck n\") and a running\n\
             odj-sampler (as \"Sampler\"), connecting every one of them to the main output\n\
             automatically, showing a channel strip — High, Mid, Low, Volume — for each.\n\
             ←/→ picks the channel and ↑/↓ picks its control — neither changes a value, just\n\
             navigates; +/- adjusts the selected one instead (Shift: ×4).\n\
             ` opens Settings, 3 tabs (Tab/Shift+Tab cycles either way). Inputs: one row\n\
             per detected input, deck and the sampler, plus one for any device that's no\n\
             longer detected but still has a MIDI mapping. Space adds the selected input to\n\
             the mix or removes it (a no-op on a deck's or the sampler's row — always\n\
             connected automatically — or on an undetected one, since there's nothing there\n\
             to connect); an added input is remembered and reconnected automatically on the\n\
             next run, same as those are. Enter drills into any row's own settings — for now\n\
             just its 4 MIDI mappings, Volume/Low/Mid/High, where Enter maps a knob to the\n\
             selected one and X clears it, Esc backs out. Outputs: one row per detected\n\
             output; Enter makes the selected one the main output, remembered the same way —\n\
             so there's no picker to get through before odj-mixer can even start (it starts\n\
             on whatever was picked last, or the internal speakers if nothing was yet), and\n\
             the output can be changed again any time from here, not just chosen once\n\
             upfront. MIDI: one row per MIDI controller odj-mixer has saved mappings for,\n\
             plus one for each currently plugged in that has none yet (each row names how\n\
             many mappings it has); Enter switches to the selected one live if it's plugged\n\
             in, X/Delete forgets its mappings (a no-op if it has none). Esc/` closes\n\
             Settings, any tab.\n\
             Nothing on the live screen itself changes a mapping, the output or the\n\
             controller; that's Settings-only.\n\
             Picking a MIDI controller at startup needs no picker either, the same way the\n\
             output doesn't, as long as one it's already learned mappings for is plugged in\n\
             (the last-used one if more than one qualifies) — only a screen full of unknown\n\
             ones falls back to asking (skip with Esc/N). Once one's picked, if any decks are\n\
             already running, walks you through mapping a knob to each deck's (and the\n\
             sampler's, if it's running) Volume/Low/Mid/High that isn't mapped yet on that\n\
             controller. R starts/stops recording the main output to a timestamped WAV file\n\
             under ~/.local/share/odj/recordings/. Press ? for the full list of keys.\n\
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

/// Picks the main output (see [`devices::builtin_output`]/
/// `Settings.mixer_output`) and a MIDI controller, then runs
/// [`ui::run`] until it quits. The controller: exactly one *known*
/// one (has at least one mapping in `Settings.mixer_midi_controllers`
/// already — see `ui::bind_knob`) currently plugged in needs no
/// picker at all; more than one known one present uses whichever
/// `Settings.mixer_midi_last` names, or the first by name if the
/// last-used one isn't among them (forgotten, or this is simply the
/// first time two have been present together); only when every
/// currently plugged-in controller is unknown does this fall through
/// to [`odj_core::midi::pick_midi_input`], same as always. Either
/// way, it's remembered as the last-used one (see
/// `ui::set_controller`) — but it only becomes "known" once something
/// actually gets mapped on it; an unmapped controller isn't worth
/// remembering as its own entry. Settings' own MIDI tab can always
/// switch to a different controller live afterward, known or not.
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

    let mut settings = odj_core::memory::load_settings();
    let routing: Arc<Mutex<Routing>> = Arc::new(Mutex::new(Routing::default()));
    // Restores the last session's output if one was saved; otherwise
    // falls back to the internal speakers (see
    // `devices::builtin_output`) rather than leaving nothing picked —
    // no more mandatory startup picker. Settings' own Outputs tab
    // (`ui::draw_settings`) can always override this, any time, which
    // saves the choice from then on. Left `None` only if nothing saved
    // parses (a settings file from before this existed) *and* no
    // onboard output was found either.
    let initial_output =
        settings.mixer_output.as_deref().and_then(devices::parse_device_key).or_else(devices::builtin_output);
    if let Some(key) = initial_output {
        routing.lock().unwrap_or_else(|e| e.into_inner()).set_main_output(Some(key));
    }

    // Marks this process as "a mixer is running" for any odj-player deck
    // or odj-sampler that checks — see `odj_core::control`'s own module
    // doc comment. Kept alive for the rest of `start`; dropping it (process exit)
    // removes the marker. No protocol, nothing to hold onto beyond this.
    let _presence = odj_core::control::Control::open("mixer").ok();

    // Picking a MIDI controller: a *known* one (already in
    // `mixer_midi_controllers` — see `ui::bind_knob`/`set_controller`)
    // that's currently plugged in needs no picker at all — the
    // last-used one specifically (`mixer_midi_last`) if more than one
    // qualifies, falling back to whichever sorts first if the last-used
    // one isn't among them (e.g. it was forgotten, or this is the first
    // time two have been present together). Only when every currently
    // plugged-in controller is unknown does this fall through to asking,
    // same as always; picking one there (or skipping) is unchanged.
    let ports = odj_core::midi::list_ports();
    let present: Vec<(String, String)> = ports
        .iter()
        .filter_map(|(id, _)| odj_core::midi::device_slot(id).map(|(name, _)| (id.clone(), name)))
        .collect();
    let mut known_present: Vec<&(String, String)> =
        present.iter().filter(|(_, name)| settings.mixer_midi_controllers.contains_key(name)).collect();
    known_present.sort_by(|a, b| a.1.cmp(&b.1));
    let chosen = if known_present.len() == 1 {
        Some(known_present[0].clone())
    } else if !known_present.is_empty() {
        let last = settings.mixer_midi_last.as_deref();
        known_present.iter().find(|(_, name)| Some(name.as_str()) == last).or(known_present.first()).map(|&p| p.clone())
    } else {
        None
    };
    let (midi_port, controller) = match chosen {
        Some((id, name)) => (Some(id), Some(name)),
        None => match odj_core::midi::pick_midi_input(terminal, None)? {
            Some(id) => {
                let name = odj_core::midi::device_slot(&id).map(|(n, _)| n).unwrap_or_else(|| id.clone());
                (Some(id), Some(name))
            }
            None => (None, None),
        },
    };
    // Remembers the chosen controller as the last-used one — skipped
    // entirely if nothing was picked, same as the old port-id
    // convention this replaces. Deliberately doesn't register it among
    // `mixer_midi_controllers` itself if it isn't there already: an
    // entry with nothing mapped in it isn't a controller worth
    // remembering (see `ui::ControllerRow`) — `ui::bind_knob` is what
    // actually creates one, the first time something's mapped on it.
    let knobs = match &controller {
        Some(name) => {
            let knobs = settings.mixer_midi_controllers.get(name).cloned().unwrap_or_default();
            settings.mixer_midi_last = Some(name.clone());
            let _ = odj_core::memory::save_settings(&settings);
            knobs
        }
        None => HashMap::new(),
    };
    let targets = Arc::new(Targets::new(knobs.iter().map(|(target, &cc)| (cc, target.clone())).collect()));

    let learning: Arc<Learning> = Arc::new(Mutex::new(None));
    let midi_conn = midi_port.as_deref().and_then(|id| {
        let routing = routing.clone();
        midi::connect(id, targets.clone(), learning.clone(), move |target, t| {
            dispatch(target, t, &routing);
        })
    });

    if midi_conn.is_some() {
        // Already-running decks need a moment to be scanned before
        // deciding what's left unmapped — the common case is starting
        // odj-mixer with decks already up, wanting to map them right
        // away rather than discovering `L` on their own.
        ui::wait_for_decks(terminal, Duration::from_millis(300))?;
        ui::learn_unmapped(terminal, &devices_deck, controller.as_deref().unwrap_or_default(), &knobs, &targets, &learning)?;
    }

    ui::run(
        terminal,
        &devices_in,
        &devices_deck,
        &devices_out,
        &routing,
        &targets,
        &learning,
        midi_conn,
        controller,
        settings.mixer_active_inputs,
    )
}

/// Applies one MIDI knob's absolute position (`t`, `0.0..=1.0`) to
/// whichever input/parameter `target` names (scaled into
/// `MIN_DB..=MAX_DB` for EQ, same convention odj-player's old knob wiring
/// used) — a no-op if that input isn't routed to anything right now.
/// `pub(crate)` so `ui::run` can rebuild the same dispatch closure
/// when Settings' own MIDI tab switches to a different controller
/// live, rather than only ever wiring one up here at startup.
pub(crate) fn dispatch(target: &str, t: f32, routing: &Mutex<Routing>) {
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
