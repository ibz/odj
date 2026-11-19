//! Owns the single main output and every input routed to it, opening
//! and tearing down capture/playback streams live as inputs appear or
//! disappear, or as the main output itself changes — every input goes
//! to the same output; there is no per-input choice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};
use cpal::Stream;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Producer, Split};

use odj_core::audio::{self, Source};
use odj_core::audio_link::{self, Listener};
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

/// How many record-mode tracks exist — a fixed number of horizontal
/// rows, not a dynamically sized list; see `record_mode.rs`.
pub(crate) const TRACKS: usize = 8;

/// Sums every input routed to one output's captured audio, each run
/// through its own Volume + 3-band EQ first — `odj_core::filter::
/// ThreeBandEq`, the same DSP odj-player's own Mixer used to apply
/// locally, now applied once, centrally, to everything odj-mixer routes.
/// Also drives record mode's own real-time plumbing: each input can
/// have its own capture tap (`MixedInput::tap`), and up to
/// [`TRACKS`] previously-recorded tracks can be mixed back in live
/// (`play_tracks`), both synced to a shared, free-running `playhead`
/// (see `record_mode.rs` for the UI-facing half of this).
pub(crate) struct MixedSource {
    inputs: HashMap<DeviceKey, MixedInput>,
    scratch: Vec<f32>,
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
    /// Frames elapsed since record mode's playhead was last at 0 —
    /// advances every render call while `running`, frozen otherwise.
    playhead: u64,
    /// Whether record mode's playhead is currently advancing at
    /// normal (1x) speed — contributes `1.0` to the render loop's own
    /// speed multiplier, added to `search` below (same additive model
    /// odj-player's own deck uses for its search-while-playing case).
    running: bool,
    /// Extra playhead speed, as a multiple of normal — `0.0` normally,
    /// `±RECORD_SEARCH`/`±RECORD_SUPER_SEARCH` while `ui::run`'s own
    /// `←`/`→` scrub hold is live (see `ui.rs`'s `apply_record_search`).
    /// Added to `running`'s own contribution, so scrubbing works
    /// whether or not the transport happens to be playing at the time
    /// — same as odj-player's own deck search.
    search: f64,
    /// A previously-recorded track's audio, decoded up front, mixed
    /// back into `out` whenever the combined speed (`running` +
    /// `search`) is nonzero — `None` for a track that's muted or not
    /// currently armed to Play.
    play_tracks: [Option<PlayTrack>; TRACKS],
}

/// A record-mode track armed to Play: its whole recording, decoded
/// once up front (same "whole file into memory" approach
/// `odj_core::track::decode` already serves odj-player's `Deck` and
/// odj-sampler's `Sampler` with — these are bounded in length, same
/// class as any other recording), and the playhead frame its first
/// frame corresponds to.
pub(crate) struct PlayTrack {
    /// Interleaved stereo.
    pub samples: Vec<f32>,
    pub start_offset: u64,
}

struct MixedInput {
    cons: ringbuf::HeapCons<f32>,
    volume: f32,
    eq: ThreeBandEq,
    /// A record-mode track's capture tap, if one's installed — this
    /// input's own post-EQ, post-volume contribution (exactly what it
    /// already adds into `out`) is pushed here too, every render call.
    tap: Option<ringbuf::HeapProd<f32>>,
    /// Suppresses this input's own contribution to the final mix,
    /// independent of its own `volume` (which stays whatever it was,
    /// unchanged, visible as-is on the live mixer screen) — record
    /// mode's own doing, while it's open, for a track that's `Mute`
    /// or `Play` rather than `Record` (see `record_mode.rs`'s own
    /// `apply_mute`/`enter`/`leave`); always `false` again the moment
    /// record mode closes, so leaving it never leaves the ordinary
    /// mixer screen silently muted by a decision made on a different
    /// screen.
    muted: bool,
}

impl MixedSource {
    fn new() -> Self {
        Self {
            inputs: HashMap::new(),
            scratch: Vec::new(),
            scratch_l: Vec::new(),
            scratch_r: Vec::new(),
            playhead: 0,
            running: false,
            search: 0.0,
            play_tracks: std::array::from_fn(|_| None),
        }
    }

