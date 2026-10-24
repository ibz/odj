//! Owns the single main output and every input routed to it, opening
//! and tearing down capture/playback streams live as inputs appear or
//! disappear, or as the main output itself changes — every input goes
//! to the same output; there is no per-input choice.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use cpal::Stream;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Split};

use odj_core::audio::{self, Source};
use odj_core::filter::ThreeBandEq;

use crate::devices::{DeviceKey, Devices};

/// Ring buffer capacity per input, in stereo samples — generous slack
/// (~43 ms at 48 kHz) for scheduling jitter between the input and output
/// callback threads; see `odj_core::audio`'s own module doc comment on
/// why no real sample-rate conversion is needed on top of that, just
/// this much give.
const RING_STEREO_SAMPLES: usize = 4096;

/// One Mixer parameter — every routed input (a hardware source or an
/// odj-player deck's virtual sink alike) has all four.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Param {
    Volume,
    Low,
    Mid,
    High,
}

impl Param {
    pub const ALL: [Param; 4] = [Param::Volume, Param::Low, Param::Mid, Param::High];
}

/// Sums every input routed to one output's captured audio, each run
/// through its own Volume + 3-band EQ first — `odj_core::filter::
/// ThreeBandEq`, the same DSP odj-player's own Mixer used to apply
/// locally, now applied once, centrally, to everything odj-mixer routes.
pub(crate) struct MixedSource {
    inputs: HashMap<DeviceKey, MixedInput>,
    scratch: Vec<f32>,
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
}

struct MixedInput {
    cons: ringbuf::HeapCons<f32>,
    volume: f32,
    eq: ThreeBandEq,
}

impl MixedSource {
    fn new() -> Self {
        Self { inputs: HashMap::new(), scratch: Vec::new(), scratch_l: Vec::new(), scratch_r: Vec::new() }
    }

    fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    fn insert(&mut self, key: DeviceKey, cons: ringbuf::HeapCons<f32>, rate: u32) {
        self.inputs.insert(key, MixedInput { cons, volume: 1.0, eq: ThreeBandEq::new(rate) });
    }

    fn remove(&mut self, key: &DeviceKey) {
        self.inputs.remove(key);
    }

    pub(crate) fn get(&self, key: &DeviceKey, param: Param) -> f32 {
        let Some(i) = self.inputs.get(key) else {
            return if param == Param::Volume { 1.0 } else { 0.0 };
        };
        match param {
            Param::Volume => i.volume,
            Param::Low => i.eq.low_db(),
            Param::Mid => i.eq.mid_db(),
            Param::High => i.eq.high_db(),
        }
    }

    pub(crate) fn set(&mut self, key: &DeviceKey, param: Param, value: f32) {
        let Some(i) = self.inputs.get_mut(key) else { return };
        match param {
            Param::Volume => i.volume = value.clamp(0.0, 1.0),
            Param::Low => i.eq.set_low_db(value),
            Param::Mid => i.eq.set_mid_db(value),
            Param::High => i.eq.set_high_db(value),
        }
    }
}

impl Source for MixedSource {
    fn render(&mut self, out: &mut [f32], _heard_at: f64) {
        out.fill(0.0);
        let frames = out.len() / 2;
        if self.scratch.len() < out.len() {
            self.scratch.resize(out.len(), 0.0);
            self.scratch_l.resize(frames, 0.0);
            self.scratch_r.resize(frames, 0.0);
        }
        let scratch = &mut self.scratch[..out.len()];
        let (l, r) = (&mut self.scratch_l[..frames], &mut self.scratch_r[..frames]);
        for input in self.inputs.values_mut() {
            let got = input.cons.pop_slice(scratch);
            scratch[got..].fill(0.0); // underrun: silence rather than stale samples
            for (i, lr) in scratch.as_chunks::<2>().0.iter().enumerate() {
                l[i] = lr[0];
                r[i] = lr[1];
            }
            input.eq.process_stereo(l, r);
            for i in 0..frames {
                out[i * 2] += l[i] * input.volume;
                out[i * 2 + 1] += r[i] * input.volume;
            }
        }
    }
}

