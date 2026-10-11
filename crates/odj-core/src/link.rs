//! How odj decks running as separate processes find each other and share
//! their tempo and beat, for sync.
//!
//! Each deck binds a Unix datagram socket, `deck-<n>`, in a shared folder and
//! every few milliseconds sends a small [`Packet`] to every other socket there.
//! All decks run on one machine, so they share the monotonic clock ([`now`]):
//! a packet says which beat is heard at which moment, and any deck can work
//! out where the others are at any other moment.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

/// Deck numbers go from 1 to this.
pub const MAX_DECKS: u8 = 4;
/// A deck not heard from for this long, in seconds, is gone.
const TIMEOUT: f64 = 0.5;
const MAGIC: &[u8; 4] = b"odj1";
const SIZE: usize = 4 + 2 + 5 * 8;

/// Seconds on the system's monotonic clock, the same in every process.
pub fn now() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

/// What a deck tells the others.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Packet {
    pub deck: u8,
    pub playing: bool,
    pub synced: bool,
    pub master: bool,
    /// The grid follows the music closely enough to lock beats to.
    pub steady: bool,
    /// Playing forward at speed, so the beat clock below can be locked to.
    pub running: bool,
    /// When this deck became master, on the [`now`] clock.
    pub master_since: f64,
    /// Tempo as heard: the track's BPM times the tempo fader; None without a grid.
    pub bpm: Option<f64>,
    /// The beat heard at `time`, counting from the grid's first downbeat.
    pub beat: f64,
    /// Beats per second actually heard at `time`, jog included.
    pub rate: f64,
    pub time: f64,
}

impl Packet {
    /// The beat this deck is heard at, at time `t`.
    pub fn beat_at(&self, t: f64) -> f64 {
        self.beat + (t - self.time) * self.rate
    }

    pub fn encode(&self) -> [u8; SIZE] {
        let mut b = [0; SIZE];
        b[..4].copy_from_slice(MAGIC);
        b[4] = self.deck;
        b[5] = [self.playing, self.synced, self.master, self.steady, self.running]
            .iter()
            .enumerate()
            .fold(0, |f, (i, &on)| f | (on as u8) << i);
        let floats = [self.master_since, self.bpm.unwrap_or(f64::NAN), self.beat, self.rate, self.time];
        for (i, x) in floats.iter().enumerate() {
            b[6 + i * 8..14 + i * 8].copy_from_slice(&x.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != SIZE || &b[..4] != MAGIC {
            return None;
        }
        let f = |i: usize| f64::from_le_bytes(b[6 + i * 8..14 + i * 8].try_into().expect("8 bytes"));
        let flag = |i: u8| b[5] & (1 << i) != 0;
        let bpm = f(1);
        Some(Self {
            deck: b[4],
            playing: flag(0),
            synced: flag(1),
            master: flag(2),
            steady: flag(3),
            running: flag(4),
            master_since: f(0),
            bpm: (bpm.is_finite() && bpm > 0.0).then_some(bpm),
            beat: f(2),
            rate: f(3),
            time: f(4),
        })
    }

    /// Whether this deck's claim to be master beats `other`'s: the newer
    /// claim wins, and at the same moment the lower deck number.
    fn outranks(&self, other: &Packet) -> bool {
        self.master_since > other.master_since
            || (self.master_since == other.master_since && self.deck < other.deck)
    }
}

/// This deck's socket among the others'.
pub struct Link {
    socket: UnixDatagram,
    dir: PathBuf,
    path: PathBuf,
    pub deck: u8,
}

impl Link {
    /// Joins the decks of this user, in `$XDG_RUNTIME_DIR/odj/decks`.
    pub fn open() -> io::Result<Self> {
        Self::open_in(&default_dir())
    }

    /// Joins the decks in `dir`, taking the first free deck number.
    pub fn open_in(dir: &Path) -> io::Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        for deck in 1..=MAX_DECKS {
            let path = dir.join(format!("deck-{deck}"));
            let socket = match UnixDatagram::bind(&path) {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::AddrInUse && is_stale(&path) => {
                    // Left behind by a deck that crashed.
                    let _ = fs::remove_file(&path);
                    match UnixDatagram::bind(&path) {
                        Ok(s) => s,
                        Err(_) => continue,
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
                Err(e) => return Err(e),
            };
            socket.set_nonblocking(true)?;
            return Ok(Self { socket, dir: dir.to_path_buf(), path, deck });
        }
        Err(io::Error::other(format!("{MAX_DECKS} decks are running already")))
    }

    /// Sends `p` to every other deck, clearing away sockets nobody listens on.
    pub fn send(&self, p: &Packet) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        let bytes = p.encode();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_deck = entry.file_name().to_str().is_some_and(|n| n.starts_with("deck-"));
            if !is_deck || path == self.path {
                continue;
            }
            if let Err(e) = self.socket.send_to(&bytes, &path)
                && e.kind() == io::ErrorKind::ConnectionRefused
            {
                let _ = fs::remove_file(&path);
            }
        }
    }

    /// The next packet waiting, if any.
    pub fn recv(&self) -> Option<Packet> {
        let mut buf = [0; 64];
        loop {
            let n = self.socket.recv(&mut buf).ok()?;
            if let Some(p) = Packet::decode(&buf[..n]) {
                return Some(p);
            }
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn default_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("odj/decks"),
        None => std::env::temp_dir().join(format!("odj-{}/decks", unsafe { libc::getuid() })),
    }
}

/// A socket file nobody is bound to any more.
fn is_stale(path: &Path) -> bool {
    UnixDatagram::unbound()
        .and_then(|s| s.send_to(&[], path))
        .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionRefused)
}

/// The other decks, as last heard from.
#[derive(Default)]
pub struct Peers(Vec<Packet>);

impl Peers {
    pub fn add(&mut self, p: Packet) {
        self.0.retain(|q| q.deck != p.deck);
        self.0.push(p);
        self.0.sort_by_key(|q| q.deck);
    }

