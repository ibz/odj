//! Folder browsing, the stand-in for a player's disc/USB track list.

use std::fs;
use std::path::{Path, PathBuf};

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "oga", "m4a", "mp4", "aac", "aif", "aiff", "caf", "mka", "webm",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// Lists folders then audio files, sorted by name.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let is_dir = path.is_dir();
            (is_dir || is_audio(&path)).then_some(Entry { path, name, is_dir })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

/// Audio files in the same folder as `track`, in browser order.
pub fn siblings(track: &Path) -> Vec<PathBuf> {
    track
        .parent()
        .map(|dir| list(dir).into_iter().filter(|e| !e.is_dir).map(|e| e.path).collect())
        .unwrap_or_default()
}

pub struct Browser {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub open: bool,
}

impl Browser {
    pub fn new(dir: PathBuf) -> Self {
        let entries = list(&dir);
        Self { dir, entries, selected: 0, open: false }
    }

    pub fn enter(&mut self, dir: PathBuf, select: Option<&Path>) {
        self.entries = list(&dir);
        self.selected = select
            .and_then(|p| self.entries.iter().position(|e| e.path == p))
            .unwrap_or(0);
        self.dir = dir;
    }

    pub fn parent(&mut self) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            let from = self.dir.clone();
            self.enter(parent, Some(&from));
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Opens a folder, or returns the selected file to load.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?;
        if entry.is_dir {
            let dir = entry.path.clone();
            self.enter(dir, None);
            None
        } else {
            Some(entry.path.clone())
        }
    }
}