struct RoutedOutput {
    mixed: Arc<Mutex<MixedSource>>,
    _stream: Stream,
    errors: audio::Errors,
}

struct RoutedInput {
    output: DeviceKey,
    _stream: Stream,
}

#[derive(Default)]
pub struct Routing {
    inputs: HashMap<DeviceKey, RoutedInput>,
    outputs: HashMap<DeviceKey, RoutedOutput>,
    main_output: Option<DeviceKey>,
}

impl Routing {
    pub fn output_of(&self, input: &DeviceKey) -> Option<DeviceKey> {
        self.inputs.get(input).map(|r| r.output.clone())
    }

    /// The single output every channel is routed to by default, or
    /// `None` before one's been picked.
    pub fn main_output(&self) -> Option<DeviceKey> {
        self.main_output.clone()
    }

    /// Changes the main output, tearing down every current route first —
    /// switching outputs isn't a seamless handover between devices with
    /// their own independent clocks, so every channel just disconnects
    /// and reconnects fresh to the new one; the live screen's own
    /// auto-connect (every detected channel, on sight, once there's a
    /// main output to connect it to) puts them all right back, same as
    /// it does for a channel that's only just appeared.
    pub fn set_main_output(&mut self, output: Option<DeviceKey>) {
        if output == self.main_output {
            return;
        }
        for input in self.inputs.keys().cloned().collect::<Vec<_>>() {
            self.clear_route(&input);
        }
        self.main_output = output;
    }

    /// Every input currently routed somewhere — the live screen forgets
    /// whichever of these a fresh device scan no longer reports.
    pub fn routed_inputs(&self) -> Vec<DeviceKey> {
        self.inputs.keys().cloned().collect()
    }

    /// `input`'s current value for `param`, or `None` if it isn't routed
    /// anywhere right now (there's nothing live to read).
    pub fn get_param(&self, input: &DeviceKey, param: Param) -> Option<f32> {
        let r = self.inputs.get(input)?;
        let o = self.outputs.get(&r.output)?;
        Some(o.mixed.lock().unwrap_or_else(|e| e.into_inner()).get(input, param))
    }

    /// A no-op if `input` isn't routed anywhere right now.
    pub fn set_param(&self, input: &DeviceKey, param: Param, value: f32) {
        if let Some(r) = self.inputs.get(input)
            && let Some(o) = self.outputs.get(&r.output)
        {
            o.mixed.lock().unwrap_or_else(|e| e.into_inner()).set(input, param, value);
        }
    }

    /// How many times [`set_route`](Self::set_route) retries a failed
    /// attempt, and how long it waits in between — long enough to
    /// absorb the brief window right after a stream opens or closes
    /// elsewhere on the same output or card where PipeWire hasn't
    /// finished settling yet (confirmed empirically: opening a stream
    /// right after another one on the same node just closed can
    /// transiently fail here, even though the exact same device opens
    /// fine moments later). A genuinely unavailable device still just
    /// fails, a little slower.
    const ROUTE_RETRIES: u32 = 2;
    const ROUTE_RETRY_DELAY: Duration = Duration::from_millis(150);

