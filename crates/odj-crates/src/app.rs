//! App state and key handling: a list of crates, drilled into one at a
//! time to manage its tracks — download new ones (still via yt-dlp into
//! `~/Downloads`, now also registered into the crate) or add existing
//! files/directories from a configured music folder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use odj_core::crates::{self, Availability, AvailabilityChecker, Crate, CrateTrack};
use odj_core::library::{self, Browser};
use odj_core::memory;
use odj_core::track;

use crate::queue::{DownloadWorker, QueueItem, Status};
use crate::search::{self, Fetcher, SearchResult, search_in_background};

/// Two presses on the same track within this window confirm removing its
/// metadata — same window odj-player's own loop-memory delete uses.
const DELETE_CONFIRM: Duration = Duration::from_secs(2);
/// How long a status message stays in the footer before it fades back to
/// the contextual hints — same value and reasoning as odj-player's and
/// odj-sampler's own `STATUS_TIME`.
const STATUS_TIME: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    /// Typing always lands here, like the player's folder browser filter.
    Search,
    Results,
    Queue,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Search => Focus::Results,
            Focus::Results => Focus::Queue,
            Focus::Queue => Focus::Search,
        }
    }
}

pub struct CrateList {
    pub rows: Vec<Crate>,
    pub selected: usize,
    /// Armed by a first `X`/Delete on this crate's directory; a second
    /// one within `DELETE_CONFIRM` actually deletes it — same two-press
    /// pattern as `CrateDetail`'s own track delete, but for something far
    /// less recoverable (the whole crate, kits included), hence requiring
    /// it here too rather than going straight through.
    pending_delete: Option<(PathBuf, Instant)>,
}

impl CrateList {
    fn load() -> Self {
        Self { rows: crates::list_crates(), selected: 0, pending_delete: None }
    }

    /// The crate dir a second `X`/Delete would actually delete right now,
    /// if any — for the UI to show "press again" only where it applies.
    pub fn armed(&self) -> Option<&Path> {
        self.pending_delete.as_ref().filter(|(_, at)| at.elapsed() < DELETE_CONFIRM).map(|(dir, _)| dir.as_path())
    }
}

/// A crate's own track roster, drilled into from [`CrateList`] — mirrors
/// odj-player's `CrateBrowser` (same `AvailabilityChecker` reuse), minus
/// anything about "active" crates: odj-crates never touches
/// `settings.active_crates`, that stays odj-player/odj-sampler's own
/// browse-tab concern.
pub struct CrateDetail {
    pub dir: PathBuf,
    pub name: String,
    tracks: Vec<CrateTrack>,
    checker: AvailabilityChecker,
    availability: HashMap<String, Availability>,
    pub selected: usize,
    /// Armed by a first `X`/Delete on this track id; a second one within
    /// `DELETE_CONFIRM` actually removes it.
    pending_delete: Option<(String, Instant)>,
}

impl CrateDetail {
    fn load(dir: PathBuf) -> Self {
        let name = Crate::load(dir.clone()).name;
        let music_dirs = memory::load_settings().music_dirs;
        let tracks = crates::list_tracks(&Crate::tracks_dir(&dir), &music_dirs);
        let checker = AvailabilityChecker::new();
        for t in &tracks {
            checker.check(t.id.clone(), t.path.clone());
        }
        Self { dir, name, tracks, checker, availability: HashMap::new(), selected: 0, pending_delete: None }
    }

    pub fn tracks(&self) -> &[CrateTrack] {
        &self.tracks
    }

    pub fn availability(&self, id: &str) -> Option<Availability> {
        self.availability.get(id).copied()
    }

    /// The track id a second `X`/Delete would actually remove right now,
    /// if any — for the UI to show "press again" only where it applies.
    pub fn armed(&self) -> Option<&str> {
        self.pending_delete.as_ref().filter(|(_, at)| at.elapsed() < DELETE_CONFIRM).map(|(id, _)| id.as_str())
    }

    fn poll(&mut self) {
        while let Some((id, a)) = self.checker.poll() {
            self.availability.insert(id, a);
        }
    }
}

/// Picking existing audio files (or whole folders of them) from a
/// configured music folder — wraps `odj_core::library::Browser`, rooted
/// the same contained way odj-player's own Folder tab is: a folder-picker
/// step first with 2+ configured, straight in with exactly one.
pub struct AddExisting {
    pub crate_dir: PathBuf,
    pub music_dirs: Vec<PathBuf>,
    /// `Some(i)` while picking among `music_dirs`; `None` once `browser`
    /// is actually open.
    pub picking_folder: Option<usize>,
    pub browser: Option<Browser>,
    /// Entries marked with `Space` for a batch add — files and/or whole
    /// folders. Empty just means "add whatever's highlighted" instead.
    selected: HashSet<PathBuf>,
    adding: Option<BatchAdder>,
    /// How far the running `adding` job has gotten, for the progress
    /// window — `(0, 0)` until its first report.
    adding_progress: (usize, usize),
}

impl AddExisting {
    /// `Some(status message)` if nothing's configured to browse at all —
    /// the caller shows it and never opens this screen.
    fn start(crate_dir: PathBuf) -> Result<Self, String> {
        let music_dirs = memory::load_settings().music_dirs;
        if music_dirs.is_empty() {
            return Err("No music folders configured — add one in odj-player first".to_string());
        }
        let (picking_folder, browser) = if music_dirs.len() == 1 {
            (None, Some(Browser::new(music_dirs[0].clone())))
        } else {
            (Some(0), None)
        };
        Ok(Self { crate_dir, music_dirs, picking_folder, browser, selected: HashSet::new(), adding: None, adding_progress: (0, 0) })
    }

    /// Entries marked for a batch add, so the list can mark them.
    pub fn selected(&self) -> &HashSet<PathBuf> {
        &self.selected
    }

    /// Whether a background add job is running — the UI shows a progress
    /// window and ignores input while it does; the actual numbers come
    /// from `adding_progress`.
    pub fn adding(&self) -> bool {
        self.adding.is_some()
    }

    pub fn adding_progress(&self) -> (usize, usize) {
        self.adding_progress
    }
}

/// How far a background add job has gotten, or its result — mirrors
/// `library_index::IndexRefresher`'s shape.
enum AddProgress {
    /// Currently on track `current` of `total` (1-indexed: `current ==
    /// total` means the last one is still being registered, not that
    /// every one is already done — `Finished` is what signals that).
    Scanning { current: usize, total: usize },
    Finished { added: usize, failed: usize },
}

/// One-shot background worker: registers a batch of files and/or whole
/// folders (expanded recursively) into a crate. Always backgrounded and
/// only ever started by an explicit key press, even for a single file —
/// decoding isn't instant, same "slow, so never automatic" rule
/// `library_index` follows.
struct BatchAdder {
    progress: mpsc::Receiver<AddProgress>,
}

