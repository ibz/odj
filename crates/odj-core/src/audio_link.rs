//! How a deck/sampler and odj-mixer talk directly over a Unix stream
//! socket when PipeWire isn't available to do it via a virtual sink
//! (`audio::NullSink`) — see `audio::is_pipewire`.
//!
//! odj-mixer binds one [`Listener`] at a well-known path; a deck or
//! odj-sampler connects out to it with a [`SocketSink`], sends a small
//! one-time handshake (its name and sample rate), then streams raw
//! interleaved stereo f32 PCM continuously, in fixed-size chunks sized
//! the same as `audio::Output`'s own buffer — no length prefix needed in
//! steady state, since the chunk size never changes mid-stream.
//!
//! There's no cpal device/callback driving the producer side the way a
//! real or virtual output would — see [`SocketSink::create`]'s own doc
//! comment for why it needs two threads instead of a single
//! render-then-write loop.
//!
//! The consumer side ([`Listener`]) accepts and reads each connection's
//! handshake the instant it connects — push-based discovery, unlike
//! `odj-mixer::devices::Devices`'s poll-every-second cpal scan — but
//! doesn't read any PCM from it until odj-mixer actually routes that
//! name (see [`Listener::take`]); unrouting gives the connection back
//! (see [`Listener::give_back`]) rather than closing it, so re-routing
//! later doesn't need a reconnect — the socket equivalent of a virtual
//! sink persisting across route toggles.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::audio::{BUFFER_FRAMES, Errors, Source};
use crate::link;

const CHUNK_FRAMES: usize = BUFFER_FRAMES as usize;
const CHUNK_SAMPLES: usize = CHUNK_FRAMES * 2; // stereo
const CHUNK_BYTES: usize = CHUNK_SAMPLES * 4; // f32

const HS_MAGIC: &[u8; 4] = b"odjl";
const HS_NAME_LEN: usize = 32;
const HS_SIZE: usize = 4 + HS_NAME_LEN + 4;

/// The one-time connection header: who's connecting, and at what rate
/// (this transport doesn't resample — see `audio_link::spawn_reader`'s
/// caller in `odj-mixer`, which rejects a mismatch rather than silently
/// misbehaving).
struct Handshake {
    name: String,
    sample_rate: u32,
}

impl Handshake {
    fn encode(&self) -> Result<[u8; HS_SIZE]> {
        if self.name.len() > HS_NAME_LEN {
            bail!("{:?} is too long for the audio-link handshake ({} > {HS_NAME_LEN} bytes)", self.name, self.name.len());
        }
        let mut b = [0u8; HS_SIZE];
        b[..4].copy_from_slice(HS_MAGIC);
        b[4..4 + self.name.len()].copy_from_slice(self.name.as_bytes());
        b[4 + HS_NAME_LEN..].copy_from_slice(&self.sample_rate.to_le_bytes());
        Ok(b)
    }

    fn decode(b: &[u8; HS_SIZE]) -> Option<Self> {
        if &b[..4] != HS_MAGIC {
            return None;
        }
        let name_bytes = &b[4..4 + HS_NAME_LEN];
        let end = name_bytes.iter().position(|&c| c == 0).unwrap_or(HS_NAME_LEN);
        let name = std::str::from_utf8(&name_bytes[..end]).ok()?.to_string();
        let sample_rate = u32::from_le_bytes(b[4 + HS_NAME_LEN..].try_into().expect("4 bytes"));
        Some(Self { name, sample_rate })
    }
}

fn write_handshake(stream: &mut UnixStream, name: &str, sample_rate: u32) -> Result<()> {
    let bytes = Handshake { name: name.to_string(), sample_rate }.encode()?;
    stream.write_all(&bytes).context("sending the audio-link handshake")?;
    Ok(())
}

fn read_handshake(stream: &mut UnixStream) -> Result<Handshake> {
    let mut buf = [0u8; HS_SIZE];
    stream.read_exact(&mut buf).context("reading the audio-link handshake")?;
    Handshake::decode(&buf).ok_or_else(|| anyhow!("not a valid audio-link handshake"))
}

fn f32_to_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

fn le_bytes_to_f32(bytes: &[u8], out: &mut [f32]) {
    for (chunk, o) in bytes.as_chunks::<4>().0.iter().zip(out.iter_mut()) {
        *o = f32::from_le_bytes(*chunk);
    }
}

