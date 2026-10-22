//! odj-mixer: mixes sound-card inputs into one chosen output, live, with a
//! volume per input.

mod ui;

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use ratatui::DefaultTerminal;

use odj_core::audio::{self, Input, Source};
use odj_core::tui;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Split};

/// Ring buffer capacity per input, in stereo samples — generous slack
/// (~43 ms at 48 kHz) for scheduling jitter between the input and output
/// callback threads; see the module doc comment on why no real sample-rate
/// conversion is needed on top of that, just this much give.
const RING_STEREO_SAMPLES: usize = 4096;

fn main() -> Result<()> {
    if std::env::args().nth(1).is_some_and(|a| a == "-h" || a == "--help") {
        println!(
            "usage: odj-mixer\n\n\
             Lists sound-card inputs, lets you toggle any number of them on, and mixes \
             whichever are enabled into one chosen output, live, each with its own volume \
             (adjustable while it runs). Needs PipeWire: routing between two cards needs its \
             shared graph clock, which plain ALSA doesn't have."
        );
        return Ok(());
    }
    let (mut terminal, _key_release) = tui::init();
    let result = start(&mut terminal);
    tui::restore(false);
    result
}

fn start(terminal: &mut DefaultTerminal) -> Result<()> {
    let host = audio::host();
    if !audio::is_pipewire(&host) {
        bail!(
            "odj-mixer needs PipeWire running — routing between two sound cards relies on \
             its shared graph clock to keep them in sync; plain ALSA has no way to do that \
             between two independent hardware clocks."
        );
    }

    let Some(output) = tui::pick_output(terminal, &host)? else { return Ok(()) };
    let Some((input_cards, picked)) = ui::pick_inputs(terminal, &host)? else { return Ok(()) };
    if picked.is_empty() {
        return Ok(());
    }

    // Kept alive for the rest of `start`: dropping either an input or the
    // output stream tears it down.
    let mut input_streams = Vec::new();
    let mut inputs = Vec::new();
    for choice in &picked {
        let input = Input::open(&host, &input_cards, choice)?;
        let (prod, cons) = HeapRb::<f32>::new(RING_STEREO_SAMPLES).split();
        let stream = input.start(prod)?;
        if let Err(e) = audio::verify_input_route(&host, &input) {
            drop(stream); // tear down rather than capture from the wrong device
            return Err(e);
        }
        inputs.push(MixedInput { name: input.name.clone(), cons, volume: 1.0 });
        input_streams.push(stream);
    }

    let mixed = Arc::new(Mutex::new(MixedSource::new(inputs)));
    let out_stream = output.start(mixed.clone())?;
    if let Err(e) = audio::verify_route(&host, &output) {
        drop(out_stream);
        drop(input_streams);
        return Err(e);
    }

    let result = ui::run(terminal, &output.name, &mixed, &output.errors);
    drop(out_stream);
    drop(input_streams);
    result
}

/// One routed input: its name (for the live screen), the consumer end of
/// its capture ring buffer, and its own volume (0.0..=1.0, default 1.0,
/// adjustable live — see `ui::run`).
struct MixedInput {
    name: String,
    cons: ringbuf::HeapCons<f32>,
    volume: f32,
}

/// Sums every enabled input's captured audio, each scaled by its own
/// volume, into the output — the hook point for "effects later": a
/// per-input `odj_core::filter::ThreeBandEq` (already built for
/// odj-player's Mixer) would sit between that input's `pop_slice` and the
/// volume scaling below, one instance per input. Not wired up yet.
pub(crate) struct MixedSource {
    inputs: Vec<MixedInput>,
    scratch: Vec<f32>,
}

impl MixedSource {
    fn new(inputs: Vec<MixedInput>) -> Self {
        Self { inputs, scratch: Vec::new() }
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.inputs.iter().map(|i| i.name.clone()).collect()
    }

    pub(crate) fn volume(&self, i: usize) -> f32 {
        self.inputs[i].volume
    }

    pub(crate) fn set_volume(&mut self, i: usize, v: f32) {
        self.inputs[i].volume = v.clamp(0.0, 1.0);
    }
}

impl Source for MixedSource {
    fn render(&mut self, out: &mut [f32], _heard_at: f64) {
        out.fill(0.0);
        if self.scratch.len() < out.len() {
            self.scratch.resize(out.len(), 0.0);
        }
        let scratch = &mut self.scratch[..out.len()];
        for input in &mut self.inputs {
            let got = input.cons.pop_slice(scratch);
            scratch[got..].fill(0.0); // underrun: silence rather than stale samples
            for (o, s) in out.iter_mut().zip(scratch.iter()) {
                *o += *s * input.volume;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Producer;

    fn input(name: &str, cap: usize) -> (ringbuf::HeapProd<f32>, MixedInput) {
        let (prod, cons) = HeapRb::<f32>::new(cap).split();
        (prod, MixedInput { name: name.into(), cons, volume: 1.0 })
    }

    #[test]
    fn mixed_source_sums_every_input_at_unity_volume() {
        let (mut p1, i1) = input("A", 8);
        let (mut p2, i2) = input("B", 8);
        p1.push_slice(&[0.2, 0.3]);
        p2.push_slice(&[0.1, -0.1]);
        let mut mixed = MixedSource::new(vec![i1, i2]);
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert!((out[0] - 0.3).abs() < 1e-6);
        assert!((out[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn mixed_source_scales_by_each_inputs_own_volume() {
        let (mut p1, mut i1) = input("A", 8);
        let (mut p2, mut i2) = input("B", 8);
        i1.volume = 0.5;
        i2.volume = 0.0; // muted
        p1.push_slice(&[1.0, 1.0]);
        p2.push_slice(&[1.0, 1.0]);
        let mut mixed = MixedSource::new(vec![i1, i2]);
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn mixed_source_silences_an_empty_or_underrun_consumer() {
        let (mut p1, i1) = input("A", 8);
        let (_p2, i2) = input("B", 8); // never fed: stays empty
        p1.push_slice(&[0.5]); // only one frame's worth of L — R underruns
        let mut mixed = MixedSource::new(vec![i1, i2]);
        let mut out = [1.0f32; 2]; // non-zero, so a bug wouldn't hide as a false pass
        mixed.render(&mut out, 0.0);
        assert_eq!(out, [0.5, 0.0]);
    }

    #[test]
    fn set_volume_clamps_to_0_1() {
        let (_p, i1) = input("A", 8);
        let mut mixed = MixedSource::new(vec![i1]);
        mixed.set_volume(0, 1.5);
        assert_eq!(mixed.volume(0), 1.0);
        mixed.set_volume(0, -0.5);
        assert_eq!(mixed.volume(0), 0.0);
    }
}
