mod app;
mod audio;
mod engine;
mod library;
mod memory;
mod stretch;
mod track;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

use app::App;
use engine::Deck;
use library::{Browser, is_audio};

fn main() -> Result<()> {
    let arg = std::env::args_os().nth(1).map(PathBuf::from);
    if arg.as_deref().is_some_and(|a| a == "-h" || a == "--help") {
        println!("usage: odj [FOLDER | FILE]\n\nA keyboard-driven DJ player. Press ? inside for keys.");
        return Ok(());
    }
    let (dir, file) = match arg {
        Some(p) if p.is_file() => (p.parent().map(PathBuf::from).unwrap_or_default(), Some(p)),
        Some(p) => (p, None),
        None => {
            let music = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Music"));
            (music.filter(|m| m.is_dir()).unwrap_or_else(|| PathBuf::from(".")), None)
        }
    };
    let dir = dir.canonicalize().unwrap_or(dir);

    let output = audio::Output::open()?;
    let deck = Arc::new(Mutex::new(Deck::new(output.sample_rate())));
    let _stream = output.start(deck.clone())?;

    let mut browser = Browser::new(dir);
    if let Some(f) = &file {
        let f = f.canonicalize().unwrap_or(f.clone());
        browser.selected = browser.entries.iter().position(|e| e.path == f).unwrap_or(0);
    }

    let mut terminal = ratatui::init();
    let key_release = supports_keyboard_enhancement().unwrap_or(false);
    if key_release {
        // Release events for plain text keys also need "all keys as escape codes".
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        );
    }

    let name = format!("{} · {} Hz", output.name, output.sample_rate());
    let mut app = App::new(deck, browser, key_release, name);
    match file {
        Some(f) if is_audio(&f) => {
            app.load(f);
        }
        _ => app.browser.open = true,
    }
    let result = run(&mut terminal, &mut app);
    app.remember();

    if key_release {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
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