    /// Whether this output has nothing left to do and can be torn
    /// down — no live input, no armed Play track, and the transport
    /// isn't running. Checking only `inputs` here would let the last
    /// hardware input being removed silently destroy an in-progress
    /// overdub session along with it.
    fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.play_tracks.iter().all(Option::is_none) && !self.running
    }

    /// Installs (`Some`) or clears (`None`) a capture tap on `key`'s
    /// own post-EQ, post-volume contribution — `false` if `key` isn't
    /// currently mixed in.
    pub(crate) fn set_input_tap(&mut self, key: &DeviceKey, tap: Option<ringbuf::HeapProd<f32>>) -> bool {
        let Some(i) = self.inputs.get_mut(key) else { return false };
        i.tap = tap;
        true
    }

    /// Suppresses (`true`) or restores (`false`) `key`'s own
    /// contribution to the final mix — `false` if `key` isn't
    /// currently mixed in.
    pub(crate) fn set_input_muted(&mut self, key: &DeviceKey, muted: bool) -> bool {
        let Some(i) = self.inputs.get_mut(key) else { return false };
        i.muted = muted;
        true
    }

    pub(crate) fn is_input_muted(&self, key: &DeviceKey) -> bool {
        self.inputs.get(key).is_some_and(|i| i.muted)
    }

    pub(crate) fn set_play_track(&mut self, slot: usize, track: Option<PlayTrack>) {
        self.play_tracks[slot] = track;
    }

    pub(crate) fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    pub(crate) fn set_search(&mut self, search: f64) {
        self.search = search;
    }

    pub(crate) fn playhead(&self) -> u64 {
        self.playhead
    }

    /// Only meaningful while `!running` — rewinding a moving playhead
    /// isn't a supported transport action (see `record_mode.rs`'s own
    /// `stop_or_rewind`).
    pub(crate) fn rewind(&mut self) {
        self.playhead = 0;
    }

    /// Jumps the playhead straight to `frames` — used once, to restore
    /// a previous session's own saved position onto a freshly opened
    /// output (see `record_mode.rs`'s own `poll`), not a transport
    /// action a key triggers.
    pub(crate) fn set_playhead(&mut self, frames: u64) {
        self.playhead = frames;
    }

    fn insert(&mut self, key: DeviceKey, cons: ringbuf::HeapCons<f32>, rate: u32) {
        self.inputs.insert(key, MixedInput { cons, volume: 1.0, eq: ThreeBandEq::new(rate), tap: None, muted: false });
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
            // `scratch` is done holding this input's raw, pre-EQ samples
            // the moment `l`/`r` are read above — safe to reuse it right
            // back below for the exact post-EQ, post-volume values a
            // capture tap should see (the next input's own `pop_slice`
            // overwrites it again before any of this is read a second
            // time).
            for i in 0..frames {
                let (ls, rs) = (l[i] * input.volume, r[i] * input.volume);
                // `muted` only ever suppresses the final mix — the
                // capture tap below still sees this input's own
                // contribution regardless, so arming a track to
                // Record still captures correctly even though record
                // mode never actually mutes a Record-armed track.
                if !input.muted {
                    out[i * 2] += ls;
                    out[i * 2 + 1] += rs;
                }
                if input.tap.is_some() {
                    scratch[i * 2] = ls;
                    scratch[i * 2 + 1] = rs;
                }
            }
            if let Some(tap) = &mut input.tap {
                tap.push_slice(scratch);
            }
        }
        // The combined speed: `running` contributes normal (1x)
        // forward speed, `search` adds a scrub on top (see this
        // struct's own doc comments) — always a whole number in
        // practice (both terms are exact multiples of 1), so every
        // per-frame read position below lands on an exact sample
        // index, no fractional interpolation needed. Gated on the
        // whole block, not just the playhead update: while this is
        // exactly 0 (stopped, not scrubbing), a Play track must stay
        // silent, not keep replaying the same frozen window of itself
        // every call.
        let speed = f64::from(u8::from(self.running)) + self.search;
        if speed != 0.0 {
            for track in self.play_tracks.iter().flatten() {
                for i in 0..frames {
                    let pos = self.playhead as i64 + (i as f64 * speed) as i64;
                    // A scrub can run the position below the track's
                    // own start (or below 0 entirely) — both are
                    // simply silence, not an error.
                    if pos >= track.start_offset as i64 {
                        let idx = (pos - track.start_offset as i64) as usize;
                        if idx < track.samples.len() / 2 {
                            out[i * 2] += track.samples[idx * 2];
                            out[i * 2 + 1] += track.samples[idx * 2 + 1];
                        }
                    }
                }
            }
            let step = (frames as f64 * speed) as i64;
            self.playhead = (self.playhead as i64 + step).max(0) as u64;
        }
    }
}

