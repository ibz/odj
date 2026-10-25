//! A plain-text, append-only log of every error odj-mixer's shown on
//! screen — `ui::draw_status_popup` shows one in a popup, but it's gone
//! the instant the next key dismisses it (any key does) or something
//! replaces it, often too fast to read in full or copy before it's
//! needed for a bug report. Every message shown there is also appended
//! here, with when it happened, so it can be found again after the
//! fact. Anywhere `ui.rs` sets its `status` to a new message should
//! call [`log`] too, right alongside — nothing enforces that
//! automatically, so it's worth checking this doc comment still holds
//! if another such call site is ever added.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// `$XDG_DATA_HOME/odj/mixer.log` (or `~/.local/share/odj/mixer.log`)
/// — the same directory the other apps already keep their own data
/// in, rather than inventing a new location just for this.
pub fn log_file() -> PathBuf {
    odj_core::memory::data_dir().join("mixer.log")
}

/// Appends `message` to [`log_file`], one line, prefixed with the
/// local date and time it was called. Best-effort: a failure to write
/// (a full disk, a permissions problem, the directory not existing yet
/// and failing to create) is silently ignored rather than crashing the
/// app over what's itself just a debugging aid.
pub fn log(message: &str) {
    let file = log_file();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(f, "{} {message}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points `XDG_DATA_HOME` at a fresh temp directory for the span of
    /// one test, same isolation trick odj-player's own `main.rs` tests
    /// use — tests across the crate share this process's environment,
    /// so each test needs its own never-reused directory rather than a
    /// shared one two tests running in parallel could collide on.
    fn isolate() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("odj-mixer-log-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &root);
        }
        root
    }

    #[test]
    fn log_appends_a_timestamped_line_without_clobbering_earlier_ones() {
        let root = isolate();
        log("first message");
        log("second message");
        let contents = std::fs::read_to_string(root.join("odj/mixer.log")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            // "YYYY-MM-DD HH:MM:SS " is 20 chars before the message.
            assert!(line.len() > 20, "{line}");
            assert!(line.chars().nth(4) == Some('-'), "starts with a date: {line}");
        }
        assert!(lines[0].ends_with("first message"));
        assert!(lines[1].ends_with("second message"));
    }
}
