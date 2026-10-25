//! MIDI input port listing and picking, shared by odj-player and
//! odj-sampler — each app wires the actual messages (note pads, CC knobs)
//! itself; this just lists what's available and lets the user pick one, or
//! skip.
//!
//! A port id (e.g. an ALSA client:port number) is just a connection-order
//! number that can change across a replug, so anything an app wants to
//! remember about a *device* (not just a session) should key off its
//! stable name instead — see [`device_slot`].

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState};

use crate::tui::{AMBER, centered};

/// Available MIDI input ports as `(id, display name)` — the id is what a
/// connection and [`device_slot`] use; it stays valid across runs
/// (backend-dependent), unlike a plain index into this list.
pub fn list_ports() -> Vec<(String, String)> {
    let Ok(input) = midir::MidiInput::new("odj") else { return Vec::new() };
    input.ports().iter().filter_map(|p| input.port_name(p).ok().map(|name| (p.id(), name))).collect()
}

/// `port_id`'s stable name, with its connection-order suffix removed —
/// confirmed by reading midir's ALSA backend source, `list_ports`'s display
/// name always ends in literally " {id}" (it's appended deliberately, to
/// keep same-named devices visually distinct in a list), so a straight
/// `strip_suffix` recovers the name as the device itself reports it, with
/// nothing connection-order-dependent left in it.
pub fn stable_name(id: &str, name: &str) -> String {
    name.strip_suffix(&format!(" {id}")).unwrap_or(name).to_string()
}

/// The current port id of the first currently-connected port named
/// `name` (its own stable name, ignoring any connection-order suffix)
/// — the inverse of [`stable_name`], for restoring a remembered
/// device by name rather than its own (replug-unstable) id. `None` if
/// nothing currently connected has that name.
pub fn port_id_for(name: &str) -> Option<String> {
    list_ports().into_iter().find(|(id, n)| stable_name(id, n) == name).map(|(id, _)| id)
}

/// `port_id`'s stable name, and its slot: how many *other* currently
/// connected ports share that same stable name and sort before it. Two
/// identical controllers plugged in together get slots 0 and 1 under their
/// shared name, so each keeps its own entry in whatever an app remembers
/// per device; a single controller keeps slot 0 across replugs even though
/// its port id (and the raw display name's suffix) changes. `None` if
/// `port_id` isn't currently connected.
pub fn device_slot(port_id: &str) -> Option<(String, usize)> {
    let ports = list_ports();
    let name = stable_name(port_id, &ports.iter().find(|(id, _)| id == port_id)?.1);
    let slot = ports
        .iter()
        .take_while(|(id, _)| id != port_id)
        .filter(|(id, n)| stable_name(id, n) == name)
        .count();
    Some((name, slot))
}

/// Lists available MIDI inputs and lets the user pick one, or skip —
/// unlike the (mandatory) audio output picker, there's always a
/// reasonable "no MIDI" outcome, so this returns `Ok(None)` rather than
/// ending the program: no ports found, or the user backed out.
/// `selected_id` (a previously-saved port, if any) decides which row starts
/// highlighted.
pub fn pick_midi_input(terminal: &mut DefaultTerminal, selected_id: Option<&str>) -> Result<Option<String>> {
    let ports = list_ports();
    if ports.is_empty() {
        return Ok(None);
    }
    let mut selected = selected_id.and_then(|id| ports.iter().position(|(pid, _)| pid == id)).unwrap_or(0);
    loop {
        terminal.draw(|f| draw_midi_picker(f, &ports, selected))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(ports.len() - 1),
            KeyCode::Enter => return Ok(Some(ports[selected].0.clone())),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Esc | KeyCode::Char('q' | 'Q' | 'n' | 'N') => return Ok(None),
            _ => {}
        }
    }
}

fn draw_midi_picker(f: &mut Frame, ports: &[(String, String)], selected: usize) {
    let area = centered(f.area(), 70, 50);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" MIDI input (optional) ")
        .title_bottom(" ↑↓ select  Enter use  Esc/N skip  Ctrl+C quit ")
        .border_style(Style::new().fg(AMBER));
    let items: Vec<ListItem> = ports.iter().map(|(_, name)| ListItem::new(name.as_str())).collect();
    let list = List::new(items).block(block).highlight_style(Style::new().fg(Color::Black).bg(AMBER)).highlight_symbol("› ");
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_name_strips_only_the_trailing_connection_order_suffix() {
        assert_eq!(stable_name("24:0", "Some Pad 24:0"), "Some Pad");
        assert_eq!(stable_name("24:0", "Some Pad"), "Some Pad", "no suffix to strip");
        assert_eq!(stable_name("24:0", "24:0 Some Pad"), "24:0 Some Pad", "only a trailing suffix counts");
    }

    #[test]
    #[ignore = "needs a real ALSA sequencer; exercises list_ports against two virtual ports"]
    fn device_slot_tells_identical_names_apart() {
        use midir::os::unix::VirtualOutput;

        // Two virtual ports sharing a name — including the client name,
        // which `port_name` folds in too — standing in for two identical
        // physical controllers plugged in at once (same make and model
        // reports the same client and port name both times).
        let _a = midir::MidiOutput::new("odj-core-test-twin").unwrap().create_virtual("odj-core-test-twin").unwrap();
        let _b = midir::MidiOutput::new("odj-core-test-twin").unwrap().create_virtual("odj-core-test-twin").unwrap();

        let ports = list_ports();
        let mut twins = ports.iter().filter(|(_, name)| name.contains("odj-core-test-twin"));
        let (id_a, _) = twins.next().expect("first virtual twin visible");
        let (id_b, _) = twins.next().expect("second virtual twin visible");

        let (name_a, slot_a) = device_slot(id_a).expect("a connected port resolves");
        let (name_b, slot_b) = device_slot(id_b).expect("a connected port resolves");
        assert_eq!(name_a, name_b, "both report the same name");
        assert_ne!(slot_a, slot_b, "but each gets its own slot under that name");
    }

    #[test]
    #[ignore = "needs a real ALSA sequencer; exercises list_ports against a virtual port"]
    fn port_id_for_finds_a_connected_ports_id_by_its_stable_name() {
        use midir::os::unix::VirtualOutput;

        let _p = midir::MidiOutput::new("odj-core-test-single").unwrap().create_virtual("odj-core-test-single").unwrap();
        let (id, name) = list_ports().into_iter().find(|(_, n)| n.contains("odj-core-test-single")).expect("virtual port visible");
        let stable = stable_name(&id, &name);
        assert_eq!(port_id_for(&stable), Some(id));
        assert_eq!(port_id_for("not a real controller"), None);
    }
}
