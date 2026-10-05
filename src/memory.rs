//! Per-track cue/loop memory and player settings, persisted across runs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::HotCue;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackMemory {
    pub hot: [Option<HotCue>; 3],
    pub memories: Vec<HotCue>,
    pub loop_in: Option<f64>,
    pub loop_out: Option<f64>,
    /// Tapped BPM, overriding detection.
    pub bpm: Option<f64>,
}

/// Player settings that survive a restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Cue to the first sound on load instead of the very start.
    pub auto_cue: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { auto_cue: true }
    }
}

pub struct Memory {
    dir: PathBuf,
    tracks: HashMap<String, TrackMemory>,
    pub settings: Settings,
}

impl Memory {
    pub fn load() -> Self {
        let dir = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("odj");
        Self {
            tracks: read_json(&dir.join("memory.json")),
            settings: read_json(&dir.join("settings.json")),
            dir,
        }
    }

    pub fn get(&self, track: &Path) -> TrackMemory {
        self.tracks.get(&key(track)).cloned().unwrap_or_default()
    }

    pub fn set(&mut self, track: &Path, mem: TrackMemory) {
        self.tracks.insert(key(track), mem);
    }

    pub fn save(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        write_json(&self.dir.join("memory.json"), &self.tracks)?;
        write_json(&self.dir.join("settings.json"), &self.settings)
    }
}

fn read_json<T: serde::de::DeserializeOwned + Default>(file: &Path) -> T {
    fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(file: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(tmp, file)
}

fn key(path: &Path) -> String {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}
