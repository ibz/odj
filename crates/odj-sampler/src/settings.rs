//! Which kit is active — odj-sampler's one field in the settings file it
//! shares with odj-player (`$XDG_CONFIG_HOME/odj/settings.json`, see
//! `odj_core::memory::Settings`). Reads and writes always go through the
//! whole shared struct, so saving here never resets odj-player's own
//! fields (`auto_cue`, `music_dirs`, …) to their defaults.

use std::path::{Path, PathBuf};

use odj_core::memory::{load_settings, save_settings};

/// Prefix for the shared settings' `kit` field when the active kit is one
/// of the built-in kits, followed by its stable id. It can't collide with
/// a real kit directory path, which is always absolute (starts with `/`).
const EMBEDDED_KIT_PREFIX: &str = "@embedded:";

/// What the settings say is active: a real kit directory, or a built-in
/// kit id (which has no directory of its own until it's changed and saved).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActiveKit {
    Saved(PathBuf),
    Embedded(String),
}

/// The active kit, if the shared settings name one and it's still
/// available — `None` covers both "never set" and "set, but since
/// deleted," so the caller always ends up offering a kit to pick. Embedded
/// ids aren't checked against the current built-in list here; the caller
/// does that and falls back to the picker for an unknown one.
///
/// The non-embedded case stores the kit's own full directory path, not a
/// slug relative to `kits_dir()` — a kit can live in the global kits
/// folder or in any active crate's own `kits/`, so only a full path
/// finds it regardless of which.
pub fn load_active_kit() -> Option<ActiveKit> {
    let raw = load_settings().kit?;
    if let Some(id) = raw.strip_prefix(EMBEDDED_KIT_PREFIX)
        && !id.is_empty()
    {
        return Some(ActiveKit::Embedded(id.to_owned()));
    }
    let dir = PathBuf::from(raw);
    dir.join("kit.json").is_file().then_some(ActiveKit::Saved(dir))
}

/// Sets (or, with `None`, clears) the active kit by its full directory,
/// leaving every other field in the shared settings file exactly as it was.
pub fn set_active_kit(dir: Option<&Path>) {
    let mut settings = load_settings();
    settings.kit = dir.map(|d| d.to_string_lossy().into_owned());
    let _ = save_settings(&settings);
}

/// Remembers one of the built-in kits by its stable id.
pub fn set_active_embedded_kit(id: &str) {
    let mut settings = load_settings();
    settings.kit = Some(format!("{EMBEDDED_KIT_PREFIX}{id}"));
    let _ = save_settings(&settings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits;

    fn with_temp_dirs<T>(f: impl FnOnce() -> T) -> T {
        let _guard = kits::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-settings-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only, same pattern used throughout this crate's tests.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            std::env::set_var("XDG_DATA_HOME", &dir);
        }
        let result = f();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("XDG_DATA_HOME");
        }
        std::fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn missing_file_is_no_active_kit() {
        with_temp_dirs(|| assert!(load_active_kit().is_none()));
    }

    #[test]
    fn embedded_marker_carries_its_id_and_needs_no_directory() {
        with_temp_dirs(|| {
            set_active_embedded_kit("acoustic");
            assert_eq!(load_active_kit(), Some(ActiveKit::Embedded("acoustic".into())));
            assert_eq!(load_settings().kit.as_deref(), Some("@embedded:acoustic"));
        });
    }

    #[test]
    fn set_active_kit_preserves_the_rest_of_the_shared_settings() {
        with_temp_dirs(|| {
            let mut settings = load_settings();
            settings.auto_cue = false;
            settings.music_dirs = vec!["/music".into()];
            let _ = save_settings(&settings);

            let dir = kits::create_kit("My Kit").unwrap();
            set_active_kit(Some(&dir));

            let after = load_settings();
            assert_eq!(after.kit.as_deref(), Some(dir.to_string_lossy().as_ref()), "odj-sampler's own field changed");
            assert!(!after.auto_cue, "odj-player's fields survived the save");
            assert_eq!(after.music_dirs, vec![PathBuf::from("/music")]);

            assert_eq!(load_active_kit(), Some(ActiveKit::Saved(dir)));
        });
    }

    #[test]
    fn a_kit_living_in_a_crates_own_folder_is_found_by_its_full_path() {
        with_temp_dirs(|| {
            let crate_dir = std::env::temp_dir().join(format!("odj-settings-crate-kit-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&crate_dir);
            odj_core::crates::Crate::create(crate_dir.clone(), "Set").unwrap();
            let kit_dir = odj_core::crates::Crate::kits_dir(&crate_dir).join("their-kit");
            std::fs::create_dir_all(&kit_dir).unwrap();
            std::fs::write(kit_dir.join("kit.json"), "{}").unwrap();

            set_active_kit(Some(&kit_dir));
            assert_eq!(load_active_kit(), Some(ActiveKit::Saved(kit_dir)));
            std::fs::remove_dir_all(&crate_dir).unwrap();
        });
    }
}