impl BatchAdder {
    fn start(crate_dir: PathBuf, targets: Vec<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut files = Vec::new();
            for target in &targets {
                if target.is_dir() {
                    files.extend(library::list_recursive(target));
                } else {
                    files.push(target.clone());
                }
            }
            let total = files.len();
            let tracks_dir = Crate::tracks_dir(&crate_dir);
            let music_dirs = memory::load_settings().music_dirs;
            let (mut added, mut failed) = (0, 0);
            for (i, path) in files.iter().enumerate() {
                let _ = tx.send(AddProgress::Scanning { current: i + 1, total });
                match register_file(&tracks_dir, &music_dirs, path) {
                    Ok(()) => added += 1,
                    Err(_) => failed += 1,
                }
            }
            let _ = tx.send(AddProgress::Finished { added, failed });
        });
        Self { progress: rx }
    }

    fn poll(&self) -> Option<AddProgress> {
        self.progress.try_recv().ok()
    }
}

/// Decodes `path` and registers it into the roster at `tracks_dir` —
/// skipping `Track::from_samples`'s key/grid/waveform analysis entirely,
/// since a crate roster entry never carries any of that (see
/// `odj_core::crates::CrateTrack`), and running it per file would make
/// "add a whole folder" far slower than it needs to be.
fn register_file(tracks_dir: &Path, music_dirs: &[PathBuf], path: &Path) -> anyhow::Result<()> {
    let d = track::decode(path)?;
    crates::register_track(tracks_dir, music_dirs, &crate_track_for(path, d))?;
    Ok(())
}

fn crate_track_for(path: &Path, d: track::Decoded) -> CrateTrack {
    CrateTrack { id: d.id, path: path.to_path_buf(), title: d.title, artist: d.artist, sample_rate: d.sample_rate, youtube_url: None }
}

/// Search/queue/download, nested under whichever crate opened it — the
/// same three-pane flow this tool used to be entirely, just reused one
/// level down. Still always downloads into the fixed `~/Downloads`.
/// How a search result relates to this crate and `~/Downloads` already —
/// shown as a marker in the results list, and decides what `Enter`/queuing
/// it actually does.
#[derive(Debug, PartialEq)]
pub enum ResultMark {
    /// Neither — queuing it downloads it as normal.
    None,
    /// Already has an entry in this crate (same YouTube URL) — queuing
    /// it is refused, there's nothing to add.
    InCrate,
    /// Not in this crate, but a file for it already sits in
    /// `~/Downloads` (from an earlier session, or for a different
    /// crate) — queuing it registers that file directly, no download.
    AlreadyDownloaded(PathBuf),
}

pub struct Download {
    pub crate_dir: PathBuf,
    fetcher: Arc<dyn Fetcher>,
    downloads_dir: PathBuf,
    /// YouTube URLs this crate's tracks already carry — kept in sync as
    /// downloads finish, so results reflect it live without reopening
    /// this screen.
    crate_youtube_urls: HashSet<String>,
    pub query: String,
    pub results: Vec<SearchResult>,
    pub searching: bool,
    search_rx: Option<mpsc::Receiver<Result<Vec<SearchResult>, String>>>,
    pub queue: Vec<QueueItem>,
    worker: DownloadWorker,
    next_seq: u64,
    /// The queued item currently dispatched to the worker, if any — kept
    /// here (not inferred from scanning statuses) so a removed-but-still-
    /// `Queued` item can never be mistaken for one in flight.
    downloading: Option<u64>,
    pub focus: Focus,
    pub selected_result: usize,
    pub selected_queue: usize,
    /// Advances every tick; drives the searching animation.
    pub spinner: u64,
}

impl Download {
    fn start(crate_dir: PathBuf, fetcher: Arc<dyn Fetcher>, downloads_dir: PathBuf, crate_youtube_urls: HashSet<String>) -> Self {
        let worker = DownloadWorker::start(fetcher.clone(), downloads_dir.clone());
        Self {
            crate_dir,
            fetcher,
            downloads_dir,
            crate_youtube_urls,
            query: String::new(),
            results: Vec::new(),
            searching: false,
            search_rx: None,
            queue: Vec::new(),
            worker,
            next_seq: 0,
            downloading: None,
            focus: Focus::Search,
            selected_result: 0,
            selected_queue: 0,
            spinner: 0,
        }
    }

    /// How `r` relates to this crate and `~/Downloads` already — see
    /// `ResultMark`.
    pub fn mark_for(&self, r: &SearchResult) -> ResultMark {
        if self.crate_youtube_urls.contains(&r.url) {
            ResultMark::InCrate
        } else if let Some(path) = search::find_downloaded(&self.downloads_dir, &r.id) {
            ResultMark::AlreadyDownloaded(path)
        } else {
            ResultMark::None
        }
    }

    /// Whether an item is actively downloading right now — leaving this
    /// screen doesn't cancel it (the worker thread has no way to
    /// interrupt a `yt-dlp` call already in flight, see `DownloadWorker`),
    /// it would just stop listening for the result, orphaning it: the
    /// file would still land in `~/Downloads` but never get registered
    /// into the crate. So leaving is refused while this is `true`.
    pub fn downloading(&self) -> bool {
        self.downloading.is_some()
    }

    /// `Some(status message)` for anything worth telling the user about
    /// this tick — a finished search, a registered download, a crash.
    fn tick(&mut self) -> Option<String> {
        self.spinner = self.spinner.wrapping_add(1);
        if let Some(rx) = &self.search_rx {
            match rx.try_recv() {
                Ok(Ok(results)) => {
                    let n = results.len();
                    self.results = results;
                    self.selected_result = 0;
                    self.searching = false;
                    self.search_rx = None;
                    // Jump straight to the results instead of leaving focus
                    // on the search box the user just finished typing in.
                    self.focus = Focus::Results;
                    return Some(format!("{n} result{}", if n == 1 { "" } else { "s" }));
                }
                Ok(Err(e)) => {
                    self.searching = false;
                    self.search_rx = None;
                    return Some(format!("Search failed: {e}"));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.searching = false;
                    self.search_rx = None;
                    return Some("Search worker crashed".to_string());
                }
            }
        }
        let mut status = None;
        while let Ok((seq, s)) = self.worker.status_rx.try_recv() {
            let finished = matches!(&s, Status::Done(_) | Status::Failed(_));
            if let Status::Done(path) = &s
                && let Some((title, url)) = self.queue.iter().find(|q| q.seq == seq).map(|q| (q.title.clone(), q.url.clone()))
            {
                status = Some(self.register(&title, &url, path));
            }
            if let Some(item) = self.queue.iter_mut().find(|q| q.seq == seq) {
                item.status = s;
            }
            if finished && self.downloading == Some(seq) {
                self.downloading = None;
            }
        }
        self.dispatch_next();
        status
    }

