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
/// Most recently-opened tracks kept in `Settings.recent_tracks`.
pub const MAX_RECENT: usize = 50;

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
    /// Pitch fader offset, in percent, as the track was last played — so a
    /// track comes back at the tempo it was mixed at. Absent means centred:
    /// a deck at 0% stores nothing, so merely loading a track never gives
    /// it a library entry it wouldn't otherwise have.
    pub tempo: Option<f64>,
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
        tempo: scalar(base.tempo, mine.tempo, theirs.tempo),
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

/// A track opened recently, enough to show it and reopen it without
/// decoding again; see `Settings.recent_tracks`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentTrack {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
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
    /// The active kit, by directory name (or `@embedded:<id>` for a
    /// built-in one) — resolved against the global kits folder or an
    /// active crate's own `kits/`, see odj-sampler's own
    /// `settings::load_active_kit`. odj-sampler.
    pub kit: Option<String>,
    /// The MIDI input odj-sampler last used, by its stable name (see
    /// `odj_core::midi::stable_name` — not its connection-order port id,
    /// which can change across a replug). Resolved back to a current port
    /// id at startup. odj-sampler.
    #[serde(default)]
    pub sampler_midi_last: Option<String>,
    /// Each pad's learned MIDI note, per device:
    /// `sampler_midi_controllers[name][pad]` for pad index `pad` (0-15).
    /// The values use `0` as the "not learned" sentinel for pads that were
    /// never pressed during [`learn_pad_notes`]. odj-sampler.
    #[serde(default)]
    pub sampler_midi_controllers: HashMap<String, std::collections::HashMap<usize, u8>>,
    /// Every MIDI controller odj-mixer's ever learned knob mappings
    /// for, keyed by its stable name (see `odj_core::midi::stable_name`
    /// — not its connection-order port id, which can change across a
    /// replug) rather than its connection-order port id. Each
    /// controller keeps its own target→CC map (same shape the single
    /// shared one used to be), so switching controllers doesn't mix up
    /// what's mapped on one with another. odj-mixer.
    #[serde(default)]
    pub mixer_midi_controllers: HashMap<String, HashMap<String, u8>>,
    /// The controller odj-mixer used last, by the same stable name
    /// `mixer_midi_controllers` is keyed by — if it, or any other
    /// known controller, is still plugged in at startup, it's used
    /// automatically, no picker needed (the last-used one specifically
    /// if more than one known controller is present at once); see
    /// `main.rs`'s own doc comment. odj-mixer.
    pub mixer_midi_last: Option<String>,
    /// Hardware inputs added to the mix, by their stable device key (see
    /// `odj_mixer::devices::device_key_string` — not a display label,
    /// which can change) — restored at startup so a previous session's
    /// channel selection survives a restart. A deck is never in this
    /// set: it's always auto-connected, never user-togglable. odj-mixer.
    #[serde(default)]
    pub mixer_active_inputs: HashSet<String>,
    /// The main output odj-mixer last used, by its stable device key
    /// (same encoding as `mixer_active_inputs`) — restored at startup
    /// so a previous session's choice survives a restart, and odj-mixer
    /// no longer needs to ask for one before it can even start; `None`
    /// means it hasn't been picked yet (from Settings' own Outputs
    /// tab), and nothing plays until it is. odj-mixer.
    pub mixer_output: Option<String>,
    /// Crates currently active, by directory name under
    /// `crate::crates::crates_root()` (see `Crate::slug`) — not a full
    /// path, so moving `$HOME` or syncing settings.json to a different
    /// machine doesn't orphan it; a crate, unlike a kit, only ever lives
    /// under that one fixed root, so the name alone is always enough to
    /// find it again, no multi-root search needed. A pure, merged,
    /// read-only *browsing* set: odj-player's own Files tab and
    /// odj-sampler's own kit picker both merge tracks/kits across every
    /// one of these (see `crate::crates::Crate`, `crate::crates::list_crates`),
    /// tagging each row by which crate(s) it came from. Never a write
    /// target: `Memory::load`'s `tracks/` and odj-sampler's `kits_dir()`
    /// always use the global library regardless of what's active here.
    /// Shared by odj-player and odj-sampler — toggling a crate in either
    /// app's "active crates" tab is visible in the other right away.
    #[serde(default)]
    pub active_crates: Vec<String>,
    /// Tracks opened recently, oldest first — capped at [`MAX_RECENT`],
    /// the oldest dropped once it's exceeded. Each `path` relative to
    /// whichever of `music_dirs` contains it, same convention (and same
    /// `relativize`/`resolve`) a library track's own path uses, so this
    /// survives a drive moving to a different mount point too — see
    /// [`resolved_recent_tracks`](Settings::resolved_recent_tracks).
    /// Shown newest-first by the Recent tab; odj-player.
    #[serde(default)]
    pub recent_tracks: Vec<RecentTrack>,
}