    /// Routes `input` (resolved via `input_devices` — `devices::Kind::
    /// Input` for a hardware source, `Kind::Deck` for an odj-player
    /// deck's virtual sink; both are routed identically otherwise) to
    /// `output` (opening whatever streams that needs, sharing an
    /// already-open output's mix if another input already uses it), or
    /// clears it with `output: None` — tearing down `input`'s own
    /// capture stream, and `output`'s playback stream too if nothing
    /// else is routed to it any more. Retries — see
    /// [`ROUTE_RETRIES`](Self::ROUTE_RETRIES) — rather than failing
    /// outright on the first attempt.
    pub fn set_route(
        &mut self,
        input_devices: &Devices,
        devices_out: &Devices,
        input: &DeviceKey,
        output: Option<&DeviceKey>,
    ) -> Result<()> {
        let mut last_err = None;
        for attempt in 0..Self::ROUTE_RETRIES {
            match self.try_set_route(input_devices, devices_out, input, output) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 < Self::ROUTE_RETRIES {
                        std::thread::sleep(Self::ROUTE_RETRY_DELAY);
                    }
                }
            }
        }
        Err(last_err.expect("ROUTE_RETRIES >= 1, so the loop runs at least once"))
    }

    fn try_set_route(
        &mut self,
        input_devices: &Devices,
        devices_out: &Devices,
        input: &DeviceKey,
        output: Option<&DeviceKey>,
    ) -> Result<()> {
        self.clear_route(input);
        let Some(output) = output else { return Ok(()) };

        if !self.outputs.contains_key(output) {
            let (cards, choice) = devices_out.resolve(output)?;
            let host = audio::host();
            let out = audio::Output::open(&host, &cards, &choice)?;
            let mixed = Arc::new(Mutex::new(MixedSource::new()));
            let stream = out.start(mixed.clone())?;
            if let Err(e) = audio::verify_route(&host, &out) {
                drop(stream);
                return Err(e);
            }
            self.outputs.insert(output.clone(), RoutedOutput { mixed, _stream: stream, errors: out.errors.clone() });
        }

        let (cards, choice) = input_devices.resolve(input)?;
        let host = audio::host();
        let inp = audio::Input::open(&host, &cards, &choice)?;
        let (prod, cons) = HeapRb::<f32>::new(RING_STEREO_SAMPLES).split();
        let stream = inp.start(prod)?;
        if let Err(e) = audio::verify_input_route(&host, &inp) {
            drop(stream); // tear down rather than capture from the wrong device
            self.drop_output_if_unused(output);
            return Err(e);
        }
        // `inp`'s own negotiated rate: PipeWire resamples every stream it
        // carries to its own shared graph clock before this code ever
        // sees a sample (the same property that makes cross-device
        // routing safe at all — see this crate's own module docs), so
        // this and the output's rate are the same rate in practice.
        let rate = inp.sample_rate();
        // Already present from the `contains_key` check above, or just
        // inserted — either way this output is now known.
        self.outputs.get(output).expect("just ensured").mixed.lock().unwrap_or_else(|e| e.into_inner()).insert(input.clone(), cons, rate);
        self.inputs.insert(input.clone(), RoutedInput { output: output.clone(), _stream: stream });
        Ok(())
    }

    fn clear_route(&mut self, input: &DeviceKey) {
        if let Some(r) = self.inputs.remove(input) {
            if let Some(o) = self.outputs.get(&r.output) {
                o.mixed.lock().unwrap_or_else(|e| e.into_inner()).remove(input);
            }
            self.drop_output_if_unused(&r.output);
        }
    }

    fn drop_output_if_unused(&mut self, output: &DeviceKey) {
        let empty = self.outputs.get(output).is_some_and(|o| o.mixed.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        if empty {
            self.outputs.remove(output);
        }
    }

    /// Forgets `input` outright — it disappeared, so there's no
    /// `Choice` left to resolve it by; same cleanup as clearing its
    /// route.
    pub fn forget_input(&mut self, input: &DeviceKey) {
        self.clear_route(input);
    }

    /// Every active output's own reported errors (xruns aside), for the
    /// live screen's status line.
    pub fn take_errors(&self) -> Vec<String> {
        self.outputs.values().filter_map(|o| o.errors.take()).collect()
    }
}

#[cfg(test)]
mod tests {
    use ringbuf::traits::Producer;

    use odj_core::audio::Route;

    use super::*;

    const RATE: u32 = 48_000;

    fn key(name: &str) -> DeviceKey {
        (name.to_string(), Route::Stereo(0, 1))
    }

    fn input(cap: usize) -> (ringbuf::HeapProd<f32>, ringbuf::HeapCons<f32>) {
        HeapRb::<f32>::new(cap).split()
    }

    #[test]
    fn mixed_source_sums_every_input_at_unity_volume_and_flat_eq() {
        let (mut p1, c1) = input(8);
        let (mut p2, c2) = input(8);
        p1.push_slice(&[0.2, 0.3]);
        p2.push_slice(&[0.1, -0.1]);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.insert(key("B"), c2, RATE);
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert!((out[0] - 0.3).abs() < 1e-4, "{out:?}");
        assert!((out[1] - 0.2).abs() < 1e-4, "{out:?}");
    }

