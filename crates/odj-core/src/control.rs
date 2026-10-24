//! A liveness marker, not a conversation: odj-mixer binds one of these at
//! startup so odj-player or odj-sampler can tell whether a mixer is
//! currently running — see [`Control::open`]/[`any_running`] — and, if
//! one is, route its own output through a virtual sink for odj-mixer to
//! capture and apply its own filters to, instead of going straight to a
//! real device. Follows
//! the same shared-directory, one-socket-per-process pattern as
//! `link.rs`'s deck-sync channel, just without anything to actually say
//! once bound: nobody sends or receives here, the file either existing or
//! not *is* the message.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

/// This process's claim to `role` in the shared control directory —
/// removed again on drop. The pid in the filename makes it unique, so
/// unlike `link::Link::open` there's no slot number to claim or retry.
/// The socket is never sent to or read from — only its bound path's
/// existence matters, to [`any_running`] — but it's kept here anyway
/// (rather than just leaving a plain file) so a stale marker left behind
/// by a crash is still tellable from a live one via the exact same
/// `is_stale` probe `link.rs`'s own sockets use.
pub struct Control {
    _socket: UnixDatagram,
    path: PathBuf,
}

impl Control {
    pub fn open(role: &str) -> io::Result<Self> {
        Self::open_in(&default_dir(), role)
    }

    pub fn open_in(dir: &Path, role: &str) -> io::Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let path = dir.join(format!("{role}-{}", std::process::id()));
        if path.exists() && is_stale(&path) {
            let _ = fs::remove_file(&path);
        }
        let socket = UnixDatagram::bind(&path)?;
        Ok(Self { _socket: socket, path })
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Whether any process currently has `Control::open(role)` bound in the
/// shared directory — odj-player's and odj-sampler's way of asking "is
/// a mixer running?" without any protocol to speak to find out.
pub fn any_running(role: &str) -> bool {
    any_running_in(&default_dir(), role)
}

fn any_running_in(dir: &Path, role: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    let prefix = format!("{role}-");
    entries.flatten().any(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)) && !is_stale(&e.path()))
}

fn default_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("odj/control"),
        None => std::env::temp_dir().join(format!("odj-{}/control", unsafe { libc::getuid() })),
    }
}

/// A socket file nobody is bound to any more.
fn is_stale(path: &Path) -> bool {
    UnixDatagram::unbound()
        .and_then(|s| s.send_to(&[], path))
        .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionRefused)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("odj-control-test-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn any_running_is_false_until_something_opens_that_role() {
        let dir = temp_dir("presence");
        assert!(!any_running_in(&dir, "mixer"), "nothing has opened yet");
        {
            let _mixer = Control::open_in(&dir, "mixer").unwrap();
            assert!(any_running_in(&dir, "mixer"));
            assert!(!any_running_in(&dir, "player"), "a different role isn't satisfied by this one");
        }
        assert!(!any_running_in(&dir, "mixer"), "dropping it removes the marker");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn any_running_ignores_a_stale_marker_left_behind_by_a_crash() {
        let dir = temp_dir("stale");
        fs::DirBuilder::new().recursive(true).create(&dir).unwrap();
        fs::write(dir.join("mixer-999999999"), b"").unwrap(); // not a real bound socket
        assert!(!any_running_in(&dir, "mixer"));
        let _ = fs::remove_dir_all(&dir);
    }
}
