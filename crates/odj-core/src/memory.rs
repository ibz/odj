//! Per-track cue/loop memory and player settings, persisted across runs.
//!
//! Each track gets its own file, `$XDG_DATA_HOME/odj/tracks/<id[..2]>/<id>.json`, named
//! by the track's audio fingerprint so cues survive renames, moves and tag edits, and
//! several odj instances only ever write the tracks they changed. `save_track_merged`
//! three-way merges against whatever's on disk, so two instances with the same track
//! open don't clobber each other's saves. Settings live in
//! `$XDG_CONFIG_HOME/odj/settings.json`, shared by odj-player and
//! odj-sampler — see [`Settings`], [`load_settings`], [`save_settings`].

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::grid::BeatGrid;
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
    /// The beat grid as the user set it, overriding detection.
    pub grid: Option<BeatGrid>,
    /// BPM tapped before odj had beat grids; `grid` gives it the detected phase.
    pub bpm: Option<f64>,
    /// Quantize turned on or off for this track, overriding the default.
    pub quantize: Option<bool>,
}

impl TrackMemory {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The track's grid: the user's, else the detected one with any old tapped BPM.
    pub fn grid(&self, track: &Track) -> Option<BeatGrid> {
        user_grid(self.grid, self.bpm, track)
    }
}

/// The grid of a track given the user's grid and old tapped BPM from its memory.
pub fn user_grid(grid: Option<BeatGrid>, tapped_bpm: Option<f64>, track: &Track) -> Option<BeatGrid> {
    grid.or_else(|| {
        let bpm = tapped_bpm?;
        let anchor = track.grid.map_or(track.first_sound, |g| g.anchor);
        Some(BeatGrid { bpm, anchor })
    })
}

/// Same position, for merge purposes — matches the tolerance `Deck` itself
/// uses to recognize "the same cue" (e.g. when deduping on store).
fn same_pos(a: f64, b: f64) -> bool {
    (a - b).abs() < 1.0
}

/// Three-way merge of this deck's change against whatever's on disk now.
/// `base` is what this deck loaded before making any changes of its own;
/// `mine` is its current state; `theirs` is the file as it stands right
/// now, which may have been written by another odj instance since `base`
/// was read. `hot` and `memories` are unioned/subtracted against `base` so
/// an add or delete either side made is preserved rather than one
/// overwriting the other; the remaining fields are single "current
/// session" values with no sensible merge, so `mine` wins if it moved from
/// `base`, else `theirs` is kept.
fn merge(base: &TrackMemory, mine: &TrackMemory, theirs: &TrackMemory) -> TrackMemory {
    fn scalar<T: PartialEq>(base: T, mine: T, theirs: T) -> T {
        if mine != base { mine } else { theirs }
    }
    TrackMemory {
        hot: std::array::from_fn(|i| scalar(base.hot[i], mine.hot[i], theirs.hot[i])),
        memories: merge_cues(&base.memories, &mine.memories, &theirs.memories),
        loop_in: scalar(base.loop_in, mine.loop_in, theirs.loop_in),
        loop_out: scalar(base.loop_out, mine.loop_out, theirs.loop_out),
        grid: scalar(base.grid, mine.grid, theirs.grid),
        bpm: scalar(base.bpm, mine.bpm, theirs.bpm),
        quantize: scalar(base.quantize, mine.quantize, theirs.quantize),
    }
}

