//! A per-music-folder cache of `relative path ↔ (id, mtime, size)`, so a
//! track that moved to a different subfolder within an already-configured
//! music folder (too far for `memory::resolve`'s own folder-root-only
//! matching) can still be found — a fallback, not a replacement: `path`
//! keeps being stored and resolved exactly as `memory.rs` already does;
//! this is only consulted when that comes up empty, and only ever on
//! explicit request (building/refreshing a whole folder's index is slow
//! enough that it should never happen as a surprise). See
//! [`LibraryIndex`]/[`IndexRefresher`].

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::library::is_audio;
use crate::memory::{data_dir, read_json, write_json};

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: Vec<IndexEntry>,
}

/// `path` is relative to the music folder this index is for.
#[derive(Clone, Serialize, Deserialize)]
struct IndexEntry {
    path: PathBuf,
    id: String,
    mtime: u64,
    size: u64,
}

/// Where a music folder's own index file lives — under odj's own data
/// dir, never inside the music folder itself (same principle that
/// already keeps crates/kits out of track folders), named from a hash of
/// the folder's absolute path so two folders sharing a basename elsewhere
/// don't collide.
fn index_file(music_dir: &Path) -> PathBuf {
    let slug = blake3::hash(music_dir.to_string_lossy().as_bytes()).to_hex();
    data_dir().join("music-index").join(format!("{}.json", &slug[..16]))
}

fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

/// Demuxes but never decodes to PCM — the id only needs the raw encoded
/// packets, not the audio itself, so this is cheap next to a real decode
/// (no resampling, no float conversion, no waveform/key analysis).
fn hash_packets(path: &Path) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(io::Error::other)?;
    let track_id = format.default_track(TrackType::Audio).ok_or_else(|| io::Error::other("no audio track"))?.id;
    let mut hasher = blake3::Hasher::new();
    loop {
        match format.next_packet() {
            Ok(Some(p)) => {
                if p.track_id == track_id {
                    hasher.update(&p.data);
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    Ok(hasher.finalize().to_hex()[..32].to_string())
}

/// Recursively lists every audio file under `dir` — nothing in this
/// codebase needs a recursive walk otherwise (`library::list` is one
/// level, for the file browser), so this is its own small helper rather
/// than a shared one; skips dotfiles, same convention as `library::list`.
fn walk_audio_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk_audio_files(&path, out);
        } else if is_audio(&path) {
            out.push(path);
        }
    }
}

pub struct LibraryIndex {
    by_path: HashMap<PathBuf, IndexEntry>,
    by_id: HashMap<String, PathBuf>,
}

impl LibraryIndex {
    fn from_entries(entries: Vec<IndexEntry>) -> Self {
        let by_id = entries.iter().map(|e| (e.id.clone(), e.path.clone())).collect();
        let by_path = entries.into_iter().map(|e| (e.path.clone(), e)).collect();
        Self { by_path, by_id }
    }

    /// Whatever's cached for `music_dir`; empty if there's nothing yet.
    pub fn load(music_dir: &Path) -> Self {
        match read_json::<IndexFile>(&index_file(music_dir)) {
            Some(f) => Self::from_entries(f.entries),
            None => Self::from_entries(Vec::new()),
        }
    }

    /// Walks `music_dir` recursively; only re-hashes a file if its
    /// mtime/size differ from what's cached, or it's new. Drops entries
    /// for files no longer present. Persists the result. Slow — call off
    /// the UI thread (see [`IndexRefresher`]).
    pub fn refresh(&mut self, music_dir: &Path) -> io::Result<()> {
        let mut files = Vec::new();
        walk_audio_files(music_dir, &mut files);

        let mut entries = Vec::with_capacity(files.len());
        for abs in &files {
            let Ok(meta) = fs::metadata(abs) else { continue };
            let Ok(rel) = abs.strip_prefix(music_dir) else { continue };
            let (mtime, size) = (mtime_secs(&meta), meta.len());
            let cached = self.by_path.get(rel);
            let id = match cached {
                Some(e) if e.mtime == mtime && e.size == size => e.id.clone(),
                _ => match hash_packets(abs) {
                    Ok(id) => id,
                    Err(_) => continue, // unreadable: just not indexed, not fatal to the whole scan
                },
            };
            entries.push(IndexEntry { path: rel.to_path_buf(), id, mtime, size });
        }

        let file = index_file(music_dir);
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        write_json(&file, &IndexFile { version: VERSION, entries: entries.clone() })?;
        *self = Self::from_entries(entries);
        Ok(())
    }

    /// `id`'s current absolute location under this index's music folder,
    /// if it has one.
    pub fn find(&self, music_dir: &Path, id: &str) -> Option<PathBuf> {
        self.by_id.get(id).map(|rel| music_dir.join(rel))
    }
}

/// How far along a refresh is, or its finished result — see
/// [`IndexRefresher::poll`].
pub enum RefreshProgress {
    Scanning { done: usize, total: usize },
    Finished(LibraryIndex),
}

/// One-shot background worker: refreshes one music folder's index,
/// reporting progress along the way — mirrors
/// `crates::AvailabilityChecker`'s channel+thread shape, but for a single
/// big job instead of a stream of small ones.
pub struct IndexRefresher {
    progress: mpsc::Receiver<RefreshProgress>,
}

