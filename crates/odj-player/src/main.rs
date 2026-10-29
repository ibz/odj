mod app;
mod engine;
mod stretch;
mod sync;
mod ui;
mod video;

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use odj_core::library::{Browser, is_audio};
use odj_core::memory::Memory;
use odj_core::{audio, tui};

use app::{App, AudioRoute};
use engine::Deck;
use sync::SyncLink;

/// What `--deck`'s picker/the virtual-sink path has going by the time
/// everything else in `start` can proceed the same way regardless of
/// which one actually ran.
struct Setup {
    deck: Arc<Mutex<Deck>>,
    output: AudioRoute,
    link: io::Result<SyncLink>,
    errors: audio::Errors,
    _stream: cpal::Stream,
    /// Only `Some` on the through-odj-mixer path — kept alive for as
    /// long as this deck runs; dropping it tears the virtual sink down.
    _sink: Option<audio::NullSink>,
}

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
                     the first.\n\n\
                     No Mixer here any more (no Volume/EQ, filters live in odj-mixer now):\n\
                     if odj-mixer is running when this starts, this deck routes its output\n\
                     through it instead of a real device, and odj-mixer applies Volume/EQ\n\
                     live to that — pick it there, as \"Deck n\". Otherwise this plays\n\
                     straight to a real output, same as always, with no filters available."
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

fn start(
    terminal: &mut DefaultTerminal,
    browser: Browser,
    file: Option<PathBuf>,
    key_release: bool,
    memory: Memory,
    pick_music_dir: bool,
    deck_number: Option<u8>,
) -> Result<()> {
    // If odj-mixer is running right now, route through a virtual sink for
    // it to capture and apply its own filters to instead of going
    // straight to a real device — see `setup_through_mixer`'s own doc
    // comment. A one-time check at startup, not re-checked afterward (if
    // odj-mixer quits or starts later mid-session, this deck doesn't
    // switch sides on its own).
    let Some(setup) = (if odj_core::control::any_running("mixer") {
        setup_through_mixer(deck_number).map(Some)
    } else {
        setup_direct(terminal, deck_number)
    })?
    else {
        return Ok(());
    };

    let mut app = App::new(setup.deck, browser, key_release, setup.output, memory);
    if let Err(e) = &setup.link {
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
    let result = run(terminal, &mut app, &setup.errors);
    app.remember();
    result
}

/// The usual path: pick a real output and play straight to it, same as
/// always. `Ok(None)` if the user backs out of the output picker.
fn setup_direct(terminal: &mut DefaultTerminal, deck_number: Option<u8>) -> Result<Option<Setup>> {
    let host = audio::host();
    let Some(output) = tui::pick_output(terminal, &host)? else { return Ok(None) };
    let deck = Arc::new(Mutex::new(Deck::new(output.sample_rate())));
    let stream = output.start(deck.clone())?;
    if let Err(e) = audio::verify_route(&host, &output) {
        drop(stream); // tear down rather than play on from the wrong device
        return Err(e);
    }
    // `deck_number` picks which deck this is, shown in the header as
    // "DECK n": explicit `--deck` wins outright; otherwise default to
    // whichever number sync claims (the first one free, 1 if nothing else
    // is running).
    let link = SyncLink::start(deck.clone(), deck_number);
    let route = AudioRoute::Direct(format!("{} · {} Hz", output.name, output.sample_rate()));
    Ok(Some(Setup { deck, output: route, link, errors: output.errors, _stream: stream, _sink: None }))
}

/// Instead of asking for a real output, creates a virtual sink (see
/// `audio::NullSink`) named for whichever deck number sync claims (no
/// picker — nothing to ask, there's exactly one sensible target) and
/// plays into that; odj-mixer discovers it the same way it discovers any
/// other input, labeled "Deck n", and applies Volume/EQ to what it
/// captures from it. The sample rate doesn't matter here the way it does
/// for a real device — PipeWire resamples everything to its own shared
/// clock regardless (the same property that lets odj-mixer route between
/// physically different cards at all).
fn setup_through_mixer(deck_number: Option<u8>) -> Result<Setup> {
    const RATE: u32 = 48_000;
    let deck = Arc::new(Mutex::new(Deck::new(RATE)));
    let link = SyncLink::start(deck.clone(), deck_number);
    let n = deck_number.or_else(|| link.as_ref().ok().map(SyncLink::deck)).unwrap_or(1);

    let sink_name = audio::deck_sink_name(n);
    let sink = audio::NullSink::create(&sink_name, &format!("Deck {n}"))?;

    let host = audio::host(); // fresh: `host()` caches its own enumeration, so this must postdate creating the sink
    let outputs = audio::output_cards(&host)?;
    let choice = audio::choices(&outputs)
        .into_iter()
        .find(|c| outputs[c.card].id() == sink_name)
        .ok_or_else(|| anyhow!("the virtual sink didn't register with PipeWire in time"))?;
    let output = audio::Output::open(&host, &outputs, &choice)?;
    let stream = output.start(deck.clone())?;
    if let Err(e) = audio::verify_route(&host, &output) {
        drop(stream);
        return Err(e);
    }
    Ok(Setup { deck, output: AudioRoute::Mixer, link, errors: output.errors, _stream: stream, _sink: Some(sink) })
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