/// Applies this deck's additions and deletions (`mine` vs. `base`) onto
/// `theirs`, so a concurrent change on the other side survives too.
fn merge_cues(base: &[Cue], mine: &[Cue], theirs: &[Cue]) -> Vec<Cue> {
    let added = mine.iter().filter(|m| !base.iter().any(|b| same_pos(b.pos, m.pos)));
    let removed: Vec<&Cue> = base.iter().filter(|b| !mine.iter().any(|m| same_pos(m.pos, b.pos))).collect();
    let mut merged: Vec<Cue> =
        theirs.iter().filter(|t| !removed.iter().any(|r| same_pos(r.pos, t.pos))).copied().collect();
    for a in added {
        if !merged.iter().any(|m| same_pos(m.pos, a.pos)) {
            merged.push(*a);
        }
    }
    merged.sort_by(|a, b| a.pos.total_cmp(&b.pos));
    merged
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

/// Settings shared by odj-player, odj-sampler and odj-mixer, one file
/// between them — each app only ever touches the field(s) it owns,
/// always through a full read-modify-write of this whole struct (see
/// [`load_settings`]/[`save_settings`]), so one app's save never
/// clobbers another's fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Cue to the first sound on load instead of the very start. odj-player.
    pub auto_cue: bool,
    /// Beatjump size, in beats. odj-player.
    pub jump_beats: f64,
    /// Size of the next beatloop, in beats. odj-player.
    pub loop_beats: f64,
    /// Folders the browser can jump into; the first that still exists is
    /// where it opens when no folder is given on the command line. odj-player.
    pub music_dirs: Vec<PathBuf>,
    /// The active kit's directory name under `kits/`. odj-sampler.
    pub kit: Option<String>,
    /// The MIDI input port odj-sampler last used, if any.
    #[serde(alias = "midi_port_id")]
    pub sampler_midi_port_id: Option<String>,
    /// Each pad's learned MIDI note, per device:
    /// `sampler_midi_pads[name][slot][i]` for pad `i` — see
    /// `odj_sampler::midi` for how device/slot keys are chosen. The
    /// array is sized to odj-sampler's pad count (16) directly rather
    /// than via its constant, since odj-core doesn't depend on
    /// odj-sampler. odj-sampler.
    #[serde(default, alias = "midi_devices")]
    pub sampler_midi_pads: HashMap<String, Vec<[u8; 16]>>,
    /// The MIDI input port odj-mixer last used, if any — kept separate
    /// from `sampler_midi_port_id`. odj-mixer.
    pub mixer_midi_port_id: Option<String>,
    /// Each learned knob's CC, keyed by a stable target string (an
    /// input's name, or `"player:<deck name>:<param>"` for a discovered
    /// player's Mixer parameter) — a flat map, since odj-mixer's set of
    /// targets is open-ended and keyed by name rather than by fixed
    /// position. odj-mixer.
    #[serde(default)]
    pub mixer_midi_knobs: HashMap<String, u8>,
    /// Hardware inputs added to the mix, by their stable device key (see
    /// `odj_mixer::devices::device_key_string` — not a display label,
    /// which can change) — restored at startup so a previous session's
    /// channel selection survives a restart. A deck is never in this
    /// set: it's always auto-connected, never user-togglable. odj-mixer.
    #[serde(default)]
    pub mixer_active_inputs: HashSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_cue: true,
            jump_beats: 4.0,
            loop_beats: 4.0,
            music_dirs: Vec::new(),
            kit: None,
            sampler_midi_port_id: None,
            sampler_midi_pads: HashMap::new(),
            mixer_midi_port_id: None,
            mixer_midi_knobs: HashMap::new(),
            mixer_active_inputs: HashSet::new(),
        }
    }
}

/// Where the shared settings file lives, `$XDG_CONFIG_HOME/odj/settings.json`.
pub fn settings_file() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("settings.json")
}

/// The saved settings, or the default when there are none yet (or they
/// can't be read) — checks [`settings_file`] first, then the old
/// `$XDG_DATA_HOME/odj/settings.json` location.
pub fn load_settings() -> Settings {
    read_json(&settings_file()).or_else(|| read_json(&data_dir().join("settings.json"))).unwrap_or_default()
}