struct RoutedOutput {
    mixed: Arc<Mutex<MixedSource>>,
    _stream: Stream,
    errors: audio::Errors,
    /// This output's own negotiated sample rate — the recorder needs it
    /// for the WAV header.
    rate: u32,
}

/// Where `RoutedInput`'s captured audio actually comes from.
enum InputSource {
    /// A cpal capture stream — a real card, or a PipeWire virtual sink.
    Device { _stream: Stream },
    /// A connection accepted by odj-mixer's own `audio_link::Listener`
    /// — torn down by stopping and joining its reader thread, which
    /// itself decides (on exit) whether to give the connection back to
    /// the listener's pending pool or forget it outright; nothing
    /// further to do here.
    Socket { stop: Arc<AtomicBool>, thread: Option<JoinHandle<()>> },
}

struct RoutedInput {
    output: DeviceKey,
    source: InputSource,
}

#[derive(Default)]
pub struct Routing {
    inputs: HashMap<DeviceKey, RoutedInput>,
    outputs: HashMap<DeviceKey, RoutedOutput>,
    main_output: Option<DeviceKey>,
    /// Set once, at startup, if `audio_link::Listener::start()` bound
    /// successfully — `None` (the default every existing test already
    /// gets) just means every input is resolved the usual cpal way.
    socket_listener: Option<Arc<Listener>>,
}

