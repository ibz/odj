//! Continuously detects which audio inputs/outputs are plugged in —
//! mirrors `discovery.rs`'s continuous player detection, but polls the
//! audio host's own device list instead of a control socket.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow};

use odj_core::audio::{self, Card, Choice, Route};

const PERIOD: Duration = Duration::from_secs(1);

/// A card+route pair's stable identity across repeated scans — a plain
/// index into whichever `Vec<Card>` a scan happened to produce doesn't
/// survive the next one, but [`Card::id`] does.
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
        let entries = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let for_thread = entries.clone();
        let thread = thread::Builder::new()
            .name("odj-devices".into())
            .spawn(move || run(kind, &for_thread, &stopping))
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
    pub fn resolve(&self, key: &DeviceKey) -> Result<(Vec<Card>, Choice)> {
        let cards = scan_cards(self.kind)?;
        let choices = audio::choices(&cards);
        let choice = choices
            .iter()
            .find(|c| cards[c.card].id() == key.0 && c.route == key.1)
            .copied()
            .ok_or_else(|| anyhow!("that device is no longer available"))?;
        Ok((cards, choice))
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

fn run(kind: Kind, entries: &Mutex<Vec<Entry>>, stop: &AtomicBool) {
    let word = if kind == Kind::Input { "In" } else { "Out" };
    while !stop.load(Ordering::Relaxed) {
        if let Ok(cards) = scan_cards(kind) {
            let choices = audio::choices(&cards);
            let fresh: Vec<Entry> = choices
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
            *entries.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        }
        thread::sleep(PERIOD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