    /// Decodes a file (just downloaded, or already sitting in
    /// `~/Downloads` from before) and registers it into the owning
    /// crate, with its `youtube_url` set — in that order:
    /// `register_track` would no-op if an entry already existed (it
    /// wouldn't yet here), and `set_youtube_url` only ever touches that
    /// one field, so either order is actually safe, but this one writes
    /// the real metadata first regardless. Tracks `url` as now-in-this-
    /// crate on success, so a later search doesn't offer it again.
    fn register(&mut self, title: &str, url: &str, path: &Path) -> String {
        let d = match track::decode(path) {
            Ok(d) => d,
            Err(e) => return format!("Downloaded {title}, but could not read it: {e}"),
        };
        let t = crate_track_for(path, d);
        let tracks_dir = Crate::tracks_dir(&self.crate_dir);
        let music_dirs = memory::load_settings().music_dirs;
        if let Err(e) = crates::register_track(&tracks_dir, &music_dirs, &t) {
            return format!("Downloaded {title}, but could not register it: {e}");
        }
        let _ = crates::set_youtube_url(&tracks_dir, &music_dirs, &t, Some(url.to_string()));
        self.crate_youtube_urls.insert(url.to_string());
        format!("Added \"{title}\" to the crate")
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Results if !self.results.is_empty() => {
                self.selected_result =
                    (self.selected_result as isize + delta).clamp(0, self.results.len() as isize - 1) as usize;
            }
            Focus::Queue if !self.queue.is_empty() => {
                self.selected_queue =
                    (self.selected_queue as isize + delta).clamp(0, self.queue.len() as isize - 1) as usize;
            }
            _ => {}
        }
    }

    /// `Some(status message)` for anything the key press itself decided
    /// right away (rather than later, through `tick`) — queuing a result
    /// already in the crate, or one found already downloaded.
    fn activate(&mut self) -> Option<String> {
        match self.focus {
            Focus::Search => {
                self.search();
                self.searching.then(|| "Searching…".to_string())
            }
            Focus::Results => self.enqueue_selected(),
            Focus::Queue => None,
        }
    }

    /// Stops waiting on the search and returns to the search box. The
    /// `yt-dlp` process behind it has no handle we can reach from here (the
    /// `youtube_dl` crate's `run()` owns it internally) so it finishes on
    /// its own in the background; dropping the receiver just discards
    /// whatever it eventually sends.
    fn cancel_search(&mut self) {
        self.searching = false;
        self.search_rx = None;
        self.focus = Focus::Search;
    }

    fn search(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() || self.searching {
            return;
        }
        self.searching = true;
        self.search_rx = Some(search_in_background(self.fetcher.clone(), query));
    }

    /// Queues the selected result — unless it's already in this crate
    /// (nothing to do), or a file for it already sits in `~/Downloads`
    /// (registered directly instead, no download needed at all).
    fn enqueue_selected(&mut self) -> Option<String> {
        let result = self.results.get(self.selected_result)?.clone();
        match self.mark_for(&result) {
            ResultMark::InCrate => Some(format!("\"{}\" is already in this crate", result.title)),
            ResultMark::AlreadyDownloaded(path) => {
                let seq = self.next_seq;
                self.next_seq += 1;
                let msg = self.register(&result.title, &result.url, &path);
                self.queue.push(QueueItem { seq, title: result.title, url: result.url, status: Status::Done(path) });
                self.selected_queue = self.queue.len() - 1;
                Some(msg)
            }
            ResultMark::None => {
                let seq = self.next_seq;
                self.next_seq += 1;
                self.queue.push(QueueItem { seq, title: result.title, url: result.url, status: Status::Queued });
                self.selected_queue = self.queue.len() - 1;
                self.dispatch_next();
                None
            }
        }
    }

    /// Starts the earliest still-`Queued` item downloading, if nothing
    /// already is. Called after enqueuing and after each download
    /// finishes, so the queue drains on its own.
    fn dispatch_next(&mut self) {
        if self.downloading.is_some() {
            return;
        }
        let Some(item) = self.queue.iter().find(|q| q.status == Status::Queued) else { return };
        self.downloading = Some(item.seq);
        self.worker.enqueue(item.seq, item.url.clone());
    }

    fn remove_selected(&mut self) -> Option<String> {
        let item = self.queue.get(self.selected_queue)?;
        if item.status != Status::Queued {
            return Some("Can't remove — already downloading".to_string());
        }
        self.queue.remove(self.selected_queue);
        if self.selected_queue > 0 && self.selected_queue >= self.queue.len() {
            self.selected_queue = self.queue.len() - 1;
        }
        None
    }
}

pub enum Screen {
    Crates(CrateList),
    /// Typing a new crate's name.
    NamingCrate(String),
    Crate(CrateDetail),
    Downloading(Download),
    AddingExisting(AddExisting),
}

pub struct App {
    fetcher: Arc<dyn Fetcher>,
    downloads_dir: PathBuf,
    pub screen: Screen,
    status: Option<(String, Instant)>,
    pub quit: bool,
}

