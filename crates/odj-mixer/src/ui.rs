//! The live screen: a real mixer's worth of channel strips, laid out
//! side by side — each with a 3-band EQ and a vertical Volume fader,
//! adjustable by hand or by MIDI knob. There's one main output for
//! everything, picked (and changeable any time) from Settings' own
//! Outputs tab (`` ` ``, see [`draw_settings`]) rather than asked for
//! upfront — restored from a previous session if there was one, else
//! the internal speakers by default (see `main.rs`'s own doc comment).
//! A running odj-player deck, or odj-sampler,
//! always gets a strip, connected automatically the instant it's
//! detected — either one is already part of the session, no separate
//! step needed. A hardware input instead waits until it's added from
//! Settings (`` ` ``), which also holds MIDI-knob mapping — nothing on
//! the live screen itself changes either. Once added, an input is
//! remembered (by its stable device key, in
//! `Settings.mixer_active_inputs`) and reconnected automatically on the
//! next run, same as a deck or odj-sampler already is.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use midir::MidiInputConnection;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Widget};

use odj_core::audio;
use odj_core::filter::{MAX_DB, MIN_DB};
use odj_core::tui::{AMBER, DIM, Help, PAGE_HINT, Page, centered, draw_help, key_hint, tab_span, wrap};

use crate::devices::{DeviceKey, Devices, Entry, device_key_string};
use crate::midi::{self, Learning, Target, Targets};
use crate::record_mode::{RecordMode, Track, TrackState};
use crate::routing::{Param, Routing, TRACKS};

/// A channel strip's step per `,`/`.` press on a control; `Shift`
/// multiplies it, same convention odj-player's own (now-removed) Mixer
/// overlay used.
const VOLUME_STEP: f32 = 0.05;
/// Likewise for an EQ band, in dB.
const EQ_STEP: f32 = 1.0;
/// How wide one channel strip is, borders included.
const STRIP_WIDTH: u16 = 14;
/// The 4 controls a strip's cursor moves between: High, Mid, Low,
/// Volume (top to bottom, same order a real channel strip reads).
const CONTROLS: usize = 4;
/// How long [`auto_connect`] leaves a failed device alone before trying
/// it again — long enough that a genuinely gone device doesn't spam
/// retries (or the log) every single frame, short enough that one
/// that was just slow to finish registering with PipeWire (a deck's or
/// odj-sampler's virtual sink can take a moment after it's first
/// detected to become actually capturable) gets picked up again within
/// a few seconds rather than staying silently unconnected for the rest
/// of the session.
const AUTO_CONNECT_RETRY: Duration = Duration::from_secs(3);
/// How long a status message stays on screen before [`status_text`]
/// starts hiding it — plain elapsed time, not "until the next key",
/// since that cleared a just-shown error the instant the very key that
/// triggered it (e.g. the `Space` that failed to connect something) was
/// itself processed, too fast to ever actually read. Matches
/// odj-player's own `STATUS_TIME`.
const STATUS_TIME: Duration = Duration::from_secs(4);
/// Settings' Buffer tab: the audio buffer sizes on offer, in frames —
/// below [`odj_core::audio::DEFAULT_BUFFER_FRAMES`] (512, included
/// here too) for lower round-trip latency, or above it for more
/// headroom against dropouts on a busier system. Kept comfortably
/// under `routing::RING_STEREO_SAMPLES`'s own fixed 4096-stereo-sample
/// ceiling even at the largest option here (1024 frames is 2048
/// stereo samples, still half that ceiling) — the ring buffer's own
/// slack isn't scaled to match a chosen buffer size, so this list
/// deliberately stops short of where that would start to matter.
const BUFFER_FRAME_OPTIONS: [u32; 5] = [64, 128, 256, 512, 1024];
/// Record mode's own `←`/`→` scrub: extra playhead speed, as a
/// multiple of normal (1x), while held — same values and the same
/// "hold, not a one-shot jump" shape as odj-player's own deck search
/// (`Deck::search`/`SEARCH`/`SUPER_SEARCH`), just applied to the
/// shared playhead instead of one deck's position.
const RECORD_SEARCH: f64 = 4.0;
const RECORD_SUPER_SEARCH: f64 = 20.0;
/// How long a hold is kept alive without a real release event before
/// it's cleared on its own — only used on a terminal that can't
/// report key-release at all (`!key_release`); on one that can, the
/// matching `KeyEventKind::Release` clears it directly instead. Same
/// value and the same fallback-only role as odj-player's own
/// `HOLD_FALLBACK`.
const RECORD_HOLD_FALLBACK: Duration = Duration::from_millis(500);

/// The help's pages (`?` or `/`, from the live screen only — Settings
/// has no help of its own, and isn't covered here either: its own
/// hint line already spells out every key directly) — the live
/// screen's own keys, then About.
pub const HELP: &[Page] = &[
    Page::Keys(
        "Mixer",
        &[
            ("← / →", "Move between channels"),
            ("↑ / ↓", "Move between controls: High, Mid, Low, Volume"),
            ("+ / -", "Adjust the selected control (Shift: ×4)"),
            ("R", "Enter multi-track recorder"),
            ("`", "Settings: pick channels, manage MIDI mappings, pick/change the output or controller"),
        ],
    ),
    Page::Keys(
        "Record",
        &[
            ("↑ / ↓", "Move between the 8 tracks"),
            ("← / →", "Hold to scrub the shared playhead, audibly, at 4× speed (Shift: 20×) — same as odj-player's own deck search"),
            ("S / Shift+S", "Cycle the selected track's state: Play → Mute → Record (Shift: backward)"),
            ("Enter", "Assign the selected track's input, from everything currently routed"),
            ("Space", "Play / pause the shared transport — restarts from 0 on its own once it reaches the end of whatever's armed to Play"),
            ("R", "Start capturing every Record-armed track (or stop, if already capturing)"),
            ("X", "Stop the transport; press again once stopped to rewind to 0"),
            ("B", "Bounce every Play-armed track down to one new stereo WAV file, timestamped, saved alongside the tracks"),
            ("M", "Leave record mode, back to the mixer"),
            ("?", "Open this help"),
        ],
    ),
    Page::About(odj_core::about!()),
];

fn param_label(p: Param) -> &'static str {
    match p {
        Param::Volume => "Volume",
        Param::Low => "Low",
        Param::Mid => "Mid",
        Param::High => "High",
    }
}

/// Which control row `i` is.
fn control_param(i: usize) -> Param {
    match i {
        0 => Param::High,
        1 => Param::Mid,
        2 => Param::Low,
        _ => Param::Volume,
    }
}

/// `(min, max, step-per-press)` for adjusting `param`, `shift` quadrupling
/// the step — same convention odj-player's own Mixer overlay used.
fn step_for(param: Param, shift: bool) -> (f32, f32, f32) {
    if param == Param::Volume {
        (0.0, 1.0, if shift { VOLUME_STEP * 4.0 } else { VOLUME_STEP })
    } else {
        (MIN_DB, MAX_DB, if shift { EQ_STEP * 4.0 } else { EQ_STEP })
    }
}

/// Keeps the *end* of `s` when it's longer than `max` chars, eliding the
/// front instead of the back. A channel strip's title is already
/// shortened (see `shorten_labels`) so what's actually different
/// between two channels sits at the end — "In 2" vs "In 3" — eliding
/// the back the usual way would crop both down to the same "UMC404HD
/// In…" and make them look identical.
fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    std::iter::once('…').chain(s.chars().skip(count - keep)).collect()
}

/// Which `Devices` a channel's input side resolves through — a hardware
/// source via `Kind::Input`, or a deck's/odj-sampler's own virtual sink
/// via `Kind::Deck`; routed identically either way once resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Group {
    Input,
    /// A deck or odj-sampler — anything auto-connected rather than
    /// user-togglable (see [`auto_connect`]); distinguished only by
    /// label ("Deck n" vs. "Sampler"), which comes straight from the
    /// sink's own description.
    Deck,
}

/// Settings' 4 tabs — `Tab`/`BackTab` (Shift+Tab) cycles through them,
/// forward or back. Inputs is everything [`draw_settings`] showed
/// before this was split into tabs at all; Outputs and MIDI both used
/// to be a mandatory screen before the live screen could even start
/// (see `main.rs`'s own doc comment) — now each is just another
/// Settings tab, so either can be changed mid-session too, not only
/// chosen once upfront. Buffer is the newest: `routing::Routing`'s own
/// requested `cpal` buffer size (see [`BUFFER_FRAME_OPTIONS`]), trading
/// round-trip latency against headroom.
#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    Inputs,
    Outputs,
    Midi,
    Buffer,
}

impl SettingsTab {
    fn next(self) -> Self {
        match self {
            SettingsTab::Inputs => SettingsTab::Outputs,
            SettingsTab::Outputs => SettingsTab::Midi,
            SettingsTab::Midi => SettingsTab::Buffer,
            SettingsTab::Buffer => SettingsTab::Inputs,
        }
    }

    fn prev(self) -> Self {
        match self {
            SettingsTab::Inputs => SettingsTab::Buffer,
            SettingsTab::Outputs => SettingsTab::Inputs,
            SettingsTab::Midi => SettingsTab::Outputs,
            SettingsTab::Buffer => SettingsTab::Midi,
        }
    }
}

/// One channel strip: a continuously-detected input or deck.
#[derive(Clone)]
struct Channel {
    key: DeviceKey,
    group: Group,
    label: String,
    available: bool,
}

/// Decks (and odj-sampler, if it's routing through here too) first
/// (furthest left on the live screen, topmost in Settings), then every
/// hardware input.
fn build_channels(in_entries: &[Entry], deck_entries: &[Entry]) -> Vec<Channel> {
    deck_entries
        .iter()
        .map(|e| Channel { key: e.key.clone(), group: Group::Deck, label: e.label.clone(), available: e.available })
        .chain(
            in_entries
                .iter()
                .map(|e| Channel { key: e.key.clone(), group: Group::Input, label: e.label.clone(), available: e.available }),
        )
        .collect()
}

/// Connects every running deck (or odj-sampler, if it's routing through
/// here too) that isn't already routed to the main output, the instant
/// it's detected — either one is already part of the session, so it
/// just plays, no manual step needed. Also reconnects
/// any hardware input named in `active_inputs` (by its stable
/// [`device_key_string`], restored from `Settings.mixer_active_inputs`
/// — see [`run`]'s Space handler, which keeps that set up to date as
/// inputs are added/removed) — everything else waits for an explicit
/// `Space` instead, since there's usually several inputs, not all of
/// them meant to be mixed in. Skips anything in `failed` less than
/// `retry_after` ago, so a device that can't be opened isn't retried
/// every single frame (or spam the log every single frame either) —
/// `run` passes [`AUTO_CONNECT_RETRY`] and clears an entry from
/// `failed` outright once its device disappears (so a reconnect gets
/// an immediate fresh try rather than waiting out the rest of that
/// window). A no-op, loudly or quietly, if there's no main output
/// picked yet.
#[allow(clippy::too_many_arguments)]
fn auto_connect(
    channels: &[Channel],
    routing: &Mutex<Routing>,
    devices_in: &Devices,
    devices_deck: &Devices,
    devices_out: &Devices,
    active_inputs: &HashSet<String>,
    failed: &mut HashMap<DeviceKey, Instant>,
    retry_after: Duration,
    status: &mut Option<(String, Instant)>,
) {
    let mut r = routing.lock().unwrap_or_else(|e| e.into_inner());
    let Some(main) = r.main_output() else { return };
    for ch in channels.iter().filter(|c| c.group == Group::Deck || active_inputs.contains(&device_key_string(&c.key))) {
        if failed.get(&ch.key).is_some_and(|t| t.elapsed() < retry_after) || r.output_of(&ch.key).is_some() {
            continue;
        }
        let devices = if ch.group == Group::Deck { devices_deck } else { devices_in };
        if let Err(e) = r.set_route(devices, devices_out, &ch.key, Some(&main)) {
            let msg = format!("Could not connect {}: {e}", ch.label);
            crate::log::log(&msg);
            set_status(status, msg);
            failed.insert(ch.key.clone(), Instant::now());
        }
    }
}

/// Shows `msg`, timestamped now — see [`status_text`] for how (and
/// when) it stops being shown.
fn set_status(status: &mut Option<(String, Instant)>, msg: impl Into<String>) {
    *status = Some((msg.into(), Instant::now()));
}

/// `status`'s message, for as long as it's within [`STATUS_TIME`] of
/// being set — after that, hidden on its own rather than needing a key
/// to dismiss it (which cleared it too fast to read when the very key
/// that triggered it, e.g. a failed `Space` toggle, was itself what
/// processed next). Every message is also `log::log`ged where it's set,
/// so one that expired before it could be read is still in
/// `log::log_file()` afterward.
fn status_text(status: &Option<(String, Instant)>) -> Option<&str> {
    status.as_ref().filter(|(_, at)| at.elapsed() < STATUS_TIME).map(|(s, _)| s.as_str())
}

/// One `←`/`→` scrub key currently held down — `strong` for Shift
/// (full `RECORD_SUPER_SEARCH` speed instead of `RECORD_SEARCH`).
/// Mirrors odj-player's own `Hold`/`hold`/`apply_holds` shape exactly,
/// just for one pair of keys instead of several.
struct Hold {
    strong: bool,
    /// Only set on a terminal that can't report real key releases
    /// (`!key_release`) — a timeout fallback that clears the hold on
    /// its own if no repeat event refreshes it first (see `record_search_hold`'s
    /// own call site for how a repeated `Press` keeps re-arming this).
    until: Option<Instant>,
}

fn record_search_hold(strong: bool, key_release: bool) -> Option<Hold> {
    let until = (!key_release).then(|| Instant::now() + RECORD_HOLD_FALLBACK);
    Some(Hold { strong, until })
}