    /// Forgets the decks gone quiet.
    pub fn expire(&mut self, now: f64) {
        self.0.retain(|p| now - p.time < TIMEOUT);
    }

    pub fn list(&self) -> &[Packet] {
        &self.0
    }
}

/// Whether `me` is master after this round, given the other decks:
/// - the first synced deck to play takes it when nobody has it;
/// - a stopped master hands it to a synced deck that is playing;
/// - between two masters the newer claim wins;
/// - a deck without a tempo can't be master.
pub fn elect(me: &Packet, peers: &[Packet]) -> bool {
    if me.bpm.is_none() {
        return false;
    }
    if !me.master {
        return me.synced && me.playing && !peers.iter().any(|p| p.master);
    }
    let outranked = peers.iter().any(|p| p.master && p.outranks(me));
    let hand_over = !me.playing && peers.iter().any(|p| p.synced && p.playing && p.bpm.is_some());
    !outranked && !hand_over
}

/// The master among the other decks.
pub fn leader(peers: &[Packet]) -> Option<&Packet> {
    peers
        .iter()
        .filter(|p| p.master && p.bpm.is_some())
        .reduce(|a, b| if b.outranks(a) { b } else { a })
}

/// How to match `bpm` to `target`: the speed factor and the multiple of the
/// target's tempo it gets, 1, 2 or ½, whichever needs the smaller change.
pub fn match_tempo(bpm: f64, target: f64) -> (f64, f64) {
    [1.0, 2.0, 0.5]
        .into_iter()
        .map(|k| (target * k / bpm, k))
        .min_by(|a, b| a.0.ln().abs().total_cmp(&b.0.ln().abs()))
        .expect("three candidates")
}

/// A phase difference in beats, wrapped to -½..½.
pub fn wrap(beats: f64) -> f64 {
    beats - beats.round()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("odj-link-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn packet(deck: u8) -> Packet {
        Packet {
            deck,
            playing: false,
            synced: false,
            master: false,
            steady: true,
            running: false,
            master_since: 0.0,
            bpm: Some(128.0),
            beat: 0.0,
            rate: 128.0 / 60.0,
            time: 0.0,
        }
    }

    #[test]
    fn packets_survive_the_round_trip() {
        let p = Packet { playing: true, master: true, running: true, beat: -3.25, time: 1234.5678, ..packet(2) };
        assert_eq!(Packet::decode(&p.encode()), Some(p));
        let none = Packet { bpm: None, ..p };
        assert_eq!(Packet::decode(&none.encode()), Some(none));
        assert_eq!(Packet::decode(&[]), None);
        assert_eq!(Packet::decode(&[0; SIZE]), None);
    }

    #[test]
    fn decks_find_each_other_with_their_own_numbers() {
        let dir = temp_dir("find");
        let a = Link::open_in(&dir).unwrap();
        let b = Link::open_in(&dir).unwrap();
        assert_eq!((a.deck, b.deck), (1, 2));
        a.send(&packet(a.deck));
        b.send(&packet(b.deck));
        assert_eq!(b.recv().map(|p| p.deck), Some(1));
        assert_eq!(a.recv().map(|p| p.deck), Some(2));
        assert_eq!(a.recv(), None, "nothing from itself");

        // A deck that quits frees its number.
        drop(a);
        let c = Link::open_in(&dir).unwrap();
        assert_eq!(c.deck, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crashed_decks_socket_is_cleared() {
        let dir = temp_dir("stale");
        fs::create_dir_all(&dir).unwrap();
        // A crash leaves the socket file behind, unbound.
        drop(UnixDatagram::bind(dir.join("deck-1")).unwrap());
        drop(UnixDatagram::bind(dir.join("deck-3")).unwrap());
        let a = Link::open_in(&dir).unwrap();
        assert_eq!(a.deck, 1, "its number is free");
        assert!(dir.join("deck-3").exists());
        a.send(&packet(1));
        assert!(!dir.join("deck-3").exists(), "a send clears it");
        drop(a);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decks_run_out() {
        let dir = temp_dir("full");
        let decks: Vec<Link> = (0..MAX_DECKS).map(|_| Link::open_in(&dir).unwrap()).collect();
        assert!(Link::open_in(&dir).is_err());
        drop(decks);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peers_expire() {
        let mut peers = Peers::default();
        peers.add(Packet { time: 10.0, ..packet(2) });
        peers.add(Packet { time: 10.2, ..packet(1) });
        peers.add(Packet { time: 10.3, ..packet(2) });
        assert_eq!(peers.list().iter().map(|p| p.deck).collect::<Vec<_>>(), [1, 2]);
        peers.expire(10.6);
        assert_eq!(peers.list().len(), 2);
        peers.expire(10.75);
        assert_eq!(peers.list().iter().map(|p| p.deck).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn the_first_synced_deck_to_play_becomes_master() {
        let me = Packet { synced: true, playing: true, ..packet(1) };
        assert!(elect(&me, &[packet(2)]));
        assert!(!elect(&Packet { synced: false, ..me }, &[]), "only synced decks");
        assert!(!elect(&Packet { playing: false, ..me }, &[]), "only playing ones");
        assert!(!elect(&Packet { bpm: None, ..me }, &[]), "only with a tempo");
        let master = Packet { master: true, ..packet(2) };
        assert!(!elect(&me, &[master]), "not while there is one");
    }

    #[test]
    fn a_stopped_master_hands_over() {
        let me = Packet { master: true, synced: true, ..packet(1) };
        let other = Packet { synced: true, playing: true, ..packet(2) };
        assert!(elect(&me, &[]), "stays master, stopped, with nobody to take over");
        assert!(elect(&me, &[Packet { synced: false, ..other }]), "only to a synced deck");
        assert!(!elect(&me, &[other]));
        assert!(elect(&Packet { playing: true, ..me }, &[other]), "not while playing");
        // Then the other takes it, being the only synced deck playing.
        assert!(elect(&other, &[Packet { master: false, ..me }]));
    }

    #[test]
    fn the_newer_claim_wins() {
        let me = Packet { master: true, master_since: 5.0, playing: true, ..packet(2) };
        let older = Packet { master: true, master_since: 4.0, ..packet(1) };
        let newer = Packet { master: true, master_since: 6.0, ..packet(3) };
        assert!(elect(&me, &[older]));
        assert!(!elect(&me, &[newer]));
        assert_eq!(leader(&[older, newer]).map(|p| p.deck), Some(3));
        // At the same moment the lower number wins.
        let same = Packet { master_since: 5.0, ..older };
        assert!(!elect(&me, &[same]));
        assert!(elect(&Packet { deck: 1, ..me }, &[Packet { deck: 2, ..same }]));
        assert_eq!(leader(&[packet(1)]), None);
    }

    #[test]
    fn tempo_matching_takes_the_nearest_multiple() {
        assert_eq!(match_tempo(124.0, 124.0), (1.0, 1.0));
        let (f, k) = match_tempo(70.0, 140.0);
        assert_eq!((f, k), (1.0, 0.5), "a 70 BPM track plays at half the master's tempo");
        let (f, k) = match_tempo(170.0, 87.0);
        assert_eq!(k, 2.0);
        assert!((f - 174.0 / 170.0).abs() < 1e-12);
        let (f, k) = match_tempo(120.0, 126.0);
        assert_eq!((f, k), (1.05, 1.0));
    }

    #[test]
    fn phase_wraps_to_half_a_beat() {
        assert!((wrap(3.25) - 0.25).abs() < 1e-12);
        assert!((wrap(-0.75) - 0.25).abs() < 1e-12);
        assert!((wrap(2.9) + 0.1).abs() < 1e-12);
        let p = Packet { beat: 10.0, rate: 2.0, time: 100.0, ..packet(1) };
        assert_eq!(p.beat_at(100.5), 11.0);
        assert_eq!(p.beat_at(99.0), 8.0);
    }
}