impl Routing {
    pub fn set_socket_listener(&mut self, listener: Arc<Listener>) {
        self.socket_listener = Some(listener);
    }

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
    /// fine moments later). Also covers a longer-running version of the
    /// same kind of race, specific to a deck's or odj-sampler's
    /// *freshly created* virtual sink: `cpal`'s own PipeWire backend
    /// pins a capture stream to its target via `PW_KEY_TARGET_OBJECT`,
    /// set from that node's `object.serial` — reading cpal's source
    /// (`host/pipewire/device.rs`) shows that serial is only known once
    /// the node's own `info` event arrives, a separate round trip from
    /// the registry's initial "this node exists" one, so a node
    /// enumerated right after it's created can still report a
    /// stale/default serial if that second round trip hasn't landed
    /// yet — pinning the stream to the wrong (or no) target, which
    /// PipeWire then silently falls back from to its default source
    /// instead (see `verify_input_route`'s own doc comment for that
    /// half). Not observed to resolve within the old, much tighter
    /// budget here. A genuinely unavailable device still just fails, a
    /// little slower.
    const ROUTE_RETRIES: u32 = 10;
    const ROUTE_RETRY_DELAY: Duration = Duration::from_millis(300);

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
            let rate = out.sample_rate();
            let mixed = Arc::new(Mutex::new(MixedSource::new()));
            let stream = out.start(mixed.clone())?;
            if let Err(e) = audio::verify_route(&host, &out) {
                drop(stream);
                return Err(e);
            }
            self.outputs.insert(output.clone(), RoutedOutput { mixed, _stream: stream, errors: out.errors.clone(), rate });
        }

        // A deck/odj-sampler connected directly over `audio_link`
        // instead of a PipeWire virtual sink — resolved by name against
        // the live `Listener` registry, not a fresh cpal device scan.
        // Checked first since such a name never resolves through
        // `input_devices` at all (it isn't a cpal device).
        if let Some(listener) = self.socket_listener.clone()
            && let Some((stream, rate)) = listener.take(&input.0)
        {
            let out_rate = self.outputs.get(output).expect("just ensured").rate;
            if rate != out_rate {
                listener.give_back(&input.0, stream, rate); // don't strand the connection over a mismatch
                self.drop_output_if_unused(output);
                bail!(
                    "{} is rendering at {rate} Hz but the output is at {out_rate} Hz — this \
                     direct link doesn't resample, so they must match for now",
                    input.0
                );
            }
            let (prod, cons) = HeapRb::<f32>::new(RING_STEREO_SAMPLES).split();
            let (stop, thread) = audio_link::spawn_reader(stream, rate, prod, input.0.clone(), listener);
            self.outputs.get(output).expect("just ensured").mixed.lock().unwrap_or_else(|e| e.into_inner()).insert(input.clone(), cons, rate);
            self.inputs.insert(input.clone(), RoutedInput { output: output.clone(), source: InputSource::Socket { stop, thread: Some(thread) } });
            return Ok(());
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
        self.inputs.insert(input.clone(), RoutedInput { output: output.clone(), source: InputSource::Device { _stream: stream } });
        Ok(())
    }

    fn clear_route(&mut self, input: &DeviceKey) {
        if let Some(r) = self.inputs.remove(input) {
            if let Some(o) = self.outputs.get(&r.output) {
                o.mixed.lock().unwrap_or_else(|e| e.into_inner()).remove(input);
            }
            if let InputSource::Socket { stop, thread } = r.source {
                stop.store(true, Ordering::Relaxed);
                if let Some(t) = thread {
                    let _ = t.join(); // give_back/forget already happened inside the thread
                }
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

    /// Installs (`Some`) or clears (`None`) a record-mode track's
    /// capture tap on `input`'s own post-EQ, post-volume contribution
    /// — `false` if `input` isn't routed anywhere right now.
    pub fn set_input_tap(&mut self, input: &DeviceKey, tap: Option<ringbuf::HeapProd<f32>>) -> bool {
        let Some(r) = self.inputs.get(input) else { return false };
        let Some(o) = self.outputs.get(&r.output) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).set_input_tap(input, tap)
    }

    /// Suppresses (`true`) or restores (`false`) `input`'s own
    /// contribution to the mix — `false` if `input` isn't routed
    /// anywhere right now.
    pub fn set_input_muted(&mut self, input: &DeviceKey, muted: bool) -> bool {
        let Some(r) = self.inputs.get(input) else { return false };
        let Some(o) = self.outputs.get(&r.output) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).set_input_muted(input, muted)
    }

    /// Whether `input`'s own contribution is currently suppressed —
    /// `false` if it isn't routed anywhere right now.
    pub fn is_input_muted(&self, input: &DeviceKey) -> bool {
        let Some(r) = self.inputs.get(input) else { return false };
        let Some(o) = self.outputs.get(&r.output) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).is_input_muted(input)
    }

    /// Installs (`Some`) or clears (`None`) record-mode track `slot`'s
    /// decoded playback buffer on whichever output is currently main.
    /// `false` under the same condition as
    /// [`set_input_tap`](Self::set_input_tap) — nothing's open yet to
    /// mix it into.
    pub fn set_play_track(&mut self, slot: usize, track: Option<PlayTrack>) -> bool {
        let Some(key) = &self.main_output else { return false };
        let Some(o) = self.outputs.get(key) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).set_play_track(slot, track);
        true
    }

    /// Starts or freezes record mode's shared playhead on the main
    /// output. `false` under the same condition as
    /// [`set_input_tap`](Self::set_input_tap).
    pub fn set_running(&mut self, running: bool) -> bool {
        let Some(key) = &self.main_output else { return false };
        let Some(o) = self.outputs.get(key) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).set_running(running);
        true
    }

    /// Sets the shared playhead's extra scrub speed — `0.0` normally,
    /// signed otherwise (see `MixedSource`'s own doc comments on
    /// `search`). `false` under the same condition as
    /// [`set_input_tap`](Self::set_input_tap) — a no-op silently
    /// absorbed by a hold key pressed before anything's routed yet.
    pub fn set_search(&mut self, search: f64) -> bool {
        let Some(key) = &self.main_output else { return false };
        let Some(o) = self.outputs.get(key) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).set_search(search);
        true
    }

    /// Record mode's shared playhead, in frames since it was last at
    /// 0 — `None` under the same condition as
    /// [`set_input_tap`](Self::set_input_tap).
    pub fn playhead(&self) -> Option<u64> {
        let key = self.main_output.as_ref()?;
        let o = self.outputs.get(key)?;
        Some(o.mixed.lock().unwrap_or_else(|e| e.into_inner()).playhead())
    }

    /// Resets the playhead to 0 — only meaningful while nothing's
    /// running (see `MixedSource::rewind`'s own doc comment). `false`
    /// under the same condition as [`set_input_tap`](Self::set_input_tap).
    pub fn rewind_playhead(&mut self) -> bool {
        let Some(key) = &self.main_output else { return false };
        let Some(o) = self.outputs.get(key) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).rewind();
        true
    }

    /// Jumps the playhead straight to `frames`, for restoring a
    /// previous session's own saved position (see
    /// `MixedSource::set_playhead`'s own doc comment). `false` under
    /// the same condition as [`set_input_tap`](Self::set_input_tap).
    pub fn set_playhead(&mut self, frames: u64) -> bool {
        let Some(key) = &self.main_output else { return false };
        let Some(o) = self.outputs.get(key) else { return false };
        o.mixed.lock().unwrap_or_else(|e| e.into_inner()).set_playhead(frames);
        true
    }

    /// The main output's own negotiated sample rate, for a recording's
    /// WAV header — `None` under the same condition as
    /// [`set_input_tap`](Self::set_input_tap).
    pub fn main_output_rate(&self) -> Option<u32> {
        self.outputs.get(self.main_output.as_ref()?).map(|o| o.rate)
    }
}