impl Settings {
    /// Records a track just opened: any existing entry for the same path
    /// is dropped first, so reopening one moves it to the top instead of
    /// leaving a stale duplicate behind. `path` is relativized against
    /// `music_dirs` before storing, same as a library track's own path.
    pub fn remember_recent(&mut self, path: PathBuf, title: String, artist: Option<String>) {
        let path = relativize(&path, &self.music_dirs);
        self.recent_tracks.retain(|t| t.path != path);
        self.recent_tracks.push(RecentTrack { path, title, artist });
        let excess = self.recent_tracks.len().saturating_sub(MAX_RECENT);
        self.recent_tracks.drain(..excess);
    }

    /// `recent_tracks`, oldest first (same order they're stored in), each
    /// path resolved back to where it should be found given the current
    /// `music_dirs` — the Recent tab shows these in reverse, newest first.
    pub fn resolved_recent_tracks(&self) -> Vec<RecentTrack> {
        self.recent_tracks
            .iter()
            .map(|t| RecentTrack { path: resolve(&t.path, &self.music_dirs), ..t.clone() })
            .collect()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_cue: true,
            jump_beats: 4.0,
            loop_beats: 4.0,
            music_dirs: Vec::new(),
            kit: None,
            sampler_midi_last: None,
            sampler_midi_controllers: HashMap::new(),
            mixer_midi_controllers: HashMap::new(),
            mixer_midi_last: None,
            mixer_active_inputs: HashSet::new(),
            mixer_output: None,
            active_crates: Vec::new(),
            recent_tracks: Vec::new(),
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

/// `path` relative to whichever of `music_dirs` contains it (the first
/// match — they never nest, see `App::add_music_dir`'s own doc comment in
/// odj-player, so at most one ever can), so a library entry survives a
/// drive moving to a different mount point, or a crate's roster moving to
/// a different machine, as long as the same relative layout is still
/// under one of them. Left absolute, unchanged, if none of them contain
/// it (e.g. a file loaded directly from the command line) — `resolve`
/// handles both cases transparently on the way back.
pub(crate) fn relativize(path: &Path, music_dirs: &[PathBuf]) -> PathBuf {
    for dir in music_dirs {
        if let Ok(rel) = path.strip_prefix(dir) {
            return rel.to_path_buf();
        }
    }
    path.to_path_buf()
}

/// The reverse of `relativize`: resolves a library entry's `path` back to
/// a real file. Already-absolute paths (old entries from before this
/// existed, or ones `relativize` couldn't place under any music folder)
/// pass through unchanged. A relative one is tried against each of
/// `music_dirs` in turn, returning the first that actually exists — so
/// adding the right folder (Ctrl+U in the browser) is all it takes for a
/// track to be found again. If none currently exist, falls back to the
/// first candidate anyway (or the bare relative path if there's no music
/// folder at all), so callers checking `is_file()`/decoding it still get
/// a sensible "not found" rather than nothing to even try.
pub(crate) fn resolve(path: &Path, music_dirs: &[PathBuf]) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    music_dirs
        .iter()
        .map(|dir| dir.join(path))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| music_dirs.first().map(|dir| dir.join(path)).unwrap_or_else(|| path.to_path_buf()))
}