    #[test]
    fn mixed_source_scales_by_each_inputs_own_volume() {
        let (mut p1, c1) = input(8);
        let (mut p2, c2) = input(8);
        p1.push_slice(&[1.0, 1.0]);
        p2.push_slice(&[1.0, 1.0]);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.insert(key("B"), c2, RATE);
        mixed.set(&key("A"), Param::Volume, 0.5);
        mixed.set(&key("B"), Param::Volume, 0.0); // muted
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert!((out[0] - 0.5).abs() < 1e-4);
        assert!((out[1] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn mixed_source_silences_an_empty_or_underrun_consumer() {
        let (mut p1, c1) = input(8);
        let (_p2, c2) = input(8); // never fed: stays empty
        p1.push_slice(&[0.5]); // only one frame's worth of L — R underruns
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.insert(key("B"), c2, RATE);
        let mut out = [1.0f32; 2]; // non-zero, so a bug wouldn't hide as a false pass
        mixed.render(&mut out, 0.0);
        assert!((out[0] - 0.5).abs() < 1e-4);
        assert!(out[1].abs() < 1e-4);
    }

    #[test]
    fn set_volume_clamps_to_0_1() {
        let (_p, c1) = input(8);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.set(&key("A"), Param::Volume, 1.5);
        assert_eq!(mixed.get(&key("A"), Param::Volume), 1.0);
        mixed.set(&key("A"), Param::Volume, -0.5);
        assert_eq!(mixed.get(&key("A"), Param::Volume), 0.0);
    }

    #[test]
    fn eq_bands_are_independently_adjustable_and_default_flat() {
        let (_p, c1) = input(8);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        assert_eq!((mixed.get(&key("A"), Param::Low), mixed.get(&key("A"), Param::Mid), mixed.get(&key("A"), Param::High)), (0.0, 0.0, 0.0));
        mixed.set(&key("A"), Param::Low, 3.0);
        assert_eq!(mixed.get(&key("A"), Param::Low), 3.0);
        assert_eq!(mixed.get(&key("A"), Param::Mid), 0.0, "unrelated bands untouched");
    }

    #[test]
    fn get_of_an_unrouted_input_is_the_neutral_default() {
        let mixed = MixedSource::new();
        assert_eq!(mixed.get(&key("Gone"), Param::Volume), 1.0);
        assert_eq!(mixed.get(&key("Gone"), Param::Low), 0.0);
    }

    #[test]
    fn routing_has_no_output_for_an_input_thats_never_been_routed() {
        let routing = Routing::default();
        assert!(routing.output_of(&key("A")).is_none());
        assert!(routing.get_param(&key("A"), Param::Volume).is_none());
    }

    #[test]
    fn forgetting_an_unrouted_input_is_a_no_op() {
        let mut routing = Routing::default();
        routing.forget_input(&key("A")); // doesn't panic
        assert!(routing.output_of(&key("A")).is_none());
    }

    #[test]
    fn set_main_output_is_a_no_op_when_unchanged() {
        let mut routing = Routing::default();
        assert_eq!(routing.main_output(), None);
        routing.set_main_output(None); // same as before: no-op
        assert_eq!(routing.main_output(), None);
        routing.set_main_output(Some(key("Speakers")));
        assert_eq!(routing.main_output(), Some(key("Speakers")));
    }

    #[test]
    fn set_route_retries_before_giving_up_on_a_persistently_bad_device() {
        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let mut routing = Routing::default();
        let start = std::time::Instant::now();
        // Neither device exists, so every attempt fails the same way —
        // the point is that it still eventually reports the error,
        // having actually slept between attempts rather than either
        // giving up instantly or retrying forever.
        let result = routing.set_route(&devices_in, &devices_out, &key("missing-in"), Some(&key("missing-out")));
        assert!(result.is_err());
        assert!(start.elapsed() >= Routing::ROUTE_RETRY_DELAY, "should have slept between attempts");
    }
}
