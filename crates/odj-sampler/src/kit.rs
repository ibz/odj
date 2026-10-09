//! The kit: what each pad holds, saved as `$XDG_DATA_HOME/odj/kit.json`.
//!
//! Only references are saved, never audio: the track's fingerprint and last known
//! path, the region in the track, and the pad's properties. At startup the pads are
//! cut from the files again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use odj_core::memory::{data_dir, read_json, write_json};

use crate::engine::{PADS, Props};

const VERSION: u32 = 1;

/// One pad's region of a track. Positions are frames at the track's sample rate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PadSource {
    pub track_id: String,
    /// Where the track was when the pad was assigned.
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub sample_rate: u32,
    /// Which cue it came from, e.g. "hot A", "memory 3", "loop".
    pub cue: String,
    pub start: f64,
    pub end: f64,
}

impl PadSource {
    pub fn seconds(&self) -> f64 {
        (self.end - self.start) / self.sample_rate as f64
    }

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
    pub pads: [KitPad; PADS],
}

impl Default for Kit {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl Kit {
    pub fn new(pads: [KitPad; PADS]) -> Self {
        Self { version: VERSION, pads }
    }

    pub fn file() -> PathBuf {
        data_dir().join("kit.json")
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
                cue: "hot B".into(),
                start: 100.0,
                end: 88_300.0,
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
