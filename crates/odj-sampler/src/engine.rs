//! The pads: owned sample data, voices and the audio render path.
//!
//! Sample data is cut and resampled to the output rate before it gets here, so the
//! callback only reads and mixes. The UI thread calls the control methods and the
//! audio callback calls `render`, both behind the same mutex.

use std::sync::Arc;

use odj_core::audio::Source;
use serde::{Deserialize, Serialize};

pub const PADS: usize = 16;
/// Frames kept after a sample's end: crossfaded into on loop wraps, faded out at the
/// end of a one-shot, so neither clicks.
pub const TAIL: usize = 256;
/// Fade when a pad starts.
const FADE_IN: usize = 32;
/// Fade when a pad is stopped or released.
const FADE_OUT: usize = 256;

/// Audio owned by a pad: interleaved stereo at the output rate, `frames` long plus
/// up to `TAIL` frames of what followed in the track.
#[derive(Clone)]
pub struct Sample {
    pub data: Arc<[f32]>,
    pub frames: usize,
}

impl Sample {
    pub fn new(data: Vec<f32>, frames: usize) -> Self {
        debug_assert!(frames <= data.len() / 2);
        Self { data: data.into(), frames }
    }

    pub fn bytes(&self) -> usize {
        self.data.len() * size_of::<f32>()
    }

    fn total(&self) -> usize {
        self.data.len() / 2
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// A press plays the sample to its end (or until stopped, when looping).
    #[default]
    OneShot,
    /// Plays while the key is held.
    Gate,
    /// A press starts, the next press stops.
    Toggle,
}

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Self::OneShot => Self::Gate,
            Self::Gate => Self::Toggle,
            Self::Toggle => Self::OneShot,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OneShot => "ONE-SHOT",
            Self::Gate => "GATE",
            Self::Toggle => "TOGGLE",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Props {
    pub mode: Mode,
    #[serde(rename = "loop")]
    pub looped: bool,
    /// One-shot: a press while playing restarts; otherwise it is ignored.
    pub retrigger: bool,
    pub gain_db: f32,
}

impl Default for Props {
    fn default() -> Self {
        Self { mode: Mode::OneShot, looped: false, retrigger: true, gain_db: 0.0 }
    }
}

pub const MIN_GAIN_DB: f32 = -24.0;
pub const MAX_GAIN_DB: f32 = 12.0;

#[derive(Clone, Copy, Default)]
struct Voice {
    active: bool,
    pos: usize,
    fade_in: usize,
    /// Frames left of the fade-out, and its length.
    fade_out: Option<(usize, usize)>,
    /// Loop wrap crossfade: where the old read position is, frames left.
    xfade: Option<(usize, usize)>,
}

impl Voice {
    fn start() -> Self {
        Self { active: true, pos: 0, fade_in: FADE_IN, fade_out: None, xfade: None }
    }

    fn stop(&mut self) {
        if self.active && self.fade_out.is_none() {
            self.fade_out = Some((FADE_OUT, FADE_OUT));
        }
    }

    fn stopping(&self) -> bool {
        self.fade_out.is_some()
    }

