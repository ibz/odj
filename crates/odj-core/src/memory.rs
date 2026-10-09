//! Per-track cue/loop memory and player settings, persisted across runs.
//!
//! Each track gets its own file, `$XDG_DATA_HOME/odj/tracks/<id[..2]>/<id>.json`, named
//! by the track's audio fingerprint so cues survive renames, moves and tag edits, and
//! several odj instances only ever write the tracks they changed. Settings live in
//! `$XDG_CONFIG_HOME/odj/settings.json`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::track::Track;

const VERSION: u32 = 1;
/// Most memory points kept per track.
pub const MAX_MEMORIES: usize = 100;

/// A cue point, or a loop when it has an out point. Positions are frames at the
/// track's sample rate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cue {
    pub pos: f64,
    /// Set when the cue stores a loop.
    pub loop_out: Option<f64>,
}

/// A track's cues as the deck uses them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackMemory {
    pub hot: [Option<Cue>; 3],
    pub memories: Vec<Cue>,
    pub loop_in: Option<f64>,
    pub loop_out: Option<f64>,
    /// Tapped BPM, overriding detection.
    pub bpm: Option<f64>,
}

impl TrackMemory {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A track's entry in the library, as its file stores it.
#[derive(Debug)]
pub struct LibraryTrack {
    pub id: String,
    /// Where the track was last loaded from; it may have moved since.
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub sample_rate: u32,
    pub memory: TrackMemory,
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

/// A track's file in the library. Positions are frames at the track's sample rate.
#[derive(Debug, Serialize, Deserialize)]
struct TrackFile {
    version: u32,
    id: String,
    /// Where the track was last loaded from; informational.
    path: PathBuf,
    title: String,
    artist: Option<String>,
    sample_rate: u32,
    #[serde(default)]
    bpm: Option<f64>,
    /// Hot cues and memory points, by position.
    #[serde(default)]
    cues: Vec<CueEntry>,
    /// The deck's current loop, which RELOOP returns to; no `loop_out` while it is
    /// still waiting for its OUT point.
    #[serde(default, rename = "loop")]
    current_loop: Option<Cue>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CueEntry {
    #[serde(flatten)]
    cue: Cue,
    /// Hot cue slot (0 = A); absent for a memory point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hot: Option<usize>,
}

impl TrackFile {
    fn new(track: &Track, mem: &TrackMemory) -> Self {
        let hot = mem.hot.iter().enumerate().filter_map(|(i, h)| h.map(|h| (Some(i), h)));
        let memories = mem.memories.iter().map(|&m| (None, m));
        let mut cues: Vec<CueEntry> =
            hot.chain(memories).map(|(hot, cue)| CueEntry { cue, hot }).collect();
        cues.sort_by(|a, b| a.cue.pos.total_cmp(&b.cue.pos));
        Self {
            version: VERSION,
            id: track.id.clone(),
            path: track.path.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            sample_rate: track.sample_rate,
            bpm: mem.bpm,
            cues,
            current_loop: mem.loop_in.map(|pos| Cue { pos, loop_out: mem.loop_out }),
        }
    }

    fn memory(&self) -> TrackMemory {
        let mut mem = TrackMemory {
            bpm: self.bpm,
            loop_in: self.current_loop.map(|l| l.pos),
            loop_out: self.current_loop.and_then(|l| l.loop_out),
            ..TrackMemory::default()
        };
        for &CueEntry { cue, hot } in &self.cues {
            match hot {
                // A hand-edited file might repeat a slot; the first one wins.
                Some(i) if i < mem.hot.len() => {
                    mem.hot[i].get_or_insert(cue);
                }
                Some(_) => {}
                None if mem.memories.len() < MAX_MEMORIES => mem.memories.push(cue),
                None => {}
            }
        }
        mem
    }
}

pub struct Memory {
    tracks_dir: PathBuf,
    config_dir: PathBuf,
    /// The pre-library `memory.json`, keyed by path, read on first need so cues
    /// move into the library as their tracks get loaded.
    legacy_file: PathBuf,
    legacy: Option<HashMap<String, TrackMemory>>,
    pub settings: Settings,
}

impl Memory {
    pub fn load() -> Self {
        let data_dir = data_dir();
        let config_dir = xdg_dir("XDG_CONFIG_HOME", ".config");
        let settings = read_json(&config_dir.join("settings.json"))
            .or_else(|| read_json(&data_dir.join("settings.json")))
            .unwrap_or_default();
        Self {
            tracks_dir: data_dir.join("tracks"),
            config_dir,
            legacy_file: data_dir.join("memory.json"),
            legacy: None,
            settings,
        }
    }

    fn track_file(&self, id: &str) -> PathBuf {
        self.tracks_dir.join(&id[..2]).join(format!("{id}.json"))
    }

    pub fn get(&mut self, track: &Track) -> TrackMemory {
        if track.id.is_empty() {
            return TrackMemory::default();
        }
        if let Some(file) = read_json::<TrackFile>(&self.track_file(&track.id)) {
            return file.memory();
        }
        let legacy = self.legacy.get_or_insert_with(|| read_json(&self.legacy_file).unwrap_or_default());
        legacy.get(&key(&track.path)).cloned().unwrap_or_default()
    }

