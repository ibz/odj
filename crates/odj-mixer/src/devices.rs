//! Continuously detects which audio inputs/outputs are plugged in —
//! mirrors `discovery.rs`'s continuous player detection, but polls the
//! audio host's own device list instead of a control socket.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow};

use odj_core::audio::{self, Card, Choice, Route, strip_dedup_suffix};
use odj_core::audio_link::Listener;

const PERIOD: Duration = Duration::from_secs(1);

/// A card+route pair's stable identity across repeated scans — a plain
/// index into whichever `Vec<Card>` a scan happened to produce doesn't
/// survive the next one, but [`Card::id`] does — except for one known
/// case, PipeWire's own dedup suffix drifting (see
/// `strip_dedup_suffix`'s own doc comment and [`Devices::resolve`]'s,
/// which tolerates exactly that one case).
pub type DeviceKey = (String, Route);

/// `key` encoded as `"<card id>#<route tag>"` — a card id never
/// contains `#` (it's a PipeWire node name or an ALSA `hw:CARD=` id),
/// so splitting the route tag back off in [`parse_device_key`] is
/// unambiguous. Used wherever a [`DeviceKey`] needs to be a map key or
/// saved to disk by its stable identity rather than its display label
/// (which can change) — `odj-mixer::midi`'s own target keys build on
/// this directly.
pub fn device_key_string(key: &DeviceKey) -> String {
    format!("{}#{}", key.0, route_tag(key.1))
}

/// The inverse of [`device_key_string`].
pub fn parse_device_key(s: &str) -> Option<DeviceKey> {
    let (card_id, tag) = s.rsplit_once('#')?;
    Some((card_id.to_string(), parse_route_tag(tag)?))
}

pub fn route_tag(route: Route) -> String {
    match route {
        Route::Stereo(l, r) => format!("s{l}-{r}"),
        Route::Mono(c) => format!("m{c}"),
    }
}

pub fn parse_route_tag(s: &str) -> Option<Route> {
    if let Some(rest) = s.strip_prefix('s') {
        let (l, r) = rest.split_once('-')?;
        return Some(Route::Stereo(l.parse().ok()?, r.parse().ok()?));
    }
    Some(Route::Mono(s.strip_prefix('m')?.parse().ok()?))
}

