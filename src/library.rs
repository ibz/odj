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

/// Fuzzy match of `query` in `text`, ignoring case and spaces in the query: the
/// query's characters must appear in order. Returns a score (higher is better) and
/// the matched character positions in `text`, choosing the placement that scores
/// best: word starts and runs of consecutive characters count extra, gaps cost.
pub fn fuzzy_match(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    const MATCH: i32 = 16;
    const WORD_START: i32 = 8;
    const CONSECUTIVE: i32 = 8;
    const NONE: i32 = i32::MIN / 2;

    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(lower).collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    let t: Vec<char> = text.chars().map(lower).collect();
    let (m, n) = (q.len(), t.len());
    let bonus = |j: usize| MATCH + if j == 0 || !t[j - 1].is_alphanumeric() { WORD_START } else { 0 };

    // score[i][j]: best score with q[..=i] matched and q[i] on t[j]; from[i][j]: where q[i-1] sat.
    let mut score = vec![vec![NONE; n]; m];
    let mut from = vec![vec![0usize; n]; m];
    for j in 0..n {
        if t[j] == q[0] {
            score[0][j] = bonus(j) - j.min(16) as i32 / 4;
        }
    }
    for i in 1..m {
        // Best score[i-1][k] + k over k < j-1, so a gap from k to j costs j-k-1.
        let mut best_gap = (NONE, 0);
        for j in 1..n {
            if j >= 2 && score[i - 1][j - 2] > NONE {
                let k = j - 2;
                best_gap = best_gap.max((score[i - 1][k] + k as i32, k));
            }
            if t[j] != q[i] {
                continue;
            }
            let run = score[i - 1][j - 1].saturating_add(CONSECUTIVE);
            let gap = best_gap.0 - (j as i32 - 1);
            let (prev, k) = if run >= gap { (run, j - 1) } else { (gap, best_gap.1) };
            if prev > NONE / 2 {
                score[i][j] = prev + bonus(j);
                from[i][j] = k;
            }
        }
    }

    let (best, mut j) = (0..n).map(|j| (score[m - 1][j], j)).max_by_key(|&(s, j)| (s, std::cmp::Reverse(j)))?;
    if best <= NONE / 2 {
        return None;
    }
    let mut positions = vec![0; m];
    for i in (0..m).rev() {
        positions[i] = j;
        j = from[i][j];
    }
    Some((best, positions))
}

pub struct Browser {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    /// Typed filter over the current folder.
    pub query: String,
    /// What the list shows: indices into `entries`, best match first, with the
    /// matched character positions of each name.
    pub visible: Vec<(usize, Vec<usize>)>,
    /// Index into `visible`.
    pub selected: usize,
    pub open: bool,
}

impl Browser {
    pub fn new(dir: PathBuf) -> Self {
        let mut browser =
            Self { dir: dir.clone(), entries: Vec::new(), query: String::new(), visible: Vec::new(), selected: 0, open: false };
        browser.enter(dir, None);
        browser
    }

    pub fn enter(&mut self, dir: PathBuf, select: Option<&Path>) {
        self.entries = list(&dir);
        self.dir = dir;
        self.query.clear();
        self.filter();
        self.selected = 0;
        if let Some(path) = select {
            self.select(path);
        }
    }

    /// Selects the entry for `path`, if it is shown.
    pub fn select(&mut self, path: &Path) {
        if let Some(i) = self.visible.iter().position(|&(i, _)| self.entries[i].path == path) {
            self.selected = i;
        }
    }

    pub fn entry(&self, visible_index: usize) -> Option<&Entry> {
        self.visible.get(visible_index).map(|&(i, _)| &self.entries[i])
    }

    fn filter(&mut self) {
        let mut matches: Vec<(i32, usize, Vec<usize>)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| fuzzy_match(&self.query, &e.name).map(|(score, pos)| (score, i, pos)))
            .collect();
        // Stable, so equal scores keep folder order (folders first, then by name).
        matches.sort_by_key(|m| std::cmp::Reverse(m.0));
        self.visible = matches.into_iter().map(|(_, i, pos)| (i, pos)).collect();
        self.selected = 0;
    }

    pub fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.filter();
    }

    /// Deletes the last typed character; false when there was nothing to delete.
    pub fn backspace(&mut self) -> bool {
        let deleted = self.query.pop().is_some();
        if deleted {
            self.filter();
        }
        deleted
    }

    /// Clears the filter; false when there was none.
    pub fn clear_query(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.query.clear();
        self.filter();
        true
    }

    pub fn close(&mut self) {
        self.open = false;
        self.clear_query();
    }

    pub fn parent(&mut self) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            let from = self.dir.clone();
            self.enter(parent, Some(&from));
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Opens a folder, or returns the selected file to load.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let entry = self.entry(self.selected)?;
        if entry.is_dir {
            let dir = entry.path.clone();
            self.enter(dir, None);
            None
        } else {
            Some(entry.path.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser(names: &[&str]) -> Browser {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("odj-browser-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in names {
            if let Some(d) = name.strip_suffix('/') {
                fs::create_dir(dir.join(d)).unwrap();
            } else {
                fs::write(dir.join(name), b"").unwrap();
            }
        }
        Browser::new(dir)
    }

    fn shown(b: &Browser) -> Vec<&str> {
        b.visible.iter().map(|&(i, _)| b.entries[i].name.as_str()).collect()
    }

    #[test]
    fn fuzzy_matches_in_order_ignoring_case_and_spaces() {
        assert!(fuzzy_match("dpaw", "Daft Punk - Around the World.mp3").is_some());
        assert!(fuzzy_match("daft punk", "Daft_Punk.mp3").is_some());
        assert!(fuzzy_match("wpd", "Daft Punk - Around the World.mp3").is_none());
        assert_eq!(fuzzy_match("ab", "xaxb").unwrap().1, vec![1, 3]);
    }

    #[test]
    fn tight_and_word_start_matches_rank_first() {
        let score = |q, t| fuzzy_match(q, t).unwrap().0;
        assert!(score("one", "One More Time.mp3") > score("one", "Get Lucky (Phone Edit).mp3"));
        assert!(score("tm", "Time Machine.mp3") > score("tm", "Atmosphere.mp3"));
        assert!(score("abc", "abc.mp3") > score("abc", "a-b-c.mp3"));
    }

    #[test]
    fn typing_filters_current_folder() {
        let mut b = browser(&["Mixes/", "Around the World.mp3", "One More Time.flac", "notes.txt"]);
        assert_eq!(shown(&b), ["Mixes", "Around the World.mp3", "One More Time.flac"]);
        for c in "omt".chars() {
            b.type_char(c);
        }
        assert_eq!(shown(&b), ["One More Time.flac"]);
        assert_eq!(b.visible[0].1, vec![0, 4, 9]);
        assert!(b.backspace());
        assert!(b.clear_query());
        assert!(!b.backspace());
        assert_eq!(shown(&b).len(), 3);
    }

    #[test]
    fn activate_uses_filtered_selection_and_resets_filter() {
        let mut b = browser(&["Alpha/", "Beta/", "song.mp3"]);
        b.type_char('b');
        assert_eq!(b.activate(), None);
        assert!(b.dir.ends_with("Beta"));
        assert!(b.query.is_empty());
    }
}
