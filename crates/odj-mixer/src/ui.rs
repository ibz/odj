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
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use odj_core::audio;
use odj_core::filter::{MAX_DB, MIN_DB};
use odj_core::tui::{AMBER, DIM, Help, Page, centered, draw_help, tab_span, wrap};

use crate::devices::{DeviceKey, Devices, Entry, device_key_string};
use crate::midi::{self, Learning, Target, Targets};
use crate::routing::{Param, Routing};

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

/// The help's pages (`?` or `/`, from the live screen only — Settings
/// has no help of its own) — the live screen's own keys, then
/// Settings' (for reference, since it can't be reopened from there),
/// then About.
pub const HELP: &[Page] = &[
    Page::Keys(
        "Mixer",
        &[
            ("← / →", "Move between channels"),
            ("↑ / ↓", "Move between its controls: High, Mid, Low, Volume"),
            ("+ / -", "Adjust the selected control (Shift: ×4)"),
            ("`", "Settings: pick channels, manage MIDI mappings, pick/change the main output"),
            ("Ctrl+C", "Quit"),
        ],
    ),
    Page::Keys(
        "Settings",
        &[
            ("Tab / BackTab", "Switch between the Inputs and Outputs tabs"),
            ("↑ / ↓", "Move between rows — every detected input and deck (Inputs), or every detected output (Outputs)"),
            ("Space", "Inputs: add the selected input to the mix, or remove it (a no-op on a deck's row — always connected — or on one that's mapped but not currently detected)"),
            ("Enter", "Inputs: open the selected row's own settings — for now just its 4 MIDI mappings · Outputs: make the selected one the main output"),
            ("↑ / ↓ (drilled in)", "Move between its 4 params"),
            ("Enter (drilled in)", "Map a knob to the selected param"),
            ("X / Delete (drilled in)", "Clear whatever's mapped to the selected param"),
            ("Esc (drilled in)", "Back to the channel list"),
            ("Esc / `", "Back to the live screen"),
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

/// Settings' 2 tabs — `Tab`/`BackTab` (Shift+Tab) switches between
/// them, either direction, since there's nowhere else for 2 tabs to
/// switch to. Inputs is everything [`draw_settings`] already showed
/// before this split; Outputs is new: picking the main output used to
/// be a mandatory screen before the live screen could even start (see
/// `main.rs`'s own doc comment) — now it's just another Settings tab,
/// so it can be changed mid-session too, not only chosen once upfront.
#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    Inputs,
    Outputs,
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
    status: &mut Option<String>,
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
            *status = Some(msg);
            failed.insert(ch.key.clone(), Instant::now());
        }
    }
}

/// Binds `cc` to `target` both live (`targets`) and in the shared settings
/// file, so the mapping survives a restart — same replace-the-old-knob
/// semantics as `Targets::bind` on both sides.
fn bind_knob(targets: &Targets, cc: u8, target: String) {
    targets.bind(cc, target.clone());
    let mut settings = odj_core::memory::load_settings();
    settings.mixer_midi_knobs.retain(|_, c| *c != cc);
    settings.mixer_midi_knobs.insert(target, cc);
    let _ = odj_core::memory::save_settings(&settings);
}