/// Clears any hold whose fallback timeout has passed, then pushes the
/// net scrub speed (forward minus backward, `0.0` if neither or both
/// are held) onto the live engine — called every record-mode tick,
/// not just when a key event arrives, so the fallback timeout (and a
/// scrub that's still held) both stay live between events. Forced to
/// `0.0` while `capturing` — moving the shared playhead abruptly
/// while a track is actively writing would corrupt that track's own
/// `start_offset`-based alignment, the same invariant the auto-restart
/// check in `record_mode::RecordMode::poll` already protects; the
/// holds themselves stay tracked underneath, so scrubbing picks back
/// up the moment the capture stops, no need to re-press the key.
fn apply_record_search(search_back: &mut Option<Hold>, search_fwd: &mut Option<Hold>, capturing: bool, routing: &Mutex<Routing>) {
    let now = Instant::now();
    for hold in [&mut *search_back, &mut *search_fwd] {
        if hold.as_ref().and_then(|h| h.until).is_some_and(|t| now >= t) {
            *hold = None;
        }
    }
    let speed = |h: &Option<Hold>| match h {
        Some(h) if h.strong => RECORD_SUPER_SEARCH,
        Some(_) => RECORD_SEARCH,
        None => 0.0,
    };
    let search = if capturing { 0.0 } else { speed(search_fwd) - speed(search_back) };
    routing.lock().unwrap_or_else(|e| e.into_inner()).set_search(search);
}

/// Binds `cc` to `target` both live (`targets`) and in `controller`'s
/// own entry of the shared settings file, so the mapping survives a
/// restart — same replace-the-old-knob semantics as `Targets::bind` on
/// both sides. `controller` is whichever one's actually connected
/// right now (see [`run`]'s own `controller` state) — each keeps its
/// own map, so mapping a knob on one never touches another's.
fn bind_knob(targets: &Targets, controller: &str, cc: u8, target: String) {
    targets.bind(cc, target.clone());
    let mut settings = odj_core::memory::load_settings();
    let knobs = settings.mixer_midi_controllers.entry(controller.to_string()).or_default();
    knobs.retain(|_, c| *c != cc);
    knobs.insert(target, cc);
    let _ = odj_core::memory::save_settings(&settings);
}

/// Clears whatever knob is bound to `target` on `controller`'s own
/// map, both live and in the shared settings file — the inverse of
/// [`bind_knob`]. Drops `controller`'s entire entry once its map goes
/// empty, same as [`forget_controller`] does outright — an entry with
/// nothing in it isn't a controller worth remembering, and would
/// otherwise show up with nothing to forget (see [`ControllerRow`]).
fn unbind_knob(targets: &Targets, controller: &str, target: &str) {
    targets.unbind(target);
    let mut settings = odj_core::memory::load_settings();
    if let Some(knobs) = settings.mixer_midi_controllers.get_mut(controller) {
        knobs.remove(target);
        if knobs.is_empty() {
            settings.mixer_midi_controllers.remove(controller);
        }
    }
    let _ = odj_core::memory::save_settings(&settings);
}

/// Adds or removes `key` from `active_inputs` (both the live set and the
/// shared settings file, so the mix survives a restart — see
/// `Settings.mixer_active_inputs`), mirroring [`bind_knob`]/
/// [`unbind_knob`]'s load-mutate-save pattern.
fn set_active(active_inputs: &mut HashSet<String>, key: &DeviceKey, active: bool) {
    let encoded = device_key_string(key);
    if active {
        active_inputs.insert(encoded.clone());
    } else {
        active_inputs.remove(&encoded);
    }
    let mut settings = odj_core::memory::load_settings();
    if active {
        settings.mixer_active_inputs.insert(encoded);
    } else {
        settings.mixer_active_inputs.remove(&encoded);
    }
    let _ = odj_core::memory::save_settings(&settings);
}

/// Saves `key` as the main output in the shared settings file, so it's
/// restored next time odj-mixer runs — same load-mutate-save pattern as
/// [`set_active`], just for `Settings.mixer_output` instead; the live
/// `Routing` itself is updated by the caller separately (it has no
/// settings file of its own to go through).
fn set_output(key: &DeviceKey) {
    let mut settings = odj_core::memory::load_settings();
    settings.mixer_output = Some(device_key_string(key));
    let _ = odj_core::memory::save_settings(&settings);
}

/// Saves `frames` as the mixer's own buffer size in the shared
/// settings file — mirrors [`set_output`]'s pattern, just for
/// `Settings.mixer_buffer_frames` instead; the live `Routing` itself
/// is updated by the caller separately.
fn set_buffer_frames_setting(frames: u32) {
    let mut settings = odj_core::memory::load_settings();
    settings.mixer_buffer_frames = frames;
    let _ = odj_core::memory::save_settings(&settings);
}

/// Saves `name` as the last-used MIDI controller — mirrors
/// [`set_output`]'s pattern, just for `Settings.mixer_midi_last`
/// instead. Deliberately *doesn't* register `name` among
/// `Settings.mixer_midi_controllers` itself if it isn't there
/// already: an entry with nothing mapped in it isn't a controller
/// worth remembering (see [`ControllerRow`]) — [`bind_knob`] is what
/// actually creates one, the first time something's mapped on it.
/// Returns whatever `name` already has mapped (or an empty map if
/// it's never been used before), for the caller to seed a fresh
/// `Targets` with when switching to it live (see [`run`]'s MIDI tab).
fn set_controller(name: &str) -> HashMap<String, u8> {
    let mut settings = odj_core::memory::load_settings();
    let knobs = settings.mixer_midi_controllers.get(name).cloned().unwrap_or_default();
    settings.mixer_midi_last = Some(name.to_string());
    let _ = odj_core::memory::save_settings(&settings);
    knobs
}

/// Removes `name` from the known MIDI controllers, and its own
/// mappings with it — the inverse of ever having been learned, not
/// just of being currently plugged in (see [`ControllerRow`]). A
/// no-op if `name` has nothing mapped already — there's nothing to
/// forget there, same as `run`'s own `X`/`Delete` handler only calls
/// this when `ControllerRow::mappings` is actually nonzero. Doesn't
/// touch anything live even if `name` happens to be the one
/// connected right now — only the next time it'd otherwise be
/// auto-picked, or shown with mappings, that this takes effect.
fn forget_controller(name: &str) {
    let mut settings = odj_core::memory::load_settings();
    settings.mixer_midi_controllers.remove(name);
    if settings.mixer_midi_last.as_deref() == Some(name) {
        settings.mixer_midi_last = None;
    }
    let _ = odj_core::memory::save_settings(&settings);
}

/// Every discovered deck's (or odj-sampler's) 4 Mixer parameters
/// (`(label, target key)`) that isn't already in `already` — hardware
/// inputs are deliberately left out of this: they're not the point of
/// this walkthrough (decks and the sampler are), and most already have
/// a physical knob on their own hardware, so auto-prompting for one on
/// every detected input would mostly just be noise. An input's
/// Volume/EQ can still be mapped on demand from Settings.
fn unmapped_targets(deck_entries: &[Entry], already: &HashMap<String, u8>) -> Vec<(String, String)> {
    let mut todo = Vec::new();
    for e in deck_entries {
        for param in Param::ALL {
            let key = midi::target_key(&Target { input: e.key.clone(), param });
            if !already.contains_key(&key) {
                todo.push((format!("{} {}", e.label, param_label(param)), key));
            }
        }
    }
    todo
}

/// Walks the user through mapping a MIDI knob to each deck's (or
/// odj-sampler's) Volume/Low/Mid/High that isn't bound to one yet
/// (`already`, `controller`'s own entry of
/// `Settings.mixer_midi_controllers`), for every one already
/// discovered by the time this is called — the same "turn this knob"
/// walkthrough odj-player's own old MIDI setup used, just over however
/// many decks (and the sampler) show up instead of one fixed deck.
/// `Esc` stops the walkthrough early; anything left unmapped (including
/// one that shows up later, or a hardware input's Volume/EQ) can still
/// be mapped on demand from Settings. A no-op if there's nothing
/// unmapped to walk through.
pub fn learn_unmapped(
    terminal: &mut DefaultTerminal,
    devices_deck: &Devices,
    controller: &str,
    already: &HashMap<String, u8>,
    targets: &Arc<Targets>,
    learning: &Arc<Learning>,
) -> Result<()> {
    let todo = unmapped_targets(&devices_deck.entries(), already);
    // A turned knob's own trailing CCs can still arrive right after it's
    // captured, while this is already waiting for the *next* one — reject
    // a repeat of any CC already captured this session (same knob, still
    // settling) rather than wrongly binding it to this target too; a knob
    // that's gone quiet by the time we ask for it just won't match.
    let mut used = HashSet::new();
    for (i, (label, target)) in todo.iter().enumerate() {
        let prompt = format!("({}/{}) {label}", i + 1, todo.len());
        let cc = loop {
            match midi::learn(terminal, learning, &prompt)? {
                Some(cc) if used.contains(&cc) => continue,
                other => break other,
            }
        };
        let Some(cc) = cc else { break };
        used.insert(cc);
        bind_knob(targets, controller, cc, target.clone());
    }
    Ok(())
}

/// Shown briefly while giving already-running decks a moment to be
/// scanned before [`learn_unmapped`] decides what's left to map.
pub fn wait_for_decks(terminal: &mut DefaultTerminal, wait: Duration) -> Result<()> {
    terminal.draw(|f| {
        let area = centered(f.area(), 50, 20);
        f.render_widget(Clear, area);
        let block = Block::bordered().title(" odj-mixer ").border_style(Style::new().fg(AMBER));
        let p = Paragraph::new("looking for running decks…").alignment(Alignment::Center).style(Style::new().fg(AMBER)).block(block);
        f.render_widget(p, area);
    })?;
    std::thread::sleep(wait);
    Ok(())
}


/// The channels the live screen shows: every deck (always — see
/// [`auto_connect`]) plus every hardware input that's been explicitly
/// added (anything else waits behind `+`, see the module doc comment).
fn visible_channels(channels: &[Channel], routing: &Routing) -> Vec<Channel> {
    channels.iter().filter(|c| c.group == Group::Deck || routing.output_of(&c.key).is_some()).cloned().collect()
}

/// Splits a hardware input's full label (always `"{interface} · {rest}"`
/// — see `odj_core::audio::Choice::label`, which built it) into its
/// interface name and the part that actually differs between that
/// interface's own channels. A label with no `" · "` (a deck's, which
/// is already short) comes back as the whole thing with an empty rest.
fn split_label(label: &str) -> (&str, &str) {
    label.split_once(" · ").unwrap_or((label, ""))
}

/// The handful of `" · "`-suffixes `Choice::label` ever actually
/// produces, shortened to just what's different — `"(mono)"`/
/// `"(stereo)"` is redundant once the number right next to it already
/// says so. Used only as the fallback for a lone hardware input, where
/// [`shorten_labels`] has nothing to diff it against.
fn shorten_suffix(suffix: &str) -> String {
    match suffix {
        "stereo" => "Stereo".to_string(),
        "left only (mono)" => "Left".to_string(),
        "right only (mono)" => "Right".to_string(),
        s => s.replace(" (mono)", "").replace(" (stereo)", ""),
    }
}

/// How many leading chars every one of `labels` has in common (0 if
/// there's only one, or none — nothing to compare it against).
fn common_prefix_len(labels: &[Vec<char>]) -> usize {
    let Some(shortest) = labels.iter().map(Vec::len).min() else { return 0 };
    if labels.len() < 2 {
        return 0;
    }
    (0..shortest).take_while(|&i| labels.iter().all(|l| l[i] == labels[0][i])).count()
}

/// Same as [`common_prefix_len`], from the end instead, not reaching
/// back past `prefix_len` (so an already-shared prefix and an
/// already-shared suffix can't both claim the same character on a
/// label that's nothing *but* that shared text).
fn common_suffix_len(labels: &[Vec<char>], prefix_len: usize) -> usize {
    let Some(shortest) = labels.iter().map(Vec::len).min() else { return 0 };
    if labels.len() < 2 {
        return 0;
    }
    let room = shortest.saturating_sub(prefix_len);
    (0..room)
        .take_while(|&i| {
            let c = labels[0][labels[0].len() - 1 - i];
            labels.iter().all(|l| l[l.len() - 1 - i] == c)
        })
        .count()
}

/// Shortens every hardware input's label for the channel strip's narrow
/// title down to whatever's actually different about it — the shared
/// lead-in (interface name, rate, "·", "In") and shared tail ("(mono)",
/// "(stereo)") get stripped from every one of them at once, whatever
/// that leaves is the channel name. With only one hardware input to
/// begin with, there's nothing to diff it against, so it falls back to
/// just its own `" · "`-suffix instead. A deck's virtual sink is always
/// a plain stereo pair, so its label is always "{name} · stereo" — the
/// suffix never says anything the name doesn't already; drop it rather
/// than diff it (a deck's name is already unique on its own).
fn shorten_labels(channels: Vec<Channel>) -> Vec<Channel> {
    let inputs: Vec<Vec<char>> = channels.iter().filter(|c| c.group == Group::Input).map(|c| c.label.chars().collect()).collect();
    let prefix = common_prefix_len(&inputs);
    let suffix = common_suffix_len(&inputs, prefix);

    channels
        .into_iter()
        .map(|ch| {
            if ch.group == Group::Deck {
                return Channel { label: split_label(&ch.label).0.to_string(), ..ch };
            }
            let chars: Vec<char> = ch.label.chars().collect();
            let label = if inputs.len() < 2 {
                let suffix = shorten_suffix(split_label(&ch.label).1);
                if suffix.is_empty() { ch.label.clone() } else { suffix }
            } else {
                let middle: String = chars[prefix..chars.len() - suffix].iter().collect();
                let middle = middle.trim();
                if middle.is_empty() { ch.label.clone() } else { middle.to_string() }
            };
            Channel { label, ..ch }
        })
        .collect()
}