/// A default main output, used when nothing's been explicitly picked
/// yet (see `main.rs::start`) rather than leaving nothing playing: the
/// first available output card [`Card::builtin`] marks as fixed,
/// onboard audio — the closest proxy there is for "the internal
/// speakers" — stereo. `None` if there's no such card right now (no
/// onboard audio at all, or the PipeWire scan just hasn't answered
/// yet); Settings' own Outputs tab can always override this, which
/// saves the choice from then on.
pub fn builtin_output() -> Option<DeviceKey> {
    let cards = audio::output_cards(&audio::host()).ok()?;
    let card = cards.iter().find(|c| c.unavailable.is_none() && c.builtin)?;
    Some((card.id().to_string(), Route::Stereo(0, 1)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Input,
    Output,
    /// An odj-player deck, or odj-sampler, routing through this
    /// odj-mixer instead of a real device — see
    /// `odj_core::audio::NullSink`/`deck_sink_name`/`sampler_sink_name`,
    /// and [`is_virtual_sink`] for how either is recognized. These are
    /// plain `Audio/Sink` nodes, so (like `Output`) they're found via
    /// `output_cards`, not `input_cards`; recognized by name and
    /// offered as routable inputs, same as a hardware one.
    Deck,
}

/// One currently-detected device: a stable identity, its display label,
/// and whether it's usable right now.
#[derive(Clone)]
pub struct Entry {
    pub key: DeviceKey,
    pub label: String,
    pub available: bool,
}

/// Runs while alive; dropping it stops scanning.
pub struct Devices {
    kind: Kind,
    entries: Arc<Mutex<Vec<Entry>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Devices {
    pub fn start(kind: Kind) -> Self {
        Self::start_with_link(kind, None)
    }

    /// Like [`start`](Self::start), but for `Kind::Deck` also merges in
    /// whatever `odj_core::audio_link::Listener` currently has connected
    /// — the push-discovered, PipeWire-free counterpart to this
    /// struct's own poll-every-second cpal scan (see `run`'s own doc
    /// comment). `listener` is ignored for `Kind::Input`/`Kind::Output`.
    pub fn start_with_link(kind: Kind, listener: Option<Arc<Listener>>) -> Self {
        let entries = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let for_thread = entries.clone();
        let thread = thread::Builder::new()
            .name("odj-devices".into())
            .spawn(move || run(kind, &for_thread, &stopping, listener.as_deref()))
            .expect("spawns the device-scanning thread");
        Self { kind, entries, stop, thread: Some(thread) }
    }

    /// Whichever devices the last scan (at most [`PERIOD`] old) found.
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-scans right now and resolves `key` off that fresh snapshot, for
    /// actually opening a stream on it — not just whatever the background
    /// thread last saw, so a device that only just appeared doesn't need
    /// to wait for the next periodic scan.
    ///
    /// Tries an exact id match first; if that fails, retries once with
    /// PipeWire's own dedup suffix ignored on both sides (see
    /// `audio::strip_dedup_suffix`'s own doc comment for why one can
    /// drift out from under an otherwise-unchanged device — confirmed:
    /// a saved `mixer_active_inputs`/`mixer_output` key can stop
    /// resolving at all after nothing more than a PipeWire restart).
    /// That fallback only succeeds if it identifies exactly one card —
    /// two different ones colliding once the suffix is ignored (e.g.
    /// two identical interfaces plugged in at once) is genuine
    /// disambiguation, not drift, so that's left as "no longer
    /// available" rather than guessing which one was meant.
    pub fn resolve(&self, key: &DeviceKey) -> Result<(Vec<Card>, Choice)> {
        let cards = scan_cards(self.kind)?;
        let choices = audio::choices(&cards);
        if let Some(choice) = choices.iter().find(|c| cards[c.card].id() == key.0 && c.route == key.1) {
            return Ok((cards, *choice));
        }
        let wanted = strip_dedup_suffix(&key.0);
        let fuzzy: Vec<Choice> = choices
            .iter()
            .filter(|c| strip_dedup_suffix(cards[c.card].id()) == wanted && c.route == key.1)
            .copied()
            .collect();
        if fuzzy.len() == 1 {
            return Ok((cards, fuzzy[0]));
        }
        Err(anyhow!("that device is no longer available"))
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn scan_cards(kind: Kind) -> Result<Vec<Card>> {
    let host = audio::host();
    match kind {
        Kind::Input => audio::input_cards(&host),
        Kind::Output | Kind::Deck => audio::output_cards(&host),
    }
}

/// Whether `id` is a deck's or odj-sampler's own virtual sink — the two
/// kinds of `Kind::Deck` entry, distinguished elsewhere only by their
/// label (which comes straight from the sink's own PipeWire
/// description, "Deck n" or "Sampler" — no extra logic needed here for
/// that part).
fn is_virtual_sink(id: &str) -> bool {
    audio::deck_number(id).is_some() || id == audio::sampler_sink_name()
}

fn run(kind: Kind, entries: &Mutex<Vec<Entry>>, stop: &AtomicBool, listener: Option<&Listener>) {
    let word = if kind == Kind::Input { "In" } else { "Out" };
    while !stop.load(Ordering::Relaxed) {
        if let Ok(cards) = scan_cards(kind) {
            let choices = audio::choices(&cards);
            let mut fresh: Vec<Entry> = choices
                .iter()
                // A deck's or odj-sampler's virtual sink is stereo-only;
                // its own mono sub-channels (which `choices` also
                // offers, same as any wider device) aren't a sensible
                // thing to route from.
                .filter(|c| kind != Kind::Deck || matches!(c.route, Route::Stereo(..)))
                .filter(|c| kind != Kind::Deck || is_virtual_sink(cards[c.card].id()))
                // The reverse, for the real-output list: neither kind of
                // virtual sink is a sensible target to route some other
                // input *into*, so both are excluded there instead.
                .filter(|c| kind != Kind::Output || !is_virtual_sink(cards[c.card].id()))
                .map(|c| Entry {
                    key: (cards[c.card].id().to_string(), c.route),
                    label: c.label(&cards, word),
                    available: c.available(&cards),
                })
                .collect();
            // A deck/odj-sampler connected directly over
            // `odj_core::audio_link` instead of a PipeWire virtual sink
            // (e.g. no PipeWire at all) — pushed into `listener`'s own
            // pending registry the instant it connects, so there's
            // nothing to "scan" here, just list what's already known.
            if kind == Kind::Deck
                && let Some(listener) = listener
            {
                for (name, _rate) in listener.names() {
                    if fresh.iter().any(|e| e.key.0 == name) {
                        continue; // already listed via the PipeWire scan
                    }
                    let label = match audio::deck_number(&name) {
                        Some(n) => format!("Deck {n} · stereo (no PipeWire)"),
                        None if name == audio::sampler_sink_name() => "Sampler · stereo (no PipeWire)".to_string(),
                        None => continue, // not a name this scan recognizes
                    };
                    fresh.push(Entry { key: (name, Route::Stereo(0, 1)), label, available: true });
                }
            }
            *entries.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        }
        thread::sleep(PERIOD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_with_link_merges_a_listeners_names_into_the_deck_scan() {
        let dir = std::env::temp_dir().join(format!("odj-devices-link-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
        }
        let listener = Arc::new(Listener::start().unwrap());
        let mut client = std::os::unix::net::UnixStream::connect(dir.join("odj/audio/mixer")).unwrap();
        // Raw handshake bytes: 4-byte magic "odjl", a 32-byte NUL-padded
        // name, then the sample rate as a u32 LE — see
        // `odj_core::audio_link`'s own module doc comment for the wire
        // format (its encoder is private to that crate).
        let mut hs = [0u8; 4 + 32 + 4];
        hs[..4].copy_from_slice(b"odjl");
        hs[4..4 + "odj-deck-2".len()].copy_from_slice(b"odj-deck-2");
        hs[36..].copy_from_slice(&48_000u32.to_le_bytes());
        std::io::Write::write_all(&mut client, &hs).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let devices = Devices::start_with_link(Kind::Deck, Some(listener));
        let found = loop {
            if let Some(e) = devices.entries().iter().find(|e| e.key.0 == "odj-deck-2") {
                break Some(e.clone());
            }
            assert!(std::time::Instant::now() < deadline, "never showed up");
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(found.unwrap().label, "Deck 2 · stereo (no PipeWire)");

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_key_round_trips_through_its_string_encoding() {
        for key in [("hw:CARD=Built-in".to_string(), Route::Stereo(0, 1)), ("alsa_input.usb-Foo".to_string(), Route::Mono(2))] {
            assert_eq!(parse_device_key(&device_key_string(&key)), Some(key));
        }
    }

    #[test]
    fn parse_device_key_rejects_garbage() {
        assert!(parse_device_key("no-route-tag").is_none());
        assert!(parse_device_key("card#x9").is_none());
    }

    #[test]
    fn is_virtual_sink_recognizes_a_deck_or_the_sampler_but_nothing_else() {
        assert!(is_virtual_sink("odj-deck-1"));
        assert!(is_virtual_sink(audio::sampler_sink_name()));
        assert!(!is_virtual_sink("hw:CARD=Built-in"));
    }
}
