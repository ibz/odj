mod app;
mod engine;
mod stretch;
mod sync;
mod ui;

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
    let arg = std::env::args_os().nth(1).map(PathBuf::from);
    if arg.as_deref().is_some_and(|a| a == "-h" || a == "--help") {
        println!("usage: odj-player [FOLDER | FILE]\n\nA keyboard-driven DJ player. Press ? inside for keys.");
        return Ok(());
    }
    let memory = Memory::load();
    let (dir, file) = match arg {
        Some(p) if p.is_file() => (p.parent().map(PathBuf::from).unwrap_or_default(), Some(p)),
        Some(p) => (p, None),
        None => (default_music_dir(&memory), None),
    };
    let dir = dir.canonicalize().unwrap_or(dir);

    let mut browser = Browser::new(dir);
    if let Some(f) = &file {
        let f = f.canonicalize().unwrap_or(f.clone());
        browser.select(&f);
    }

    let (mut terminal, key_release) = tui::init();
    let result = start(&mut terminal, browser, file, key_release, memory);
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

    let name = format!("{} · {} Hz", output.name, output.sample_rate());
    let link = SyncLink::start(deck.clone());
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