impl App {
    pub fn new(fetcher: Arc<dyn Fetcher>, downloads_dir: PathBuf) -> Self {
        Self { fetcher, downloads_dir, screen: Screen::Crates(CrateList::load()), status: None, quit: false }
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// `None` once `STATUS_TIME` has passed, so the footer fades back to
    /// the contextual hints instead of a one-off message sitting there
    /// forever.
    pub fn status(&self) -> Option<&str> {
        self.status.as_ref().filter(|(_, at)| at.elapsed() < STATUS_TIME).map(|(msg, _)| msg.as_str())
    }

    pub fn tick(&mut self) {
        match &mut self.screen {
            Screen::Downloading(d) => {
                if let Some(msg) = d.tick() {
                    self.set_status(msg);
                }
            }
            Screen::AddingExisting(_) => self.tick_adding_existing(),
            Screen::Crate(c) => c.poll(),
            _ => {}
        }
    }

    fn tick_adding_existing(&mut self) {
        let Screen::AddingExisting(a) = &mut self.screen else { return };
        let Some(adder) = &a.adding else { return };
        match adder.poll() {
            Some(AddProgress::Scanning { current, total }) => {
                a.adding_progress = (current, total);
            }
            Some(AddProgress::Finished { added, failed }) => {
                let Screen::AddingExisting(a) = &mut self.screen else { return };
                a.adding = None;
                self.set_status(if failed == 0 {
                    format!("Added {added} track{}", if added == 1 { "" } else { "s" })
                } else {
                    format!("Added {added}, {failed} couldn't be read")
                });
            }
            None => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // No holds in this tool — nothing needs a release event, so there's
        // nothing to disambiguate it from a fresh press; just ignore it.
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match &self.screen {
            Screen::Crates(_) => self.crate_list_key(key),
            Screen::NamingCrate(_) => self.naming_crate_key(key),
            Screen::Crate(_) => self.crate_detail_key(key),
            Screen::Downloading(_) => self.downloading_key(key),
            Screen::AddingExisting(_) => self.adding_existing_key(key),
        }
    }

    // --- Crate list ----------------------------------------------------------

    fn crate_list_key(&mut self, key: KeyEvent) {
        if !matches!(key.code, KeyCode::Char('x') | KeyCode::Delete)
            && let Screen::Crates(list) = &mut self.screen
        {
            list.pending_delete = None;
        }
        let Screen::Crates(list) = &self.screen else { return };
        let count = list.rows.len();
        match key.code {
            KeyCode::Up => {
                let Screen::Crates(list) = &mut self.screen else { return };
                list.selected = list.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let Screen::Crates(list) = &mut self.screen else { return };
                list.selected = (list.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Right if count > 0 => {
                let Screen::Crates(list) = &self.screen else { return };
                let dir = list.rows[list.selected].dir.clone();
                self.screen = Screen::Crate(CrateDetail::load(dir));
            }
            KeyCode::Char('n') => self.screen = Screen::NamingCrate(String::new()),
            KeyCode::Char('x') | KeyCode::Delete if count > 0 => self.delete_selected_crate(),
            _ => {}
        }
    }

    /// `X`/Delete on the selected crate: first press arms it, a second
    /// one on the *same* crate within `DELETE_CONFIRM` deletes the whole
    /// directory (`Crate::delete`) — track memory, kits, everything,
    /// though never the audio files its tracks pointed at, which live
    /// elsewhere. Also drops it from `Settings.active_crates` if it was
    /// there, so odj-player/odj-sampler don't keep listing a now-missing
    /// crate as active.
    fn delete_selected_crate(&mut self) {
        let Screen::Crates(list) = &mut self.screen else { return };
        let Some(c) = list.rows.get(list.selected) else { return };
        let (dir, name, slug) = (c.dir.clone(), c.name.clone(), c.slug());
        let armed = list.pending_delete.as_ref().is_some_and(|(pd, at)| *pd == dir && at.elapsed() < DELETE_CONFIRM);
        if !armed {
            list.pending_delete = Some((dir, Instant::now()));
            self.set_status(format!("Press X/Delete again to permanently delete \"{name}\" and everything in it"));
            return;
        }
        list.pending_delete = None;
        match Crate::delete(&dir) {
            Ok(()) => {
                let mut settings = memory::load_settings();
                if let Some(i) = settings.active_crates.iter().position(|s| *s == slug) {
                    settings.active_crates.remove(i);
                    let _ = memory::save_settings(&settings);
                }
                self.screen = Screen::Crates(CrateList::load());
                self.set_status(format!("Deleted \"{name}\""));
            }
            Err(e) => self.set_status(format!("Could not delete \"{name}\": {e}")),
        }
    }

    // --- Naming a new crate ----------------------------------------------------

    /// Takes the raw `KeyEvent`, not just its `KeyCode`, so Ctrl/Alt combos
    /// don't get typed as text.
    fn naming_crate_key(&mut self, key: KeyEvent) {
        let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if typing => {
                let Screen::NamingCrate(name) = &mut self.screen else { return };
                name.push(c);
            }
            KeyCode::Backspace => {
                let Screen::NamingCrate(name) = &mut self.screen else { return };
                name.pop();
            }
            KeyCode::Enter => {
                let Screen::NamingCrate(name) = &self.screen else { return };
                let name = name.trim().to_string();
                if !name.is_empty() {
                    self.create_named_crate(&name);
                }
            }
            KeyCode::Esc => self.screen = Screen::Crates(CrateList::load()),
            _ => {}
        }
    }

    /// Under the fixed `crates_root()` (not user-configurable), either
    /// adopts a directory that's already sitting there under this exact
    /// name's slug and already looks like a crate (e.g. `git clone`d in by
    /// hand ahead of time) or creates a fresh one, deduped against
    /// whatever else is already there, then opens it. Both odj-player and
    /// odj-sampler discover it on their own "active crates" tab via
    /// `list_crates()` — there's no separate "known" list to maintain.
    fn create_named_crate(&mut self, name: &str) {
        let root = crates::crates_root();
        let candidate = root.join(crates::slugify(name));
        let already_a_crate = Crate::manifest_file(&candidate).is_file() || Crate::tracks_dir(&candidate).is_dir();
        let (dir, result) = if already_a_crate {
            (candidate.clone(), Ok(()))
        } else {
            let dir = root.join(crates::unique_slug(&root, name));
            (dir.clone(), Crate::create(dir, name).map(|_| ()))
        };
        match result {
            Ok(()) => self.screen = Screen::Crate(CrateDetail::load(dir)),
            Err(e) => self.set_status(format!("Could not create crate: {e}")),
        }
    }

    // --- A crate's own track list ------------------------------------------

    fn crate_detail_key(&mut self, key: KeyEvent) {
        if !matches!(key.code, KeyCode::Char('x') | KeyCode::Delete)
            && let Screen::Crate(c) = &mut self.screen
        {
            c.pending_delete = None;
        }
        let Screen::Crate(c) = &self.screen else { return };
        let count = c.tracks.len();
        match key.code {
            KeyCode::Esc => self.screen = Screen::Crates(CrateList::load()),
            KeyCode::Up => {
                let Screen::Crate(c) = &mut self.screen else { return };
                c.selected = c.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let Screen::Crate(c) = &mut self.screen else { return };
                c.selected = (c.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('x') | KeyCode::Delete => self.delete_selected_track(),
            KeyCode::Char('d') => self.open_download(),
            KeyCode::Char('a') => self.open_adding_existing(),
            _ => {}
        }
    }

    /// `X`/Delete on the selected track: first press arms it, a second
    /// one on the *same* track within `DELETE_CONFIRM` removes just its
    /// metadata (`Memory::forget_track`) — the audio file at its `path`
    /// is never touched, there is no way to reach it from here other than
    /// that one field.
    fn delete_selected_track(&mut self) {
        let Screen::Crate(c) = &mut self.screen else { return };
        let Some(id) = c.tracks.get(c.selected).map(|t| t.id.clone()) else { return };
        let armed = c.pending_delete.as_ref().is_some_and(|(pid, at)| *pid == id && at.elapsed() < DELETE_CONFIRM);
        if !armed {
            c.pending_delete = Some((id, Instant::now()));
            self.set_status("Press X/Delete again to remove this track — the audio file stays");
            return;
        }
        c.pending_delete = None;
        let dir = c.dir.clone();
        let result = crates::forget_track(&Crate::tracks_dir(&dir), &id);
        let Screen::Crate(c) = &mut self.screen else { return };
        match result {
            Ok(()) => {
                c.tracks.retain(|t| t.id != id);
                c.selected = c.selected.min(c.tracks.len().saturating_sub(1));
                self.set_status("Removed from the crate — the audio file was left untouched");
            }
            Err(e) => self.set_status(format!("Could not remove track: {e}")),
        }
    }

    fn open_download(&mut self) {
        let Screen::Crate(c) = &self.screen else { return };
        let crate_dir = c.dir.clone();
        let crate_youtube_urls: HashSet<String> = c.tracks().iter().filter_map(|t| t.youtube_url.clone()).collect();
        self.screen = Screen::Downloading(Download::start(crate_dir, self.fetcher.clone(), self.downloads_dir.clone(), crate_youtube_urls));
    }

    fn open_adding_existing(&mut self) {
        let Screen::Crate(c) = &self.screen else { return };
        let crate_dir = c.dir.clone();
        match AddExisting::start(crate_dir) {
            Ok(a) => self.screen = Screen::AddingExisting(a),
            Err(msg) => self.set_status(msg),
        }
    }

    /// Back from `Downloading`/`AddingExisting` to the crate's own track
    /// list — reloaded fresh, so anything just registered shows up.
    fn back_to_crate(&mut self) {
        let dir = match &self.screen {
            Screen::Downloading(d) => d.crate_dir.clone(),
            Screen::AddingExisting(a) => a.crate_dir.clone(),
            _ => return,
        };
        self.screen = Screen::Crate(CrateDetail::load(dir));
    }

    // --- Downloading ---------------------------------------------------------

    fn downloading_key(&mut self, key: KeyEvent) {
        let Screen::Downloading(d) = &self.screen else { return };
        let searching = d.searching;
        let query_empty = d.query.is_empty();
        let downloading = d.downloading();
        match key.code {
            KeyCode::Esc if searching => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                d.cancel_search();
                self.set_status("Search cancelled");
            }
            KeyCode::Esc if !query_empty => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                d.query.clear();
                d.focus = Focus::Search;
            }
            // Leaving wouldn't stop it (the worker has no way to interrupt
            // a `yt-dlp` call already in flight) — it would just orphan it:
            // the file would still land in `~/Downloads` but never get
            // registered into the crate, with nothing left on screen to
            // say so. Refuse instead of pretending to cancel.
            KeyCode::Esc if downloading => {
                self.set_status("A download is in progress — wait for it to finish before leaving");
            }
            KeyCode::Esc => self.back_to_crate(),
            KeyCode::Tab => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                d.focus = d.focus.next();
            }
            KeyCode::Backspace => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                d.query.pop();
            }
            KeyCode::Up => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                d.move_selection(-1);
            }
            KeyCode::Down => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                d.move_selection(1);
            }
            KeyCode::Enter => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                if let Some(msg) = d.activate() {
                    self.set_status(msg);
                }
            }
            KeyCode::Delete if d.focus == Focus::Queue => self.remove_selected_download(),
            KeyCode::Char('x') if d.focus == Focus::Queue => self.remove_selected_download(),
            KeyCode::Char(c) => {
                let Screen::Downloading(d) = &mut self.screen else { return };
                d.focus = Focus::Search;
                d.query.push(c);
            }
            _ => {}
        }
    }

    fn remove_selected_download(&mut self) {
        let Screen::Downloading(d) = &mut self.screen else { return };
        if let Some(msg) = d.remove_selected() {
            self.set_status(msg);
        }
    }

    // --- Adding an existing file or folder -------------------------------------

    fn adding_existing_key(&mut self, key: KeyEvent) {
        let Screen::AddingExisting(a) = &self.screen else { return };
        if a.adding.is_some() {
            return;
        }
        if let Some(i) = a.picking_folder {
            self.picking_folder_key(i, key.code);
            return;
        }
        // Bare `a` adds instead of typing into the filter, so a query
        // can never contain the letter — accepted trade-off for a short,
        // uncombinated key.
        if key.code == KeyCode::Char('a') && !key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            self.add_selection();
            return;
        }
        enum Action {
            None,
            UpOrClose,
            ToggleSelected(PathBuf),
        }
        let typing = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Screen::AddingExisting(a) = &mut self.screen else { return };
        let Some(browser) = &mut a.browser else { return };
        let action = match key.code {
            // Space, not a bare letter: toggles the highlighted entry for
            // a batch add (see `a` above) without being swallowed by the
            // filter — it has no effect on matching there anyway
            // (`fuzzy_match` ignores whitespace in the query).
            KeyCode::Char(' ') => match browser.entry(browser.selected) {
                Some(entry) => Action::ToggleSelected(entry.path.clone()),
                None => Action::None,
            },
            KeyCode::Char(c) if typing => {
                browser.type_char(c);
                Action::None
            }
            KeyCode::Up => {
                browser.move_by(-1);
                Action::None
            }
            KeyCode::Down => {
                browser.move_by(1);
                Action::None
            }
            KeyCode::Backspace => {
                if browser.backspace() {
                    Action::None
                } else {
                    Action::UpOrClose
                }
            }
            KeyCode::Left => Action::UpOrClose,
            // Enter/Right is pure navigation: drills into a folder, same
            // as ever; a no-op on a file — there's nowhere to drill into,
            // and adding is `a`'s job now, not Enter's.
            KeyCode::Enter | KeyCode::Right => {
                browser.activate();
                Action::None
            }
            KeyCode::Esc => {
                if browser.clear_query() {
                    Action::None
                } else {
                    Action::UpOrClose
                }
            }
            _ => Action::None,
        };
        match action {
            Action::None => {}
            Action::UpOrClose => self.up_in_adding_existing(),
            Action::ToggleSelected(path) => {
                let Screen::AddingExisting(a) = &mut self.screen else { return };
                if !a.selected.remove(&path) {
                    a.selected.insert(path);
                }
            }
        }
    }

    fn picking_folder_key(&mut self, i: usize, code: KeyCode) {
        let Screen::AddingExisting(a) = &mut self.screen else { return };
        match code {
            KeyCode::Up => a.picking_folder = Some(i.saturating_sub(1)),
            KeyCode::Down => a.picking_folder = Some((i + 1).min(a.music_dirs.len().saturating_sub(1))),
            KeyCode::Enter | KeyCode::Right => {
                let dir = a.music_dirs[i].clone();
                a.browser = Some(Browser::new(dir));
                a.picking_folder = None;
            }
            KeyCode::Esc => self.back_to_crate(),
            _ => {}
        }
    }

    /// Going up from a music dir's own root goes back to the folder
    /// picker (with 2+ configured) rather than above it — containment is
    /// enforced here, same approach odj-player's Folder tab already uses,
    /// not by restricting `Browser` itself.
    fn up_in_adding_existing(&mut self) {
        let Screen::AddingExisting(a) = &mut self.screen else { return };
        if let Some(browser) = &mut a.browser
            && browser.dir.parent().is_some() && !a.music_dirs.iter().any(|d| d == &browser.dir)
        {
            browser.parent();
            return;
        }
        if a.music_dirs.len() > 1 {
            a.browser = None;
            a.picking_folder = Some(0);
            return;
        }
        self.back_to_crate();
    }

    /// `a`: adds whatever's marked with `Space`, or — with nothing marked
    /// — just the highlighted entry, same as pressing Space on it alone
    /// then `a`. Either way it's one background batch (see `BatchAdder`),
    /// with a progress window, even for a single file: decoding isn't
    /// instant and shouldn't look like it froze.
    fn add_selection(&mut self) {
        let Screen::AddingExisting(a) = &self.screen else { return };
        let crate_dir = a.crate_dir.clone();
        let targets: Vec<PathBuf> = if !a.selected.is_empty() {
            a.selected.iter().cloned().collect()
        } else {
            let Some(browser) = &a.browser else { return };
            let Some(entry) = browser.entry(browser.selected) else { return };
            vec![entry.path.clone()]
        };
        let adder = BatchAdder::start(crate_dir, targets);
        let Screen::AddingExisting(a) = &mut self.screen else { return };
        a.selected.clear();
        a.adding_progress = (0, 0);
        a.adding = Some(adder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_util::{FakeFetcher, result};
    use std::time::{Duration, Instant};

    fn with_temp_dirs<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let dir = std::env::temp_dir().join(format!("odj-crates-app-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only, same pattern used throughout the workspace's tests.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let result = f(&dir);
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn app(fetcher: Arc<dyn Fetcher>, downloads_dir: PathBuf) -> App {
        App::new(fetcher, downloads_dir)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// Runs `tick()` until `done` is true or `timeout` elapses, so tests
    /// don't race the background search/download threads.
    fn wait_for(app: &mut App, timeout: Duration, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + timeout;
        while !done(app) {
            assert!(Instant::now() < deadline, "condition never became true");
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn creating_a_crate_is_discoverable_and_opens_its_empty_track_list() {
        with_temp_dirs("create", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            press(&mut app, KeyCode::Char('n'));
            type_str(&mut app, "My Set");
            press(&mut app, KeyCode::Enter);

            let Screen::Crate(c) = &app.screen else { panic!("expected the new crate's track list") };
            assert_eq!(c.name, "My Set");
            assert!(c.tracks().is_empty());

            assert!(crates::list_crates().iter().any(|cr| cr.name == "My Set"));
        });
    }

    #[test]
    fn esc_from_an_empty_name_cancels_back_to_the_list() {
        with_temp_dirs("cancel-name", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            press(&mut app, KeyCode::Char('n'));
            press(&mut app, KeyCode::Esc);
            assert!(matches!(app.screen, Screen::Crates(_)));
        });
    }

    fn create_crate(app: &mut App, name: &str) -> PathBuf {
        press(app, KeyCode::Char('n'));
        type_str(app, name);
        press(app, KeyCode::Enter);
        let Screen::Crate(c) = &app.screen else { panic!("just created it") };
        c.dir.clone()
    }

    #[test]
    fn a_download_reaching_done_registers_a_track_with_its_youtube_url() {
        with_temp_dirs("download-registers", |dir| {
            let fetcher = Arc::new(FakeFetcher::new(vec![result("One More Time")]));
            let mut app = app(fetcher, dir.join("downloads"));
            create_crate(&mut app, "Set");

            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter); // search
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.searching,
                _ => false,
            });
            press(&mut app, KeyCode::Enter); // queue "One More Time"
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => matches!(d.queue.first().map(|q| &q.status), Some(Status::Done(_))),
                _ => false,
            });
            // The registration message lands as a status once `tick` sees `Done`.
            assert!(app.status().is_some_and(|s| s.contains("Added") && s.contains("One More Time")), "{:?}", app.status());

            press(&mut app, KeyCode::Esc); // clears the leftover "x" query
            press(&mut app, KeyCode::Esc); // now back to the crate
            let Screen::Crate(c) = &app.screen else { panic!("Esc with an empty query goes back") };
            assert_eq!(c.tracks().len(), 1);
            assert_eq!(c.tracks()[0].youtube_url.as_deref(), Some("https://example.com/One More Time"));
        });
    }

    #[test]
    fn a_result_already_in_the_crate_is_marked_and_queuing_it_is_refused() {
        with_temp_dirs("already-in-crate", |dir| {
            let fetcher = Arc::new(FakeFetcher::new(vec![result("One More Time")]));
            let mut app = app(fetcher.clone(), dir.join("downloads"));
            create_crate(&mut app, "Set");

            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.searching,
                _ => false,
            });
            press(&mut app, KeyCode::Enter); // queue it for real, the first time
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => matches!(d.queue.first().map(|q| &q.status), Some(Status::Done(_))),
                _ => false,
            });
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc); // back to the crate, now with one track

            // Reopen Download for the same crate: it's already in there.
            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.searching,
                _ => false,
            });
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert_eq!(d.mark_for(&d.results[0]), ResultMark::InCrate);

            press(&mut app, KeyCode::Enter); // try to queue it again
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert!(d.queue.is_empty(), "nothing to add — it's already there");
            assert!(app.status().is_some_and(|s| s.contains("already in this crate")), "{:?}", app.status());
            assert_eq!(fetcher.downloaded.lock().unwrap().len(), 1, "only the first, real download ever happened");
        });
    }

    #[test]
    fn a_result_already_downloaded_is_registered_without_a_real_download() {
        with_temp_dirs("already-downloaded", |dir| {
            let downloads = dir.join("downloads");
            std::fs::create_dir_all(&downloads).unwrap();
            let r = result("Robot Rock");
            odj_core::wav::write_wav(&downloads.join(format!("Robot Rock [{}].wav", r.id)), &[0.1, -0.1, 0.2, -0.2], 2, 44_100).unwrap();

            let fetcher = Arc::new(FakeFetcher::new(vec![r]));
            let mut app = app(fetcher.clone(), downloads.clone());
            create_crate(&mut app, "Set");

            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.searching,
                _ => false,
            });
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert!(matches!(d.mark_for(&d.results[0]), ResultMark::AlreadyDownloaded(_)));

            press(&mut app, KeyCode::Enter); // registers the existing file directly
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert!(matches!(d.queue.first().map(|q| &q.status), Some(Status::Done(_))), "no queueing/downloading step at all");
            assert!(app.status().is_some_and(|s| s.contains("Added")), "{:?}", app.status());
            assert!(fetcher.downloaded.lock().unwrap().is_empty(), "never actually downloaded");

            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);
            let Screen::Crate(c) = &app.screen else { panic!("Esc with an empty query goes back") };
            assert_eq!(c.tracks().len(), 1);
        });
    }

    #[test]
    fn adding_an_existing_file_registers_it_without_a_youtube_url() {
        with_temp_dirs("add-file", |dir| {
            let music = dir.join("music");
            std::fs::create_dir_all(&music).unwrap();
            odj_core::wav::write_wav(&music.join("track.wav"), &[0.1, -0.1, 0.2, -0.2], 2, 44_100).unwrap();
            let mut settings = odj_core::memory::load_settings();
            settings.music_dirs = vec![music.clone()];
            odj_core::memory::save_settings(&settings).unwrap();

            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('a'));

            let Screen::AddingExisting(a) = &app.screen else { panic!("one music folder: straight into the browser") };
            assert!(a.browser.is_some());
            press(&mut app, KeyCode::Enter); // navigation only now: a no-op on this file
            let Screen::AddingExisting(a) = &app.screen else { unreachable!() };
            assert!(a.browser.is_some(), "Enter never adds a file any more");

            press(&mut app, KeyCode::Char('a')); // adds the highlighted track.wav
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::AddingExisting(a) => !a.adding(),
                _ => false,
            });

            press(&mut app, KeyCode::Esc); // back to the crate
            let Screen::Crate(c) = &app.screen else { panic!("Esc with an empty query goes back") };
            assert_eq!(c.tracks().len(), 1);
            assert_eq!(c.tracks()[0].youtube_url, None);
        });
    }

    #[test]
    fn a_on_a_directory_adds_every_track_under_it_recursively() {
        with_temp_dirs("add-dir", |dir| {
            let music = dir.join("music");
            std::fs::create_dir_all(music.join("album")).unwrap();
            odj_core::wav::write_wav(&music.join("album/a.wav"), &[0.1, -0.1], 2, 44_100).unwrap();
            odj_core::wav::write_wav(&music.join("album/b.wav"), &[0.2, -0.2, 0.3], 2, 44_100).unwrap();
            let mut settings = odj_core::memory::load_settings();
            settings.music_dirs = vec![music.clone()];
            odj_core::memory::save_settings(&settings).unwrap();

            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('a')); // open the add-existing browser
            press(&mut app, KeyCode::Char('a')); // the only entry: album/, added recursively

            wait_for(&mut app, Duration::from_secs(5), |a| a.status().is_some_and(|s| s.starts_with("Added")));
            assert!(app.status().is_some_and(|s| s.contains("2")), "{:?}", app.status());

            press(&mut app, KeyCode::Esc);
            let Screen::Crate(c) = &app.screen else { panic!("Esc with an empty query goes back") };
            assert_eq!(c.tracks().len(), 2);
        });
    }

    #[test]
    fn space_marks_several_entries_and_a_adds_them_all_in_one_batch() {
        with_temp_dirs("add-multi", |dir| {
            let music = dir.join("music");
            std::fs::create_dir_all(&music).unwrap();
            odj_core::wav::write_wav(&music.join("a.wav"), &[0.1, -0.1], 2, 44_100).unwrap();
            odj_core::wav::write_wav(&music.join("b.wav"), &[0.2, -0.2, 0.3], 2, 44_100).unwrap();
            odj_core::wav::write_wav(&music.join("c.wav"), &[0.3, -0.3, 0.1, -0.1], 2, 44_100).unwrap();
            let mut settings = odj_core::memory::load_settings();
            settings.music_dirs = vec![music.clone()];
            odj_core::memory::save_settings(&settings).unwrap();

            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('a')); // open the add-existing browser

            press(&mut app, KeyCode::Char(' ')); // mark a.wav
            let Screen::AddingExisting(a) = &app.screen else { unreachable!() };
            assert_eq!(a.selected().len(), 1);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Char(' ')); // mark b.wav too — c.wav stays unmarked
            let Screen::AddingExisting(a) = &app.screen else { unreachable!() };
            assert_eq!(a.selected().len(), 2);

            press(&mut app, KeyCode::Char('a')); // commit both marked, not the highlighted one
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::AddingExisting(a) => !a.adding(),
                _ => false,
            });
            let Screen::AddingExisting(a) = &app.screen else { unreachable!() };
            assert!(a.selected().is_empty(), "the batch clears the marks once it's started");

            press(&mut app, KeyCode::Esc);
            let Screen::Crate(c) = &app.screen else { panic!("Esc with an empty query goes back") };
            assert_eq!(c.tracks().len(), 2, "only the two marked tracks, not c.wav");
        });
    }

    #[test]
    fn deleting_a_track_needs_two_presses_and_leaves_the_file_alone() {
        with_temp_dirs("delete-track", |dir| {
            let music = dir.join("music");
            std::fs::create_dir_all(&music).unwrap();
            let file = music.join("track.wav");
            odj_core::wav::write_wav(&file, &[0.1, -0.1], 2, 44_100).unwrap();
            let mut settings = odj_core::memory::load_settings();
            settings.music_dirs = vec![music.clone()];
            odj_core::memory::save_settings(&settings).unwrap();

            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('a')); // open the add-existing browser
            press(&mut app, KeyCode::Char('a')); // add the highlighted track.wav
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::AddingExisting(a) => !a.adding(),
                _ => false,
            });
            press(&mut app, KeyCode::Esc); // back to the crate
            let Screen::Crate(c) = &app.screen else { panic!("just added one") };
            assert_eq!(c.tracks().len(), 1);

            press(&mut app, KeyCode::Char('x'));
            let Screen::Crate(c) = &app.screen else { unreachable!() };
            assert_eq!(c.tracks().len(), 1, "first press only arms it");
            press(&mut app, KeyCode::Char('x'));
            let Screen::Crate(c) = &app.screen else { unreachable!() };
            assert!(c.tracks().is_empty(), "second press removes it");

            assert!(file.is_file(), "the audio file itself was never touched");
        });
    }

    #[test]
    fn deleting_a_crate_needs_two_presses_and_removes_it_from_the_list() {
        with_temp_dirs("delete-crate", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            let crate_dir = create_crate(&mut app, "Doomed Set");
            press(&mut app, KeyCode::Esc); // back to the list

            press(&mut app, KeyCode::Char('x'));
            let Screen::Crates(list) = &app.screen else { unreachable!() };
            assert_eq!(list.rows.len(), 1, "first press only arms it");
            assert!(list.armed() == Some(crate_dir.as_path()));
            assert!(app.status().is_some_and(|s| s.contains("Doomed Set")), "{:?}", app.status());

            press(&mut app, KeyCode::Char('x'));
            let Screen::Crates(list) = &app.screen else { unreachable!() };
            assert!(list.rows.is_empty(), "second press deletes it");
            assert!(!crate_dir.exists(), "the whole directory is gone");
            assert!(crates::list_crates().is_empty());
        });
    }

    #[test]
    fn moving_the_selection_disarms_a_pending_crate_delete() {
        with_temp_dirs("delete-crate-disarm", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            create_crate(&mut app, "Set A");
            press(&mut app, KeyCode::Esc);
            create_crate(&mut app, "Set B");
            press(&mut app, KeyCode::Esc);

            press(&mut app, KeyCode::Char('x')); // arms whichever is selected
            let Screen::Crates(list) = &app.screen else { unreachable!() };
            assert!(list.armed().is_some());

            press(&mut app, KeyCode::Up); // moving disarms it, same as the track list
            let Screen::Crates(list) = &app.screen else { unreachable!() };
            assert!(list.armed().is_none());

            press(&mut app, KeyCode::Char('x'));
            let Screen::Crates(list) = &app.screen else { unreachable!() };
            assert_eq!(list.rows.len(), 2, "still just armed, not deleted — the disarm reset the count");
        });
    }

    #[test]
    fn deleting_an_active_crate_drops_it_from_settings_too() {
        with_temp_dirs("delete-crate-active", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            let crate_dir = create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Esc);

            let slug = crate_dir.file_name().unwrap().to_string_lossy().into_owned();
            let mut settings = memory::load_settings();
            settings.active_crates = vec![slug.clone()];
            memory::save_settings(&settings).unwrap();

            press(&mut app, KeyCode::Char('x'));
            press(&mut app, KeyCode::Char('x'));

            assert!(!memory::load_settings().active_crates.contains(&slug), "no dangling reference left behind");
        });
    }

    #[test]
    fn typing_builds_the_query_and_switches_focus_to_search() {
        with_temp_dirs("typing", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('d'));
            let Screen::Downloading(d) = &mut app.screen else { unreachable!() };
            d.focus = Focus::Queue;
            type_str(&mut app, "daft punk");
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert_eq!(d.query, "daft punk");
            assert_eq!(d.focus, Focus::Search);
        });
    }

    #[test]
    fn esc_cancels_an_in_flight_search_and_returns_to_search() {
        with_temp_dirs("cancel-search", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(vec![result("One More Time")])), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "daft punk");
            press(&mut app, KeyCode::Enter);
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert!(d.searching);
            press(&mut app, KeyCode::Esc);
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert!(!d.searching);
            assert_eq!(d.focus, Focus::Search);
            assert!(d.results.is_empty());
        });
    }

    #[test]
    fn esc_is_refused_while_a_download_is_in_flight() {
        with_temp_dirs("esc-during-download", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(vec![result("One More Time")])), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.searching,
                _ => false,
            });

            press(&mut app, KeyCode::Enter); // queue it — starts downloading synchronously
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert!(d.downloading());

            press(&mut app, KeyCode::Esc); // clears the leftover "x" query first
            press(&mut app, KeyCode::Esc); // now it's the download that's in the way
            assert!(matches!(app.screen, Screen::Downloading(_)), "Esc must not leave while a download is in flight");
            assert!(app.status().is_some_and(|s| s.contains("in progress")), "{:?}", app.status());

            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.downloading(),
                _ => false,
            });
            press(&mut app, KeyCode::Esc);
            assert!(matches!(app.screen, Screen::Crate(_)), "Esc leaves once nothing's downloading any more");
        });
    }

    #[test]
    fn removing_a_queued_item_before_its_turn_never_downloads_it() {
        with_temp_dirs("remove-queued", |dir| {
            let fetcher = Arc::new(FakeFetcher::new(vec![result("One More Time"), result("Around the World")]));
            let mut app = app(fetcher.clone(), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.searching,
                _ => false,
            });

            press(&mut app, KeyCode::Enter); // queue "One More Time" — starts downloading at once
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter); // queue "Around the World" — stays Queued behind it
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert_eq!(d.queue[1].status, Status::Queued);

            press(&mut app, KeyCode::Tab); // -> Queue
            press(&mut app, KeyCode::Down); // select "Around the World"
            press(&mut app, KeyCode::Char('x')); // remove it before its turn
            let Screen::Downloading(d) = &app.screen else { unreachable!() };
            assert_eq!(d.queue.len(), 1);

            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => matches!(d.queue[0].status, Status::Done(_)),
                _ => false,
            });
            std::thread::sleep(Duration::from_millis(50));
            app.tick();
            assert_eq!(
                *fetcher.downloaded.lock().unwrap(),
                vec!["https://example.com/One More Time".to_string()],
                "the removed item should never reach the fetcher"
            );
        });
    }

    #[test]
    fn ctrl_c_quits() {
        with_temp_dirs("quit", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.quit);
        });
    }

    #[test]
    fn failed_search_sets_a_status_message_without_crashing() {
        with_temp_dirs("failed-search", |dir| {
            let mut app = app(Arc::new(FakeFetcher::failing("no internet")), dir.join("downloads"));
            create_crate(&mut app, "Set");
            press(&mut app, KeyCode::Char('d'));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_for(&mut app, Duration::from_secs(2), |a| match &a.screen {
                Screen::Downloading(d) => !d.searching,
                _ => false,
            });
            assert_eq!(app.status(), Some("Search failed: no internet"));
        });
    }

    /// Writes an ANSI dump of `app`'s current screen under
    /// `target/screenshots/`, for a CI step to rasterize into the
    /// README/website images — same shared directory and convention as
    /// odj-player's/odj-sampler's own screenshot tests.
    fn screenshot_dir() -> PathBuf {
        let dir = std::env::var_os("ODJ_SCREENSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/screenshots")));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dump_screenshot(name: &str, app: &App, cols: u16, rows: u16) {
        use odj_core::tui::to_ansi;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut t = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        t.draw(|f| crate::ui::draw(f, app)).unwrap();
        std::fs::write(screenshot_dir().join(format!("{name}.ans")), to_ansi(t.backend().buffer())).unwrap();
    }

    #[test]
    #[ignore = "writes target/screenshots/*.ans; run with `cargo test -- --ignored screenshot`"]
    fn screenshot_odj_crates() {
        with_temp_dirs("screenshot", |dir| {
            let mut app = app(Arc::new(FakeFetcher::new(Vec::new())), dir.join("downloads"));
            dump_screenshot("crates-list", &app, 100, 24);

            create_crate(&mut app, "Techno Night");
            dump_screenshot("crate-detail", &app, 100, 24);

            press(&mut app, KeyCode::Char('d'));
            let Screen::Downloading(d) = &mut app.screen else { unreachable!() };
            d.query = "daft punk".into();
            d.results =
                vec![result("One More Time"), result("Around the World"), result("Harder, Better, Faster, Stronger")];
            d.queue = vec![
                QueueItem { seq: 1, title: "Digital Love".into(), url: "https://example.com/1".into(), status: Status::Done(PathBuf::from("/tmp/1.m4a")) },
                QueueItem {
                    seq: 2,
                    title: "Robot Rock".into(),
                    url: "https://example.com/2".into(),
                    status: Status::Downloading(42.0),
                },
                QueueItem {
                    seq: 3,
                    title: "Too Long".into(),
                    url: "https://example.com/3".into(),
                    status: Status::Failed("network error".into()),
                },
            ];
            d.focus = Focus::Results;
            d.selected_result = 1;
            dump_screenshot("crates-download", &app, 100, 24);
        });
    }
}
