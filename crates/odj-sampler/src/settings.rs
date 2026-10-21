//! General odj-sampler settings — separate from `midi.json` (MIDI-specific)
//! and each kit's own `kit.json` — saved as `$XDG_DATA_HOME/odj/settings.json`.
//! Currently just which kit is active; other app-level preferences belong
//! here too, rather than as more one-off top-level files.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use odj_core::memory::{data_dir, read_json, write_json};

use crate::kits::kits_dir;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The active kit's directory name under `kits/`.
    pub kit: Option<String>,
}

impl Settings {
    pub fn file() -> PathBuf {
        data_dir().join("settings.json")
    }

    /// The saved settings, or the default (no active kit) when there are
    /// none yet (or they can't be read).
    pub fn load(file: &Path) -> Self {
        read_json(file).unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_json(file, self)
    }
}

/// The active kit's directory, if `settings.json` names one and that kit's
/// `kit.json` still exists — `None` covers both "never set" and "set, but
/// since deleted," so the caller always ends up offering a kit to pick.
pub fn load_active_kit() -> Option<PathBuf> {
    let dir = kits_dir().join(Settings::load(&Settings::file()).kit?);
    dir.join("kit.json").is_file().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let dir = std::env::temp_dir().join(format!("odj-settings-{}", std::process::id()));
        let file = dir.join("settings.json");
        let settings = Settings { kit: Some("my-kit".into()) };
        settings.save(&file).unwrap();
        assert_eq!(Settings::load(&file), settings);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_is_no_active_kit() {
        assert_eq!(Settings::load(Path::new("/nonexistent/settings.json")), Settings::default());
    }
}
