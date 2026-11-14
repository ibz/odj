//! A "crate": a user-managed directory — typically also a git repo the user
//! inits, commits and pushes entirely by hand — bundling a curated set's
//! per-track memory ([`Crate::tracks_dir`]) and odj-sampler kits
//! ([`Crate::kits_dir`]), including kit `.wav` files. The track audio itself
//! never lives here: just its cue/loop/grid memory, keyed by the track's
//! content id same as the global library (see `memory::Memory`), so the
//! audio can be anywhere — a local drive, a USB drive, or not currently
//! reachable at all. This module never touches git itself.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::memory::{read_json, write_json};
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

    /// Deletes the whole crate directory — its track memory, its kits
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
