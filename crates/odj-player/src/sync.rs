//! The sync thread: every few milliseconds it tells the other decks where
//! this one is and hands what they say to the deck.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use odj_core::link::{self, Link, Peers};

use crate::engine::Deck;

const PERIOD: Duration = Duration::from_millis(10);

/// Runs while alive; dropping it leaves the other decks cleanly.
pub struct SyncLink {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SyncLink {
    /// Joins the other decks.
    pub fn start(deck: Arc<Mutex<Deck>>) -> io::Result<Self> {
        let link = Link::open()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::Builder::new()
            .name("odj-sync".into())
            .spawn(move || run(&link, &deck, &stopping))?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for SyncLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(link: &Link, deck: &Mutex<Deck>, stop: &AtomicBool) {
    let mut peers = Peers::default();
    while !stop.load(Ordering::Relaxed) {
        while let Some(p) = link.recv() {
            if p.deck != link.deck {
                peers.add(p);
            }
        }
        let now = link::now();
        peers.expire(now);
        let me = deck.lock().unwrap_or_else(|e| e.into_inner()).update_link(link.deck, peers.list(), now);
        link.send(&me);
        thread::sleep(PERIOD);
    }
}