/// One row of Settings' single channel list — 2 lines tall, since
/// there's room for more than just a label and an indicator here: the
/// label and whether it's currently mixed into the main output on the
/// first, its own [`midi_summary`] on the second. `Space` toggles the
/// mixed-in state for an input; a deck is always connected
/// automatically (see [`auto_connect`]) and isn't user-togglable, so
/// `Space` is a no-op on one — [`draw_settings`] shows it with a
/// visibly different, non-interactive indicator rather than the
/// on/off one an input gets. `Enter` on either kind of row drills into
/// its own 4 [`ParamRow`]s — this works even for a row that's `!present`
/// (see below), so a mapping to a device that's since disappeared can
/// still be seen and cleared, not just one that's currently detected.
struct ChannelRow {
    key: DeviceKey,
    group: Group,
    label: String,
    added: bool,
    /// Whether this row's device is actually among the currently
    /// detected ones — `false` for a row that exists here only because
    /// it still has a MIDI mapping (see [`build_channel_rows`]). Always
    /// implies `!added` (an absent device obviously isn't routed to
    /// anything); `Space` is a no-op on one of these too, same as on a
    /// deck's row, and [`draw_settings`] marks it with its own
    /// distinct, non-interactive indicator.
    present: bool,
    /// This row's own 4 mapped CCs, High/Mid/Low/Volume (same order
    /// [`control_param`] uses) — shown on the row's own 2nd line, so a
    /// mapping can be seen without drilling in to check one at a time.
    midi: [Option<u8>; 4],
}

/// Every currently detected [`Channel`], plus one more row for any
/// device that's since disappeared but still has at least one MIDI
/// knob mapped to it (`targets.mapped_inputs()`, checked against
/// `channels` so a still-detected device isn't duplicated) — otherwise
/// a mapping to a deck that's quit, or an input that's been unplugged,
/// would become invisible (and so uncheckable, and unclearable) the
/// moment it's gone, even though the mapping itself quietly lives on
/// in `Settings.mixer_midi_controllers` either way. Such a row has no live
/// label or availability to show, so [`phantom_channel`] derives
/// what it can from the device key alone.
fn build_channel_rows(channels: &[Channel], routing: &Routing, targets: &Targets) -> Vec<ChannelRow> {
    let midi_for = |key: &DeviceKey| -> [Option<u8>; 4] {
        std::array::from_fn(|i| targets.cc_for(&midi::target_key(&Target { input: key.clone(), param: control_param(i) })))
    };
    let mut rows: Vec<ChannelRow> = channels
        .iter()
        .map(|ch| ChannelRow {
            key: ch.key.clone(),
            group: ch.group,
            label: ch.label.clone(),
            added: routing.output_of(&ch.key).is_some(),
            present: true,
            midi: midi_for(&ch.key),
        })
        .collect();
    let known: HashSet<&DeviceKey> = channels.iter().map(|c| &c.key).collect();
    for key in targets.mapped_inputs() {
        if known.contains(&key) {
            continue;
        }
        let (group, label) = phantom_channel(&key);
        let midi = midi_for(&key);
        rows.push(ChannelRow { key, group, label, added: false, present: false, midi });
    }
    rows
}

/// A best-effort `(group, label)` for a device that has a MIDI mapping
/// but isn't currently detected — there's no live [`Entry`] to read a
/// proper label from, so this falls back to recognizing a deck's or
/// odj-sampler's own virtual-sink naming (see `audio::deck_number`/
/// `audio::sampler_sink_name`) for those, and the raw device id plus
/// its route for anything else (a hardware input, unplugged).
fn phantom_channel(key: &DeviceKey) -> (Group, String) {
    if let Some(n) = audio::deck_number(&key.0) {
        return (Group::Deck, format!("Deck {n}"));
    }
    if key.0 == audio::sampler_sink_name() {
        return (Group::Deck, "Sampler".to_string());
    }
    let route = match key.1 {
        audio::Route::Stereo(l, r) => format!("{}-{}", l + 1, r + 1),
        audio::Route::Mono(c) => format!("{}", c + 1),
    };
    (Group::Input, format!("{} · {route}", key.0))
}

/// One row of Settings' MIDI tab: the union of every controller
/// `Settings.mixer_midi_controllers` has at least one saved mapping
/// for (whether or not it's plugged in right now) and every one
/// currently plugged in that has none yet — so a controller with
/// mappings still shows up even unplugged, with the option to forget
/// them, while a brand new one can still be picked the moment it's
/// seen, not just once it's been used before. `mappings` is 0 for the
/// latter — there's nothing to forget on one of those, same as
/// there's nothing to switch to on one that `!present`.
struct ControllerRow {
    name: String,
    present: bool,
    mappings: usize,
}

fn build_controller_rows(known: &HashMap<String, HashMap<String, u8>>) -> Vec<ControllerRow> {
    let ports = odj_core::midi::list_ports();
    let mut names: Vec<String> = known.keys().cloned().collect();
    for (id, label) in &ports {
        let name = odj_core::midi::stable_name(id, label);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let present = ports.iter().any(|(id, label)| odj_core::midi::stable_name(id, label) == name);
            let mappings = known.get(&name).map_or(0, HashMap::len);
            ControllerRow { name, present, mappings }
        })
        .collect()
}

/// One row of a [`ChannelRow`]'s own drill-in: one of its 4 params,
/// with whatever CC is currently bound to it, if any — `Enter` maps
/// (or remaps) one, `X`/`Delete` clears it.
struct ParamRow {
    target: String,
    param: Param,
    cc: Option<u8>,
}

/// A [`ChannelRow`]'s own 2nd line: its MIDI mapping summary,
/// High/Mid/Low/Volume in that order (same as the channel strip itself
/// reads, top to bottom, and the same order [`ChannelRow::midi`] is
/// built in) — "—" for whichever of the 4 isn't mapped.
fn midi_summary(midi: &[Option<u8>; 4]) -> String {
    let cc = |cc: Option<u8>| cc.map_or("—".to_string(), |c| c.to_string());
    format!("MIDI  H:{}  M:{}  L:{}  V:{}", cc(midi[0]), cc(midi[1]), cc(midi[2]), cc(midi[3]))
}

fn build_param_rows(key: &DeviceKey, targets: &Targets) -> Vec<ParamRow> {
    Param::ALL
        .into_iter()
        .map(|param| {
            let target = midi::target_key(&Target { input: key.clone(), param });
            let cc = targets.cc_for(&target);
            ParamRow { target, param, cc }
        })
        .collect()
}

