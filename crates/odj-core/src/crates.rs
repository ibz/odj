//! A "crate": a user-managed directory — typically also a git repo the user
//! inits, commits and pushes entirely by hand — bundling a curated track
//! roster ([`Crate::tracks_dir`], [`CrateTrack`]) and odj-sampler kits
//! ([`Crate::kits_dir`]), including kit `.wav` files. The track audio itself
//! never lives here, and neither does its DJ memory (cues, loops, grid,
//! tempo) — that's always the global library's job (see
//! `odj_core::memory::Memory`); a crate only ever records which tracks
//! belong to it, keyed by the track's content id so renames/moves/retags
//! never lose it. This module never touches git itself.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::memory::{read_json, relativize, resolve, write_json};
use crate::track::decode;

const MANIFEST_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    name: String,
}

/// A crate: its directory, plus a display name distinct from the directory
/// path — mirrors a kit's `name` vs. its directory slug.
#[derive(Clone, Debug, PartialEq)]
pub struct Crate {
    pub dir: PathBuf,
    pub name: String,
}

impl Crate {
    pub fn manifest_file(dir: &Path) -> PathBuf {
        dir.join("crate.json")
    }

    pub fn tracks_dir(dir: &Path) -> PathBuf {
        dir.join("tracks")
    }

    pub fn kits_dir(dir: &Path) -> PathBuf {
        dir.join("kits")
    }