fn default_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("odj/audio"),
        None => std::env::temp_dir().join(format!("odj-{}/audio", unsafe { libc::getuid() })),
    }
}

/// Drives a `Source` with a plain paced thread (no cpal device/callback
/// involved at all) and streams the rendered PCM to odj-mixer's
/// [`Listener`] over a Unix socket — used instead of `audio::NullSink` +
/// `audio::Output::start` when `audio::is_pipewire` says no (checked on a
/// freshly-made `audio::host()`, since that's the one reliable way to
/// tell — see the callers' own doc comments). Dropping it stops both of
/// its threads and closes the connection.
///
/// Needs two threads, not one: a Unix `SOCK_STREAM` `write()` gives no
/// atomicity guarantee — if odj-mixer's receive buffer is behind, a
/// write can land *partially*, and once that happens the fixed-size
/// chunk framing is misaligned forever (no length prefix fixes this on
/// its own, since the prefix itself can be partially written too). So a
/// **render-ticker** thread paces itself on the monotonic clock and
/// calls `render()` directly, handing off each complete chunk to a
/// **writer** thread over a small bounded channel — on backpressure
/// (queue full), the render-ticker drops that whole tick's chunk rather
/// than ever handing over a partial one, and the writer never starts a
/// new chunk's `write_all` until the previous one fully lands. The wire
/// is then always chunk-aligned by construction.
pub struct SocketSink {
    stop: Arc<AtomicBool>,
    render_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    pub errors: Errors,
}

impl SocketSink {
    /// Connects to odj-mixer's well-known socket, handshakes as `name`
    /// (see `audio::deck_sink_name`/`audio::sampler_sink_name`) at
    /// `sample_rate`, and starts driving `source`.
    pub fn create<S: Source>(name: &str, sample_rate: u32, source: Arc<Mutex<S>>) -> Result<Self> {
        Self::create_at(&default_dir().join("mixer"), name, sample_rate, source)
    }

    /// `path`-parametrized for tests.
    fn create_at<S: Source>(path: &Path, name: &str, sample_rate: u32, source: Arc<Mutex<S>>) -> Result<Self> {
        let mut stream = UnixStream::connect(path).with_context(|| format!("connecting to odj-mixer at {}", path.display()))?;
        write_handshake(&mut stream, name, sample_rate)?;
        let stop = Arc::new(AtomicBool::new(false));
        let errors = Errors::default();
        // ~85ms of slack at 48kHz/512-frame chunks — same spirit as
        // odj-mixer's own `RING_STEREO_SAMPLES`.
        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(8);

        let writer_errors = errors.clone();
        let writer_thread = thread::Builder::new()
            .name("odj-link-writer".into())
            .spawn(move || run_writer(stream, &rx, &writer_errors))
            .context("spawning the socket-writer thread")?;

        let render_stop = stop.clone();
        let render_thread = thread::Builder::new()
            .name("odj-link-render".into())
            .spawn(move || run_render_ticker(sample_rate, &source, &tx, &render_stop))
            .context("spawning the render-ticker thread")?;

        Ok(Self { stop, render_thread: Some(render_thread), writer_thread: Some(writer_thread), errors })
    }
}

impl Drop for SocketSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Joining the render thread first: it dropping its `tx` end is
        // what unblocks the writer thread's own `rx.recv()`.
        if let Some(t) = self.render_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.writer_thread.take() {
            let _ = t.join();
        }
    }
}

fn run_render_ticker<S: Source>(rate: u32, source: &Mutex<S>, tx: &mpsc::SyncSender<Vec<f32>>, stop: &AtomicBool) {
    let tick = CHUNK_FRAMES as f64 / rate as f64;
    let mut next = link::now();
    while !stop.load(Ordering::Relaxed) {
        let mut buf = vec![0f32; CHUNK_SAMPLES];
        source.lock().unwrap_or_else(|e| e.into_inner()).render(&mut buf, next);
        let _ = tx.try_send(buf); // queue full: drop this whole tick, never a partial one
        next += tick;
        let now = link::now();
        if next > now {
            thread::sleep(Duration::from_secs_f64(next - now));
        } else {
            next = now; // fell behind: resync instead of bursting to catch up
        }
    }
}

