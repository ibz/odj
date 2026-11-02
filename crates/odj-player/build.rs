//! Links `librubberband` (Master Tempo's time-stretch — see `src/stretch.rs`'s
//! `#[link(name = "rubberband")]`). On Linux the system linker already finds
//! it on the default search path with no help (this crate built with no
//! build script at all until now); this exists for platforms where it isn't,
//! e.g. Homebrew's copy on macOS, which `pkg-config` finds via its own
//! `rubberband.pc`.
//!
//! A failed probe (pkg-config itself missing, or no `.pc` file shipped) is a
//! silent no-op: falls through to the plain `#[link(...)]` directive exactly
//! as if this file didn't exist, so this can never make an already-working
//! Linux build worse.

fn main() {
    let _ = pkg_config::probe_library("rubberband");
}
