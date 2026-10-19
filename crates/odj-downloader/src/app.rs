//! App state and key handling.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::queue::{DownloadWorker, QueueItem, Status};
use crate::search::{Fetcher, SearchResult, search_in_background};

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

pub struct App {
    fetcher: Arc<dyn Fetcher>,
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
    pub status: Option<String>,
    pub quit: bool,
    /// Advances every tick; drives the searching animation.
    pub spinner: u64,
}

impl App {
    pub fn new(fetcher: Arc<dyn Fetcher>, dest: PathBuf) -> Self {
        let worker = DownloadWorker::start(fetcher.clone(), dest);
        Self {
            fetcher,
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
            status: None,
            quit: false,
            spinner: 0,
        }
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        if let Some(rx) = &self.search_rx {
            match rx.try_recv() {
                Ok(Ok(results)) => {
                    self.status = Some(format!("{} result{}", results.len(), if results.len() == 1 { "" } else { "s" }));
                    self.results = results;
                    self.selected_result = 0;
                    self.searching = false;
                    self.search_rx = None;
                    // Jump straight to the results instead of leaving focus
                    // on the search box the user just finished typing in.
                    self.focus = Focus::Results;
                }
                Ok(Err(e)) => {
                    self.status = Some(format!("Search failed: {e}"));
                    self.searching = false;
                    self.search_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = Some("Search worker crashed".to_string());
                    self.searching = false;
                    self.search_rx = None;
                }
            }
        }
        while let Ok((seq, status)) = self.worker.status_rx.try_recv() {
            let finished = matches!(&status, Status::Done | Status::Failed(_));
            if let Some(item) = self.queue.iter_mut().find(|q| q.seq == seq) {
                item.status = status;
            }
            if finished && self.downloading == Some(seq) {
                self.downloading = None;
            }
        }
        self.dispatch_next();
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
        match key.code {
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::Esc if self.searching => self.cancel_search(),
            KeyCode::Esc => {
                self.query.clear();
                self.focus = Focus::Search;
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Enter => self.activate(),
            KeyCode::Delete if self.focus == Focus::Queue => self.remove_selected(),
            KeyCode::Char('x') if self.focus == Focus::Queue => self.remove_selected(),
            KeyCode::Char(c) => {
                self.focus = Focus::Search;
                self.query.push(c);
            }
            _ => {}
        }
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

    fn activate(&mut self) {
        match self.focus {
            Focus::Search => self.search(),
            Focus::Results => self.enqueue_selected(),
            Focus::Queue => {}
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
        self.status = Some("Search cancelled".to_string());
        self.focus = Focus::Search;
    }

    fn search(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() || self.searching {
            return;
        }
        self.searching = true;
        self.status = Some("Searching…".to_string());
        self.search_rx = Some(search_in_background(self.fetcher.clone(), query));
    }

    fn enqueue_selected(&mut self) {
        let Some(result) = self.results.get(self.selected_result) else { return };
        let seq = self.next_seq;
        self.next_seq += 1;
        self.status = Some(format!("Queued {}", result.title));
        self.queue.push(QueueItem { seq, title: result.title.clone(), url: result.url.clone(), status: Status::Queued });
        self.selected_queue = self.queue.len() - 1;
        self.dispatch_next();
    }

    /// Starts the earliest still-`Queued` item downloading, if nothing
    /// already is. Called after enqueuing and after each download
    /// finishes, so the queue drains on its own — "one at a time" is
    /// enforced here rather than by buffering every item in the worker's
    /// channel up front, which would let a removed-but-not-yet-started
    /// item still silently download in the background.
    fn dispatch_next(&mut self) {
        if self.downloading.is_some() {
            return;
        }
        let Some(item) = self.queue.iter().find(|q| q.status == Status::Queued) else { return };
        self.downloading = Some(item.seq);
        self.worker.enqueue(item.seq, item.url.clone());
    }

    fn remove_selected(&mut self) {
        let Some(item) = self.queue.get(self.selected_queue) else { return };
        if item.status != Status::Queued {
            self.status = Some("Can't remove — already downloading".to_string());
            return;
        }
        self.queue.remove(self.selected_queue);
        if self.selected_queue > 0 && self.selected_queue >= self.queue.len() {
            self.selected_queue = self.queue.len() - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_util::{FakeFetcher, result};
    use std::time::{Duration, Instant};

    fn app(results: Vec<SearchResult>) -> App {
        app_with_fetcher(results).0
    }

    fn app_with_fetcher(results: Vec<SearchResult>) -> (App, Arc<FakeFetcher>) {
        let fetcher = Arc::new(FakeFetcher::new(results));
        (App::new(fetcher.clone(), PathBuf::from("/tmp")), fetcher)
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
    fn typing_builds_the_query_and_switches_focus_to_search() {
        let mut app = app(Vec::new());
        app.focus = Focus::Queue;
        type_str(&mut app, "daft punk");
        assert_eq!(app.query, "daft punk");
        assert_eq!(app.focus, Focus::Search);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.query, "daft pun");
    }

    #[test]
    fn enter_in_search_runs_a_search_and_jumps_to_results_when_done() {
        let mut app = app(vec![result("One More Time"), result("Around the World")]);
        type_str(&mut app, "daft punk");
        press(&mut app, KeyCode::Enter);
        assert!(app.searching);
        wait_for(&mut app, Duration::from_secs(2), |a| !a.searching);
        assert_eq!(app.results.len(), 2);
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn esc_cancels_an_in_flight_search_and_returns_to_search() {
        let mut app = app(vec![result("One More Time")]);
        type_str(&mut app, "daft punk");
        press(&mut app, KeyCode::Enter);
        assert!(app.searching);
        press(&mut app, KeyCode::Esc);
        assert!(!app.searching);
        assert_eq!(app.focus, Focus::Search);
        assert!(app.results.is_empty(), "a cancelled search shouldn't later populate results");

        // The background thread may still be running and deliver its
        // result after cancelling; once disconnected, ticking can't
        // resurrect it.
        std::thread::sleep(Duration::from_millis(50));
        app.tick();
        assert!(app.results.is_empty());
        assert!(!app.searching);
    }

    #[test]
    fn enter_on_a_result_queues_it_and_tracks_its_status() {
        let mut app = app(vec![result("One More Time")]);
        type_str(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        wait_for(&mut app, Duration::from_secs(2), |a| !a.searching);

        assert_eq!(app.focus, Focus::Results); // search jumps here automatically
        press(&mut app, KeyCode::Enter); // queue it
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].title, "One More Time");

        wait_for(&mut app, Duration::from_secs(2), |a| a.queue[0].status == Status::Done);
    }

    #[test]
    fn queueing_several_downloads_them_one_after_another_without_further_input() {
        let (mut app, fetcher) = app_with_fetcher(vec![result("One More Time"), result("Around the World")]);
        type_str(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        wait_for(&mut app, Duration::from_secs(2), |a| !a.searching);

        press(&mut app, KeyCode::Enter); // queue "One More Time"
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // queue "Around the World"
        assert_eq!(app.queue.len(), 2);

        // Both finish with no further key presses: the second isn't
        // started by the user, it's dispatched once the first is done.
        wait_for(&mut app, Duration::from_secs(2), |a| {
            a.queue.iter().all(|q| q.status == Status::Done)
        });
        assert_eq!(
            *fetcher.downloaded.lock().unwrap(),
            vec!["https://example.com/One More Time".to_string(), "https://example.com/Around the World".to_string()]
        );
    }

    #[test]
    fn removing_a_queued_item_before_its_turn_never_downloads_it() {
        let (mut app, fetcher) = app_with_fetcher(vec![result("One More Time"), result("Around the World")]);
        type_str(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        wait_for(&mut app, Duration::from_secs(2), |a| !a.searching);

        press(&mut app, KeyCode::Enter); // queue "One More Time" — starts downloading at once
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // queue "Around the World" — stays Queued behind it
        assert_eq!(app.queue[1].status, Status::Queued);

        press(&mut app, KeyCode::Tab); // -> Queue
        press(&mut app, KeyCode::Down); // select "Around the World"
        press(&mut app, KeyCode::Char('x')); // remove it before its turn
        assert_eq!(app.queue.len(), 1);

        wait_for(&mut app, Duration::from_secs(2), |a| a.queue[0].status == Status::Done);
        std::thread::sleep(Duration::from_millis(50));
        app.tick();
        assert_eq!(
            *fetcher.downloaded.lock().unwrap(),
            vec!["https://example.com/One More Time".to_string()],
            "the removed item should never reach the fetcher"
        );
    }

    #[test]
    fn queued_item_can_be_removed_but_not_once_downloading() {
        let mut app = app(vec![result("One More Time")]);
        type_str(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        wait_for(&mut app, Duration::from_secs(2), |a| !a.searching);
        press(&mut app, KeyCode::Enter); // queue it (focus is already Results)

        press(&mut app, KeyCode::Tab); // -> Queue
        press(&mut app, KeyCode::Char('x'));
        assert!(app.queue.is_empty(), "a still-queued item should be removable");
    }

    #[test]
    fn esc_clears_the_query_and_returns_focus_to_search() {
        let mut app = app(Vec::new());
        type_str(&mut app, "hello");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.query, "");
        assert_eq!(app.focus, Focus::Search);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = app(Vec::new());
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn failed_search_sets_a_status_message_without_crashing() {
        let fetcher = Arc::new(FakeFetcher::failing("no internet"));
        let mut app = App::new(fetcher, PathBuf::from("/tmp"));
        type_str(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        wait_for(&mut app, Duration::from_secs(2), |a| !a.searching);
        assert_eq!(app.status, Some("Search failed: no internet".to_string()));
        assert!(app.results.is_empty());
    }
}
