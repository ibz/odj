mod app;
mod engine;
mod stretch;
mod ui;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use odj_core::library::{Browser, is_audio};
use odj_core::{audio, tui};

use app::App;
use engine::Deck;

fn main() -> Result<()> {
    let arg = std::env::args_os().nth(1).map(PathBuf::from);
    if arg.as_deref().is_some_and(|a| a == "-h" || a == "--help") {
        println!("usage: odj-player [FOLDER | FILE]\n\nA keyboard-driven DJ player. Press ? inside for keys.");
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

    let mut browser = Browser::new(dir);
    if let Some(f) = &file {
        let f = f.canonicalize().unwrap_or(f.clone());
        browser.select(&f);
    }

    let (mut terminal, key_release) = tui::init();
    let result = start(&mut terminal, browser, file, key_release);
    tui::restore(key_release);
    result
}

fn start(terminal: &mut DefaultTerminal, browser: Browser, file: Option<PathBuf>, key_release: bool) -> Result<()> {
    let host = audio::host();
    let Some(output) = tui::pick_output(terminal, &host)? else {
        return Ok(());
    };
    let deck = Arc::new(Mutex::new(Deck::new(output.sample_rate())));
    let _stream = output.start(deck.clone())?;

    let name = format!("{} · {} Hz", output.name, output.sample_rate());
    let mut app = App::new(deck, browser, key_release, name);
    match file {
        Some(f) if is_audio(&f) => {
            app.load(f);
        }
        _ => app.browser.open = true,
    }
    let result = run(terminal, &mut app);
    app.remember();
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