#[cfg(test)]
mod tests {
    use ringbuf::traits::{Observer, Producer};

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

    #[test]
    fn mixed_source_pushes_an_inputs_post_eq_post_volume_contribution_into_its_own_tap() {
        let (mut p1, c1) = input(8);
        p1.push_slice(&[1.0, 1.0]);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.set(&key("A"), Param::Volume, 0.5);
        let (tap_prod, mut tap_cons) = input(8);
        assert!(mixed.set_input_tap(&key("A"), Some(tap_prod)));
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        let mut tapped = [0.0f32; 2];
        assert_eq!(tap_cons.pop_slice(&mut tapped), 2);
        // Volume-scaled (0.5), not the raw 1.0 that was pushed in.
        assert!((tapped[0] - 0.5).abs() < 1e-4, "{tapped:?}");
        assert!((tapped[1] - 0.5).abs() < 1e-4, "{tapped:?}");
        assert_eq!(tapped, out, "matches what actually went into the mix");
    }

    #[test]
    fn mixed_source_input_tap_is_independent_per_input() {
        let (mut p1, c1) = input(8);
        let (mut p2, c2) = input(8);
        p1.push_slice(&[0.2, 0.3]);
        p2.push_slice(&[0.4, 0.5]);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.insert(key("B"), c2, RATE);
        let (tap_prod, mut tap_cons) = input(8);
        mixed.set_input_tap(&key("A"), Some(tap_prod));
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        let mut tapped = [0.0f32; 2];
        assert_eq!(tap_cons.pop_slice(&mut tapped), 2);
        assert_eq!(tapped, [0.2, 0.3], "only A's own contribution, not B's");
    }

    #[test]
    fn mixed_source_input_tap_drops_rather_than_blocking_on_overrun() {
        let (_p, c1) = input(8);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        let (tap_prod, tap_cons) = input(4); // smaller than what gets pushed below
        mixed.set_input_tap(&key("A"), Some(tap_prod));
        let mut out = [0.0f32; 4];
        // Several renders in a row would overrun a 4-sample tap; this must
        // not panic or block.
        for _ in 0..5 {
            mixed.render(&mut out, 0.0);
        }
        assert!(tap_cons.occupied_len() <= 4);
    }

    #[test]
    fn set_input_tap_is_a_no_op_for_an_unrouted_key() {
        let mut mixed = MixedSource::new();
        let (prod, _cons) = input(8);
        assert!(!mixed.set_input_tap(&key("Gone"), Some(prod)));
    }