fn run_writer(mut stream: UnixStream, rx: &mpsc::Receiver<Vec<f32>>, errors: &Errors) {
    while let Ok(chunk) = rx.recv() {
        let bytes = f32_to_le_bytes(&chunk);
        if let Err(e) = stream.write_all(&bytes) {
            errors.report(format!("odj-mixer connection lost: {e}"));
            break;
        }
    }
}

/// A connection accepted and handshaked, not yet routed to anything.
struct Pending {
    stream: UnixStream,
    sample_rate: u32,
}

/// odj-mixer's single accept point for this transport — the push-based
/// counterpart to `odj-mixer::devices::Devices`'s poll-every-second cpal
/// scan. Started once, at startup, and kept alive for the whole session.
pub struct Listener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    /// Every name currently connected — pending *or* actively routed
    /// alike. [`names`](Self::names) lists this, not `pending`'s own
    /// keys: a deck/sampler must stay visible in odj-mixer's device scan
    /// for as long as its connection is alive, the same way a PipeWire
    /// virtual sink stays visible in cpal's own enumeration regardless
    /// of whether something's currently capturing it — otherwise
    /// odj-mixer's existing "forget a routed input the latest scan no
    /// longer reports" cleanup (see `odj-mixer::ui::run`) would tear
    /// down the route the instant it succeeded, since `take`ing it would
    /// make it vanish from a `pending`-only listing. Only [`forget`](Self::forget)
    /// removes an entry; [`take`](Self::take)/[`give_back`](Self::give_back)
    /// never touch this.
    connected: Arc<Mutex<HashMap<String, u32>>>,
}

impl Listener {
    pub fn start() -> Result<Self> {
        Self::start_in(&default_dir())
    }

    /// `dir`-parametrized for tests.
    fn start_in(dir: &Path) -> Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let path = dir.join("mixer");
        // Only one odj-mixer is ever expected (same cardinality as
        // `control::Control::open("mixer")`) — a stale file left behind
        // by a crash is detected the same way `link.rs`/`control.rs`
        // detect theirs, just via a connect attempt (this is a
        // `SOCK_STREAM`) instead of a zero-byte datagram send.
        if path.exists() && UnixStream::connect(&path).is_err() {
            let _ = fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        listener.set_nonblocking(true)?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let for_thread = pending.clone();
        let connected_for_thread = connected.clone();
        let thread = thread::Builder::new()
            .name("odj-link-accept".into())
            .spawn(move || accept_loop(listener, &for_thread, &connected_for_thread, &stopping))
            .context("spawning the accept thread")?;
        Ok(Self { stop, thread: Some(thread), pending, connected })
    }

