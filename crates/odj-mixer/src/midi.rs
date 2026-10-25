//! odj-mixer's MIDI: one persistent connection, dispatching each knob's
//! Control Change to whichever target it's bound to — one Mixer
//! parameter of a routed input (a hardware source or an odj-player
//! deck's virtual sink alike) — looked up from a live, editable map
//! rather than a fixed set learned once at startup, since odj-mixer's
//! set of targets grows and shrinks as inputs and decks come and go.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Alignment;
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Paragraph};

use odj_core::tui::{AMBER, centered};

use crate::devices::{DeviceKey, device_key_string, parse_device_key};
use crate::routing::Param;

/// What a target key (see [`Targets`]) names: one Mixer parameter of a
/// routed input, by its stable [`DeviceKey`] (not its display label,
/// which can change).
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub input: DeviceKey,
    pub param: Param,
}

/// `target`'s key is `"input:<device key>:<param>"` — see
/// [`device_key_string`] for the `<device key>` part; splitting back
/// off the param in [`parse_target`] is unambiguous since that part
/// never contains `:` itself.
pub fn target_key(target: &Target) -> String {
    format!("input:{}:{}", device_key_string(&target.input), param_name(target.param))
}

fn param_name(p: Param) -> &'static str {
    match p {
        Param::Volume => "volume",
        Param::Low => "low",
        Param::Mid => "mid",
        Param::High => "high",
    }
}

/// The inverse of [`target_key`].
pub fn parse_target(s: &str) -> Option<Target> {
    let rest = s.strip_prefix("input:")?;
    let (device_part, param_str) = rest.rsplit_once(':')?;
    let param = match param_str {
        "volume" => Param::Volume,
        "low" => Param::Low,
        "mid" => Param::Mid,
        "high" => Param::High,
        _ => return None,
    };
    Some(Target { input: parse_device_key(device_part)?, param })
}

/// The live CC→target map, shared with the MIDI callback — `target` is a
/// stable string key (`"input:<name>"` or `"player:<name>:<param>"`),
/// looked up by whatever applies the change; see `ui::run`'s dispatch.
#[derive(Default)]
pub struct Targets(Mutex<HashMap<u8, String>>);

impl Targets {
    pub fn new(map: HashMap<u8, String>) -> Self {
        Self(Mutex::new(map))
    }

    fn get(&self, cc: u8) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&cc).cloned()
    }

    /// Binds `cc` to `target`, replacing whatever `target` (not `cc`) was
    /// bound to before — each target has at most one knob.
    pub fn bind(&self, cc: u8, target: String) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, t| *t != target);
        map.insert(cc, target);
    }

    /// The CC currently bound to `target`, if any — the reverse of
    /// `get`, for showing current mappings back to the user (the
    /// Settings screen).
    pub fn cc_for(&self, target: &str) -> Option<u8> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(_, t)| *t == target).map(|(&cc, _)| cc)
    }

    /// Clears whatever knob is bound to `target`, if any.
    pub fn unbind(&self, target: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, t| t != target);
    }

    /// Every distinct device at least one currently-bound knob targets
    /// — for Settings to still list a channel whose device isn't
    /// currently detected, as long as it has a mapping worth seeing
    /// (and clearing) there.
    pub fn mapped_inputs(&self) -> HashSet<DeviceKey> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|t| parse_target(t))
            .map(|t| t.input)
            .collect()
    }
}

/// Set by [`learn`] while it's waiting for a knob; [`connect`]'s callback
/// sends the very next CC down it instead of dispatching that one.
pub type Learning = Mutex<Option<mpsc::Sender<u8>>>;

/// Opens `port_id`, calling `dispatch(target, value)` (`value` is
/// `0.0..=1.0`, the Control Change's absolute position) for every CC
/// bound in `targets` — except while [`learn`] is pending, when the next
/// CC is captured for it instead.
pub fn connect(
    port_id: &str,
    targets: Arc<Targets>,
    learning: Arc<Learning>,
    dispatch: impl Fn(&str, f32) + Send + 'static,
) -> Option<midir::MidiInputConnection<()>> {
    let mut input = midir::MidiInput::new("odj").ok()?;
    input.ignore(midir::Ignore::ActiveSense | midir::Ignore::Time | midir::Ignore::Sysex);
    let port = input.find_port_by_id(port_id)?;
    input
        .connect(
            &port,
            "odj-mixer-in",
            move |_stamp, message, &mut ()| {
                let &[status, cc, value, ..] = message else { return };
                if status & 0xF0 != 0xB0 {
                    return;
                }
                if let Some(tx) = learning.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(cc);
                    return;
                }
                if let Some(target) = targets.get(cc) {
                    dispatch(&target, value as f32 / 127.0);
                }
            },
            (),
        )
        .ok()
}