impl IndexRefresher {
    /// Spawns the thread immediately and starts scanning `music_dir`.
    pub fn start(music_dir: PathBuf) -> Self {
        let (tx, progress) = mpsc::channel();
        std::thread::spawn(move || {
            let mut files = Vec::new();
            walk_audio_files(&music_dir, &mut files);
            let total = files.len();
            let mut index = LibraryIndex::load(&music_dir);
            // `refresh` re-walks and does the real work; the walk above is
            // only to report a meaningful `total` up front, so progress
            // isn't just "0 of unknown" until it's already done.
            let _ = tx.send(RefreshProgress::Scanning { done: 0, total });
            let _ = index.refresh(&music_dir);
            let _ = tx.send(RefreshProgress::Finished(index));
        });
        Self { progress }
    }

    pub fn poll(&self) -> Option<RefreshProgress> {
        self.progress.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::write_wav;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("odj-library-index-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn isolate(root: &Path) {
        unsafe {
            std::env::set_var("XDG_DATA_HOME", root);
        }
    }

    #[test]
    fn refresh_finds_files_and_persists_them() {
        let root = temp_dir("refresh");
        let music = root.join("music");
        fs::create_dir_all(music.join("sub")).unwrap();
        isolate(&root);
        write_wav(&music.join("sub/a.wav"), &[0.1, -0.1], 2, 44_100).unwrap();

        let mut index = LibraryIndex::load(&music);
        index.refresh(&music).unwrap();
        let id = crate::track::decode(&music.join("sub/a.wav")).unwrap().id;
        assert_eq!(index.find(&music, &id), Some(music.join("sub/a.wav")));

        // Persisted: a freshly loaded index (no refresh) finds it too.
        let reloaded = LibraryIndex::load(&music);
        assert_eq!(reloaded.find(&music, &id), Some(music.join("sub/a.wav")));
    }

    #[test]
    fn unchanged_files_are_not_rehashed() {
        let root = temp_dir("unchanged");
        let music = root.join("music");
        fs::create_dir_all(&music).unwrap();
        isolate(&root);
        write_wav(&music.join("a.wav"), &[0.1, -0.1], 2, 44_100).unwrap();

        let mut index = LibraryIndex::load(&music);
        index.refresh(&music).unwrap();
        let real_id = crate::track::decode(&music.join("a.wav")).unwrap().id;

        // Poison the cached entry so a real re-hash would be obvious.
        let entry = index.by_path.get_mut(Path::new("a.wav")).unwrap();
        entry.id = "deadbeef".to_string();
        let file = index_file(&music);
        write_json(&file, &IndexFile { version: VERSION, entries: index.by_path.values().cloned().collect() }).unwrap();

        let mut index = LibraryIndex::load(&music);
        index.refresh(&music).unwrap(); // mtime/size unchanged: should keep the poisoned id, not re-hash
        assert_eq!(index.find(&music, "deadbeef"), Some(music.join("a.wav")));
        assert_eq!(index.find(&music, &real_id), None, "a real re-hash would have overwritten the poisoned entry");
    }

    #[test]
    fn a_changed_file_gets_rehashed() {
        let root = temp_dir("changed");
        let music = root.join("music");
        fs::create_dir_all(&music).unwrap();
        isolate(&root);
        write_wav(&music.join("a.wav"), &[0.1, -0.1], 2, 44_100).unwrap();

        let mut index = LibraryIndex::load(&music);
        index.refresh(&music).unwrap();
        let old_id = crate::track::decode(&music.join("a.wav")).unwrap().id;

        std::thread::sleep(std::time::Duration::from_secs(1)); // mtime granularity
        write_wav(&music.join("a.wav"), &[0.2, -0.2, 0.3], 2, 44_100).unwrap();
        let new_id = crate::track::decode(&music.join("a.wav")).unwrap().id;
        assert_ne!(old_id, new_id);

        index.refresh(&music).unwrap();
        assert_eq!(index.find(&music, &new_id), Some(music.join("a.wav")));
        assert_eq!(index.find(&music, &old_id), None);
    }

    #[test]
    fn a_removed_file_drops_out_of_the_index() {
        let root = temp_dir("removed");
        let music = root.join("music");
        fs::create_dir_all(&music).unwrap();
        isolate(&root);
        write_wav(&music.join("a.wav"), &[0.1, -0.1], 2, 44_100).unwrap();

        let mut index = LibraryIndex::load(&music);
        index.refresh(&music).unwrap();
        let id = crate::track::decode(&music.join("a.wav")).unwrap().id;
        assert!(index.find(&music, &id).is_some());

        fs::remove_file(music.join("a.wav")).unwrap();
        index.refresh(&music).unwrap();
        assert_eq!(index.find(&music, &id), None);
    }

    #[test]
    fn find_is_none_for_an_unknown_id() {
        let root = temp_dir("unknown");
        fs::create_dir_all(&root).unwrap();
        isolate(&root);
        let index = LibraryIndex::load(&root);
        assert_eq!(index.find(&root, "nope"), None);
    }

    #[test]
    fn refresher_reports_progress_then_a_usable_index() {
        let root = temp_dir("refresher");
        let music = root.join("music");
        fs::create_dir_all(&music).unwrap();
        isolate(&root);
        write_wav(&music.join("a.wav"), &[0.1, -0.1], 2, 44_100).unwrap();
        let id = crate::track::decode(&music.join("a.wav")).unwrap().id;

        let refresher = IndexRefresher::start(music.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut finished = None;
        while finished.is_none() && std::time::Instant::now() < deadline {
            if let Some(RefreshProgress::Finished(index)) = refresher.poll() {
                finished = Some(index);
            }
        }
        let index = finished.expect("refresher finished within the deadline");
        assert_eq!(index.find(&music, &id), Some(music.join("a.wav")));
    }
}
