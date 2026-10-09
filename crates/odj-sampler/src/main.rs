//! odj-sampler: plays the cues stored by odj-player from 16 pads.

mod app;
mod engine;
mod kit;
mod loader;
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
             The kit is kept in {}.",
            Kit::file().display()
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
    let _stream = output.start(sampler.clone())?;

    let name = format!("{} · {} Hz", output.name, output.sample_rate());
    let mut app = App::new(sampler, output.sample_rate(), Kit::file(), key_release, name);
    run(terminal, &mut app)
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
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