/// A track's file in the library. Positions are frames at the track's sample rate.
#[derive(Debug, Serialize, Deserialize)]
struct TrackFile {
    version: u32,
    id: String,
    /// Where the track was last loaded from — relative to a music folder
    /// when it's under one (see `relativize`/`resolve`), so the whole
    /// library stays usable across a moved drive or mount point.
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
    /// Pitch fader offset, in percent; only written for a track last played
    /// off-centre.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tempo: Option<f64>,
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
            tempo: mem.tempo,
            cues,
            current_loop: mem.loop_in.map(|pos| Cue { pos, loop_out: mem.loop_out }),
        }
    }

    fn memory(&self) -> TrackMemory {
        let mut mem = TrackMemory {
            grid: self.grid,
            bpm: self.bpm,
            quantize: self.quantize,
            tempo: self.tempo,
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
    /// Always the global library's `tracks/` — the only DJ memory
    /// (cues/loops/grid/tempo) there is; a crate's own roster never
    /// carries any (see `odj_core::crates::CrateTrack`), so there's no
    /// equivalent "memory for this crate" to load.
    pub fn load() -> Self {
        Self { tracks_dir: data_dir().join("tracks"), settings: load_settings() }
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
                path: resolve(&file.path, &self.settings.music_dirs),
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
        let mut tf = TrackFile::new(track, mem);
        tf.path = relativize(&tf.path, &self.settings.music_dirs);
        write_json(&file, &tf)
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
        let mut tf = TrackFile::new(track, &merged);
        tf.path = relativize(&tf.path, &self.settings.music_dirs);
        write_json(&file, &tf)
    }

    /// Rewrites just `path` for `id` — the self-healing step once a
    /// `library_index::LibraryIndex` fallback finds a track `resolve`
    /// couldn't on its own (moved to a different subfolder of a music
    /// dir, say) — so every later lookup hits the cheap path again
    /// instead of needing the index a second time. A no-op if there's no
    /// existing entry for `id` (nothing to heal).
    pub fn heal_path(&self, id: &str, new_path: &Path) -> std::io::Result<()> {
        let file = self.track_file(id);
        let Some(mut tf) = read_json::<TrackFile>(&file) else { return Ok(()) };
        tf.path = relativize(new_path, &self.settings.music_dirs);
        write_json(&file, &tf)
    }

    pub fn save_settings(&self) -> std::io::Result<()> {
        save_settings(&self.settings)
    }
}

/// Where odj keeps its data, `$XDG_DATA_HOME/odj`.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// `$<var>/odj` if set; otherwise a conventional per-OS fallback under
/// `$HOME` — `fallback` (an XDG-style path, e.g. `.local/share`) on
/// Linux and everywhere else, or macOS's own
/// `Library/Application Support` on macOS specifically (which has no
/// `XDG_DATA_HOME`/`XDG_CONFIG_HOME` by convention at all, so this
/// branch is the one that actually applies there in practice; this app
/// has no meaningful data/config split, so both land in the same place
/// there rather than inventing one).
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os(var) {
        return PathBuf::from(dir).join("odj");
    }
    let Some(home) = std::env::var_os("HOME") else { return PathBuf::from(".").join("odj") };
    let home = PathBuf::from(home);
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/odj")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(fallback).join("odj")
    }
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
            tempo: Some(-3.5),
        }
    }

    /// Points `XDG_DATA_HOME`/`HOME` at a fresh temp directory for the
    /// span of one test — tests across the crate share this process's
    /// environment, so each test needs its own directory rather than a
    /// shared one two tests running in parallel could collide on.
    fn isolate_home(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("odj-xdg-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::set_var("HOME", &root);
        }
        root
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn data_dir_falls_back_to_the_xdg_dotfolder_under_home() {
        let root = isolate_home("linux");
        assert_eq!(data_dir(), root.join(".local/share/odj"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn data_dir_falls_back_to_application_support_on_macos() {
        let root = isolate_home("macos");
        assert_eq!(data_dir(), root.join("Library/Application Support/odj"));
        let _ = fs::remove_dir_all(&root);
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
    fn a_centred_pitch_fader_writes_no_tempo_at_all() {
        let t = track("ab98", "/a");
        let off_centre = TrackFile::new(&t, &TrackMemory { tempo: Some(2.5), ..TrackMemory::default() });
        let json = serde_json::to_string(&off_centre).unwrap();
        assert!(json.contains(r#""tempo":2.5"#), "{json}");

        let centred = TrackFile::new(&t, &TrackMemory { tempo: None, ..TrackMemory::default() });
        let json = serde_json::to_string(&centred).unwrap();
        assert!(!json.contains("tempo"), "{json}");
    }

    #[test]
    fn cues_follow_the_audio_not_the_path() {
        let mut m = memory("moved");
        m.save_track(&track("ab11", "/old/place.mp3"), &sample()).unwrap();
        assert_eq!(m.get(&track("ab11", "/new/place.mp3")), sample());
    }

    #[test]
    fn save_track_stores_a_path_relative_to_its_music_dir() {
        let mut m = memory("relative-path");
        m.settings.music_dirs = vec![PathBuf::from("/music")];
        let t = track("ab77", "/music/sub/track.mp3");
        m.save_track(&t, &sample()).unwrap();

        // The file on disk stores the relative path...
        let raw = read_json::<TrackFile>(&m.track_file("ab77")).unwrap();
        assert_eq!(raw.path, PathBuf::from("sub/track.mp3"));

        // ...and reading it back through `tracks()` resolves it to the
        // absolute path again, using the same music dir.
        assert_eq!(m.tracks()[0].path, PathBuf::from("/music/sub/track.mp3"));
    }

    #[test]
    fn a_track_found_under_a_newly_added_music_dir_resolves_there() {
        let dir = std::env::temp_dir().join(format!("odj-memory-relative-resolve-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let old_root = dir.join("old-drive");
        let new_root = dir.join("new-drive");
        fs::create_dir_all(old_root.join("sub")).unwrap();
        fs::create_dir_all(new_root.join("sub")).unwrap();
        fs::write(new_root.join("sub/track.mp3"), []).unwrap(); // only exists under the new root

        let mut m = Memory {
            tracks_dir: dir.join("tracks"),
            settings: Settings { music_dirs: vec![old_root.clone()], ..Settings::default() },
        };
        let t = track("ab88", &old_root.join("sub/track.mp3").display().to_string());
        m.save_track(&t, &sample()).unwrap();

        // Still configured with only the old (now-missing) root: falls
        // back to that candidate, even though it doesn't actually exist.
        assert_eq!(m.tracks()[0].path, old_root.join("sub/track.mp3"));

        // Add the folder where the file actually is now — found without
        // touching the saved entry at all.
        m.settings.music_dirs.push(new_root.clone());
        assert_eq!(m.tracks()[0].path, new_root.join("sub/track.mp3"));
    }

    #[test]
    fn a_path_outside_every_music_dir_is_saved_and_resolved_as_is() {
        let mut m = memory("absolute-fallback");
        m.settings.music_dirs = vec![PathBuf::from("/music")];
        let t = track("ab99", "/elsewhere/track.mp3");
        m.save_track(&t, &sample()).unwrap();

        let raw = read_json::<TrackFile>(&m.track_file("ab99")).unwrap();
        assert_eq!(raw.path, PathBuf::from("/elsewhere/track.mp3"), "left absolute, not relativized to an unrelated dir");
        assert_eq!(m.tracks()[0].path, PathBuf::from("/elsewhere/track.mp3"));
    }

    #[test]
    fn an_old_style_absolute_path_on_disk_still_resolves_unchanged() {
        let m = memory("legacy-absolute");
        fs::create_dir_all(m.tracks_dir.join("ab")).unwrap();
        let json = r#"{"version":1,"id":"abcc","path":"/music/old/track.mp3","title":"T","artist":null,"sample_rate":44100}"#;
        fs::write(m.tracks_dir.join("ab/abcc.json"), json).unwrap();
        assert_eq!(m.tracks()[0].path, PathBuf::from("/music/old/track.mp3"));
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
    fn heal_path_updates_only_the_path() {
        let mut m = memory("heal-path");
        m.settings.music_dirs = vec![PathBuf::from("/music")];
        let t = track("ab50", "/music/old/place.mp3");
        m.save_track(&t, &sample()).unwrap();

        m.heal_path("ab50", Path::new("/music/new/place.mp3")).unwrap();

        let raw = read_json::<TrackFile>(&m.track_file("ab50")).unwrap();
        assert_eq!(raw.path, PathBuf::from("new/place.mp3"), "relativized, same as any other save");
        assert_eq!(m.get(&t), sample(), "everything else is untouched");
    }

    #[test]
    fn heal_path_is_a_no_op_without_an_existing_entry() {
        let m = memory("heal-path-missing");
        assert!(m.heal_path("ab51", Path::new("/music/wherever.mp3")).is_ok());
        assert!(!m.track_file("ab51").exists());
    }

    #[test]
    fn remember_recent_reopening_moves_it_to_the_top_instead_of_duplicating() {
        let mut s = Settings::default();
        s.remember_recent(PathBuf::from("/a.mp3"), "A".into(), None);
        s.remember_recent(PathBuf::from("/b.mp3"), "B".into(), Some("Artist".into()));
        s.remember_recent(PathBuf::from("/a.mp3"), "A".into(), None);
        let paths: Vec<&Path> = s.recent_tracks.iter().map(|t| t.path.as_path()).collect();
        assert_eq!(paths, [Path::new("/b.mp3"), Path::new("/a.mp3")], "b, then a moved back to the top on reopening");
    }

    #[test]
    fn remember_recent_drops_the_oldest_past_the_cap() {
        let mut s = Settings::default();
        for i in 0..MAX_RECENT + 5 {
            s.remember_recent(PathBuf::from(format!("/{i}.mp3")), format!("T{i}"), None);
        }
        assert_eq!(s.recent_tracks.len(), MAX_RECENT);
        assert_eq!(s.recent_tracks[0].path, PathBuf::from("/5.mp3"), "the first 5 fell off the oldest end");
        assert_eq!(s.recent_tracks.last().unwrap().path, PathBuf::from(format!("/{}.mp3", MAX_RECENT + 4)));
    }

    #[test]
    fn remember_recent_stores_a_path_relative_to_its_music_dir() {
        let mut s = Settings { music_dirs: vec![PathBuf::from("/music")], ..Settings::default() };
        s.remember_recent(PathBuf::from("/music/sub/track.mp3"), "T".into(), None);
        assert_eq!(s.recent_tracks[0].path, PathBuf::from("sub/track.mp3"));

        // ...and reading it back resolves it to the absolute path again,
        // using the same music dir.
        assert_eq!(s.resolved_recent_tracks()[0].path, PathBuf::from("/music/sub/track.mp3"));
    }

    #[test]
    fn remember_recent_keeps_a_path_outside_every_music_dir_absolute() {
        let mut s = Settings { music_dirs: vec![PathBuf::from("/music")], ..Settings::default() };
        s.remember_recent(PathBuf::from("/elsewhere/track.mp3"), "T".into(), None);
        assert_eq!(s.recent_tracks[0].path, PathBuf::from("/elsewhere/track.mp3"), "left absolute, not relativized to an unrelated dir");
        assert_eq!(s.resolved_recent_tracks()[0].path, PathBuf::from("/elsewhere/track.mp3"));
    }

    #[test]
    fn remember_recent_reopening_moves_it_to_the_top_even_stored_relative() {
        let mut s = Settings { music_dirs: vec![PathBuf::from("/music")], ..Settings::default() };
        s.remember_recent(PathBuf::from("/music/a.mp3"), "A".into(), None);
        s.remember_recent(PathBuf::from("/music/b.mp3"), "B".into(), None);
        s.remember_recent(PathBuf::from("/music/a.mp3"), "A".into(), None);
        let paths: Vec<&Path> = s.recent_tracks.iter().map(|t| t.path.as_path()).collect();
        assert_eq!(paths, [Path::new("b.mp3"), Path::new("a.mp3")], "b, then a moved back to the top on reopening, deduped by the relative path");
    }

    #[test]
    fn load_always_resolves_the_global_tracks_dir_regardless_of_active_crates() {
        let dir = std::env::temp_dir().join(format!("odj-memory-crate-load-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let crate_dir = dir.join("my-crate");
        crate::crates::Crate::create(crate_dir.clone(), "Set").unwrap();
        let settings = Settings { active_crates: vec!["my-crate".to_string()], ..Settings::default() };
        save_settings(&settings).unwrap();

        let m = Memory::load();
        assert_eq!(m.tracks_dir, data_dir().join("tracks"), "active crates are a browsing set, never a write target");
    }
}
