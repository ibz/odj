mod app;
mod engine;
mod midi;
mod stretch;
mod sync;
mod ui;
mod video;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use odj_core::library::{Browser, is_audio};
use odj_core::memory::Memory;
use odj_core::{audio, tui};

use app::App;
use engine::Deck;
use sync::SyncLink;

fn main() -> Result<()> {
    let mut deck_number: Option<u8> = None;
    let mut positional: Option<PathBuf> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str().map(str::to_string).as_deref() {
            Some("-h") | Some("--help") => {
                println!(
                    "usage: odj-player [FOLDER | FILE] [--deck 1|2]\n\n\
                     A keyboard-driven DJ player. Press ? inside for keys.\n\
                     Which deck this is (shown as \"DECK n\") defaults to whichever number\n\
                     sync claims — 1 if nothing else is running — so --deck is usually only\n\
                     needed to force a specific number, e.g. running a second deck before\n\
                     the first. It also tells this process which half of a shared MIDI\n\
                     controller's Mixer-knob mapping (Volume, Low, Mid, High) is its own,\n\
                     picked at startup; the port and each device's learned CCs are kept in\n\
                     the settings file shared with odj-sampler, {}.",
                    odj_core::memory::settings_file().display()
                );
                return Ok(());
            }
            Some("--deck") => match args.next().and_then(|v| v.to_str()?.parse::<u8>().ok()) {
                Some(n @ (1 | 2)) => deck_number = Some(n),
                _ => {
                    eprintln!("--deck needs 1 or 2");
                    return Ok(());
                }
            },
            _ => positional = Some(PathBuf::from(a)),
        }
    }
    let memory = Memory::load();
    // No folder named on the command line, and more than one saved to
    // choose from: let the user pick instead of silently guessing one.
    let mut pick_music_dir = false;
    let (dir, file) = match positional {
        Some(p) if p.is_file() => (p.parent().map(PathBuf::from).unwrap_or_default(), Some(p)),
        Some(p) => (p, None),
        None => {
            pick_music_dir = memory.settings.music_dirs.len() > 1;
            (default_music_dir(&memory), None)
        }
    };
    let dir = dir.canonicalize().unwrap_or(dir);

    let mut browser = Browser::new(dir);
    if let Some(f) = &file {
        let f = f.canonicalize().unwrap_or(f.clone());
        browser.select(&f);
    }

    let (mut terminal, key_release) = tui::init();
    let result = start(&mut terminal, browser, file, key_release, memory, pick_music_dir, deck_number);
    tui::restore(key_release);
    result
}

/// Where the browser opens with no folder on the command line: the first
/// saved music folder that still exists, else `~/Music`, else home, else
/// the current directory.
fn default_music_dir(memory: &Memory) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    memory
        .settings
        .music_dirs
        .iter()
        .cloned()
        .chain(home.as_ref().map(|h| h.join("Music")))
        .chain(home)
        .find(|d| d.is_dir())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Offers a MIDI input for the Mixer's knobs (Volume, Low, Mid, High),
/// disambiguated by `deck_number` (1 or 2, from `--deck`) — see `midi.rs`'s
/// module doc comment for why that's needed. Mirrors odj-sampler's own
/// pick-port → (load-or-learn) → save → connect sequence in its
/// `main.rs::start`, but for CC knobs instead of note pads.
fn setup_knob_midi(
    terminal: &mut DefaultTerminal,
    deck: &Arc<Mutex<Deck>>,
    deck_number: u8,
) -> Result<Option<midir::MidiInputConnection<()>>> {
    let mut knobs = midi::KnobConfig::load();
    let midi_port = odj_core::midi::pick_midi_input(terminal, knobs.port_id.as_deref())?;
    let deck_index = (deck_number - 1) as usize;
    let Some((name, slot)) = midi_port.as_deref().and_then(odj_core::midi::device_slot) else { return Ok(None) };
    let id = midi_port.unwrap();
    knobs.port_id = Some(id.clone());
    // A device/slot we haven't seen before (for this deck): walk the user
    // through turning each knob so we learn its CC, rather than assuming a
    // layout. Unrecognized device, same `None` either way: the port's
    // gone, or the user cancelled partway through.
    let ccs = match knobs.knobs_for(&name, slot, deck_index) {
        Some(ccs) => Some(ccs),
        None => midi::learn_knobs(terminal, &id)?,
    };
    Ok(ccs.and_then(|ccs| {
        knobs.set_knobs_for(name, slot, deck_index, ccs);
        let _ = knobs.save();
        midi::connect(&id, deck.clone(), ccs)
    }))
}