/// Waits for the next knob turned on this connection — Esc/Ctrl+C cancels
/// (`Ok(None)`). `label` names what's being mapped, shown while waiting.
///
/// A knob being turned sends a whole burst of CCs, not just one; only the
/// very first reaches us (`connect`'s callback hands it over via
/// `take()`, so every CC after that falls through to plain dispatch
/// instead of being offered here again). But if the caller re-arms this
/// for the *next* knob while that burst is still trailing off, the next
/// CC in it — still the same physical knob — would get offered here too
/// and wrongly captured as if it were a new one. This doesn't try to
/// filter that out itself (there's no way to tell "still the old knob"
/// from "a new one" by looking at one CC in isolation); see
/// `ui::learn_unmapped`, which calls this in a loop and rejects whatever
/// it's already captured earlier in the same walkthrough instead.
pub fn learn(terminal: &mut DefaultTerminal, learning: &Arc<Learning>, label: &str) -> Result<Option<u8>> {
    let (tx, rx) = mpsc::channel();
    *learning.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let result = loop {
        terminal.draw(|f| draw_learn_screen(f, label))?;
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && (key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            break None;
        }
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(cc) => break Some(cc),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break None,
        }
    };
    *learning.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(result)
}

fn draw_learn_screen(f: &mut Frame, label: &str) {
    draw_learn_popup(f, &format!("turn the knob for\n{label}"));
}

fn draw_learn_popup(f: &mut Frame, text: &str) {
    let area = centered(f.area(), 60, 40);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Map a MIDI knob ")
        .title_bottom(" Esc cancel ")
        .border_style(Style::new().fg(AMBER));
    let paragraph = Paragraph::new(text).alignment(Alignment::Center).style(Style::new().fg(AMBER)).block(block);
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use odj_core::audio::Route;

    use super::*;

    #[test]
    fn bind_replaces_the_same_targets_old_cc_not_the_new_ccs_old_target() {
        let targets = Targets::default();
        targets.bind(10, "input:Mic".into());
        targets.bind(11, "input:Turntable".into());
        assert_eq!(targets.get(10), Some("input:Mic".into()));
        assert_eq!(targets.get(11), Some("input:Turntable".into()));

        // Re-mapping "Mic" to a different knob drops its old CC entirely,
        // rather than leaving two knobs bound to the same target.
        targets.bind(12, "input:Mic".into());
        assert_eq!(targets.get(10), None, "the old knob no longer does anything");
        assert_eq!(targets.get(12), Some("input:Mic".into()));
        assert_eq!(targets.get(11), Some("input:Turntable".into()), "unrelated bindings survive");
    }

    #[test]
    fn cc_for_finds_the_knob_bound_to_a_target() {
        let targets = Targets::default();
        assert_eq!(targets.cc_for("input:Mic"), None);
        targets.bind(10, "input:Mic".into());
        assert_eq!(targets.cc_for("input:Mic"), Some(10));
        assert_eq!(targets.cc_for("input:Other"), None);
    }

    #[test]
    fn unbind_clears_only_the_named_targets_knob() {
        let targets = Targets::default();
        targets.bind(10, "input:Mic".into());
        targets.bind(11, "input:Turntable".into());
        targets.unbind("input:Mic");
        assert_eq!(targets.get(10), None);
        assert_eq!(targets.cc_for("input:Mic"), None);
        assert_eq!(targets.get(11), Some("input:Turntable".into()), "unrelated bindings survive");
    }

    #[test]
    fn target_keys_round_trip_through_parse_target() {
        for route in [Route::Stereo(0, 1), Route::Mono(2)] {
            for param in Param::ALL {
                let target = Target { input: ("hw:CARD=Built-in".to_string(), route), param };
                let key = target_key(&target);
                assert_eq!(parse_target(&key), Some(target));
            }
        }
    }

    #[test]
    fn parse_target_rejects_garbage() {
        assert!(parse_target("nonsense").is_none());
        assert!(parse_target("input:card#s0-1:not-a-param").is_none());
        assert!(parse_target("input:no-route-tag").is_none());
        assert!(parse_target("input:card#x9:volume").is_none());
    }
}
