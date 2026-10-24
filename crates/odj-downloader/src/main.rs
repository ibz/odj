//! odj-downloader: fetches tracks via yt-dlp for later use in odj-player.

mod app;
mod queue;
mod search;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

use app::App;
use search::YtDlp;

fn main() -> Result<()> {
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        println!(
            "usage: odj-downloader\n\n\
             Search YouTube, queue a result, it downloads into {}.",
            downloads_dir().display()
        );
        return Ok(());
    }
    let (mut terminal, enhanced) = init();
    let result = run(&mut terminal);
    restore(enhanced);
    result
}

/// `~/Downloads`, or the current directory if `$HOME` isn't set.
fn downloads_dir() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_else(|| PathBuf::from("."))
}

/// Opens the full-screen terminal. This tool has no holds (nothing needs a
/// key-release event, unlike odj-player's jog/search), so unlike
/// odj-core's `tui::init` this doesn't ask for `REPORT_EVENT_TYPES` — just
/// disambiguation, so modified keys (e.g. Ctrl+C) parse unambiguously. The
/// flag says whether the terminal supports the enhancement at all. Kept
/// local rather than depending on odj-core for this, since odj-core's own
/// dependencies (symphonia, cpal, pipewire) are unconditional and this tool
/// never touches audio.
fn init() -> (DefaultTerminal, bool) {
    let terminal = ratatui::init();
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        );
    }
    (terminal, enhanced)
}

/// Undoes `init`.
fn restore(enhanced: bool) {
    // A keystroke this app never got around to reading — most likely a
    // trailing release event right as the very key that's quitting it
    // (e.g. Ctrl+C) is read — would otherwise sit unread in the
    // terminal's own input buffer and reach the shell as this
    // protocol's raw escape codes once this app's gone and the shell
    // itself (which doesn't speak it) reads it back. Drain it first,
    // while the terminal's still sending in the mode that produced it.
    while let Ok(true) = event::poll(Duration::ZERO) {
        let _ = event::read();
    }
    if enhanced {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
}

fn run(terminal: &mut DefaultTerminal) -> Result<()> {
    std::fs::create_dir_all(downloads_dir()).ok();
    let mut app = App::new(Arc::new(YtDlp), downloads_dir());
    while !app.quit {
        app.tick();
        terminal.draw(|f| ui::draw(f, &app))?;
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