fn start(
    terminal: &mut DefaultTerminal,
    browser: Browser,
    file: Option<PathBuf>,
    key_release: bool,
    memory: Memory,
    pick_music_dir: bool,
    deck_number: Option<u8>,
) -> Result<()> {
    let host = audio::host();
    let Some(output) = tui::pick_output(terminal, &host)? else {
        return Ok(());
    };
    let deck = Arc::new(Mutex::new(Deck::new(output.sample_rate())));
    let stream = output.start(deck.clone())?;
    if let Err(e) = audio::verify_route(&host, &output) {
        drop(stream); // tear down rather than play on from the wrong device
        return Err(e);
    }

    // `deck_number` picks which deck this is, for both sync (shown in the
    // header as "DECK n") and the Mixer's MIDI knobs: explicit `--deck`
    // wins outright; otherwise default to whichever number sync claims
    // (the first one free, 1 if nothing else is running) — the common
    // case, needing no flag at all once only one process wants MIDI knobs.
    let link = SyncLink::start(deck.clone(), deck_number);
    let deck_number = deck_number.or_else(|| link.as_ref().ok().map(SyncLink::deck));

    // Kept alive for the rest of `start`, closing automatically when it
    // returns. Only offered for deck 1 or 2 — the Mixer's knob mapping only
    // has two halves (see `midi.rs`'s module doc comment) — so a 3rd/4th
    // synced deck simply has no MIDI knobs unless given its own `--deck
    // 1`/`--deck 2` explicitly (sharing a slot with another deck's knobs).
    let midi_conn = match deck_number {
        Some(n @ (1 | 2)) => setup_knob_midi(terminal, &deck, n)?,
        _ => None,
    };

    let name =
        format!("{} · {} Hz{}", output.name, output.sample_rate(), if midi_conn.is_some() { " · MIDI" } else { "" });
    let mut app = App::new(deck, browser, key_release, name, memory);
    if let Err(e) = &link {
        app.set_status(format!("Sync unavailable: {e}"));
    }
    match file {
        Some(f) if is_audio(&f) => {
            app.load(f);
        }
        // Nothing configured yet: straight to "my music folders" instead of
        // a browser that has nowhere useful to call home.
        _ if app.music_dirs().is_empty() => app.settings_screen.open_to_music_dirs(),
        // Several saved folders and no folder named on the command line:
        // let the user pick instead of opening whichever one happened to
        // be saved first.
        _ if pick_music_dir => app.settings_screen.open_to_switch(0),
        _ => app.browser.open = true,
    }
    let result = run(terminal, &mut app, &output.errors);
    app.remember();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use odj_core::memory::{Settings, write_json};

    /// Loads a `Memory` with `XDG_CONFIG_HOME`/`HOME` pointed at `root`, and
    /// `root/config/odj/settings.json` holding `music_dirs`.
    fn memory_at(root: &std::path::Path, music_dirs: &[&std::path::Path]) -> Memory {
        let config = root.join("config");
        std::fs::create_dir_all(config.join("odj")).unwrap();
        let settings =
            Settings { music_dirs: music_dirs.iter().map(|p| p.to_path_buf()).collect(), ..Settings::default() };
        write_json(&config.join("odj/settings.json"), &settings).unwrap();
        // Tests across the crate share this process's environment, so only
        // mutate it for the span of this one test, which sets it right back.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &config);
            std::env::set_var("XDG_DATA_HOME", &config);
            std::env::set_var("HOME", root);
        }
        Memory::load()
    }

    #[test]
    fn music_dir_falls_back_in_order() {
        let root = std::env::temp_dir().join(format!("odj-main-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (saved, saved2, music, gone) = (root.join("saved"), root.join("saved2"), root.join("Music"), root.join("gone"));
        std::fs::create_dir_all(&saved).unwrap();
        std::fs::create_dir_all(&saved2).unwrap();
        std::fs::create_dir_all(&music).unwrap();

        // The first saved folder wins when it still exists.
        let memory = memory_at(&root, &[&saved, &saved2]);
        assert_eq!(default_music_dir(&memory), saved);

        // A first entry that's gone falls through to the next saved folder.
        let memory = memory_at(&root, &[&gone, &saved2]);
        assert_eq!(default_music_dir(&memory), saved2);

        // None of the saved folders exist: falls back to ~/Music.
        let memory = memory_at(&root, &[&gone]);
        assert_eq!(default_music_dir(&memory), music);

        // Nothing saved falls back to ~/Music too.
        let memory = memory_at(&root, &[]);
        assert_eq!(default_music_dir(&memory), music);

        // No ~/Music either: falls back to home.
        std::fs::remove_dir_all(&music).unwrap();
        let memory = memory_at(&root, &[]);
        assert_eq!(default_music_dir(&memory), root);
    }
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, errors: &audio::Errors) -> Result<()> {
    while !app.quit {
        if let Some(e) = errors.take() {
            app.set_status(e);
        }
        app.tick();
        let snapshot = app.deck.lock().unwrap_or_else(|e| e.into_inner()).snapshot();
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