    #[test]
    fn routing_set_input_tap_is_a_no_op_without_a_route() {
        let mut routing = Routing::default();
        let (prod, _cons) = input(8);
        assert!(!routing.set_input_tap(&key("A"), Some(prod)));
    }

    #[test]
    fn mixed_source_muted_input_is_silent_but_still_pushes_to_its_own_tap() {
        let (mut p1, c1) = input(8);
        p1.push_slice(&[0.5, 0.5]);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        let (tap_prod, mut tap_cons) = input(8);
        mixed.set_input_tap(&key("A"), Some(tap_prod));
        assert!(mixed.set_input_muted(&key("A"), true));

        let mut out = [9.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert_eq!(out, [0.0, 0.0], "muted: nothing added to the final mix");

        let mut tapped = [0.0f32; 2];
        assert_eq!(tap_cons.pop_slice(&mut tapped), 2);
        assert_eq!(tapped, [0.5, 0.5], "still captured — muted only suppresses the mix, not the tap");
    }

    #[test]
    fn mixed_source_unmuting_an_input_restores_its_contribution() {
        let (mut p1, c1) = input(8);
        p1.push_slice(&[0.5, 0.5]);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.set_input_muted(&key("A"), true);
        mixed.set_input_muted(&key("A"), false);
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn set_input_muted_is_a_no_op_for_an_unrouted_key() {
        let mut mixed = MixedSource::new();
        assert!(!mixed.set_input_muted(&key("Gone"), true));
    }

    #[test]
    fn routing_set_input_muted_is_a_no_op_without_a_route() {
        let mut routing = Routing::default();
        assert!(!routing.set_input_muted(&key("A"), true));
    }

    #[test]
    fn mixed_source_mixes_a_play_tracks_samples_starting_at_its_start_offset() {
        let mut mixed = MixedSource::new();
        mixed.set_play_track(0, Some(PlayTrack { samples: vec![0.5, -0.5, 0.25, -0.25], start_offset: 2 }));
        mixed.set_running(true);
        let mut out = [9.0f32; 8]; // 4 frames
        mixed.render(&mut out, 0.0);
        assert_eq!(out, [0.0, 0.0, 0.0, 0.0, 0.5, -0.5, 0.25, -0.25], "silent before start_offset, then its samples, silent after its end");
    }

    #[test]
    fn mixed_source_play_track_is_silent_and_frozen_while_not_running() {
        let mut mixed = MixedSource::new();
        mixed.set_play_track(0, Some(PlayTrack { samples: vec![0.5, -0.5], start_offset: 0 }));
        for _ in 0..5 {
            let mut out = [9.0f32; 2];
            mixed.render(&mut out, 0.0);
            assert_eq!(out, [0.0, 0.0], "not a repeated non-zero window — genuinely silent");
        }
        assert_eq!(mixed.playhead(), 0, "frozen, not advancing");
    }

    #[test]
    fn mixed_source_playhead_advances_by_frames_rendered_only_while_running() {
        let mut mixed = MixedSource::new();
        let mut out = [0.0f32; 4]; // 2 frames
        mixed.render(&mut out, 0.0);
        assert_eq!(mixed.playhead(), 0, "not running yet");
        mixed.set_running(true);
        mixed.render(&mut out, 0.0);
        assert_eq!(mixed.playhead(), 2);
        mixed.render(&mut out, 0.0);
        assert_eq!(mixed.playhead(), 4);
    }

    #[test]
    fn mixed_source_rewind_resets_playhead_to_zero() {
        let mut mixed = MixedSource::new();
        mixed.set_running(true);
        mixed.render(&mut [0.0f32; 4], 0.0);
        assert_eq!(mixed.playhead(), 2);
        mixed.rewind();
        assert_eq!(mixed.playhead(), 0);
    }

    #[test]
    fn mixed_source_play_track_and_live_input_sum_together() {
        let (mut p1, c1) = input(8);
        p1.push_slice(&[0.1, 0.1]);
        let mut mixed = MixedSource::new();
        mixed.insert(key("A"), c1, RATE);
        mixed.set_play_track(0, Some(PlayTrack { samples: vec![0.2, 0.2], start_offset: 0 }));
        mixed.set_running(true);
        let mut out = [0.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert!((out[0] - 0.3).abs() < 1e-4, "{out:?}");
        assert!((out[1] - 0.3).abs() < 1e-4, "{out:?}");
    }

    #[test]
    fn mixed_source_search_scrubs_the_playhead_even_while_stopped() {
        let mut mixed = MixedSource::new();
        mixed.set_search(4.0); // not running
        mixed.render(&mut [0.0f32; 4], 0.0); // 2 frames
        assert_eq!(mixed.playhead(), 8, "scrub works independently of the normal transport");
    }

    #[test]
    fn mixed_source_search_adds_to_the_normal_running_speed() {
        let mut mixed = MixedSource::new();
        mixed.set_running(true);
        mixed.set_search(4.0);
        mixed.render(&mut [0.0f32; 4], 0.0); // 2 frames: (1.0 + 4.0) * 2 = 10
        assert_eq!(mixed.playhead(), 10, "running (1x) and search are additive, same as odj-player's own deck search");
    }

    #[test]
    fn mixed_source_search_reads_a_play_track_at_the_scaled_stride() {
        let mut mixed = MixedSource::new();
        // One distinct sample per frame, so reading at a 4-frame stride
        // is directly observable in which values land in `out`.
        let samples: Vec<f32> = (0..40).map(|i| i as f32).flat_map(|v| [v, v]).collect();
        mixed.set_play_track(0, Some(PlayTrack { samples, start_offset: 0 }));
        mixed.set_search(4.0);
        let mut out = [0.0f32; 6]; // 3 frames: reads frame 0, 4, 8
        mixed.render(&mut out, 0.0);
        assert_eq!(out, [0.0, 0.0, 4.0, 4.0, 8.0, 8.0]);
        assert_eq!(mixed.playhead(), 12);
    }

    #[test]
    fn mixed_source_search_runs_backward_and_clamps_the_playhead_at_zero() {
        let mut mixed = MixedSource::new();
        mixed.set_playhead(2);
        mixed.set_search(-4.0);
        mixed.render(&mut [0.0f32; 4], 0.0); // 2 frames backward: 2 - 2*4 = -6, clamped
        assert_eq!(mixed.playhead(), 0, "never goes negative");
    }

    #[test]
    fn mixed_source_is_silent_when_running_and_search_exactly_cancel_out() {
        let mut mixed = MixedSource::new();
        mixed.set_play_track(0, Some(PlayTrack { samples: vec![0.5, 0.5], start_offset: 0 }));
        mixed.set_running(true);
        mixed.set_search(-1.0); // 1.0 (running) + -1.0 (search) == 0.0
        let mut out = [9.0f32; 2];
        mixed.render(&mut out, 0.0);
        assert_eq!(out, [0.0, 0.0], "a net-zero combined speed is still frozen, same as fully stopped");
        assert_eq!(mixed.playhead(), 0);
    }

    #[test]
    fn routing_main_output_rate_is_none_before_anything_is_routed() {
        let mut routing = Routing::default();
        assert_eq!(routing.main_output_rate(), None);
        routing.set_main_output(Some(key("Speakers")));
        // A main output is set, but nothing's actually open for it yet.
        assert_eq!(routing.main_output_rate(), None);
    }

    #[test]
    fn clear_route_gives_a_socket_connection_back_rather_than_closing_it() {
        let dir = std::env::temp_dir().join(format!("odj-routing-link-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
        }
        let listener = Arc::new(Listener::start().unwrap());
        // A real handshake, not a bare cloned stream: `names()` reflects
        // what `accept_loop` registered as "connected", separately from
        // what's merely "pending" — see `Listener`'s own doc comment.
        let mut client = std::os::unix::net::UnixStream::connect(dir.join("odj/audio/mixer")).unwrap();
        write_raw_handshake(&mut client, "odj-deck-1", RATE);
        let (server, rate) = {
            let mut found = None;
            for _ in 0..200 {
                if let Some(r) = listener.take("odj-deck-1") {
                    found = Some(r);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            found.expect("the handshake was accepted")
        };

        // Manually wires up a routed socket input, the same shape
        // `try_set_route`'s own socket branch builds, without needing a
        // real output device open just to reach it (`clear_route` is a
        // no-op on the `self.outputs` side when there's no entry there,
        // same as it already is for any input whose output disappeared).
        let (prod, _cons) = input(RING_STEREO_SAMPLES);
        let (stop, thread) = audio_link::spawn_reader(server, rate, prod, "odj-deck-1".to_string(), listener.clone());
        let mut routing = Routing::default();
        routing.inputs.insert(
            key("odj-deck-1"),
            RoutedInput { output: key("Speakers"), source: InputSource::Socket { stop, thread: Some(thread) } },
        );

        routing.clear_route(&key("odj-deck-1"));
        assert!(routing.output_of(&key("odj-deck-1")).is_none());
        // Still listed as connected (unlike "pending", this never clears
        // just because something routed it) ...
        assert_eq!(listener.names(), [("odj-deck-1".to_string(), RATE)]);
        // ... and the reader thread's own clean-stop path gave the
        // connection back to the pending pool rather than dropping it —
        // proof it ran to completion (stopped and joined) rather than
        // being abandoned.
        assert!(listener.take("odj-deck-1").is_some(), "given back, so routable again without reconnecting");

        drop(client);
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Raw handshake bytes: 4-byte magic "odjl", a 32-byte NUL-padded
    /// name, then the sample rate as a u32 LE — see
    /// `odj_core::audio_link`'s own module doc comment for the wire
    /// format (its encoder is private to that crate).
    fn write_raw_handshake(stream: &mut std::os::unix::net::UnixStream, name: &str, sample_rate: u32) {
        let mut hs = [0u8; 4 + 32 + 4];
        hs[..4].copy_from_slice(b"odjl");
        hs[4..4 + name.len()].copy_from_slice(name.as_bytes());
        hs[36..].copy_from_slice(&sample_rate.to_le_bytes());
        std::io::Write::write_all(stream, &hs).unwrap();
    }

    fn any_output_key(devices_out: &Devices) -> DeviceKey {
        for _ in 0..200 {
            if let Some(e) = devices_out.entries().iter().find(|e| e.available) {
                return e.key.clone();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no output device found on this machine");
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn set_route_prefers_a_pending_socket_connection_over_a_cpal_scan() {
        let dir = std::env::temp_dir().join(format!("odj-routing-link-prefer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
        }
        let listener = Arc::new(Listener::start().unwrap());
        let mut client = std::os::unix::net::UnixStream::connect(dir.join("odj/audio/mixer")).unwrap();
        write_raw_handshake(&mut client, "odj-deck-1", 48_000);

        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let output_key = any_output_key(&devices_out);

        let mut routing = Routing::default();
        routing.set_socket_listener(listener.clone());
        routing.set_route(&devices_in, &devices_out, &key("odj-deck-1"), Some(&output_key)).unwrap();
        assert_eq!(routing.output_of(&key("odj-deck-1")), Some(output_key), "took the socket branch, never touched input_devices");

        routing.clear_route(&key("odj-deck-1"));
        drop(client);
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn set_route_rejects_a_sample_rate_mismatch_and_gives_the_connection_back() {
        let dir = std::env::temp_dir().join(format!("odj-routing-link-mismatch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
        }
        let listener = Arc::new(Listener::start().unwrap());
        let mut client = std::os::unix::net::UnixStream::connect(dir.join("odj/audio/mixer")).unwrap();
        write_raw_handshake(&mut client, "odj-deck-9", 1); // 1 Hz: never a real device's negotiated rate

        let devices_in = Devices::start(crate::devices::Kind::Input);
        let devices_out = Devices::start(crate::devices::Kind::Output);
        let output_key = any_output_key(&devices_out);

        let mut routing = Routing::default();
        routing.set_socket_listener(listener.clone());
        let err = routing.set_route(&devices_in, &devices_out, &key("odj-deck-9"), Some(&output_key)).unwrap_err();
        assert!(err.to_string().contains("1 Hz"), "{err}");
        assert!(routing.output_of(&key("odj-deck-9")).is_none(), "not routed");
        assert_eq!(listener.names(), [("odj-deck-9".to_string(), 1)], "given back, not stranded");

        drop(client);
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