/// The live screen: one channel strip per running deck (connected
/// automatically, see [`auto_connect`]) plus every hardware input
/// that's been explicitly added, side by side. `←`/`→` picks the
/// channel and `↑`/`↓` picks the control within it (High, Mid, Low,
/// Volume, wrapping top to bottom and back) — neither ever changes a
/// value, so they're always safe to tap. `+`/`-` adjusts the selected
/// control instead (`Shift`: ×4, a no-op until the channel's actually
/// connected). `` ` `` (one of the globals pinned to the right of the
/// hint line, see [`draw_mixer`]) opens Settings, 3 tabs
/// ([`SettingsTab`], cycled either direction with `Tab`/`BackTab`).
/// Inputs is one list of every detected input and deck, plus (see
/// [`build_channel_rows`]) a row for any device that's no longer
/// detected but still has a MIDI mapping. `↑`/`↓` picks a row, `Space`
/// adds the selected input to the mix or removes it again (a no-op on
/// a deck's row — it's always connected automatically, not
/// user-togglable — or on one that isn't currently detected, for the
/// opposite reason: there's nothing there to connect) — this also
/// persists, so an added input is still there (and reconnected
/// automatically) next time odj-mixer runs; `active_inputs` is this
/// run's starting set, loaded from `Settings.mixer_active_inputs` by
/// the caller. `Enter` drills into the selected row's own settings —
/// for now just its 4 MIDI mappings, Volume/Low/Mid/High, where
/// `Enter` maps a knob to the selected one and `X`/`Delete` clears it,
/// `Esc` backs out to the channel list. Outputs is every detected
/// output; `↑`/`↓` picks one and `Enter` makes it the main output
/// (also persisted, to `Settings.mixer_output`, restored at startup by
/// the caller instead of asking — see `main.rs`'s own doc comment) —
/// switching tears down and reconnects every channel fresh (see
/// `Routing::set_main_output`), the same auto-connect that puts a
/// freshly detected one back doing the same for all of them here.
/// MIDI ([`build_controller_rows`]) is every controller with at least
/// one saved mapping, plus every one currently present that has none
/// yet (each row names how many it has — 0 for the latter); `↑`/`↓`
/// picks one, `Enter` switches to it live if it's present (tearing
/// down whatever MIDI connection there was, if any, and seeding
/// `targets` — see `Targets::replace` — from that controller's own
/// saved knobs, or an empty map if it has none), `X`/`Delete` forgets
/// one's mappings instead — a no-op if it has none to forget, same as
/// `Enter` is on one that isn't present. `controller` (this run's
/// starting one, if any) and `midi_conn` (its live connection, ditto)
/// are both `None`
/// together or `Some` together — see `main.rs`'s own doc comment for
/// how the initial one's chosen. `Esc`/`` ` `` from any tab's own list
/// returns to the live screen. Nothing on the live screen itself
/// changes a mapping, the output or the controller any more; that
/// only happens from here. `Ctrl+C` quits — `q`/`Esc` don't, here or
/// anywhere else in odj-mixer; `Esc` is reserved for backing out of
/// whatever's open instead (Settings, help, a drill-in). `?`/`/` opens
/// [`HELP`] from the live screen, taking over the keys until
/// `Esc`/`?`/`/` closes it again — Settings has no help of its own,
/// since every key there is already spelled out in its own hint line.
// `midi_conn`'s whole purpose is outliving its own scope with the
// right lifetime (dropping it tears down that connection) — it's
// never actually read, so the compiler's own unused-assignment check
// doesn't apply here the way it would to a value meant to be used.
#[allow(clippy::too_many_arguments, unused_assignments)]
pub fn run(
    terminal: &mut DefaultTerminal,
    devices_in: &Devices,
    devices_deck: &Devices,
    devices_out: &Devices,
    routing: &Arc<Mutex<Routing>>,
    targets: &Arc<Targets>,
    learning: &Arc<Learning>,
    #[allow(unused_variables)] mut midi_conn: Option<MidiInputConnection<()>>,
    mut controller: Option<String>,
    mut active_inputs: HashSet<String>,
    key_release: bool,
) -> Result<()> {
    let mut selected_channel = 0usize;
    let mut selected_control = 0usize;
    let mut settings_open = false;
    let mut settings_tab = SettingsTab::Inputs;
    let mut channel_selected = 0usize;
    // `Some(key)` once `Enter` drills into that channel's own 4 params.
    let mut mapping_drill: Option<DeviceKey> = None;
    let mut param_selected = 0usize;
    let mut output_selected = 0usize;
    let mut midi_selected = 0usize;
    let mut buffer_selected = 0usize;
    let mut help = Help::default();
    let mut failed: HashMap<DeviceKey, Instant> = HashMap::new();
    let mut record_mode = RecordMode::load();
    let mut record_mode_open = false;
    let mut record_selected = 0usize;
    // `Some(picker-selected-index)` once `Enter` drills into the
    // selected track's own input picker.
    let mut record_picker: Option<usize> = None;
    // `←`/`→` scrubbing the shared playhead — see `apply_record_search`.
    let mut record_search_back: Option<Hold> = None;
    let mut record_search_fwd: Option<Hold> = None;
    // See `set_status`/`status_text` for how long this actually stays
    // shown for.
    let mut status: Option<(String, Instant)> = None;

    loop {
        let in_entries = devices_in.entries();
        let deck_entries = devices_deck.entries();
        {
            let mut r = routing.lock().unwrap_or_else(|e| e.into_inner());
            let known: HashSet<&DeviceKey> = in_entries.iter().chain(&deck_entries).map(|e| &e.key).collect();
            for gone in r.routed_inputs().into_iter().filter(|k| !known.contains(k)).collect::<Vec<_>>() {
                r.forget_input(&gone);
            }
            failed.retain(|k, _| known.contains(k));
            for e in r.take_errors() {
                crate::log::log(&e);
                set_status(&mut status, e);
            }
            drop(r);
            for msg in record_mode.poll(routing, |k| known.contains(k)) {
                crate::log::log(&msg);
                set_status(&mut status, msg);
            }
        }

        auto_connect(
            &build_channels(&in_entries, &deck_entries),
            routing,
            devices_in,
            devices_deck,
            devices_out,
            &active_inputs,
            &mut failed,
            AUTO_CONNECT_RETRY,
            &mut status,
        );

        if settings_open {
            let out_entries = devices_out.entries();
            let channel_rows = {
                let r = routing.lock().unwrap_or_else(|e| e.into_inner());
                build_channel_rows(&build_channels(&in_entries, &deck_entries), &r, targets)
            };
            let main_output = routing.lock().unwrap_or_else(|e| e.into_inner()).main_output();
            let known_controllers = odj_core::memory::load_settings().mixer_midi_controllers;
            let controller_rows = build_controller_rows(&known_controllers);
            let (buffer_frames, buffer_rate) = {
                let r = routing.lock().unwrap_or_else(|e| e.into_inner());
                (r.buffer_frames(), r.main_output_rate().unwrap_or(48_000))
            };
            channel_selected = channel_selected.min(channel_rows.len().saturating_sub(1));
            output_selected = output_selected.min(out_entries.len().saturating_sub(1));
            midi_selected = midi_selected.min(controller_rows.len().saturating_sub(1));
            buffer_selected = buffer_selected.min(BUFFER_FRAME_OPTIONS.len() - 1);
            // The drilled-into channel disappearing (unplugged, deck
            // quit) backs out to the channel list rather than pointing
            // at nothing.
            if mapping_drill.as_ref().is_some_and(|k| !channel_rows.iter().any(|r| &r.key == k)) {
                mapping_drill = None;
            }
            let drilled = mapping_drill.as_ref().map(|key| {
                let label = channel_rows.iter().find(|r| &r.key == key).map_or("", |r| &r.label);
                (label.to_string(), build_param_rows(key, targets))
            });
            if let Some((_, rows)) = &drilled {
                param_selected = param_selected.min(rows.len().saturating_sub(1));
            }

            terminal.draw(|f| {
                draw_settings(
                    f,
                    settings_tab,
                    &channel_rows,
                    channel_selected,
                    drilled.as_ref().map(|(label, rows)| (label.as_str(), rows.as_slice())),
                    param_selected,
                    &out_entries,
                    output_selected,
                    main_output.as_ref(),
                    &controller_rows,
                    midi_selected,
                    controller.as_deref(),
                    buffer_selected,
                    buffer_frames,
                    buffer_rate,
                    status_text(&status),
                );
            })?;
            if event::poll(Duration::from_millis(100))? {
                let Event::Key(k) = event::read()? else { continue };
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                if let Some((label, rows)) = &drilled {
                    match k.code {
                        KeyCode::Esc => mapping_drill = None,
                        KeyCode::Up => param_selected = param_selected.saturating_sub(1),
                        KeyCode::Down => param_selected = (param_selected + 1).min(rows.len().saturating_sub(1)),
                        KeyCode::Enter if !rows.is_empty() => {
                            let row = &rows[param_selected];
                            match &controller {
                                None => set_status(&mut status, "No MIDI controller connected"),
                                Some(name) => {
                                    if let Some(cc) = midi::learn(terminal, learning, &format!("{label} {}", param_label(row.param)))? {
                                        bind_knob(targets, name, cc, row.target.clone());
                                    }
                                }
                            }
                        }
                        KeyCode::Char('x' | 'X') | KeyCode::Delete if !rows.is_empty() => {
                            if let Some(name) = &controller {
                                unbind_knob(targets, name, &rows[param_selected].target);
                            }
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    }
                    continue;
                }
                // `Tab`/`BackTab` (Shift+Tab) cycles tabs either way —
                // not handled per-tab below since it's the one key
                // every tab shares.
                if matches!(k.code, KeyCode::Tab | KeyCode::BackTab) {
                    settings_tab = if k.code == KeyCode::Tab { settings_tab.next() } else { settings_tab.prev() };
                    continue;
                }
                match settings_tab {
                    SettingsTab::Inputs => match k.code {
                        KeyCode::Esc | KeyCode::Char('`') => settings_open = false,
                        KeyCode::Up => channel_selected = channel_selected.saturating_sub(1),
                        KeyCode::Down => channel_selected = (channel_selected + 1).min(channel_rows.len().saturating_sub(1)),
                        KeyCode::Char(' ') if channel_rows.get(channel_selected).is_some_and(|r| r.group == Group::Input && r.present) => {
                            let row = &channel_rows[channel_selected];
                            let mut r = routing.lock().unwrap_or_else(|e| e.into_inner());
                            let target = if row.added { None } else { r.main_output() };
                            match r.set_route(devices_in, devices_out, &row.key, target.as_ref()) {
                                Ok(()) => set_active(&mut active_inputs, &row.key, target.is_some()),
                                Err(e) => {
                                    let msg = format!("Could not connect {}: {e}", row.label);
                                    crate::log::log(&msg);
                                    set_status(&mut status, msg);
                                }
                            }
                            failed.remove(&row.key);
                        }
                        KeyCode::Enter if !channel_rows.is_empty() => {
                            mapping_drill = Some(channel_rows[channel_selected].key.clone());
                            param_selected = 0;
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    },
                    SettingsTab::Outputs => match k.code {
                        KeyCode::Esc | KeyCode::Char('`') => settings_open = false,
                        KeyCode::Up => output_selected = output_selected.saturating_sub(1),
                        KeyCode::Down => output_selected = (output_selected + 1).min(out_entries.len().saturating_sub(1)),
                        KeyCode::Enter if out_entries.get(output_selected).is_some_and(|o| o.available) => {
                            let key = out_entries[output_selected].key.clone();
                            // Switching outputs tears down and reconnects
                            // every route (see `Routing::set_main_output`'s
                            // own doc comment), which would otherwise
                            // silently drop every record-mode tap and
                            // Play track along with it — stop them
                            // first, with their own explanation.
                            let was_running = record_mode.is_running();
                            record_mode.stop_everything(routing);
                            if was_running {
                                let msg = "Record mode stopped: main output changed".to_string();
                                crate::log::log(&msg);
                                set_status(&mut status, msg);
                            }
                            routing.lock().unwrap_or_else(|e| e.into_inner()).set_main_output(Some(key.clone()));
                            set_output(&key);
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    },
                    SettingsTab::Midi => match k.code {
                        KeyCode::Esc | KeyCode::Char('`') => settings_open = false,
                        KeyCode::Up => midi_selected = midi_selected.saturating_sub(1),
                        KeyCode::Down => midi_selected = (midi_selected + 1).min(controller_rows.len().saturating_sub(1)),
                        KeyCode::Enter
                            if controller_rows
                                .get(midi_selected)
                                .is_some_and(|r| r.present && Some(r.name.as_str()) != controller.as_deref()) =>
                        {
                            let name = controller_rows[midi_selected].name.clone();
                            if let Some(id) = odj_core::midi::port_id_for(&name) {
                                // Tear down whatever was connected
                                // before switching — a `MidiInputConnection`
                                // is tied to one specific port, so the old
                                // one can't just be redirected.
                                midi_conn = None;
                                let knobs = set_controller(&name);
                                targets.replace(knobs.iter().map(|(t, &cc)| (cc, t.clone())).collect());
                                let dispatch_routing = routing.clone();
                                midi_conn = midi::connect(&id, targets.clone(), learning.clone(), move |target, t| {
                                    crate::dispatch(target, t, &dispatch_routing);
                                });
                                controller = Some(name);
                            }
                        }
                        KeyCode::Char('x' | 'X') | KeyCode::Delete if controller_rows.get(midi_selected).is_some_and(|r| r.mappings > 0) => {
                            forget_controller(&controller_rows[midi_selected].name);
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    },
                    SettingsTab::Buffer => match k.code {
                        KeyCode::Esc | KeyCode::Char('`') => settings_open = false,
                        KeyCode::Up => buffer_selected = buffer_selected.saturating_sub(1),
                        KeyCode::Down => buffer_selected = (buffer_selected + 1).min(BUFFER_FRAME_OPTIONS.len() - 1),
                        KeyCode::Enter if BUFFER_FRAME_OPTIONS[buffer_selected] != buffer_frames => {
                            let frames = BUFFER_FRAME_OPTIONS[buffer_selected];
                            // Same hazard, same guard, as switching the
                            // main output (see that arm's own comment
                            // just above) — changing the buffer size
                            // tears down every route exactly the same
                            // way.
                            let was_running = record_mode.is_running();
                            record_mode.stop_everything(routing);
                            if was_running {
                                let msg = "Record mode stopped: buffer size changed".to_string();
                                crate::log::log(&msg);
                                set_status(&mut status, msg);
                            }
                            routing.lock().unwrap_or_else(|e| e.into_inner()).set_buffer_frames(frames);
                            set_buffer_frames_setting(frames);
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    },
                }
            }
            continue;
        }

        if record_mode_open {
            apply_record_search(&mut record_search_back, &mut record_search_fwd, record_mode.is_capturing(), routing);
            let labels: HashMap<DeviceKey, String> =
                in_entries.iter().chain(&deck_entries).map(|e| (e.key.clone(), e.label.clone())).collect();
            let mut routed_inputs = routing.lock().unwrap_or_else(|e| e.into_inner()).routed_inputs();
            routed_inputs.sort_by(|a, b| labels.get(a).map(String::as_str).unwrap_or_default().cmp(labels.get(b).map(String::as_str).unwrap_or_default()));

            if let Some(sel) = &mut record_picker {
                *sel = (*sel).min(routed_inputs.len().saturating_sub(1));
            }
            let rate = routing.lock().unwrap_or_else(|e| e.into_inner()).main_output_rate().unwrap_or(48_000);
            let playhead_frames = record_mode.playhead(routing);
            let elapsed = Duration::from_secs_f64(playhead_frames as f64 / rate as f64);
            let muted: [bool; TRACKS] = std::array::from_fn(|i| {
                record_mode.tracks()[i].input.as_ref().is_some_and(|k| routing.lock().unwrap_or_else(|e| e.into_inner()).is_input_muted(k))
            });

            terminal.draw(|f| {
                draw_record_mode(
                    f,
                    record_mode.tracks(),
                    record_selected,
                    &muted,
                    elapsed,
                    rate,
                    record_mode.is_running(),
                    record_mode.is_capturing(),
                    &labels,
                    record_picker.map(|sel| (routed_inputs.as_slice(), sel)),
                    status_text(&status),
                );
                if help.is_open() {
                    draw_help(f, &help, HELP);
                }
            })?;

            if event::poll(Duration::from_millis(100))? {
                let Event::Key(k) = event::read()? else { continue };
                if k.kind == KeyEventKind::Release {
                    // Only `←`/`→` care about a real release — every
                    // other record-mode key is a one-shot action, same
                    // as the rest of this app.
                    match k.code {
                        KeyCode::Left => record_search_back = None,
                        KeyCode::Right => record_search_fwd = None,
                        _ => {}
                    }
                    continue;
                }
                // The help takes the keys that turn and scroll it; nothing
                // underneath acts while it's open — same as the live screen.
                if help.is_open() {
                    match k.code {
                        KeyCode::Esc | KeyCode::Char('?' | '/') => help.close(),
                        _ => {
                            help.key(k.code, k.modifiers.contains(KeyModifiers::SHIFT), HELP.len());
                        }
                    }
                    continue;
                }
                if let Some(sel) = record_picker {
                    match k.code {
                        KeyCode::Esc => record_picker = None,
                        KeyCode::Up => record_picker = Some(sel.saturating_sub(1)),
                        KeyCode::Down => record_picker = Some((sel + 1).min(routed_inputs.len().saturating_sub(1))),
                        KeyCode::Enter if !routed_inputs.is_empty() => {
                            record_mode.set_input(record_selected, Some(routed_inputs[sel].clone()), routing);
                            record_picker = None;
                        }
                        KeyCode::Char('x' | 'X') | KeyCode::Delete => {
                            record_mode.set_input(record_selected, None, routing);
                            record_picker = None;
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    }
                    continue;
                }
                let shift = k.modifiers.contains(KeyModifiers::SHIFT);
                let repeat = k.kind == KeyEventKind::Repeat;
                match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Char('?' | '/') => help.toggle(),
                    KeyCode::Char('m' | 'M') => {
                        record_mode_open = false;
                        record_search_back = None;
                        record_search_fwd = None;
                        routing.lock().unwrap_or_else(|e| e.into_inner()).set_search(0.0);
                        record_mode.leave(routing);
                    }
                    KeyCode::Up => record_selected = record_selected.saturating_sub(1),
                    KeyCode::Down => record_selected = (record_selected + 1).min(TRACKS - 1),
                    // Hold-to-scrub the shared playhead, same shape as
                    // odj-player's own deck search: a fresh press (or,
                    // on a terminal with no real release events, any
                    // repeat too — see `record_search_hold`'s own doc
                    // comment) arms or re-arms the hold; the matching
                    // `Release` above (or the fallback timeout) clears
                    // it. `apply_record_search` turns whichever holds
                    // are live into the engine's actual scrub speed,
                    // every tick, not just here.
                    KeyCode::Left if !repeat || !key_release => record_search_back = record_search_hold(shift, key_release),
                    KeyCode::Right if !repeat || !key_release => record_search_fwd = record_search_hold(shift, key_release),
                    KeyCode::Char('s' | 'S') => {
                        let forward = !shift;
                        if let Err(e) = record_mode.cycle_state(record_selected, forward, routing) {
                            let msg = format!("Track {}: {e}", record_selected + 1);
                            crate::log::log(&msg);
                            set_status(&mut status, msg);
                        }
                    }
                    KeyCode::Enter => record_picker = Some(0),
                    // Same key as odj-player's own Play/Pause.
                    KeyCode::Char(' ') => record_mode.toggle_play(routing),
                    KeyCode::Char('r' | 'R') => {
                        for msg in record_mode.toggle_record(routing) {
                            crate::log::log(&msg);
                            set_status(&mut status, msg);
                        }
                    }
                    KeyCode::Char('x' | 'X') => record_mode.stop_or_rewind(routing),
                    KeyCode::Char('b' | 'B') => {
                        let msg = match record_mode.bounce() {
                            Ok(path) => format!("Bounced to {}", path.display()),
                            Err(e) => format!("Could not bounce: {e}"),
                        };
                        crate::log::log(&msg);
                        set_status(&mut status, msg);
                    }
                    _ => {}
                }
            }
            continue;
        }

        let channels: Vec<Channel> = {
            let r = routing.lock().unwrap_or_else(|e| e.into_inner());
            shorten_labels(visible_channels(&build_channels(&in_entries, &deck_entries), &r))
        };
        selected_channel = if channels.is_empty() { 0 } else { selected_channel.min(channels.len() - 1) };

        let current_cc = channels
            .get(selected_channel)
            .and_then(|ch| targets.cc_for(&midi::target_key(&Target { input: ch.key.clone(), param: control_param(selected_control) })));

        terminal.draw(|f| {
            let r = routing.lock().unwrap_or_else(|e| e.into_inner());
            draw_mixer(f, &channels, selected_channel, selected_control, current_cc, &r, status_text(&status), record_mode.is_capturing());
            if help.is_open() {
                draw_help(f, &help, HELP);
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            let Event::Key(k) = event::read()? else { continue };
            if k.kind == KeyEventKind::Release {
                continue;
            }
            // The help takes the keys that turn and scroll it; nothing
            // underneath acts while it's open.
            if help.is_open() {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('?' | '/') => help.close(),
                    _ => {
                        help.key(k.code, k.modifiers.contains(KeyModifiers::SHIFT), HELP.len());
                    }
                }
                continue;
            }
            match k.code {
                KeyCode::Char('?' | '/') => help.toggle(),
                KeyCode::Char('`') => settings_open = true,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Char('r' | 'R') => {
                    record_mode_open = true;
                    for msg in record_mode.enter(routing) {
                        crate::log::log(&msg);
                        set_status(&mut status, msg);
                    }
                }
                KeyCode::Left | KeyCode::Right if !channels.is_empty() => {
                    let forward = k.code == KeyCode::Right;
                    selected_channel = if forward {
                        (selected_channel + 1) % channels.len()
                    } else {
                        (selected_channel + channels.len() - 1) % channels.len()
                    };
                }
                KeyCode::Up | KeyCode::Down if !channels.is_empty() => {
                    let forward = k.code == KeyCode::Down;
                    selected_control = if forward {
                        (selected_control + 1) % CONTROLS
                    } else {
                        (selected_control + CONTROLS - 1) % CONTROLS
                    };
                }
                // `=` too: some terminals report Shift+`=` as the
                // literal `+`, others as plain `=` with a Shift
                // modifier we don't bother checking — accepting either
                // avoids depending on which.
                KeyCode::Char('+' | '=' | '-') if !channels.is_empty() => {
                    let param = control_param(selected_control);
                    let forward = matches!(k.code, KeyCode::Char('+' | '='));
                    let ch = &channels[selected_channel];
                    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
                    let r = routing.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(v) = r.get_param(&ch.key, param) {
                        let (lo, hi, step) = step_for(param, shift);
                        let sign = if forward { 1.0 } else { -1.0 };
                        r.set_param(&ch.key, param, (v + sign * step).clamp(lo, hi));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Each key highlighted (bold, set apart from the rest), what it does dim —
/// the same convention odj-player's and odj-sampler's own footers use.
fn hints_line(parts: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, d)) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(k.to_string(), Style::new().fg(Color::Gray).bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(d.to_string(), Style::new().fg(DIM)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

#[allow(clippy::too_many_arguments)]
fn draw_mixer(
    f: &mut Frame,
    channels: &[Channel],
    selected_channel: usize,
    selected_control: usize,
    current_cc: Option<u8>,
    routing: &Routing,
    status: Option<&str>,
    record_mode_capturing: bool,
) {
    let [main_area, hint_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());

    if channels.is_empty() {
        let msg = Line::from(vec![
            Span::styled("(no channels — start a deck or odj-sampler, or press ", Style::new().fg(DIM)),
            Span::styled(" ` ", Style::new().fg(Color::Black).bg(AMBER)),
            Span::styled(" to add an input)", Style::new().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(msg).alignment(Alignment::Center), main_area);
    } else {
        let constraints: Vec<Constraint> = channels.iter().map(|_| Constraint::Length(STRIP_WIDTH)).collect();
        let strips = Layout::horizontal(constraints).split(main_area);
        for (i, (ch, strip_area)) in channels.iter().zip(strips.iter()).enumerate() {
            let control = (i == selected_channel).then_some(selected_control);
            draw_channel(f, *strip_area, ch, i == selected_channel, control, routing);
        }
    }

    // Contextual keys left, globals pinned right — mirrors odj-player's
    // `draw_footer`, the formatting (and naming: "Settings"/"quit",
    // not "Preferences"/"exit") every app's footer is meant to follow;
    // see its own doc comment for the convention itself.
    let mut left = hints_line(&[("←→ ↑↓", "move"), ("+/-", "adjust (Shift ×4)")]);
    if let Some(cc) = current_cc {
        left.push_span(Span::styled(format!("  (CC {cc})"), Style::new().fg(DIM)));
    }
    // A record-mode capture can be running even while looking at this
    // screen (nothing here pauses it) — same red "still recording"
    // badge the old single-file `Recorder` showed, just driven by
    // record mode's own state now.
    let rec_badge = record_mode_capturing
        .then(|| Line::from(Span::styled(" ● REC ", Style::new().fg(Color::Black).bg(Color::Red).bold())));
    let rec_w = rec_badge.as_ref().map_or(0, |l| l.width() as u16);
    let right = hints_line(&[
        ("R", "multi-track recorder"),
        ("`", "settings"),
        ("?", "help"),
        ("Ctrl+C", "quit"),
    ]);
    let rw = right.width() as u16;
    let [left_area, rec_area, _, right_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(rec_w),
        Constraint::Length(2),
        Constraint::Length(rw),
    ])
    .areas(hint_area);
    f.render_widget(Paragraph::new(left), left_area);
    if let Some(badge) = rec_badge {
        f.render_widget(Paragraph::new(badge), rec_area);
    }
    f.render_widget(Paragraph::new(right), right_area);
    draw_status_popup(f, status);
}

fn draw_channel(f: &mut Frame, area: Rect, ch: &Channel, selected: bool, active_control: Option<usize>, routing: &Routing) {
    let name_color = if !ch.available { Color::Red } else if selected { AMBER } else { DIM };
    // -2 for the block's own corners, another -2 for the padding space
    // this adds around the title below — otherwise ratatui clips
    // whatever's left over on its own, re-cropping exactly the part
    // `truncate` just went out of its way to keep.
    let title = truncate(&ch.label, area.width.saturating_sub(4) as usize);
    let block = Block::bordered().title(format!(" {title} ")).border_style(Style::new().fg(name_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // A narrow VU meter alongside the fader, its own column rather
    // than folded into the fader's own bar — the two mean different
    // things (where the volume control is set vs. how loud the signal
    // actually, dynamically, is right now), so conflating them would
    // make a hot signal on a low fader look deceptively quiet.
    let [fader_area, meter_area] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(3)]).areas(inner);

    let row_style = |i: usize| {
        if active_control == Some(i) { Style::new().fg(Color::Black).bg(AMBER) } else { Style::new().fg(Color::White) }
    };
    let eq_line = |label: &str, value: Option<f32>, i: usize| {
        let text = match value {
            Some(v) => format!("{label} {v:+.0}"),
            None => format!("{label} —"),
        };
        Line::from(Span::styled(text, row_style(i)))
    };

    let mut lines = vec![
        eq_line("Hi", routing.get_param(&ch.key, Param::High), 0),
        eq_line("Mi", routing.get_param(&ch.key, Param::Mid), 1),
        eq_line("Lo", routing.get_param(&ch.key, Param::Low), 2),
        Line::from(""),
    ];
    // How many of `lines`' rows are header rows, not yet the fader/meter
    // itself — both columns start with this many blank rows, so their
    // own bars line up row for row even though the fader's own column
    // also carries the EQ readouts above it and the meter's doesn't.
    let header_rows = lines.len();

    // The Volume fader: filled bottom-up over whatever vertical room is
    // left, like a real channel strip's — the dominant visual element.
    let volume = routing.get_param(&ch.key, Param::Volume);
    let fader_rows = inner.height.saturating_sub(lines.len() as u16 + 1).max(1);
    let fraction = volume.unwrap_or(0.0).clamp(0.0, 1.0);
    let filled = ((fader_rows as f32) * fraction).round() as u16;
    for row in 0..fader_rows {
        let is_filled = row >= fader_rows.saturating_sub(filled);
        let bar = if is_filled { "▓▓▓▓▓" } else { "░░░░░" };
        let color = if active_control == Some(3) { AMBER } else if volume.is_some() { Color::White } else { DIM };
        lines.push(Line::from(Span::styled(bar, Style::new().fg(color))).alignment(Alignment::Center));
    }
    let vol_label = volume.map_or("—".to_string(), |v| format!("{:.0}%", v * 100.0));
    lines.push(Line::from(Span::styled(vol_label, row_style(3))).alignment(Alignment::Center));

    f.render_widget(Paragraph::new(lines), fader_area);

    // The meter itself: same row count and fader-bottom-up fill logic
    // as the fader's own bar, just driven by `routing.input_level`
    // (a live, decaying signal strength) instead of the fader's
    // static `volume` — and colored in zones (green/yellow/red,
    // bottom to top) rather than a single color, same convention a
    // real VU meter's colored scale uses, so a hot signal reads as hot
    // at a glance rather than needing the exact number read off it.
    let mut meter_lines: Vec<Line> = (0..header_rows).map(|_| Line::from("")).collect();
    let level = routing.input_level(&ch.key).clamp(0.0, 1.0);
    let meter_filled = ((fader_rows as f32) * level).round() as u16;
    for row in 0..fader_rows {
        let is_filled = row >= fader_rows.saturating_sub(meter_filled);
        let bar = if is_filled { "▓▓" } else { "░░" };
        let zone =
            if row < fader_rows / 10 { Color::Red } else if row < fader_rows * 3 / 10 { Color::Yellow } else { Color::Green };
        let color = if is_filled { zone } else { DIM };
        meter_lines.push(Line::from(Span::styled(bar, Style::new().fg(color))).alignment(Alignment::Center));
    }
    meter_lines.push(Line::from(""));
    f.render_widget(Paragraph::new(meter_lines), meter_area);
}

/// The record-mode screen's shared waveform timeline is always this
/// many seconds wide. While the playhead is still inside it (`now_secs
/// <= RECORD_WINDOW_SECS`), the window sits fixed at `0..
/// RECORD_WINDOW_SECS` and the playhead visibly travels across it;
/// past that, the window scrolls to keep the playhead pinned at the
/// right edge instead, same as any real multitrack editor's own
/// follow-the-playhead view — continuous motion (the waveform sliding
/// left under a fixed playhead column) rather than a periodic jump a
/// rescale-to-fit approach would cause at every window boundary.
const RECORD_WINDOW_SECS: f64 = 30.0;
/// One track row's total height, borders included: 1 info line (state
/// + assigned input) plus [`RECORD_WAVE_ROWS`] of waveform.
const RECORD_ROW_HEIGHT: u16 = 2 + 1 + RECORD_WAVE_ROWS;
const RECORD_WAVE_ROWS: u16 = 2;

/// Record mode (`R` from the live screen, `M` back): 8 fixed,
/// full-width track rows ([`draw_record_track`]) instead of
/// [`draw_mixer`]'s own vertical strips — "horizontal tracks" was the
/// request, and each row already needs more label room (its assigned
/// input's name) than a 14-column strip affords. Every row shares one
/// scrolling [`RECORD_WINDOW_SECS`]-wide timeline (see its own doc
/// comment) so the same playhead column lines up across all 8 — see
/// [`draw_record_track`] for how each row's own waveform ([`RecordWave`])
/// is drawn against it. `picker`, once `Enter` drills into the
/// selected row, swaps the body for the list of currently-routed
/// inputs to assign instead — same whole-body-swap shape
/// [`draw_settings`]'s own `drilled` uses, not a popup over the tracks
/// (there's no existing "overlay on top of another live screen"
/// convention in this app to borrow instead).
#[allow(clippy::too_many_arguments)]
fn draw_record_mode(
    f: &mut Frame,
    tracks: &[Track; TRACKS],
    selected: usize,
    muted: &[bool; TRACKS],
    elapsed: Duration,
    rate: u32,
    running: bool,
    capturing: bool,
    labels: &HashMap<DeviceKey, String>,
    picker: Option<(&[DeviceKey], usize)>,
    status: Option<&str>,
) {
    let [main_area, hint_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    let [status_area, tracks_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(main_area);
    let now_secs = elapsed.as_secs_f64();
    let window_start_secs = (now_secs - RECORD_WINDOW_SECS).max(0.0);

    draw_record_status(f, status_area, elapsed, running, capturing);
    if let Some((items, picker_selected)) = picker {
        draw_record_picker(f, tracks_area, items, picker_selected, labels, selected);
    } else {
        let rows = Layout::vertical([Constraint::Length(RECORD_ROW_HEIGHT); TRACKS]).split(tracks_area);
        for (i, (track, area)) in tracks.iter().zip(rows.iter()).enumerate() {
            draw_record_track(f, *area, i, track, i == selected, muted[i], labels, window_start_secs, now_secs, rate);
        }
    }

    let transport = if picker.is_some() {
        hints_line(&[("↑↓", "select"), ("Enter", "assign"), ("X", "clear"), ("Esc", "back")])
    } else {
        hints_line(&[
            ("Space", if running { "pause" } else { "play" }),
            ("R", "record"),
            ("X", "stop / rewind"),
            ("↑↓", "select"),
            ("←→", "scrub"),
            ("S", "state"),
            ("Enter", "assign input"),
            ("B", "bounce to disk"),
        ])
    };
    let right = hints_line(&[("M", "mixer"), ("?", "help"), ("Ctrl+C", "quit")]);
    let rw = right.width() as u16;
    let [left_area, right_area] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(rw)]).areas(hint_area);
    f.render_widget(Paragraph::new(transport), left_area);
    f.render_widget(Paragraph::new(right), right_area);
    draw_status_popup(f, status);
}

/// Same orange odj-player's own play-state row uses for "paused"
/// (`CUE_COLOR`) — kept as its own exact RGB value rather than
/// reusing this app's own, slightly different `AMBER`, since the
/// point is to visually match that row specifically.
const RECORD_PAUSE_COLOR: Color = Color::Rgb(255, 140, 0);

/// A dedicated row above the 8 tracks, mirroring odj-player's own
/// play-state + time status line (`status_groups` in its `ui.rs`):
/// a colored badge — black, bold text on a colored background, same
/// convention as every on/off badge in either app — for whichever of
/// capturing/running/stopped currently applies, then the shared
/// playhead's own elapsed time, dim label plus bold value. No
/// remaining-time or blink-on-low-time variant: unlike a loaded
/// track, record mode's own timeline has no fixed end to count down
/// to.
fn draw_record_status(f: &mut Frame, area: Rect, elapsed: Duration, running: bool, capturing: bool) {
    let (state, color) = if capturing {
        ("● REC  ", Color::Red)
    } else if running {
        ("▶ PLAY ", Color::Green)
    } else {
        ("‖ PAUSE", RECORD_PAUSE_COLOR)
    };
    let s = elapsed.as_secs();
    let line = Line::from(vec![
        Span::styled(format!(" {state} "), Style::new().fg(Color::Black).bg(color).bold()),
        Span::raw(" "),
        Span::styled("TIME ", Style::new().fg(DIM)),
        Span::styled(format!("{:02}:{:02}", s / 60, s % 60), Style::new().bold().fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// One full-width track row: an info line (state glyph, assigned
/// input's label) over [`RECORD_WAVE_ROWS`] of its own waveform
/// ([`RecordWave`]), drawn against the screen's shared timeline so
/// every row's playhead column lines up.
#[allow(clippy::too_many_arguments)]
fn draw_record_track(
    f: &mut Frame,
    area: Rect,
    index: usize,
    track: &Track,
    selected: bool,
    muted: bool,
    labels: &HashMap<DeviceKey, String>,
    window_start_secs: f64,
    now_secs: f64,
    rate: u32,
) {
    let border_color = if selected { AMBER } else { DIM };
    let block = Block::bordered().title(format!(" Track {} ", index + 1)).border_style(Style::new().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [info_area, wave_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let (state_label, state_color) = match track.state {
        TrackState::Mute => ("MUTE", DIM),
        TrackState::Record => ("REC", Color::Red),
        TrackState::Play => ("PLAY", Color::Green),
    };
    let input_label = track.input.as_ref().map(|k| labels.get(k).cloned().unwrap_or_else(|| k.0.clone()));
    let mut spans = vec![
        Span::styled(format!("{state_label:<5}"), Style::new().fg(state_color).bold()),
        Span::raw("  "),
    ];
    match &input_label {
        Some(l) => spans.push(Span::styled(l.clone(), Style::new().fg(Color::White))),
        None => spans.push(Span::styled("— unassigned —", Style::new().fg(DIM))),
    }
    // Confirms, right where you're looking, that this track's own
    // state is actually doing something to the live mix — not just a
    // label. Only ever shown for a track with an input assigned; a
    // Record-armed one is never muted (see `record_mode.rs`'s own
    // `apply_mute`), so this never shows alongside "REC".
    if muted {
        spans.push(Span::styled(" 🔇 live muted", Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), info_area);

    f.render_widget(
        RecordWave {
            wave: &track.wave,
            scale: track.wave_scale,
            start_secs: track.wave_start_secs,
            capturing: track.is_capturing(),
            capture_start_secs: track.start_offset as f64 / rate.max(1) as f64,
            window_start_secs,
            now_secs,
        },
        wave_area,
    );
}

/// One track row's own horizontal waveform, over the record-mode
/// screen's shared, scrolling [`RECORD_WINDOW_SECS`]-wide timeline
/// (`window_start_secs..window_start_secs + RECORD_WINDOW_SECS`) — a
/// single-sided, bottom-up cousin of odj-player's own zoomed `Wave`
/// widget (same `BARS` sub-cell resolution and color palette),
/// simplified for a 2-row-tall strip rather than a dedicated editor
/// pane: no mirroring, no bands, just peak level. The playhead column
/// (`now_secs`) is highlighted the same way regardless of whether
/// this particular track has anything recorded there yet, so it reads
/// as one line moving through all 8 rows together, not 8 independent
/// ones. A track mid-capture shows a plain solid block from its own
/// current pass's start to `now` instead of (stale,
/// about-to-be-overwritten) cached wave data.
struct RecordWave<'a> {
    wave: &'a [odj_core::track::WaveBin],
    scale: f32,
    start_secs: f64,
    capturing: bool,
    capture_start_secs: f64,
    window_start_secs: f64,
    now_secs: f64,
}

impl Widget for RecordWave<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        const WAVE_BG: Color = Color::Rgb(14, 14, 18);
        const PLAYHEAD_BG: Color = Color::Rgb(210, 40, 40);
        const WAVE_FG: Color = Color::Rgb(225, 230, 240);

        if area.width == 0 || area.height == 0 {
            return;
        }
        let (w, h) = (area.width as usize, area.height as usize);
        let per_col = RECORD_WINDOW_SECS / w as f64;
        let depth = (h * 8) as f32;
        let now_col = (((self.now_secs - self.window_start_secs) / per_col).floor() as i64).clamp(0, w as i64 - 1) as u16;

        for x in 0..w as u16 {
            let t0 = self.window_start_secs + x as f64 * per_col;
            let t1 = t0 + per_col;
            let fg = if self.capturing { Color::Red } else { WAVE_FG };
            let filled_eighths = if self.capturing {
                if t1 > self.capture_start_secs && t0 < self.now_secs { depth as usize } else { 0 }
            } else if self.wave.is_empty() {
                0
            } else {
                let rel1 = t1 - self.start_secs;
                if rel1 <= 0.0 {
                    0
                } else {
                    let rel0 = (t0 - self.start_secs).max(0.0);
                    let b0 = (rel0 * odj_core::track::WAVE_RATE) as usize;
                    if b0 >= self.wave.len() {
                        0
                    } else {
                        let b1 = ((rel1 * odj_core::track::WAVE_RATE).ceil() as usize).clamp(b0 + 1, self.wave.len());
                        let peak = self.wave[b0..b1].iter().fold(0.0f32, |m, b| m.max(b.peak));
                        ((peak / self.scale.max(1e-6)).min(1.0) * depth).round() as usize
                    }
                }
            };
            let bg = if x == now_col { PLAYHEAD_BG } else { WAVE_BG };
            for row in 0..h as u16 {
                let Some(cell) = buf.cell_mut((area.x + x, area.y + row)) else { continue };
                let from_bottom = (h as u16 - 1 - row) as usize;
                let level = filled_eighths.saturating_sub(from_bottom * 8).min(8);
                cell.set_char(BARS[level]).set_fg(fg).set_bg(bg);
            }
        }
    }
}

/// The input-assignment picker a selected track's own `Enter` drills
/// into — every currently-routed input/deck, same source list (and
/// label lookup) the live screen itself would show, since those are
/// the only ones with any audio actually flowing to tap.
fn draw_record_picker(f: &mut Frame, area: Rect, items: &[DeviceKey], selected: usize, labels: &HashMap<DeviceKey, String>, track: usize) {
    let block = Block::bordered()
        .title(format!(" Track {} — assign input ", track + 1))
        .border_style(Style::new().fg(AMBER));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list_items: Vec<ListItem> = if items.is_empty() {
        vec![ListItem::new("(nothing routed yet — add a channel from the live screen first)").fg(DIM)]
    } else {
        items.iter().map(|k| ListItem::new(labels.get(k).cloned().unwrap_or_else(|| k.0.clone()))).collect()
    };
    let list = List::new(list_items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, inner, &mut state);
}

/// Settings (`` ` ``): [`SettingsTab::Inputs`] is one list of every
/// detected input and deck (`channel_rows`/`channel_selected`), or,
/// once `Enter` drills into one of its rows, `drilled`'s own 4
/// [`ParamRow`]s instead — `Tab`/`BackTab` can't reach another tab
/// while drilled in, same as it can't close Settings itself from
/// there either, so `tab` doesn't affect this branch at all.
/// [`SettingsTab::Outputs`] is every detected output
/// (`out_entries`/`output_selected`), `main_output` marking which one,
/// if any, is currently picked. [`SettingsTab::Midi`] is every known
/// or present MIDI controller (`controller_rows`/`midi_selected`),
/// `controller` marking which one, if any, is currently connected.
/// [`SettingsTab::Buffer`] is every option in
/// [`BUFFER_FRAME_OPTIONS`] (`buffer_selected` is just the cursor;
/// `buffer_frames` — `routing::Routing::buffer_frames`'s own current
/// value — marks which one's actually applied, same relationship
/// `output_selected`/`main_output` already have), `buffer_rate` only
/// used to turn each option's frame count into an estimated `ms` for
/// display. Whichever one's showing, a tab row atop the list shows
/// which of the 4 is current — styled like the help overlay's own
/// tabs (see
/// `odj_core::tui::tab_span`), not shown at all while drilled in. See
/// `run`'s own doc comment for what the keys do either way. Its own
/// hint line spells out every key already, so — like odj-player's own
/// Settings screen — there's no separate help here, and no globals
/// (`?`/`Ctrl+C`) to pin right either.
#[allow(clippy::too_many_arguments)]
fn draw_settings(
    f: &mut Frame,
    tab: SettingsTab,
    channel_rows: &[ChannelRow],
    channel_selected: usize,
    drilled: Option<(&str, &[ParamRow])>,
    param_selected: usize,
    out_entries: &[Entry],
    output_selected: usize,
    main_output: Option<&DeviceKey>,
    controller_rows: &[ControllerRow],
    midi_selected: usize,
    controller: Option<&str>,
    buffer_selected: usize,
    buffer_frames: u32,
    buffer_rate: u32,
    status: Option<&str>,
) {
    let area = centered(f.area(), 70, 70);
    f.render_widget(Clear, area);
    // Esc left and Tab right, both badge-highlighted, same convention
    // (and the exact same 2 colors: `AMBER` for Esc, `PAGE_HINT` for
    // Tab) the help overlay's own bottom border uses — see
    // `draw_help`'s own `hints`, `Esc`/"Close" left, `Tab`/"Next page"
    // right. The middle, plain-text hint is everything specific to
    // the current tab (or to the drill-in), which `draw_help` has no
    // equivalent of since a help page is read-only.
    let middle = match (tab, &drilled) {
        (_, Some(_)) => " Enter map  X clear ",
        (SettingsTab::Inputs, None) => " Space toggle  Enter settings ",
        (SettingsTab::Outputs, None) => " Enter use ",
        (SettingsTab::Midi, None) => " Enter use  X forget mappings ",
        (SettingsTab::Buffer, None) => " Enter apply ",
    };
    let esc_action = if drilled.is_some() { "Back" } else { "Close" };
    let title = match &drilled {
        Some((label, _)) => format!(" Settings — Inputs — {label} "),
        None => " Settings ".to_string(),
    };
    let mut block = Block::bordered()
        .title(title)
        .title_bottom(key_hint("Esc", esc_action, AMBER).left_aligned())
        .title_bottom(Line::styled(middle, Style::new().fg(DIM)).centered())
        .border_style(Style::new().fg(AMBER));
    // `Tab`/`BackTab` can't reach another tab while drilled in (see
    // `run`'s own doc comment), so this is left off the border
    // entirely then rather than advertising a key that wouldn't do
    // anything.
    if drilled.is_none() {
        block = block.title_bottom(key_hint("Tab", "Next tab", PAGE_HINT).right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some((_, rows)) = drilled {
        let items: Vec<ListItem> =
            rows.iter().map(|r| ListItem::new(format!("{:<10}{}", param_label(r.param), r.cc.map_or("—".to_string(), |c| format!("CC {c}"))))).collect();
        let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
        let mut state = ListState::default().with_selected(Some(param_selected));
        f.render_stateful_widget(list, inner, &mut state);
        draw_status_popup(f, status);
        return;
    }

    // A tab row, styled like the help overlay's own tabs (see
    // `tab_span`) — black on amber for whichever's current, plain
    // amber otherwise — so it's visually the same "which one's
    // selected" convention everywhere it's used.
    let [tabs_area, _, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    let tabs = Line::from(vec![
        tab_span("Inputs", tab == SettingsTab::Inputs),
        Span::raw(" "),
        tab_span("Outputs", tab == SettingsTab::Outputs),
        Span::raw(" "),
        tab_span("MIDI", tab == SettingsTab::Midi),
        Span::raw(" "),
        tab_span("Buffer", tab == SettingsTab::Buffer),
    ]);
    f.render_widget(Paragraph::new(tabs), tabs_area);

    match tab {
        SettingsTab::Inputs => draw_settings_inputs(f, body, channel_rows, channel_selected),
        SettingsTab::Outputs => draw_settings_outputs(f, body, out_entries, output_selected, main_output),
        SettingsTab::Midi => draw_settings_midi(f, body, controller_rows, midi_selected, controller),
        SettingsTab::Buffer => draw_settings_buffer(f, body, buffer_selected, buffer_frames, buffer_rate),
    }
    draw_status_popup(f, status);
}

fn draw_settings_inputs(f: &mut Frame, inner: Rect, channel_rows: &[ChannelRow], channel_selected: usize) {
    let items: Vec<ListItem> = if channel_rows.is_empty() {
        vec![ListItem::new("(no inputs or decks detected yet)").fg(DIM)]
    } else {
        // The highlight symbol only reserves its own width on the
        // selected row, but `List` pads every row to match, so this is
        // the real width every row actually has to work with.
        let width = inner.width.saturating_sub(2) as usize;
        channel_rows
            .iter()
            .map(|r| {
                // A deck's indicator is a different shape, not just a
                // different color, since — unlike an input's — it's
                // not something `Space` can actually change here; a
                // row that's `!present` gets its own 3rd indicator for
                // the same reason — it's just as uninteractive, but
                // for not currently existing rather than for being a
                // deck, so it shouldn't look like either of those.
                let (icon, style) = match (r.present, r.group, r.added) {
                    (false, ..) => ("✕", DIM),
                    (true, Group::Input, true) => ("●", AMBER),
                    (true, Group::Input, false) => ("○", DIM),
                    (true, Group::Deck, true) => ("◆", AMBER),
                    (true, Group::Deck, false) => ("◇", DIM),
                };
                let style = Style::new().fg(style);
                let pad = width.saturating_sub(r.label.chars().count() + 1).max(1);
                let line1 = Line::from(vec![Span::raw(r.label.clone()), Span::raw(" ".repeat(pad)), Span::styled(icon, style)]);
                let line2 = Line::styled(format!("  {}", midi_summary(&r.midi)), Style::new().fg(DIM));
                ListItem::new(vec![line1, line2])
            })
            .collect()
    };
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(channel_selected));
    f.render_stateful_widget(list, inner, &mut state);
}

/// `main_output`'s own row gets `●` (AMBER); every other one gets `○`
/// (DIM, or red if `!available` — same as a hardware input row's name
/// turns red on the live screen for the same reason: it's currently
/// unusable). 1 line per row: unlike an input or deck, an output has
/// no MIDI mapping of its own to show a 2nd line for.
fn draw_settings_outputs(f: &mut Frame, inner: Rect, out_entries: &[Entry], output_selected: usize, main_output: Option<&DeviceKey>) {
    let items: Vec<ListItem> = if out_entries.is_empty() {
        vec![ListItem::new("(no outputs detected yet)").fg(DIM)]
    } else {
        let width = inner.width.saturating_sub(2) as usize;
        out_entries
            .iter()
            .map(|o| {
                let is_main = main_output == Some(&o.key);
                let (icon, icon_color) = if is_main { ("●", AMBER) } else { ("○", DIM) };
                let label_color = if !o.available { Color::Red } else if is_main { AMBER } else { DIM };
                let pad = width.saturating_sub(o.label.chars().count() + 1).max(1);
                ListItem::new(Line::from(vec![
                    Span::styled(o.label.clone(), Style::new().fg(label_color)),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(icon, Style::new().fg(icon_color)),
                ]))
            })
            .collect()
    };
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(output_selected));
    f.render_stateful_widget(list, inner, &mut state);
}

/// 3 icons, same convention [`draw_settings_inputs`] uses for its own
/// `!present` rows (a distinct shape, not just a color, for "exists
/// only because of a saved mapping, not currently usable"): `✕` (DIM)
/// for one that isn't plugged in right now, `●` (AMBER) for the one
/// that's actually connected, `○` (DIM) for any other one that's
/// plugged in but not the active one. Each row also names how many
/// mappings it has, right-aligned ahead of the icon — 0 for one
/// that's present but's never had anything mapped on it yet — since
/// that's the one thing about a controller [`draw_settings_outputs`]
/// has no equivalent of (an output is never "forgettable" the way a
/// controller's own mappings are).
fn draw_settings_midi(f: &mut Frame, inner: Rect, rows: &[ControllerRow], selected: usize, controller: Option<&str>) {
    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new("(no MIDI controller used yet)").fg(DIM)]
    } else {
        let width = inner.width.saturating_sub(2) as usize;
        rows.iter()
            .map(|r| {
                // Same 3-icon convention `draw_settings_inputs` uses
                // for its own `!present` rows — a distinct shape
                // (not just a color) for "exists only because of a
                // saved mapping, not currently usable", separate from
                // "present but not the active one".
                let is_current = controller == Some(r.name.as_str());
                let (icon, icon_color) = match (r.present, is_current) {
                    (false, _) => ("✕", DIM),
                    (true, true) => ("●", AMBER),
                    (true, false) => ("○", DIM),
                };
                let mappings = format!("{} mapping{}", r.mappings, if r.mappings == 1 { "" } else { "s" });
                // Name on the left, the mapping count right-aligned
                // against the icon — same idea as `draw_settings_inputs`'
                // icon padding, just with an extra right-aligned field
                // ahead of it.
                let pad = width.saturating_sub(r.name.chars().count() + mappings.chars().count() + 2).max(1);
                ListItem::new(Line::from(vec![
                    Span::raw(r.name.clone()),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(mappings, Style::new().fg(DIM)),
                    Span::raw(" "),
                    Span::styled(icon, Style::new().fg(icon_color)),
                ]))
            })
            .collect()
    };
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, inner, &mut state);
}

/// Same `●`/`○` convention [`draw_settings_outputs`] uses — `current`
/// marks the row that's actually applied, not merely under the
/// cursor (`selected`), same relationship `main_output`/
/// `output_selected` already have there. `rate` only turns each
/// option's frame count into an estimated `~ms` figure for display
/// (the main output's own negotiated rate once one's open, else a
/// 48 kHz guess) — it plays no other part in what gets applied.
fn draw_settings_buffer(f: &mut Frame, inner: Rect, selected: usize, current: u32, rate: u32) {
    let width = inner.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = BUFFER_FRAME_OPTIONS
        .iter()
        .map(|&frames| {
            let is_current = frames == current;
            let (icon, icon_color) = if is_current { ("●", AMBER) } else { ("○", DIM) };
            let label_color = if is_current { AMBER } else { DIM };
            let ms = f64::from(frames) / f64::from(rate) * 1000.0;
            let label = format!("{frames} frames (~{ms:.1} ms)");
            let pad = width.saturating_sub(label.chars().count() + 1).max(1);
            ListItem::new(Line::from(vec![
                Span::styled(label, Style::new().fg(label_color)),
                Span::raw(" ".repeat(pad)),
                Span::styled(icon, Style::new().fg(icon_color)),
            ]))
        })
        .collect();
    let list = List::new(items).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, inner, &mut state);
}

/// `centered(f.area(), 80, 20)`'s own width, but tall enough to fit
/// however many lines `status` actually wraps to at that width — an
/// I/O error (a device name plus whatever reason PipeWire gave) easily
/// ran past the fixed 20%-of-screen height this used to assume,
/// clipping the very text that mattered rather than genuinely
/// overflowing anywhere visible.
fn draw_status_popup(f: &mut Frame, status: Option<&str>) {
    let Some(s) = status else { return };
    let full = f.area();
    let width = centered(full, 80, 20).width;
    // -2 for the block's own corners, another -2 for a column of
    // padding either side, same margin `draw_channel`'s own title
    // uses.
    let lines = wrap(s, width.saturating_sub(4).max(1) as usize);
    let height = (lines.len() as u16 + 2).min(full.height);
    let area = Rect { x: full.x + (full.width - width) / 2, y: full.y + full.height.saturating_sub(height) / 2, width, height };
    f.render_widget(Clear, area);
    let warn = Block::bordered().title(" Status ").border_style(Style::new().fg(Color::Red));
    f.render_widget(Paragraph::new(lines.join("\n")).block(warn), area);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use odj_core::audio::Route;

    use super::*;

    fn entry(id: &str, label: &str) -> Entry {
        Entry { key: (id.to_string(), Route::Stereo(0, 1)), label: label.to_string(), available: true }
    }

    #[test]
    fn status_text_shows_a_fresh_message_and_hides_an_expired_one() {
        let mut status = None;
        assert_eq!(status_text(&status), None);
        set_status(&mut status, "something went wrong");
        assert_eq!(status_text(&status), Some("something went wrong"));
        // Backdate it past `STATUS_TIME` instead of actually sleeping
        // that long.
        status.as_mut().unwrap().1 -= STATUS_TIME;
        assert_eq!(status_text(&status), None, "expired on its own, not needing a key to clear it");
    }

    #[test]
    fn unmapped_targets_covers_every_decks_4_params() {
        let decks = [entry("odj-deck-1", "Deck 1")];
        let todo = unmapped_targets(&decks, &HashMap::new());
        assert_eq!(todo.len(), 4);
        for param in Param::ALL {
            let key = midi::target_key(&Target { input: decks[0].key.clone(), param });
            assert!(todo.iter().any(|(_, t)| *t == key));
        }
    }

    #[test]
    fn unmapped_targets_skips_whatever_is_already_bound() {
        let decks = [entry("odj-deck-1", "Deck 1")];
        let bound = midi::target_key(&Target { input: decks[0].key.clone(), param: Param::Volume });
        let already = HashMap::from([(bound.clone(), 2u8)]);
        let todo = unmapped_targets(&decks, &already);
        assert_eq!(todo.len(), 3, "Low/Mid/High still unmapped");
        assert!(!todo.iter().any(|(_, t)| *t == bound));
    }

    #[test]
    fn unmapped_targets_is_empty_when_everything_is_already_bound() {
        assert!(unmapped_targets(&[], &HashMap::new()).is_empty());
    }

    #[test]
    fn build_channels_lists_decks_then_inputs() {
        let inputs = [entry("mic", "Mic")];
        let decks = [entry("odj-deck-1", "Deck 1")];
        let channels = build_channels(&inputs, &decks);
        assert_eq!(channels.len(), 2);
        assert!(channels[0].group == Group::Deck);
        assert!(channels[1].group == Group::Input);
    }

    #[test]
    fn visible_channels_always_includes_decks_but_only_added_inputs() {
        let inputs = [entry("mic", "Mic")];
        let decks = [entry("odj-deck-1", "Deck 1")];
        let channels = build_channels(&inputs, &decks);
        let routing = Routing::default();

        let visible = visible_channels(&channels, &routing);
        assert_eq!(visible.len(), 1, "the un-added input is hidden, the deck isn't");
        assert_eq!(visible[0].group, Group::Deck);
    }

    #[test]
    fn split_label_separates_the_interface_from_whats_actually_different() {
        assert_eq!(split_label("UMC404HD 192k · In 2 (mono)"), ("UMC404HD 192k", "In 2 (mono)"));
        assert_eq!(split_label("UMC404HD 192k · stereo"), ("UMC404HD 192k", "stereo"));
        assert_eq!(split_label("Deck 1"), ("Deck 1", ""), "no separator: a deck's own short label");
    }

    #[test]
    fn shorten_suffix_drops_the_redundant_mono_stereo_qualifier() {
        assert_eq!(shorten_suffix("In 2 (mono)"), "In 2");
        assert_eq!(shorten_suffix("In 3–4 (stereo)"), "In 3–4");
        assert_eq!(shorten_suffix("stereo"), "Stereo");
        assert_eq!(shorten_suffix("left only (mono)"), "Left");
        assert_eq!(shorten_suffix("right only (mono)"), "Right");
        assert_eq!(shorten_suffix("in use"), "in use", "an unavailable device's reason passes through");
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn common_prefix_len_is_0_with_fewer_than_2_labels() {
        assert_eq!(common_prefix_len(&[]), 0);
        assert_eq!(common_prefix_len(&[chars("In 1")]), 0);
    }

    #[test]
    fn common_prefix_len_stops_at_the_first_difference() {
        assert_eq!(common_prefix_len(&[chars("In 1 (mono)"), chars("In 2 (mono)")]), 3, "\"In \" matches, the digit doesn't");
        assert_eq!(common_prefix_len(&[chars("In 1"), chars("In 1")]), 4, "identical strings: the whole thing matches");
    }

    #[test]
    fn common_suffix_len_stops_short_of_the_already_claimed_prefix() {
        // Every char here is shared, so without the `prefix_len` guard
        // this would double-count past the string's own length.
        assert_eq!(common_suffix_len(&[chars("x"), chars("x")], 1), 0);
        assert_eq!(common_suffix_len(&[chars("In 1 (mono)"), chars("In 2 (mono)")], 3), 7, "\" (mono)\" minus the leading space it shares with nothing left over");
    }

    fn input_channel(id: &str, label: &str) -> Channel {
        Channel { key: entry(id, label).key, group: Group::Input, label: label.to_string(), available: true }
    }

    fn deck_channel(id: &str, label: &str) -> Channel {
        Channel { key: entry(id, label).key, group: Group::Deck, label: label.to_string(), available: true }
    }

    #[test]
    fn shorten_labels_keeps_only_what_actually_differs_on_one_interface() {
        let channels = vec![input_channel("mic1", "UMC404HD 192k · In 1 (mono)"), input_channel("mic2", "UMC404HD 192k · In 2 (mono)")];

        let short = shorten_labels(channels);
        assert_eq!(short[0].label, "1");
        assert_eq!(short[1].label, "2");
    }

    #[test]
    fn shorten_labels_keeps_the_whole_interface_name_when_thats_what_differs() {
        // Both say "In 1" — the only thing different between them is
        // which interface, so that's what's left once the identical
        // "· In 1 (mono)" tail is stripped from both.
        let channels = vec![input_channel("a", "UMC404HD 192k · In 1 (mono)"), input_channel("b", "Scarlett 2i2 · In 1 (mono)")];

        let short = shorten_labels(channels);
        assert_eq!(short[0].label, "UMC404HD 192k");
        assert_eq!(short[1].label, "Scarlett 2i2");
    }

    #[test]
    fn shorten_labels_keeps_both_interface_and_channel_once_a_3rd_entry_needs_it() {
        let channels = vec![
            input_channel("a1", "UMC404HD 192k · In 1 (mono)"),
            input_channel("a2", "UMC404HD 192k · In 2 (mono)"),
            input_channel("b1", "Scarlett 2i2 · In 1 (mono)"),
        ];

        let short = shorten_labels(channels);
        assert_eq!(short[0].label, "UMC404HD 192k · In 1");
        assert_eq!(short[1].label, "UMC404HD 192k · In 2");
        assert_eq!(short[2].label, "Scarlett 2i2 · In 1");
    }

    #[test]
    fn shorten_labels_falls_back_to_the_full_label_when_two_are_genuinely_identical() {
        // Nothing left to diff on — rather than collapse both to an
        // empty name, keep what they actually are so it's at least
        // clear they're indistinguishable (a sign of a real duplicate
        // device, not a labeling bug).
        let channels = vec![input_channel("a", "UMC404HD 192k · In 1 (mono)"), input_channel("b", "UMC404HD 192k · In 1 (mono)")];

        let short = shorten_labels(channels);
        assert_eq!(short[0].label, "UMC404HD 192k · In 1 (mono)");
        assert_eq!(short[1].label, "UMC404HD 192k · In 1 (mono)");
    }

    #[test]
    fn shorten_labels_uses_the_suffix_for_a_lone_input_with_nothing_to_diff() {
        let channels = vec![input_channel("mic", "UMC404HD 192k · In 1 (mono)")];
        let short = shorten_labels(channels);
        assert_eq!(short[0].label, "In 1");
    }

    #[test]
    fn shorten_labels_drops_a_decks_redundant_stereo_suffix() {
        // A deck's real label (built the same way a hardware input's
        // is, by `odj_core::audio::Choice::label`) always ends in this
        // exact suffix, since its virtual sink is always a plain
        // stereo pair.
        let channels = vec![deck_channel("odj-deck-1", "Deck 1 · stereo")];
        let short = shorten_labels(channels);
        assert_eq!(short[0].label, "Deck 1");
    }

    #[test]
    fn build_channel_rows_covers_inputs_and_decks_with_their_added_state() {
        let inputs = [entry("mic", "Mic")];
        let decks = [entry("odj-deck-1", "Deck 1")];
        let channels = build_channels(&inputs, &decks);
        let mut routing = Routing::default();
        routing.set_main_output(Some(entry("out", "Out").key));

        let rows = build_channel_rows(&channels, &routing, &Targets::default());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "Deck 1");
        assert_eq!(rows[0].group, Group::Deck);
        assert!(rows[0].present);
        assert_eq!(rows[1].label, "Mic");
        assert_eq!(rows[1].group, Group::Input);
        assert!(!rows[1].added, "never explicitly added");
        assert!(rows[1].present);
    }

    #[test]
    fn build_channel_rows_adds_a_row_for_a_mapped_device_thats_no_longer_detected() {
        let routing = Routing::default();
        let targets = Targets::default();
        let gone = ("missing-deck".to_string(), Route::Stereo(0, 1));
        targets.bind(5, midi::target_key(&Target { input: gone.clone(), param: Param::Volume }));

        // Not in `channels` at all — nothing currently detects it. Its
        // id doesn't match a deck's or odj-sampler's own naming, so
        // it's treated as a (now-unplugged) hardware input — see the
        // dedicated `phantom_channel` tests for that labeling itself.
        let rows = build_channel_rows(&[], &routing, &targets);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, gone);
        assert_eq!(rows[0].group, Group::Input);
        assert!(!rows[0].present);
        assert!(!rows[0].added, "an absent device is never routed to anything");
    }

    #[test]
    fn build_channel_rows_doesnt_duplicate_a_still_detected_mapped_device() {
        let decks = [entry("odj-deck-1", "Deck 1")];
        let channels = build_channels(&[], &decks);
        let routing = Routing::default();
        let targets = Targets::default();
        targets.bind(5, midi::target_key(&Target { input: channels[0].key.clone(), param: Param::Volume }));

        let rows = build_channel_rows(&channels, &routing, &targets);
        assert_eq!(rows.len(), 1, "the live row covers it already, no separate phantom row");
        assert!(rows[0].present);
    }

    #[test]
    fn phantom_channel_recognizes_a_decks_or_odj_samplers_own_sink_name() {
        assert_eq!(phantom_channel(&("odj-deck-2".to_string(), Route::Stereo(0, 1))), (Group::Deck, "Deck 2".to_string()));
        assert_eq!(
            phantom_channel(&(audio::sampler_sink_name().to_string(), Route::Stereo(0, 1))),
            (Group::Deck, "Sampler".to_string())
        );
    }

    #[test]
    fn phantom_channel_falls_back_to_the_raw_id_and_route_for_a_hardware_input() {
        let (group, label) = phantom_channel(&("hw:CARD=Built-in".to_string(), Route::Mono(2)));
        assert_eq!(group, Group::Input);
        assert_eq!(label, "hw:CARD=Built-in · 3");
    }

    #[test]
    fn build_controller_rows_includes_a_known_controller_even_when_not_present() {
        // A name nothing on the machine actually running this test could
        // coincidentally share, so this stays deterministic regardless
        // of whatever real MIDI ports happen to be connected.
        let known = HashMap::from([(
            "definitely-not-a-real-controller-xyz".to_string(),
            HashMap::from([("input:Mic:volume".to_string(), 5u8)]),
        )]);
        let rows = build_controller_rows(&known);
        let row = rows.iter().find(|r| r.name == "definitely-not-a-real-controller-xyz").expect("the known row is still listed");
        assert_eq!(row.mappings, 1);
        assert!(!row.present, "nothing real is named this");
    }

    #[test]
    fn build_channel_rows_carries_each_rows_own_4_mapped_ccs() {
        let decks = [entry("odj-deck-1", "Deck 1")];
        let channels = build_channels(&[], &decks);
        let routing = Routing::default();
        let targets = Targets::default();
        let high = midi::target_key(&Target { input: channels[0].key.clone(), param: Param::High });
        let volume = midi::target_key(&Target { input: channels[0].key.clone(), param: Param::Volume });
        targets.bind(2, high);
        targets.bind(9, volume);

        let rows = build_channel_rows(&channels, &routing, &targets);
        assert_eq!(rows[0].midi, [Some(2), None, None, Some(9)], "High/Mid/Low/Volume, in that order");
    }

    #[test]
    fn midi_summary_marks_an_unmapped_param_with_a_dash() {
        assert_eq!(midi_summary(&[Some(2), None, None, Some(9)]), "MIDI  H:2  M:—  L:—  V:9");
        assert_eq!(midi_summary(&[None, None, None, None]), "MIDI  H:—  M:—  L:—  V:—");
    }

    #[test]
    fn build_param_rows_covers_a_single_channels_4_params() {
        let key = entry("mic", "Mic").key;
        let targets = Targets::default();
        let bound = midi::target_key(&Target { input: key.clone(), param: Param::Low });
        targets.bind(5, bound.clone());

        let rows = build_param_rows(&key, &targets);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().any(|r| r.param == Param::Low && r.target == bound && r.cc == Some(5)));
        assert_eq!(rows.iter().filter(|r| r.cc.is_none()).count(), 3);
    }

    #[test]
    fn truncate_keeps_the_end_so_similar_labels_stay_distinguishable() {
        assert_eq!(truncate("UMC404HD In 2", 12), "…C404HD In 2");
        assert_eq!(truncate("UMC404HD In 3", 12), "…C404HD In 3");
        assert_eq!(truncate("short", 12), "short", "fits already, untouched");
    }

    #[test]
    fn control_param_covers_all_4_controls_in_display_order() {
        assert_eq!(control_param(0), Param::High);
        assert_eq!(control_param(1), Param::Mid);
        assert_eq!(control_param(2), Param::Low);
        assert_eq!(control_param(3), Param::Volume);
    }

    #[test]
    fn the_help_pages_cover_every_key_bound_in_run() {
        let keys: Vec<&str> = HELP
            .iter()
            .flat_map(|p| match p {
                Page::Keys(_, keys) => keys.iter().map(|(k, _)| *k).collect(),
                Page::About(_) => Vec::new(),
            })
            .collect();
        for k in [
            "← / →",
            "↑ / ↓",
            "+ / -",
            "`",
            "Space",
            "Enter",
        ] {
            assert!(keys.contains(&k), "{k}");
        }
    }

    #[test]
    fn auto_connect_ignores_a_hardware_input_thats_not_in_the_active_set() {
        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_deck = Devices::start(crate::devices::Kind::Deck);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let routing = Mutex::new(Routing::default());
        routing.lock().unwrap().set_main_output(Some(entry("missing-out", "Out").key));
        let channels = [Channel { key: entry("mic", "Mic").key, group: Group::Input, label: "Mic".into(), available: true }];
        let mut failed = HashMap::new();
        let mut status = None;
        auto_connect(&channels, &routing, &devices_in, &devices_deck, &devices_out, &HashSet::new(), &mut failed, AUTO_CONNECT_RETRY, &mut status);
        assert!(routing.lock().unwrap().output_of(&channels[0].key).is_none());
        assert!(failed.is_empty(), "never attempted, so never failed either");
    }

    #[test]
    fn auto_connect_retries_a_hardware_input_thats_in_the_active_set() {
        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_deck = Devices::start(crate::devices::Kind::Deck);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let routing = Mutex::new(Routing::default());
        routing.lock().unwrap().set_main_output(Some(entry("missing-out", "Out").key));
        let channels = [Channel { key: entry("missing-mic", "Mic").key, group: Group::Input, label: "Mic".into(), available: true }];
        let active = HashSet::from([device_key_string(&channels[0].key)]);
        let mut failed = HashMap::new();
        let mut status = None;
        // Neither device actually exists, so this can't succeed — the
        // point is that a previously-active input is attempted at all
        // (unlike one that was never added), the same as a deck is.
        auto_connect(&channels, &routing, &devices_in, &devices_deck, &devices_out, &active, &mut failed, AUTO_CONNECT_RETRY, &mut status);
        assert!(failed.contains_key(&channels[0].key));
    }

    #[test]
    fn auto_connect_is_a_no_op_without_a_main_output() {
        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_deck = Devices::start(crate::devices::Kind::Deck);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let routing = Mutex::new(Routing::default());
        let channels = [Channel { key: entry("odj-deck-1", "Deck 1").key, group: Group::Deck, label: "Deck 1".into(), available: true }];
        let mut failed = HashMap::new();
        let mut status = None;
        auto_connect(&channels, &routing, &devices_in, &devices_deck, &devices_out, &HashSet::new(), &mut failed, AUTO_CONNECT_RETRY, &mut status);
        assert!(routing.lock().unwrap().output_of(&channels[0].key).is_none());
        assert!(failed.is_empty());
    }

    #[test]
    fn auto_connect_remembers_a_deck_it_couldnt_open_so_it_isnt_retried_every_frame() {
        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_deck = Devices::start(crate::devices::Kind::Deck);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let routing = Mutex::new(Routing::default());
        routing.lock().unwrap().set_main_output(Some(entry("missing-out", "Out").key));
        let channels = [Channel { key: entry("missing-deck", "Deck 1").key, group: Group::Deck, label: "Deck 1".into(), available: true }];
        let mut failed = HashMap::new();
        let mut status = None;
        // Neither device actually exists, so this can't succeed — the
        // point is that it's remembered as failed rather than erroring
        // again next call, as long as it's retried within `retry_after`.
        auto_connect(&channels, &routing, &devices_in, &devices_deck, &devices_out, &HashSet::new(), &mut failed, AUTO_CONNECT_RETRY, &mut status);
        assert!(failed.contains_key(&channels[0].key));
        status = None;
        auto_connect(&channels, &routing, &devices_in, &devices_deck, &devices_out, &HashSet::new(), &mut failed, AUTO_CONNECT_RETRY, &mut status);
        assert!(status.is_none(), "an already-failed device isn't retried (and doesn't re-report an error) within retry_after");
    }

    #[test]
    fn auto_connect_retries_a_previously_failed_device_once_retry_after_has_passed() {
        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_deck = Devices::start(crate::devices::Kind::Deck);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let routing = Mutex::new(Routing::default());
        routing.lock().unwrap().set_main_output(Some(entry("missing-out", "Out").key));
        let channels = [Channel { key: entry("missing-deck", "Deck 1").key, group: Group::Deck, label: "Deck 1".into(), available: true }];
        let mut failed = HashMap::new();
        let mut status = None;
        auto_connect(&channels, &routing, &devices_in, &devices_deck, &devices_out, &HashSet::new(), &mut failed, AUTO_CONNECT_RETRY, &mut status);
        assert!(failed.contains_key(&channels[0].key));
        std::thread::sleep(Duration::from_millis(5));
        status = None;
        // A tiny `retry_after`, not the real `AUTO_CONNECT_RETRY` — the
        // point here is only that a device the elapsed time clears gets
        // a fresh attempt (and so a fresh, re-reported error), not that
        // it literally takes 3 real seconds in a test.
        auto_connect(&channels, &routing, &devices_in, &devices_deck, &devices_out, &HashSet::new(), &mut failed, Duration::from_millis(1), &mut status);
        assert!(status.is_some(), "retry_after elapsed, so this device gets attempted (and fails, and reports) again");
    }

    // --- Screenshots for CI's own `Screenshots` workflow (.github/workflows/screenshots.yml)
    // — same `--ignored screenshot` convention odj-player/odj-sampler/odj-crates already use,
    // just calling the free `draw_*` functions directly instead of going through an `App`
    // (odj-mixer has none): odj-mixer's own screens are plain functions over explicit
    // parameters, not a `.snapshot()`/`crate::ui::draw(f, app, &snapshot)` entry point.
    //
    // `draw_mixer`'s own channel strips only show real EQ/Volume values once a channel is
    // actually routed to a real, open output — which needs real audio hardware CI can't
    // offer. Rather than make this screenshot depend on that (like `routing.rs`'s/
    // `record_mode.rs`'s own `#[ignore = "opens a real output on this machine"]` tests,
    // which would simply fail in a GitHub Actions runner), it uses a bare `Routing::default()`
    // and leans on `Channel::available` instead (no routing needed at all) to still show two
    // different visual states.

    fn screenshot_dir() -> PathBuf {
        let dir = std::env::var_os("ODJ_SCREENSHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/screenshots")));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dump_screenshot(name: &str, cols: u16, rows: u16, draw: impl FnOnce(&mut Frame)) {
        use odj_core::tui::to_ansi;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut t = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        t.draw(draw).unwrap();
        std::fs::write(screenshot_dir().join(format!("{name}.ans")), to_ansi(t.backend().buffer())).unwrap();
    }

    /// A synthetic waveform — a plain sine-shaped envelope, not real
    /// decoded audio — just to give the screenshot's bars something
    /// other than flat silence to show.
    fn fixture_wave(duration_secs: f64, scale: f32) -> Vec<odj_core::track::WaveBin> {
        let bins = (duration_secs * odj_core::track::WAVE_RATE) as usize;
        (0..bins)
            .map(|i| {
                let t = i as f64 / odj_core::track::WAVE_RATE;
                let peak = (0.5 + 0.5 * (t * 2.0).sin()) as f32 * scale;
                odj_core::track::WaveBin { peak, low: peak * 0.6, mid: peak * 0.3, high: peak * 0.1, clipped: false }
            })
            .collect()
    }

    #[test]
    #[ignore = "writes target/screenshots/*.ans; run with `cargo test -- --ignored screenshot`"]
    fn screenshot_mixer() {
        let routing = Routing::default();
        let mut unavailable = input_channel("broken", "Audio Interface");
        unavailable.available = false;
        let channels =
            [deck_channel("odj-deck-1", "Deck 1"), deck_channel("odj-sampler", "Sampler"), input_channel("mic", "Vocal Mic"), unavailable];
        dump_screenshot("mixer", 120, 40, |f| {
            draw_mixer(f, &channels, 0, 3, Some(42), &routing, Some("Could not connect Audio Interface: device busy"), true);
        });
    }

    #[test]
    #[ignore = "writes target/screenshots/*.ans; run with `cargo test -- --ignored screenshot`"]
    fn screenshot_record_mode() {
        let deck = entry("odj-deck-1", "Deck 1").key;
        let mic = entry("mic", "Vocal Mic").key;
        let labels: HashMap<DeviceKey, String> = [(deck.clone(), "Deck 1".to_string()), (mic.clone(), "Vocal Mic".to_string())].into_iter().collect();

        let mut tracks: [Track; TRACKS] = std::array::from_fn(|_| Track::default());
        tracks[0].input = Some(deck.clone());
        tracks[0].state = TrackState::Play;
        tracks[0].wave = fixture_wave(12.0, 0.8);
        tracks[0].wave_scale = 0.8;
        tracks[0].wave_end_offset = 12 * 48_000;

        tracks[1].input = Some(mic);
        tracks[1].state = TrackState::Record;

        tracks[2].input = Some(deck);
        tracks[2].state = TrackState::Mute;
        tracks[2].wave = fixture_wave(8.0, 0.5);
        tracks[2].wave_scale = 0.5;
        tracks[2].wave_start_secs = 2.0;
        tracks[2].wave_end_offset = 10 * 48_000;

        // Only a `Record`-armed track monitors its own live input — see
        // `record_mode.rs`'s own `apply_mute`.
        let muted = [true, false, true, false, false, false, false, false];

        dump_screenshot("mixer-record-mode", 120, 40, |f| {
            draw_record_mode(f, &tracks, 0, &muted, Duration::from_secs(14), 48_000, true, true, &labels, None, Some("Recording to track 2"));
        });
    }
}