    /// This crate's directory name under [`crates_root`] — what
    /// `Settings.active_crates` stores, resolved back with
    /// `crates_root().join(slug)`. Not `name`, which is a separate,
    /// freely-editable display label.
    pub fn slug(&self) -> String {
        self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// `name` falls back to the directory's own name when there's no
    /// manifest yet (a directory the user `git init`'d and populated by
    /// hand before ever opening it here).
    pub fn load(dir: PathBuf) -> Self {
        let name = read_json::<Manifest>(&Self::manifest_file(&dir))
            .map(|m| m.name)
            .unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        Self { dir, name }
    }

    /// Creates `tracks/`, `kits/` and `crate.json` under `dir`. Never runs git.
    pub fn create(dir: PathBuf, name: &str) -> io::Result<Self> {
        fs::create_dir_all(Self::tracks_dir(&dir))?;
        fs::create_dir_all(Self::kits_dir(&dir))?;
        write_json(&Self::manifest_file(&dir), &Manifest { version: MANIFEST_VERSION, name: name.to_string() })?;
        Ok(Self { dir, name: name.to_string() })
    }

    /// Deletes the whole crate directory — its track roster, its kits
    /// (including any kit `.wav` files), everything. The underlying audio
    /// files this crate's tracks pointed at are never touched; they live
    /// elsewhere. Irreversible: there's no trash, this is a plain
    /// `rm -r`. A no-op if `dir` doesn't exist.
    pub fn delete(dir: &Path) -> io::Result<()> {
        match fs::remove_dir_all(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

const TRACK_VERSION: u32 = 1;

/// One track referenced from a crate's own roster — just enough to find,
/// show and play it. Never DJ memory (cues, loops, grid, tempo); that
/// always lives in the global library instead (see
/// `odj_core::memory::Memory`), keyed by the same content id, so a track
/// can be in any number of crates without duplicating what it sounds
/// like to actually play it.
#[derive(Clone, Debug, PartialEq)]
pub struct CrateTrack {
    pub id: String,
    /// Resolved to an absolute path against `music_dirs` on the way out
    /// of [`list_tracks`] — on disk it's relative when possible, same
    /// convention as a library track's own path (see
    /// `odj_core::memory::Settings.music_dirs`).
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub sample_rate: u32,
    /// Where to download the audio from if it's missing; see
    /// [`set_youtube_url`].
    pub youtube_url: Option<String>,
}

/// A crate roster entry's file, as `tracks/<id[..2]>/<id>.json` stores it
/// (see [`track_entry_file`]) — mirrors `odj_core::memory`'s own
/// `TrackFile`, minus everything about DJ memory a crate never carries.
#[derive(Debug, Serialize, Deserialize)]
struct TrackEntry {
    version: u32,
    id: String,
    path: PathBuf,
    title: String,
    artist: Option<String>,
    sample_rate: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    youtube_url: Option<String>,
}

/// Sharded the same way the global library's own tracks are (see
/// `odj_core::memory::Memory`'s private `track_file`), by the track's
/// content id, so a crate's roster scales past a flat directory and
/// survives the track's title/path changing.
fn track_entry_file(tracks_dir: &Path, id: &str) -> PathBuf {
    tracks_dir.join(&id[..2]).join(format!("{id}.json"))
}

/// Every track in `tracks_dir` (a crate's own, see [`Crate::tracks_dir`]),
/// each path resolved against `music_dirs` — mirrors
/// `odj_core::memory::Memory::tracks`, unreadable files skipped the same way.
pub fn list_tracks(tracks_dir: &Path, music_dirs: &[PathBuf]) -> Vec<CrateTrack> {
    let Ok(shards) = fs::read_dir(tracks_dir) else { return Vec::new() };
    shards
        .flatten()
        .filter_map(|shard| fs::read_dir(shard.path()).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|file| file.extension().is_some_and(|e| e == "json"))
        .filter_map(|file| read_json::<TrackEntry>(&file))
        .map(|e| CrateTrack {
            id: e.id,
            path: resolve(&e.path, music_dirs),
            title: e.title,
            artist: e.artist,
            sample_rate: e.sample_rate,
            youtube_url: e.youtube_url,
        })
        .collect()
}

/// Adds `track` to the roster if its id isn't there yet — a no-op (not
/// even a file touch) if it already is, same as
/// `odj_core::memory::Memory::register_track`.
pub fn register_track(tracks_dir: &Path, music_dirs: &[PathBuf], track: &CrateTrack) -> io::Result<()> {
    if track.id.is_empty() {
        return Ok(());
    }
    let file = track_entry_file(tracks_dir, &track.id);
    if file.exists() {
        return Ok(());
    }
    fs::create_dir_all(file.parent().expect("roster entries live in a shard folder"))?;
    write_json(
        &file,
        &TrackEntry {
            version: TRACK_VERSION,
            id: track.id.clone(),
            path: relativize(&track.path, music_dirs),
            title: track.title.clone(),
            artist: track.artist.clone(),
            sample_rate: track.sample_rate,
            youtube_url: None,
        },
    )
}

pub fn youtube_url(tracks_dir: &Path, id: &str) -> Option<String> {
    read_json::<TrackEntry>(&track_entry_file(tracks_dir, id)).and_then(|e| e.youtube_url)
}

/// Sets/clears the YouTube URL for `id`, creating a minimal entry from
/// `track`'s own fields if there's none yet (tagging a result that's
/// never been added to the crate before) — never decodes the audio
/// itself. Everything else about an existing entry is left untouched
/// (an existing entry's own title/path/etc. win over `track`'s, which
/// may just be a search result's best guess).
pub fn set_youtube_url(tracks_dir: &Path, music_dirs: &[PathBuf], track: &CrateTrack, url: Option<String>) -> io::Result<()> {
    let file = track_entry_file(tracks_dir, &track.id);
    let mut entry = read_json::<TrackEntry>(&file).unwrap_or_else(|| TrackEntry {
        version: TRACK_VERSION,
        id: track.id.clone(),
        path: relativize(&track.path, music_dirs),
        title: track.title.clone(),
        artist: track.artist.clone(),
        sample_rate: track.sample_rate,
        youtube_url: None,
    });
    entry.youtube_url = url;
    fs::create_dir_all(file.parent().expect("roster entries live in a shard folder"))?;
    write_json(&file, &entry)
}

/// Rewrites just `path` for `id` — the self-healing step once a
/// `library_index::LibraryIndex` fallback finds a track `resolve`
/// couldn't on its own. A no-op if there's no existing entry for `id`.
pub fn heal_path(tracks_dir: &Path, music_dirs: &[PathBuf], id: &str, new_path: &Path) -> io::Result<()> {
    let file = track_entry_file(tracks_dir, id);
    let Some(mut entry) = read_json::<TrackEntry>(&file) else { return Ok(()) };
    entry.path = relativize(new_path, music_dirs);
    write_json(&file, &entry)
}

/// Removes `id`'s own roster entry. There is no audio file reachable
/// from here to delete, only the path it was last seen at. A no-op if
/// there's no entry for `id`.
pub fn forget_track(tracks_dir: &Path, id: &str) -> io::Result<()> {
    match fs::remove_file(track_entry_file(tracks_dir, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The one folder every crate lives under — fixed, not user-configurable,
/// same as `odj_sampler::kits::kits_dir`'s own global fallback.
pub fn crates_root() -> PathBuf {
    crate::memory::data_dir().join("crates")
}

/// Every crate that currently exists under [`crates_root`], by name —
/// mirrors `odj_sampler::kits::list_kits`'s shape exactly. The filesystem
/// is authoritative here; both odj-player and odj-sampler's "active
/// crates" tabs list straight from this, with no separate cache.
pub fn list_crates() -> Vec<Crate> {
    let Ok(entries) = fs::read_dir(crates_root()) else { return Vec::new() };
    let mut crates: Vec<Crate> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|dir| dir.is_dir() && (Crate::manifest_file(dir).is_file() || Crate::tracks_dir(dir).is_dir()))
        .map(Crate::load)
        .collect();
    crates.sort_by_key(|c| c.name.to_lowercase());
    crates
}

/// Lowercase, non-alphanumeric runs collapsed to a single `-`, trimmed;
/// `"crate"` if that leaves nothing (e.g. a name that was only
/// punctuation) — mirrors `odj_sampler::kits::slugify`.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() { "crate".to_string() } else { slug }
}

/// [`slugify`], then `-2`, `-3`, … appended until it names a directory that
/// doesn't exist yet directly under `root` — so a new crate never collides
/// with one already there.
pub fn unique_slug(root: &Path, name: &str) -> String {
    let base = slugify(name);
    let mut slug = base.clone();
    let mut n = 2;
    while root.join(&slug).exists() {
        slug = format!("{base}-{n}");
        n += 1;
    }
    slug
}

/// Generalizes odj-sampler's `loader::Failure::{Missing, Changed}` pattern
/// for a crate's curated track list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// The file at the recorded path decodes to the recorded id.
    Available,
    /// A file is there, but it's not the same audio any more.
    Changed,
    /// No file at the recorded path.
    Missing,
}

/// One-off check (e.g. the row under the cursor) — see [`AvailabilityChecker`]
/// for checking many rows off the UI thread.
pub fn check_availability(path: &Path, id: &str) -> Availability {
    if !path.is_file() {
        return Availability::Missing;
    }
    match decode(path) {
        Ok(d) if d.id == id => Availability::Available,
        Ok(_) => Availability::Changed,
        Err(_) => Availability::Missing,
    }
}

/// Background worker so listing dozens of tracks never blocks the UI —
/// mirrors `odj_sampler::loader::Loader`'s channel+thread shape, generalized
/// to id+path instead of pad audio.
pub struct AvailabilityChecker {
    jobs: std::sync::mpsc::Sender<(String, PathBuf)>,
    done: std::sync::mpsc::Receiver<(String, Availability)>,
}

impl AvailabilityChecker {
    pub fn new() -> Self {
        let (jobs, job_rx) = std::sync::mpsc::channel::<(String, PathBuf)>();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (id, path) in job_rx {
                if done_tx.send((id.clone(), check_availability(&path, &id))).is_err() {
                    return;
                }
            }
        });
        Self { jobs, done }
    }

    pub fn check(&self, id: String, path: PathBuf) {
        let _ = self.jobs.send((id, path));
    }

    pub fn poll(&self) -> Option<(String, Availability)> {
        self.done.try_recv().ok()
    }
}

impl Default for AvailabilityChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::write_wav;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("odj-crates-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn create_round_trips_through_load() {
        let dir = temp_dir("create");
        Crate::create(dir.clone(), "My Set").unwrap();
        assert!(Crate::tracks_dir(&dir).is_dir());
        assert!(Crate::kits_dir(&dir).is_dir());
        let loaded = Crate::load(dir.clone());
        assert_eq!(loaded.name, "My Set");
        assert_eq!(loaded.dir, dir);
    }

    #[test]
    fn slug_is_the_directory_name_not_the_display_name() {
        let dir = temp_dir("slug");
        let c = Crate::create(dir.clone(), "My Set").unwrap();
        assert_eq!(c.slug(), dir.file_name().unwrap().to_string_lossy());
        assert_ne!(c.slug(), c.name, "the slug is the directory name, not the editable display name");
    }

    #[test]
    fn crates_root_is_fixed_under_the_data_dir() {
        assert_eq!(crates_root(), crate::memory::data_dir().join("crates"));
    }

    #[test]
    fn delete_removes_the_whole_crate_directory() {
        let dir = temp_dir("delete");
        Crate::create(dir.clone(), "Doomed Set").unwrap();
        fs::write(Crate::kits_dir(&dir).join("kept.wav"), [0u8; 4]).unwrap();
        assert!(dir.is_dir());

        Crate::delete(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn delete_is_a_no_op_without_an_existing_directory() {
        let dir = temp_dir("delete-missing");
        assert!(Crate::delete(&dir).is_ok());
    }

    fn crate_track(id: &str, path: &str) -> CrateTrack {
        CrateTrack { id: id.into(), path: path.into(), title: "T".into(), artist: None, sample_rate: 44_100, youtube_url: None }
    }

    #[test]
    fn register_track_round_trips_through_list_tracks() {
        let dir = temp_dir("register-roundtrip");
        register_track(&dir, &[], &crate_track("ab00", "/music/a.flac")).unwrap();
        let tracks = list_tracks(&dir, &[]);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "ab00");
        assert_eq!(tracks[0].path, PathBuf::from("/music/a.flac"));
        assert_eq!(tracks[0].sample_rate, 44_100);
    }

    #[test]
    fn register_track_is_a_no_op_once_an_entry_exists() {
        let dir = temp_dir("register-no-op");
        register_track(&dir, &[], &crate_track("ab01", "/a")).unwrap();
        // A second registration under the same id, different title, must
        // not clobber the first — same "already there, leave it" contract
        // as the global library's own `register_track`.
        let mut changed = crate_track("ab01", "/a");
        changed.title = "Different".into();
        register_track(&dir, &[], &changed).unwrap();
        assert_eq!(list_tracks(&dir, &[])[0].title, "T");
    }

    #[test]
    fn register_track_stores_a_path_relative_to_its_music_dir() {
        let dir = temp_dir("register-relative");
        let music_dirs = [PathBuf::from("/music")];
        register_track(&dir, &music_dirs, &crate_track("ab02", "/music/sub/track.mp3")).unwrap();

        let raw = read_json::<TrackEntry>(&track_entry_file(&dir, "ab02")).unwrap();
        assert_eq!(raw.path, PathBuf::from("sub/track.mp3"));

        // ...and listing it back resolves it to the absolute path again.
        assert_eq!(list_tracks(&dir, &music_dirs)[0].path, PathBuf::from("/music/sub/track.mp3"));
    }

    #[test]
    fn set_youtube_url_preserves_an_existing_entrys_own_fields() {
        let dir = temp_dir("youtube-url-preserve");
        register_track(&dir, &[], &crate_track("ab03", "/a")).unwrap();

        // A later call with different (e.g. stale search-result) title/path
        // must not overwrite the entry already on disk — only the URL.
        let mut stale = crate_track("ab03", "/elsewhere");
        stale.title = "Stale Title".into();
        set_youtube_url(&dir, &[], &stale, Some("https://youtu.be/x".into())).unwrap();

        let tracks = list_tracks(&dir, &[]);
        assert_eq!(tracks[0].title, "T", "existing title wins over the stale one passed in");
        assert_eq!(tracks[0].path, PathBuf::from("/a"), "existing path wins too");
        assert_eq!(tracks[0].youtube_url, Some("https://youtu.be/x".into()));
        assert_eq!(youtube_url(&dir, "ab03"), Some("https://youtu.be/x".into()));
    }

    #[test]
    fn set_youtube_url_creates_a_minimal_entry_for_a_track_never_registered() {
        let dir = temp_dir("youtube-url-new");
        let track = crate_track("ab04", "/music/missing.mp3");
        set_youtube_url(&dir, &[], &track, Some("https://youtu.be/z".into())).unwrap();
        assert_eq!(youtube_url(&dir, "ab04"), Some("https://youtu.be/z".into()));
        assert_eq!(list_tracks(&dir, &[])[0].title, "T");
    }

    #[test]
    fn heal_path_updates_only_the_path() {
        let dir = temp_dir("heal-path");
        let music_dirs = [PathBuf::from("/music")];
        register_track(&dir, &music_dirs, &crate_track("ab05", "/music/old/place.mp3")).unwrap();

        heal_path(&dir, &music_dirs, "ab05", Path::new("/music/new/place.mp3")).unwrap();

        let raw = read_json::<TrackEntry>(&track_entry_file(&dir, "ab05")).unwrap();
        assert_eq!(raw.path, PathBuf::from("new/place.mp3"), "relativized, same as a fresh registration");
        assert_eq!(list_tracks(&dir, &music_dirs)[0].title, "T", "everything else untouched");
    }

    #[test]
    fn heal_path_is_a_no_op_without_an_existing_entry() {
        let dir = temp_dir("heal-path-missing");
        assert!(heal_path(&dir, &[], "ab06", Path::new("/wherever.mp3")).is_ok());
        assert!(!track_entry_file(&dir, "ab06").exists());
    }

    #[test]
    fn forget_track_removes_only_that_entry() {
        let dir = temp_dir("forget-track");
        register_track(&dir, &[], &crate_track("ab07", "/a")).unwrap();
        register_track(&dir, &[], &crate_track("ab08", "/b")).unwrap();

        forget_track(&dir, "ab07").unwrap();

        assert_eq!(list_tracks(&dir, &[]).len(), 1);
        assert_eq!(list_tracks(&dir, &[])[0].id, "ab08");
    }

    #[test]
    fn forget_track_is_a_no_op_without_an_existing_entry() {
        let dir = temp_dir("forget-track-missing");
        assert!(forget_track(&dir, "ab09").is_ok());
    }

    #[test]
    fn list_tracks_is_empty_without_a_tracks_dir() {
        let dir = temp_dir("list-missing");
        assert!(list_tracks(&dir, &[]).is_empty());
    }

    #[test]
    fn list_crates_finds_and_sorts_them_skipping_non_crate_dirs() {
        let dir = std::env::temp_dir().join(format!("odj-crates-list-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
        }
        let root = crates_root();
        Crate::create(root.join("zebra-set"), "Zebra Set").unwrap();
        Crate::create(root.join("alpha-set"), "Alpha Set").unwrap();
        fs::create_dir_all(root.join("not-a-crate")).unwrap();

        let found = list_crates();
        assert_eq!(found.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Alpha Set", "Zebra Set"]);
    }

    #[test]
    fn slugify_handles_punctuation_and_spaces() {
        assert_eq!(slugify("My Crate!!"), "my-crate");
        assert_eq!(slugify("  Techno_Set  "), "techno-set");
        assert_eq!(slugify("***"), "crate");
        assert_eq!(slugify(""), "crate");
    }

    #[test]
    fn unique_slug_dedupes_against_existing_directories() {
        let root = temp_dir("unique-slug");
        fs::create_dir_all(root.join("my-set")).unwrap();
        assert_eq!(unique_slug(&root, "My Set"), "my-set-2");
        fs::create_dir_all(root.join("my-set-2")).unwrap();
        assert_eq!(unique_slug(&root, "My Set"), "my-set-3");
        assert_eq!(unique_slug(&root, "Totally New"), "totally-new");
    }

    #[test]
    fn load_falls_back_to_the_directory_name_without_a_manifest() {
        let dir = temp_dir("no-manifest");
        fs::create_dir_all(&dir).unwrap();
        let loaded = Crate::load(dir.clone());
        assert_eq!(loaded.name, dir.file_name().unwrap().to_string_lossy());
    }

    #[test]
    fn check_availability_reports_missing_changed_and_available() {
        let dir = temp_dir("availability");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wav");

        assert_eq!(check_availability(&path, "whatever"), Availability::Missing);

        write_wav(&path, &[0.1, -0.1, 0.2, -0.2], 2, 44_100).unwrap();
        let id = decode(&path).unwrap().id;
        assert_eq!(check_availability(&path, &id), Availability::Available);
        assert_eq!(check_availability(&path, "not-the-real-id"), Availability::Changed);
    }

    #[test]
    fn availability_checker_reports_results_asynchronously() {
        let dir = temp_dir("checker");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wav");
        write_wav(&path, &[0.1, -0.1], 2, 44_100).unwrap();
        let id = decode(&path).unwrap().id;

        let checker = AvailabilityChecker::new();
        checker.check(id.clone(), path.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut result = None;
        while result.is_none() && std::time::Instant::now() < deadline {
            result = checker.poll();
        }
        assert_eq!(result, Some((id, Availability::Available)));
    }
}