    /// Adds `frames` of the sample to `out` and moves on.
    fn mix(&mut self, s: &Sample, looped: bool, gain: f32, out: &mut [f32]) {
        let total = s.total();
        let at = |pos: usize| if pos < total { (s.data[pos * 2], s.data[pos * 2 + 1]) } else { (0.0, 0.0) };
        for frame in out.as_chunks_mut::<2>().0 {
            if !self.active {
                return;
            }
            if self.pos >= total {
                self.active = false;
                return;
            }
            let (mut l, mut r) = at(self.pos);
            if let Some((old, left)) = self.xfade.as_mut() {
                let g = *left as f32 / TAIL as f32;
                let (ol, or) = at(*old);
                l = l * (1.0 - g) + ol * g;
                r = r * (1.0 - g) + or * g;
                *old += 1;
                *left -= 1;
                if *left == 0 {
                    self.xfade = None;
                }
            }
            let mut env = gain;
            if self.fade_in > 0 {
                env *= 1.0 - self.fade_in as f32 / FADE_IN as f32;
                self.fade_in -= 1;
            }
            if let Some((left, len)) = self.fade_out.as_mut() {
                if *left == 0 {
                    self.active = false;
                    return;
                }
                env *= *left as f32 / *len as f32;
                *left -= 1;
            }
            frame[0] += l * env;
            frame[1] += r * env;

            self.pos += 1;
            if self.pos == s.frames && !self.stopping() {
                let tail = total - s.frames;
                if looped {
                    if tail > 0 {
                        self.xfade = Some((self.pos, tail.min(TAIL)));
                    }
                    self.pos = 0;
                } else if tail > 0 {
                    // Let the tail ring out instead of cutting at the end point.
                    self.fade_out = Some((tail, tail));
                } else {
                    self.active = false;
                }
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct Pad {
    pub sample: Option<Sample>,
    pub props: Props,
}

/// What the UI needs to draw, copied out so drawing never holds the lock.
#[derive(Clone, Copy, Default)]
pub struct PadState {
    pub playing: bool,
    /// Playback position, 0..1 of the sample.
    pub progress: f32,
}

#[derive(Clone)]
pub struct Snapshot {
    pub pads: [PadState; PADS],
    pub preview: Option<PadState>,
}

pub struct Sampler {
    pads: [Pad; PADS],
    voices: [Voice; PADS],
    /// The sample editor's audition, always looping.
    preview: Option<Sample>,
    preview_voice: Voice,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            pads: Default::default(),
            voices: [Voice::default(); PADS],
            preview: None,
            preview_voice: Voice::default(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let state = |v: &Voice, s: Option<&Sample>| PadState {
            playing: v.active,
            progress: s.map_or(0.0, |s| v.pos.min(s.frames) as f32 / s.frames.max(1) as f32),
        };
        Snapshot {
            pads: std::array::from_fn(|i| state(&self.voices[i], self.pads[i].sample.as_ref())),
            preview: self.preview.as_ref().map(|s| state(&self.preview_voice, Some(s))),
        }
    }

    #[cfg(test)]
    pub fn pad(&self, i: usize) -> &Pad {
        &self.pads[i]
    }

    /// Puts a sample on a pad, silencing it, and returns the old one so the caller
    /// can free it outside the audio lock.
    pub fn set_sample(&mut self, i: usize, sample: Option<Sample>) -> Option<Sample> {
        self.voices[i] = Voice::default();
        std::mem::replace(&mut self.pads[i].sample, sample)
    }

    pub fn set_props(&mut self, i: usize, props: Props) {
        self.pads[i].props = props;
    }

    /// A pad key went down. Returns false for an empty pad.
    pub fn press(&mut self, i: usize) -> bool {
        let pad = &self.pads[i];
        if pad.sample.is_none() {
            return false;
        }
        let v = &mut self.voices[i];
        let playing = v.active && !v.stopping();
        match pad.props.mode {
            Mode::OneShot if playing && !pad.props.retrigger => {}
            Mode::Toggle if playing => v.stop(),
            // Gate presses while playing are key repeats.
            Mode::Gate if playing => {}
            _ => *v = Voice::start(),
        }
        true
    }

    /// A pad key came up.
    pub fn release(&mut self, i: usize) {
        if self.pads[i].props.mode == Mode::Gate {
            self.voices[i].stop();
        }
    }

    pub fn stop(&mut self, i: usize) {
        self.voices[i].stop();
    }

    pub fn stop_all(&mut self) {
        for v in &mut self.voices {
            v.stop();
        }
        self.preview_voice.stop();
    }

    /// Starts (or with `None` ends) the editor's looping audition. Returns the old
    /// preview to free outside the lock.
    pub fn set_preview(&mut self, sample: Option<Sample>) -> Option<Sample> {
        self.preview_voice = if sample.is_some() { Voice::start() } else { Voice::default() };
        std::mem::replace(&mut self.preview, sample)
    }

    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        for (pad, voice) in self.pads.iter().zip(&mut self.voices) {
            if let Some(s) = &pad.sample
                && voice.active
            {
                voice.mix(s, pad.props.looped, db_to_gain(pad.props.gain_db), out);
            }
        }
        if let Some(s) = &self.preview {
            self.preview_voice.mix(s, true, 1.0, out);
        }
        for x in out {
            *x = soft_clip(*x);
        }
    }
}

impl Source for Sampler {
    fn render(&mut self, out: &mut [f32]) {
        Sampler::render(self, out);
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Linear up to 0.8, then bends smoothly towards ±1 so stacked pads don't wrap or crackle.
fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

/// Cuts `start..end` (frames at `rate`) out of interleaved stereo `samples` and
/// resamples it to `out_rate`, keeping up to `TAIL` output frames of what follows.
pub fn cut(samples: &[f32], rate: u32, start: f64, end: f64, out_rate: u32) -> Sample {
    let total = samples.len() / 2;
    let start = start.clamp(0.0, total as f64);
    let end = end.clamp(start, total as f64);
    let step = rate as f64 / out_rate as f64;
    let frames = ((end - start) / step).round() as usize;
    let available = ((total as f64 - start) / step).floor().max(0.0) as usize;
    let len = (frames + TAIL).min(available);
    let mut data = Vec::with_capacity(len * 2);
    for k in 0..len {
        let (l, r) = if rate == out_rate {
            let i = start as usize + k;
            (samples[i * 2], samples[i * 2 + 1])
        } else {
            sample_at(samples, total, start + k as f64 * step)
        };
        data.push(l);
        data.push(r);
    }
    Sample::new(data, frames.min(len))
}

/// Cubic interpolation of interleaved stereo at a fractional frame.
fn sample_at(s: &[f32], frames: usize, pos: f64) -> (f32, f32) {
    let i = pos.floor() as isize;
    let t = (pos - i as f64) as f32;
    let max = frames as isize - 1;
    let at = |k: isize, c: usize| s[k.clamp(0, max) as usize * 2 + c];
    let cubic = |c: usize| {
        let (y0, y1, y2, y3) = (at(i - 1, c), at(i, c), at(i + 1, c), at(i + 2, c));
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * t + c2) * t + c1) * t + y1
    };
    (cubic(0), cubic(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A constant 0.5 sample, `frames` long with `tail` extra frames.
    fn flat(frames: usize, tail: usize) -> Sample {
        Sample::new(vec![0.5; (frames + tail) * 2], frames)
    }

    fn sampler_with(props: Props, sample: Sample) -> Sampler {
        let mut s = Sampler::new();
        s.set_sample(0, Some(sample));
        s.set_props(0, props);
        s
    }

    fn run(s: &mut Sampler, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        s.render(&mut out);
        out
    }

    #[test]
    fn empty_pad_does_not_play() {
        let mut s = Sampler::new();
        assert!(!s.press(3));
        assert!(run(&mut s, 64).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn one_shot_plays_to_the_end_and_through_its_tail() {
        let mut s = sampler_with(Props::default(), flat(1000, TAIL));
        s.press(0);
        let out = run(&mut s, 2000);
        assert_eq!(out[200 * 2], 0.5);
        assert!(out[1100 * 2] > 0.0 && out[1100 * 2] < 0.5, "tail fades out");
        assert_eq!(out[1999 * 2], 0.0);
        assert!(!s.snapshot().pads[0].playing);
    }

    #[test]
    fn looping_wraps_until_stopped() {
        let props = Props { looped: true, ..Props::default() };
        let mut s = sampler_with(props, flat(1000, TAIL));
        s.press(0);
        let out = run(&mut s, 5000);
        assert_eq!(out[4500 * 2], 0.5);
        s.stop(0);
        run(&mut s, FADE_OUT + 1);
        assert!(!s.snapshot().pads[0].playing);
    }

    #[test]
    fn gate_stops_on_release() {
        let props = Props { mode: Mode::Gate, looped: true, ..Props::default() };
        let mut s = sampler_with(props, flat(1000, 0));
        s.press(0);
        run(&mut s, 500);
        s.press(0); // key repeat: keeps playing from where it was
        assert!((s.snapshot().pads[0].progress - 0.5).abs() < 0.01);
        s.release(0);
        let out = run(&mut s, FADE_OUT + 10);
        assert_eq!(out[(FADE_OUT + 5) * 2], 0.0);
        assert!(!s.snapshot().pads[0].playing);
    }

    #[test]
    fn toggle_starts_and_stops() {
        let props = Props { mode: Mode::Toggle, looped: true, ..Props::default() };
        let mut s = sampler_with(props, flat(1000, 0));
        s.press(0);
        run(&mut s, 100);
        assert!(s.snapshot().pads[0].playing);
        s.press(0);
        run(&mut s, FADE_OUT + 1);
        assert!(!s.snapshot().pads[0].playing);
    }

    #[test]
    fn one_shot_without_retrigger_ignores_presses() {
        let props = Props { retrigger: false, ..Props::default() };
        let mut s = sampler_with(props, flat(1000, 0));
        s.press(0);
        run(&mut s, 500);
        s.press(0);
        assert!(s.snapshot().pads[0].progress >= 0.5);
        let mut s = sampler_with(Props::default(), flat(1000, 0));
        s.press(0);
        run(&mut s, 500);
        s.press(0);
        assert_eq!(s.snapshot().pads[0].progress, 0.0);
    }

    #[test]
    fn gain_and_mixing() {
        let mut s = sampler_with(Props { gain_db: -6.0206, ..Props::default() }, flat(1000, 0));
        s.set_sample(1, Some(flat(1000, 0)));
        s.press(0);
        s.press(1);
        let out = run(&mut s, 100);
        // 0.25 + 0.5 = 0.75, below the clipper's knee.
        assert!((out[50 * 2] - 0.75).abs() < 1e-3, "{}", out[50 * 2]);
    }

    #[test]
    fn clipper_stays_within_range_and_is_continuous() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(10.0) <= 1.0 && soft_clip(-10.0) >= -1.0);
        assert!((soft_clip(0.8001) - 0.8001).abs() < 1e-3);
    }

    #[test]
    fn cut_keeps_rate_region_and_tail() {
        let src: Vec<f32> = (0..10_000).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let s = cut(&src, 48_000, 1000.0, 3000.0, 48_000);
        assert_eq!(s.frames, 2000);
        assert_eq!(s.data.len() / 2, 2000 + TAIL);
        assert_eq!((s.data[0], s.data[1]), (1000.0, -1000.0));
        // At the very end of the track there is no tail.
        let s = cut(&src, 48_000, 9000.0, 10_000.0, 48_000);
        assert_eq!((s.frames, s.data.len() / 2), (1000, 1000));
    }

    #[test]
    fn cut_resamples_to_the_output_rate() {
        let src: Vec<f32> = (0..44_100).flat_map(|i| [i as f32, i as f32]).collect();
        let s = cut(&src, 44_100, 0.0, 44_100.0 / 2.0, 48_000);
        assert_eq!(s.frames, 24_000);
        // A ramp stays a ramp: frame k reads source frame k * 44.1/48.
        let k = 1000;
        assert!((s.data[k * 2] - k as f32 * 44_100.0 / 48_000.0).abs() < 1e-2);
    }
}