    /// `(name, sample_rate)` for every connection currently alive,
    /// pending or actively routed alike — merged into
    /// `odj-mixer::devices::Devices`'s own `Kind::Deck` list.
    pub fn names(&self) -> Vec<(String, u32)> {
        self.connected.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(n, &r)| (n.clone(), r)).collect()
    }

    /// Takes `name`'s connection for routing — `None` if nothing by that
    /// name is currently pending.
    pub fn take(&self, name: &str) -> Option<(UnixStream, u32)> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(name).map(|p| (p.stream, p.sample_rate))
    }

    /// Returns a connection to the pending pool once a route is cleared
    /// — lets re-routing later skip reconnecting. A no-op (drops
    /// `stream`) if a fresher connection under the same name showed up
    /// meanwhile.
    pub fn give_back(&self, name: &str, stream: UnixStream, sample_rate: u32) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).entry(name.to_string()).or_insert(Pending { stream, sample_rate });
    }

    /// Drops `name` outright — called instead of [`give_back`](Self::give_back)
    /// when the connection died (EOF/error) rather than being cleanly unrouted.
    pub fn forget(&self, name: &str) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        self.connected.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(
    listener: UnixListener,
    pending: &Mutex<HashMap<String, Pending>>,
    connected: &Mutex<HashMap<String, u32>>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // A slow or bad client can't hang this loop — give it a
                // couple of seconds to send its handshake, no more.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                if let Ok(hs) = read_handshake(&mut stream) {
                    let _ = stream.set_read_timeout(None);
                    connected.lock().unwrap_or_else(|e| e.into_inner()).insert(hs.name.clone(), hs.sample_rate);
                    pending.lock().unwrap_or_else(|e| e.into_inner()).insert(hs.name, Pending { stream, sample_rate: hs.sample_rate });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Spawns the thread that fills `prod` from `stream` — the socket
/// equivalent of `audio::build_input`'s cpal callback, called from
/// odj-mixer's own routing exactly where it would otherwise build a
/// cpal `Input` stream. Decides on its own exit whether to
/// [`Listener::give_back`] (clean stop) or [`Listener::forget`] (the
/// connection died) — so the caller just stops and joins the returned
/// handle like any other background thread here, with no stream handle
/// to deal with afterward.
pub fn spawn_reader(
    mut stream: UnixStream,
    sample_rate: u32,
    mut prod: ringbuf::HeapProd<f32>,
    name: String,
    listener: Arc<Listener>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    use ringbuf::traits::Producer;

    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = thread::Builder::new()
        .name("odj-link-reader".into())
        .spawn(move || {
            // Lets `stop` be polled between chunks rather than blocking forever.
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = vec![0u8; CHUNK_BYTES];
            let mut floats = vec![0f32; CHUNK_SAMPLES];
            let mut fatal = false;
            'outer: while !stopping.load(Ordering::Relaxed) {
                let mut got = 0;
                while got < buf.len() {
                    match stream.read(&mut buf[got..]) {
                        Ok(0) => {
                            fatal = true;
                            break 'outer;
                        }
                        Ok(n) => got += n,
                        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                            if stopping.load(Ordering::Relaxed) {
                                break 'outer;
                            }
                            continue; // keep assembling this same chunk
                        }
                        Err(_) => {
                            fatal = true;
                            break 'outer;
                        }
                    }
                }
                le_bytes_to_f32(&buf, &mut floats);
                let _ = prod.push_slice(&floats); // overrun: drop the rest, same as cpal's own Input
            }
            if fatal {
                listener.forget(&name);
            } else {
                listener.give_back(&name, stream, sample_rate);
            }
        })
        .expect("spawns the socket-reader thread");
    (stop, thread)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("odj-audio-link-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Polls `f` until it returns `Some`, or panics after ~2s — the
    /// accept loop/reader threads here only ever settle asynchronously.
    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        for _ in 0..200 {
            if let Some(v) = f() {
                return v;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting");
    }

    struct Counter(f32);
    impl Source for Counter {
        fn render(&mut self, out: &mut [f32], _heard_at: f64) {
            out.fill(self.0);
            self.0 += 1.0;
        }
    }

    #[test]
    fn handshake_round_trips_through_encode_decode() {
        let hs = Handshake { name: "odj-deck-1".to_string(), sample_rate: 48_000 };
        let decoded = Handshake::decode(&hs.encode().unwrap()).unwrap();
        assert_eq!(decoded.name, "odj-deck-1");
        assert_eq!(decoded.sample_rate, 48_000);
    }

    #[test]
    fn handshake_rejects_a_name_thats_too_long() {
        let hs = Handshake { name: "a".repeat(HS_NAME_LEN + 1), sample_rate: 48_000 };
        assert!(hs.encode().is_err());
    }

    #[test]
    fn handshake_decode_rejects_garbage() {
        assert!(Handshake::decode(&[0u8; HS_SIZE]).is_none());
    }

    #[test]
    fn listener_accepts_a_connection_and_lists_its_handshake_name() {
        let dir = temp_dir("accept");
        let listener = Listener::start_in(&dir).unwrap();
        let mut client = UnixStream::connect(dir.join("mixer")).unwrap();
        write_handshake(&mut client, "odj-deck-1", 48_000).unwrap();
        let names = wait_for(|| {
            let names = listener.names();
            (!names.is_empty()).then_some(names)
        });
        assert_eq!(names, [("odj-deck-1".to_string(), 48_000)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_then_give_back_round_trips_the_same_connection() {
        let dir = temp_dir("take");
        let listener = Listener::start_in(&dir).unwrap();
        let mut client = UnixStream::connect(dir.join("mixer")).unwrap();
        write_handshake(&mut client, "odj-sampler", 44_100).unwrap();
        wait_for(|| (!listener.names().is_empty()).then_some(()));

        let (stream, rate) = listener.take("odj-sampler").unwrap();
        assert_eq!(rate, 44_100);
        // Still listed: `names()` reflects "connected", not "pending" —
        // a routed connection must stay visible in odj-mixer's device
        // scan (see `Listener`'s own doc comment on `connected`), unlike
        // the no-longer-available-to-take-again `pending` entry.
        assert_eq!(listener.names(), [("odj-sampler".to_string(), 44_100)]);
        assert!(listener.take("odj-sampler").is_none(), "no longer pending, so a second take finds nothing");

        listener.give_back("odj-sampler", stream, rate);
        assert_eq!(listener.names(), [("odj-sampler".to_string(), 44_100)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn give_back_is_a_noop_when_a_fresher_connection_already_replaced_it() {
        let dir = temp_dir("fresher");
        let listener = Listener::start_in(&dir).unwrap();
        let mut first = UnixStream::connect(dir.join("mixer")).unwrap();
        write_handshake(&mut first, "odj-deck-1", 48_000).unwrap();
        wait_for(|| (!listener.names().is_empty()).then_some(()));
        let (stale_stream, stale_rate) = listener.take("odj-deck-1").unwrap();

        let mut second = UnixStream::connect(dir.join("mixer")).unwrap();
        write_handshake(&mut second, "odj-deck-1", 44_100).unwrap();
        wait_for(|| listener.names().iter().any(|(n, r)| n == "odj-deck-1" && *r == 44_100).then_some(()));

        listener.give_back("odj-deck-1", stale_stream, stale_rate);
        assert_eq!(listener.names(), [("odj-deck-1".to_string(), 44_100)], "the fresher connection wasn't clobbered");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_removes_a_connection_from_both_pending_and_the_connected_list() {
        let dir = temp_dir("forget");
        let listener = Listener::start_in(&dir).unwrap();
        let mut client = UnixStream::connect(dir.join("mixer")).unwrap();
        write_handshake(&mut client, "odj-deck-1", 48_000).unwrap();
        wait_for(|| (!listener.names().is_empty()).then_some(()));

        listener.forget("odj-deck-1");
        assert!(listener.names().is_empty(), "no longer listed at all");
        assert!(listener.take("odj-deck-1").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_chunk_survives_producer_to_consumer_round_trip() {
        let dir = temp_dir("round-trip");
        let listener = Listener::start_in(&dir).unwrap();
        let source = Arc::new(Mutex::new(Counter(1.0)));
        let sink = SocketSink::create_at(&dir.join("mixer"), "odj-deck-1", 48_000, source).unwrap();

        let (stream, rate) = wait_for(|| listener.take("odj-deck-1"));
        let mut buf = vec![0u8; CHUNK_BYTES];
        let mut s = stream;
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        s.read_exact(&mut buf).unwrap();
        let mut floats = vec![0f32; CHUNK_SAMPLES];
        le_bytes_to_f32(&buf, &mut floats);
        assert_eq!(rate, 48_000);
        assert!(floats.iter().all(|&v| v == floats[0]), "a whole, uncorrupted chunk: {:?}", &floats[..4]);

        drop(sink);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backpressure_drops_whole_ticks_never_partial_ones() {
        // A tiny channel and a slow reader force the render-ticker to
        // drop ticks — if dropping ever corrupted the framing instead of
        // cleanly skipping whole chunks, a chunk landing here would mix
        // samples from two different ticks instead of being internally
        // uniform (`Counter` fills a whole chunk with one value).
        let dir = temp_dir("backpressure");
        let listener = Listener::start_in(&dir).unwrap();
        let source = Arc::new(Mutex::new(Counter(1.0)));
        let sink = SocketSink::create_at(&dir.join("mixer"), "odj-deck-1", 48_000, source).unwrap();
        thread::sleep(Duration::from_millis(100)); // let several ticks render unread

        let (mut stream, _) = wait_for(|| listener.take("odj-deck-1"));
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = vec![0u8; CHUNK_BYTES];
        for _ in 0..3 {
            stream.read_exact(&mut buf).unwrap();
            let mut floats = vec![0f32; CHUNK_SAMPLES];
            le_bytes_to_f32(&buf, &mut floats);
            assert!(floats.iter().all(|&v| v == floats[0]), "framing stayed aligned: {:?}", &floats[..4]);
        }

        drop(sink);
        let _ = fs::remove_dir_all(&dir);
    }
}