/// Saves the whole shared struct to [`settings_file`]. Callers that only
/// own one field should [`load_settings`] first, change just that field,
/// and save the result — not construct a fresh default.
pub fn save_settings(settings: &Settings) -> std::io::Result<()> {
    let file = settings_file();
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    write_json(&file, settings)
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
    /// Only written once the user has corrected the detected grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grid: Option<BeatGrid>,
    /// Tapped BPM of older versions, read into `grid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bpm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quantize: Option<bool>,
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
            grid: mem.grid,
            bpm: mem.bpm,
            quantize: mem.quantize,
            cues,
            current_loop: mem.loop_in.map(|pos| Cue { pos, loop_out: mem.loop_out }),
        }
    }

    fn memory(&self) -> TrackMemory {
        let mut mem = TrackMemory {
            grid: self.grid,
            bpm: self.bpm,
            quantize: self.quantize,
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
    pub settings: Settings,
}

impl Memory {
    pub fn load() -> Self {
        let data_dir = data_dir();
        Self { tracks_dir: data_dir.join("tracks"), settings: load_settings() }
    }

    fn track_file(&self, id: &str) -> PathBuf {
        self.tracks_dir.join(&id[..2]).join(format!("{id}.json"))
    }

    pub fn get(&mut self, track: &Track) -> TrackMemory {
        if track.id.is_empty() {
            return TrackMemory::default();
        }
        read_json::<TrackFile>(&self.track_file(&track.id)).map(|file| file.memory()).unwrap_or_default()
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

    /// Like `save_track`, but merges with whatever's on disk right now
    /// instead of overwriting it outright — so a second odj instance with
    /// the same track loaded, which saved its own changes in the
    /// meantime, doesn't have them silently discarded. `base` is this
    /// deck's own memory as it was when loaded (before any of *its*
    /// changes); see `merge`.
    pub fn save_track_merged(&self, track: &Track, base: &TrackMemory, mine: &TrackMemory) -> std::io::Result<()> {
        if track.id.is_empty() {
            return Ok(());
        }
        let file = self.track_file(&track.id);
        let theirs = read_json::<TrackFile>(&file).map(|f| f.memory()).unwrap_or_default();
        let merged = merge(base, mine, &theirs);
        if merged.is_empty() && !file.exists() {
            return Ok(());
        }
        fs::create_dir_all(file.parent().expect("track files live in a shard folder"))?;
        write_json(&file, &TrackFile::new(track, &merged))
    }

    pub fn save_settings(&self) -> std::io::Result<()> {
        save_settings(&self.settings)
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
        Memory { tracks_dir: dir.join("tracks"), settings: Settings::default() }
    }

    fn sample() -> TrackMemory {
        TrackMemory {
            hot: [Some(Cue { pos: 100.0, loop_out: None }), None, Some(Cue { pos: 50.0, loop_out: Some(90.0) })],
            memories: vec![Cue { pos: 10.0, loop_out: None }, Cue { pos: 200.0, loop_out: Some(300.0) }],
            loop_in: Some(200.0),
            loop_out: Some(300.0),
            grid: Some(BeatGrid { bpm: 124.5, anchor: 1_234.5 }),
            bpm: None,
            quantize: Some(false),
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
    fn concurrent_decks_merge_instead_of_clobbering() {
        let mut m = memory("concurrent");
        let t = track("ab00000000000000000000000000ab00", "/music/b.flac");

        // The shared starting point both decks loaded.
        let base = sample();
        m.save_track(&t, &base).unwrap();

        // Deck A adds a memory point, based on what it loaded.
        let mut a = base.clone();
        a.memories.push(Cue { pos: 500.0, loop_out: None });
        m.save_track_merged(&t, &base, &a).unwrap();

        // Deck B, independently based on the same original load, clears a
        // hot cue — and saves *after* A, which would clobber A's addition
        // outright with a plain overwrite.
        let mut b = base.clone();
        b.hot[0] = None;
        m.save_track_merged(&t, &base, &b).unwrap();

        let result = m.get(&t);
        assert!(result.memories.iter().any(|c| c.pos == 500.0), "A's addition survived B's later save");
        assert_eq!(result.hot[0], None, "B's deletion also survived");
        assert_eq!(result.hot[2], base.hot[2], "untouched fields keep their value");
    }

    #[test]
    fn scalar_fields_dont_get_stomped_by_a_save_that_never_touched_them() {
        let mut m = memory("scalar-conflict");
        let t = track("cd00000000000000000000000000cd00", "/music/c.flac");
        let base = sample();
        m.save_track(&t, &base).unwrap();

        let mut a = base.clone();
        a.quantize = Some(true);
        m.save_track_merged(&t, &base, &a).unwrap();

        // B saves after A, but never changed quantize itself (still equal
        // to base) — its save shouldn't carry the old value back over A's.
        let b = base.clone();
        m.save_track_merged(&t, &base, &b).unwrap();

        assert_eq!(m.get(&t).quantize, Some(true), "A's real change outlives B's unrelated save");
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
    fn old_tapped_bpm_gets_the_detected_phase() {
        let json = r#"{"version":1,"id":"ab88","path":"/a","title":"T","artist":null,"sample_rate":44100,"bpm":126.0}"#;
        let file: TrackFile = serde_json::from_str(json).unwrap();
        let mem = file.memory();
        let mut t = track("ab88", "/a");
        t.grid = Some(BeatGrid { bpm: 125.7, anchor: 300.0 });
        assert_eq!(mem.grid(&t), Some(BeatGrid { bpm: 126.0, anchor: 300.0 }));
        t.grid = None;
        t.first_sound = 42.0;
        assert_eq!(mem.grid(&t), Some(BeatGrid { bpm: 126.0, anchor: 42.0 }));
        // Once the deck saves it as a grid, the old field is gone.
        let saved = TrackMemory { grid: mem.grid(&t), bpm: None, ..mem };
        let json = serde_json::to_string(&TrackFile::new(&t, &saved)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["grid"], serde_json::json!({"bpm": 126.0, "anchor": 42.0}));
        assert!(v.get("bpm").is_none(), "{json}");
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

}