    /// Every track in the library, in no particular order; unreadable files are skipped.
    pub fn tracks(&self) -> Vec<LibraryTrack> {
        let Ok(shards) = fs::read_dir(&self.tracks_dir) else { return Vec::new() };
        shards
            .flatten()
            .filter_map(|shard| fs::read_dir(shard.path()).ok())
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|file| file.extension().is_some_and(|e| e == "json"))
            .filter_map(|file| read_json::<TrackFile>(&file))
            .map(|file| LibraryTrack {
                memory: file.memory(),
                id: file.id,
                path: file.path,
                title: file.title,
                artist: file.artist,
                sample_rate: file.sample_rate,
            })
            .collect()
    }

    /// Writes the track's file; skipped for a track that never had cues.
    pub fn save_track(&self, track: &Track, mem: &TrackMemory) -> std::io::Result<()> {
        if track.id.is_empty() {
            return Ok(());
        }
        let file = self.track_file(&track.id);
        if mem.is_empty() && !file.exists() {
            return Ok(());
        }
        fs::create_dir_all(file.parent().expect("track files live in a shard folder"))?;
        write_json(&file, &TrackFile::new(track, mem))
    }

    pub fn save_settings(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        write_json(&self.config_dir.join("settings.json"), &self.settings)
    }
}

/// Where odj keeps its data, `$XDG_DATA_HOME/odj`.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("odj")
}

pub fn read_json<T: serde::de::DeserializeOwned>(file: &Path) -> Option<T> {
    fs::read_to_string(file).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// Writes through a temporary file so a crash never leaves a half-written file.
pub fn write_json<T: Serialize>(file: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, json)?;
    fs::rename(tmp, file)
}

/// The legacy `memory.json` key.
fn key(path: &Path) -> String {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, path: &str) -> Track {
        let mut t = Track::from_samples(path.into(), "T".into(), None, 44_100, vec![0.0; 4]);
        t.id = id.into();
        t
    }

    fn memory(name: &str) -> Memory {
        let dir = std::env::temp_dir().join(format!("odj-memory-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Memory {
            tracks_dir: dir.join("tracks"),
            config_dir: dir.join("config"),
            legacy_file: dir.join("memory.json"),
            legacy: None,
            settings: Settings::default(),
        }
    }

    fn sample() -> TrackMemory {
        TrackMemory {
            hot: [Some(Cue { pos: 100.0, loop_out: None }), None, Some(Cue { pos: 50.0, loop_out: Some(90.0) })],
            memories: vec![Cue { pos: 10.0, loop_out: None }, Cue { pos: 200.0, loop_out: Some(300.0) }],
            loop_in: Some(200.0),
            loop_out: Some(300.0),
            bpm: Some(124.0),
        }
    }

    #[test]
    fn round_trips_through_track_file() {
        let mut m = memory("roundtrip");
        let t = track("abcdef0123456789abcdef0123456789", "/music/a.flac");
        m.save_track(&t, &sample()).unwrap();
        assert!(m.tracks_dir.join("ab/abcdef0123456789abcdef0123456789.json").exists());
        assert_eq!(m.get(&t), sample());
    }

    #[test]
    fn file_lists_cues_by_position_with_hot_slots() {
        let file = TrackFile::new(&track("ab00", "/a"), &sample());
        let cues: Vec<(f64, Option<usize>)> = file.cues.iter().map(|c| (c.cue.pos, c.hot)).collect();
        assert_eq!(cues, [(10.0, None), (50.0, Some(2)), (100.0, Some(0)), (200.0, None)]);
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains(r#""loop":{"pos":200.0,"loop_out":300.0}"#), "{json}");
    }

    #[test]
    fn cues_follow_the_audio_not_the_path() {
        let mut m = memory("moved");
        m.save_track(&track("ab11", "/old/place.mp3"), &sample()).unwrap();
        assert_eq!(m.get(&track("ab11", "/new/place.mp3")), sample());
    }

    #[test]
    fn lists_every_track_in_the_library() {
        let m = memory("list");
        m.save_track(&track("ab55", "/music/a.flac"), &sample()).unwrap();
        m.save_track(&track("cd66", "/music/b.flac"), &sample()).unwrap();
        // Half-written leftovers of a crash are not tracks.
        fs::write(m.tracks_dir.join("ab/ab77.json.1.tmp"), "{").unwrap();
        let mut ids: Vec<String> = m.tracks().into_iter().map(|t| t.id).collect();
        ids.sort();
        assert_eq!(ids, ["ab55", "cd66"]);
        assert_eq!(m.tracks()[0].memory, sample());
    }

    #[test]
    fn empty_memory_writes_nothing_new() {
        let m = memory("empty");
        let t = track("ab22", "/a");
        m.save_track(&t, &TrackMemory::default()).unwrap();
        assert!(!m.track_file("ab22").exists());
    }

    #[test]
    fn imports_from_legacy_memory_json() {
        let mut m = memory("legacy");
        fs::create_dir_all(m.legacy_file.parent().unwrap()).unwrap();
        let legacy = HashMap::from([("/music/old.mp3".to_string(), sample())]);
        write_json(&m.legacy_file, &legacy).unwrap();
        assert_eq!(m.get(&track("ab33", "/music/old.mp3")), sample());
        assert_eq!(m.get(&track("ab44", "/music/other.mp3")), TrackMemory::default());
    }
}
