//! The kit: what each pad holds, saved as `kit.json` inside the kit's own
//! directory under `$XDG_DATA_HOME/odj/kits/` (see `kits.rs` for the
//! directory layout, listing, and switching between multiple kits).
//!
//! A pad's `PadSource.path` always points at a `.wav` the kit owns in its
//! own `samples/` directory, already cut to exactly the region assigned —
//! every assignment or re-trim writes a fresh cut there rather than
//! re-reading the original track at playback time (see
//! `App::copy_into_kit`). Only a reference (fingerprint, path, title,
//! artist, pad properties) is saved here; the samples themselves live as
//! files, and are decoded again at startup.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use odj_core::memory::{read_json, write_json};

use crate::engine::{PADS, Props};

const VERSION: u32 = 3;

/// One pad's sample, already cut to its final region — see the module docs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PadSource {
    pub track_id: String,
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub sample_rate: u32,
}

impl PadSource {
    pub fn name(&self) -> String {
        match &self.artist {
            Some(a) => format!("{a} – {}", self.title),
            None => self.title.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KitPad {
    pub source: Option<PadSource>,
    #[serde(default)]
    pub props: Props,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Kit {
    version: u32,
    /// Shown in the kit browser; the directory it lives in (see `kits.rs`)
    /// is a separate, generated slug, so renaming later won't need to move
    /// anything on disk.
    #[serde(default)]
    pub name: String,
    pub pads: [KitPad; PADS],
}

impl Default for Kit {
    fn default() -> Self {
        Self::new(String::new(), Default::default())
    }
}

impl Kit {
    pub fn new(name: String, pads: [KitPad; PADS]) -> Self {
        Self { version: VERSION, name, pads }
    }

    /// The saved kit, or an empty one when there is none (or it can't be read).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Mode;

    #[test]
    fn round_trips() {
        let dir = std::env::temp_dir().join(format!("odj-kit-{}", std::process::id()));
        let file = dir.join("kit.json");
        let mut kit = Kit::default();
        kit.pads[5] = KitPad {
            source: Some(PadSource {
                track_id: "ab12".into(),
                path: "/music/a.flac".into(),
                title: "A".into(),
                artist: None,
                sample_rate: 44_100,
            }),
            props: Props { mode: Mode::Gate, looped: true, retrigger: false, gain_db: -3.0 },
        };
        kit.save(&file).unwrap();
        assert_eq!(Kit::load(&file), kit);
        let json = std::fs::read_to_string(&file).unwrap();
        assert!(json.contains(r#""mode": "gate""#), "{json}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_is_an_empty_kit() {
        assert_eq!(Kit::load(Path::new("/nonexistent/kit.json")), Kit::default());
    }
}