/// Clears whatever knob is bound to `target`, both live and in the
/// shared settings file — the inverse of [`bind_knob`].
fn unbind_knob(targets: &Targets, target: &str) {
    targets.unbind(target);
    let mut settings = odj_core::memory::load_settings();
    settings.mixer_midi_knobs.remove(target);
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
/// (`already`, from `Settings.mixer_midi_knobs`), for every one already
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
        bind_knob(targets, cc, target.clone());
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
/// in `Settings.mixer_midi_knobs` either way. Such a row has no live
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
/// hint line, see [`draw_mixer`]) opens Settings, 2 tabs
/// ([`SettingsTab`], switched either direction with `Tab`/`BackTab`).
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
/// `Esc`/`` ` `` from either tab's own list returns to the live screen.
/// Nothing on the live screen itself changes a mapping or the output
/// any more; that only happens from here. `Ctrl+C` quits — `q`/`Esc` don't, here or
/// anywhere else in odj-mixer; `Esc` is reserved for backing out of
/// whatever's open instead (Settings, help, a drill-in). `?`/`/` opens
/// [`HELP`] from the live screen, taking over the keys until
/// `Esc`/`?`/`/` closes it again — Settings has no help of its own,
/// since every key there is already spelled out in its own hint line.
#[allow(clippy::too_many_arguments)]
pub fn run(
    terminal: &mut DefaultTerminal,
    devices_in: &Devices,
    devices_deck: &Devices,
    devices_out: &Devices,
    routing: &Arc<Mutex<Routing>>,
    targets: &Arc<Targets>,
    learning: &Arc<Learning>,
    midi_connected: bool,
    mut active_inputs: HashSet<String>,
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
    let mut help = Help::default();
    let mut failed: HashMap<DeviceKey, Instant> = HashMap::new();
    // Shown via `draw_status_popup` until something replaces it or any
    // key dismisses it (see the two event-handling blocks below) —
    // otherwise an error would sit on screen forever once hit. Every
    // assignment below also calls `log::log`, so a message that's come
    // and gone before it could be read (or copied) is still in
    // `log::log_file()` afterward.
    let mut status: Option<String> = None;

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
                status = Some(e);
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
            channel_selected = channel_selected.min(channel_rows.len().saturating_sub(1));
            output_selected = output_selected.min(out_entries.len().saturating_sub(1));
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
                    status.as_deref(),
                );
            })?;
            if event::poll(Duration::from_millis(100))? {
                let Event::Key(k) = event::read()? else { continue };
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                status = None; // any key dismisses whatever's shown
                if let Some((label, rows)) = &drilled {
                    match k.code {
                        KeyCode::Esc => mapping_drill = None,
                        KeyCode::Up => param_selected = param_selected.saturating_sub(1),
                        KeyCode::Down => param_selected = (param_selected + 1).min(rows.len().saturating_sub(1)),
                        KeyCode::Enter if !rows.is_empty() => {
                            let row = &rows[param_selected];
                            if !midi_connected {
                                status = Some("No MIDI controller connected".into());
                            } else if let Some(cc) = midi::learn(terminal, learning, &format!("{label} {}", param_label(row.param)))? {
                                bind_knob(targets, cc, row.target.clone());
                            }
                        }
                        KeyCode::Char('x' | 'X') | KeyCode::Delete if !rows.is_empty() => {
                            unbind_knob(targets, &rows[param_selected].target);
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    }
                    continue;
                }
                // `Tab`/`BackTab` (Shift+Tab) switches tabs either way,
                // same as there's nowhere else for them to switch
                // between — not handled per-tab below since it's the
                // one key both tabs share.
                if matches!(k.code, KeyCode::Tab | KeyCode::BackTab) {
                    settings_tab = match settings_tab {
                        SettingsTab::Inputs => SettingsTab::Outputs,
                        SettingsTab::Outputs => SettingsTab::Inputs,
                    };
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
                                    status = Some(msg);
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
                            routing.lock().unwrap_or_else(|e| e.into_inner()).set_main_output(Some(key.clone()));
                            set_output(&key);
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                        _ => {}
                    },
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
            draw_mixer(f, &channels, selected_channel, selected_control, current_cc, &r, status.as_deref());
            if help.is_open() {
                draw_help(f, &help, HELP);
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            let Event::Key(k) = event::read()? else { continue };
            if k.kind == KeyEventKind::Release {
                continue;
            }
            status = None; // any key dismisses whatever's shown
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

fn draw_mixer(f: &mut Frame, channels: &[Channel], selected_channel: usize, selected_control: usize, current_cc: Option<u8>, routing: &Routing, status: Option<&str>) {
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
    let left = match current_cc {
        Some(cc) => Line::styled(format!(" ←→ ↑↓ move  +/- adjust (Shift ×4)  (CC {cc})"), Style::new().fg(DIM)),
        None => Line::styled(" ←→ ↑↓ move  +/- adjust (Shift ×4)", Style::new().fg(DIM)),
    };
    let right = Line::styled("` settings  ? help  Ctrl+C quit ", Style::new().fg(DIM));
    let rw = right.width() as u16;
    let [left_area, _, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Length(rw)]).areas(hint_area);
    f.render_widget(Paragraph::new(left), left_area);
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

    f.render_widget(Paragraph::new(lines), inner);
}

/// Settings (`` ` ``): [`SettingsTab::Inputs`] is one list of every
/// detected input and deck (`channel_rows`/`channel_selected`), or,
/// once `Enter` drills into one of its rows, `drilled`'s own 4
/// [`ParamRow`]s instead — `Tab`/`BackTab` can't reach
/// [`SettingsTab::Outputs`] while drilled in, same as it can't close
/// Settings itself from there either, so `tab` doesn't affect this
/// branch at all. Otherwise `SettingsTab::Outputs` is every detected
/// output (`out_entries`/`output_selected`), `main_output` marking
/// which one, if any, is currently picked. Either way, a tab row atop
/// the list shows which of the 2 is current — styled like the help
/// overlay's own tabs (see `odj_core::tui::tab_span`), not shown at
/// all while drilled in. See `run`'s own doc comment for what the keys
/// do either way. Its own hint line spells out every
/// key already, so — like odj-player's own Settings screen — there's
/// no separate help here, and no globals (`?`/`Ctrl+C`) to pin right
/// either.
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
    status: Option<&str>,
) {
    let area = centered(f.area(), 70, 70);
    f.render_widget(Clear, area);
    let hint = match (tab, &drilled) {
        (_, Some(_)) => " Enter map  X clear  Esc back ",
        (SettingsTab::Inputs, None) => " Tab outputs  Space toggle  Enter settings  Esc/` close ",
        (SettingsTab::Outputs, None) => " Tab inputs  Enter use  Esc/` close ",
    };
    let title = match &drilled {
        Some((label, _)) => format!(" Settings — Inputs — {label} "),
        None => " Settings ".to_string(),
    };
    let block = Block::bordered().title(title).title_bottom(hint).border_style(Style::new().fg(AMBER));
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
    ]);
    f.render_widget(Paragraph::new(tabs), tabs_area);

    match tab {
        SettingsTab::Inputs => draw_settings_inputs(f, body, channel_rows, channel_selected),
        SettingsTab::Outputs => draw_settings_outputs(f, body, out_entries, output_selected, main_output),
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
    use odj_core::audio::Route;

    use super::*;

    fn entry(id: &str, label: &str) -> Entry {
        Entry { key: (id.to_string(), Route::Stereo(0, 1)), label: label.to_string(), available: true }
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
            "Ctrl+C",
            "Space",
            "Enter",
            "Tab / BackTab",
            "X / Delete (drilled in)",
            "Esc (drilled in)",
            "Esc / `",
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
}
