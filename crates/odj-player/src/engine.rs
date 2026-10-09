//! The deck: transport, cue, loops, tempo and the audio render path.
//!
//! The UI thread calls the control methods and the audio callback calls
//! `render`, both behind the same mutex. Positions are in source frames.

use std::sync::Arc;

use odj_core::audio::Source;
use odj_core::memory::{Cue, MAX_MEMORIES, TrackMemory};
use odj_core::track::Track;

use crate::stretch::Stretcher;

/// Length of the crossfade used to hide clicks on jumps and loop wraps.
const XFADE: usize = 96;
/// Largest block fed to the time stretcher.
const MAX_BLOCK: usize = 4096;
/// CD frames per second, the unit of the paused jog and the time display.
pub const CD_FRAMES: f64 = 75.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TempoRange {
    R6,
    R10,
    R16,
    Wide,
}

impl TempoRange {
    pub fn percent(self) -> f64 {
        match self {
            Self::R6 => 6.0,
            Self::R10 => 10.0,
            Self::R16 => 16.0,
            Self::Wide => 100.0,
        }
    }

    /// Resolution of the pitch fader in this range.
    pub fn step(self) -> f64 {
        match self {
            Self::R6 => 0.02,
            Self::R10 | Self::R16 => 0.05,
            Self::Wide => 0.5,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::R6 => Self::R10,
            Self::R10 => Self::R16,
            Self::R16 => Self::Wide,
            Self::Wide => Self::R6,
        }
    }
}

/// What the UI needs to draw a frame, copied out so rendering never holds the lock.
#[derive(Clone)]
pub struct Snapshot {
    pub track: Option<Arc<Track>>,
    pub pos: f64,
    pub playing: bool,
    pub reverse: bool,
    pub speed: f64,
    pub tempo: f64,
    pub range: TempoRange,
    pub master_tempo: bool,
    pub master_tempo_available: bool,
    pub bend: f64,
    pub cue: f64,
    pub cue_preview: bool,
    pub hot: [Option<Cue>; 3],
    pub memories: Vec<Cue>,
    pub loop_in: Option<f64>,
    pub loop_out: Option<f64>,
    pub looping: bool,
    pub loop_adjust: bool,
    pub start_time: f64,
    pub brake_time: f64,
    pub bpm: Option<f64>,
    pub bpm_tapped: bool,
    pub loop_beats: Option<f64>,
}

pub struct Deck {
    pub track: Option<Arc<Track>>,
    out_rate: u32,
    pos: f64,
    playing: bool,
    reverse: bool,
    /// Motor speed, -1..1, ramped by the start and brake times.
    speed: f64,
    tempo: f64,
    range: TempoRange,
    master_tempo: bool,
    /// Temporary speed change from the jog keys, in percent.
    pub bend: f64,
    /// Extra speed while a search button is held, signed, in multiples of normal speed.
    pub search: f64,
    cue: f64,
    cue_preview: bool,
    cue_latched: bool,
    hot: [Option<Cue>; 3],
    /// Stored cue/loop points, sorted by position (MEMORY/CALL).
    memories: Vec<Cue>,
    loop_in: Option<f64>,
    loop_out: Option<f64>,
    looping: bool,
    /// Loop Out pressed while looping: the jog moves the out point.
    loop_adjust: bool,
    pub start_time: f64,
    pub brake_time: f64,
    bpm_override: Option<f64>,
    xfade: Option<(f64, usize)>,
    gain: f32,
    stretch: Option<Stretcher>,
    stretch_primed: bool,
    discard: usize,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    gen_l: Vec<f32>,
    gen_r: Vec<f32>,
}

impl Deck {
    pub fn new(out_rate: u32) -> Self {
        Self {
            track: None,
            out_rate,
            pos: 0.0,
            playing: false,
            reverse: false,
            speed: 0.0,
            tempo: 0.0,
            range: TempoRange::R10,
            master_tempo: false,
            bend: 0.0,
            search: 0.0,
            cue: 0.0,
            cue_preview: false,
            cue_latched: false,
            hot: [None; 3],
            memories: Vec::new(),
            loop_in: None,
            loop_out: None,
            looping: false,
            loop_adjust: false,
            start_time: 0.0,
            brake_time: 0.25,
            bpm_override: None,
            xfade: None,
            gain: 0.0,
            stretch: Stretcher::new(out_rate, MAX_BLOCK),
            stretch_primed: false,
            discard: 0,
            out_l: vec![0.0; MAX_BLOCK],
            out_r: vec![0.0; MAX_BLOCK],
            gen_l: vec![0.0; MAX_BLOCK],
            gen_r: vec![0.0; MAX_BLOCK],
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            track: self.track.clone(),
            pos: self.pos,
            playing: self.playing,
            reverse: self.reverse,
            speed: self.speed,
            tempo: self.tempo,
            range: self.range,
            master_tempo: self.master_tempo,
            master_tempo_available: self.stretch.is_some(),
            bend: self.bend,
            cue: self.cue,
            cue_preview: self.cue_preview,
            hot: self.hot,
            memories: self.memories.clone(),
            loop_in: self.loop_in,
            loop_out: self.loop_out,
            looping: self.looping,
            loop_adjust: self.is_adjusting_loop(),
            start_time: self.start_time,
            brake_time: self.brake_time,
            bpm: self.bpm(),
            bpm_tapped: self.bpm_override.is_some(),
            loop_beats: self.loop_beats(),
        }
    }

    /// Loads a track and returns the previous one so the caller can drop it
    /// outside the lock. With `auto_cue` the cue goes to the first sound.
    pub fn load(&mut self, track: Arc<Track>, mem: &TrackMemory, auto_cue: bool) -> Option<Arc<Track>> {
        self.cue = if auto_cue { track.first_sound } else { 0.0 };
        self.pos = self.cue;
        self.hot = mem.hot;
        self.memories = mem.memories.clone();
        self.loop_in = mem.loop_in;
        self.loop_out = mem.loop_out;
        self.bpm_override = mem.bpm;
        self.looping = false;
        self.loop_adjust = false;
        self.playing = false;
        self.speed = 0.0;
        self.reverse = false;
        self.cue_preview = false;
        self.xfade = None;
        self.stretch_primed = false;
        self.track.replace(track)
    }

    pub fn memory(&self) -> TrackMemory {
        TrackMemory {
            hot: self.hot,
            memories: self.memories.clone(),
            loop_in: self.loop_in,
            loop_out: self.loop_out,
            bpm: self.bpm_override,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    fn sample_rate(&self) -> f64 {
        self.track.as_ref().map_or(self.out_rate, |t| t.sample_rate) as f64
    }

    fn last_frame(&self) -> f64 {
        self.track.as_ref().map_or(0.0, |t| (t.frames().max(1) - 1) as f64)
    }

    fn direction(&self) -> f64 {
        if self.reverse { -1.0 } else { 1.0 }
    }

    fn tempo_factor(&self) -> f64 {
        1.0 + self.tempo / 100.0
    }

    /// The track's own BPM (tapped or detected), before the pitch fader.
    pub fn bpm(&self) -> Option<f64> {
        self.bpm_override.or(self.track.as_ref().and_then(|t| t.bpm))
    }

    /// Sets the BPM from a tapped tempo, which is heard after the pitch fader.
    pub fn set_tapped_bpm(&mut self, heard: f64) {
        self.bpm_override = Some(heard / self.tempo_factor());
    }

    pub fn clear_tapped_bpm(&mut self) {
        self.bpm_override = None;
    }

    fn beat_frames(&self) -> Option<f64> {
        self.bpm().map(|bpm| 60.0 / bpm * self.sample_rate())
    }

    pub fn stretch_latency_ms(&self) -> Option<f64> {
        self.stretch.as_ref().map(|s| s.latency() as f64 * 1000.0 / self.out_rate as f64)
    }

    fn jump(&mut self, to: f64) {
        let to = to.clamp(0.0, self.last_frame());
        if self.speed != 0.0 || self.search != 0.0 {
            self.xfade = Some((self.pos, XFADE));
        }
        self.pos = to;
    }

    fn start_now(&mut self) {
        self.playing = true;
        self.speed = self.direction();
    }

    fn stop_now(&mut self) {
        self.playing = false;
        self.speed = 0.0;
    }

    // --- Transport -------------------------------------------------------

    pub fn play_pause(&mut self) {
        if self.track.is_none() {
            return;
        }
        if self.cue_preview {
            // Pressing Play while holding Cue keeps playing after release.
            self.cue_latched = true;
            return;
        }
        self.playing = !self.playing;
    }

    /// The cue button. `can_hold` is false when key releases can't be seen,
    /// which disables the hold-to-preview behaviour.
    pub fn cue_down(&mut self, can_hold: bool) {
        if self.track.is_none() || self.cue_preview {
            return;
        }
        if self.playing {
            self.jump(self.cue);
            self.stop_now();
        } else if (self.pos - self.cue).abs() < 1.0 {
            if can_hold {
                self.cue_preview = true;
                self.cue_latched = false;
                self.start_now();
            }
        } else {
            self.cue = self.pos;
        }
    }

    pub fn cue_up(&mut self) {
        if !self.cue_preview {
            return;
        }
        self.cue_preview = false;
        if !self.cue_latched {
            self.jump(self.cue);
            self.stop_now();
        }
    }

    pub fn toggle_reverse(&mut self) {
        self.reverse = !self.reverse;
    }

    /// Moves the paused playhead, like turning the jog in pause mode.
    pub fn step_paused(&mut self, cd_frames: f64) {
        if !self.playing {
            let delta = cd_frames * self.sample_rate() / CD_FRAMES;
            self.pos = (self.pos + delta).clamp(0.0, self.last_frame());
        }
    }

    pub fn step_beats_paused(&mut self, beats: f64) {
        let frames = self.beat_frames().unwrap_or(self.sample_rate());
        self.step_paused(beats * frames * CD_FRAMES / self.sample_rate());
    }

    // --- Hot cues ----------------------------------------------------------

    /// Stores (or overwrites) a hot cue: the active loop, else the playhead.
    pub fn set_hot_cue(&mut self, i: usize) {
        if self.track.is_none() {
            return;
        }
        self.hot[i] = Some(match (self.looping, self.loop_in, self.loop_out) {
            (true, Some(pos), Some(out)) => Cue { pos, loop_out: Some(out) },
            _ => Cue { pos: self.pos, loop_out: None },
        });
    }

    /// Jumps to a hot cue and plays. Returns false if the slot is empty.
    pub fn hot_cue(&mut self, i: usize) -> bool {
        let Some(h) = self.hot[i] else { return false };
        self.cue_preview = false;
        self.jump(h.pos);
        if let Some(out) = h.loop_out {
            self.loop_in = Some(h.pos);
            self.loop_out = Some(out);
            self.looping = true;
        } else {
            self.looping = false;
        }
        self.loop_adjust = false;
        self.start_now();
        true
    }

    pub fn clear_hot_cue(&mut self, i: usize) {
        self.hot[i] = None;
    }

    // --- Memory cues ---------------------------------------------------------

    /// Stores the cue point, or the active loop, as a memory point.
    pub fn store_memory(&mut self) -> Option<usize> {
        self.track.as_ref()?;
        let entry = match (self.looping, self.loop_in, self.loop_out) {
            (true, Some(pos), Some(out)) => Cue { pos, loop_out: Some(out) },
            _ => Cue { pos: self.cue, loop_out: None },
        };
        if self.memories.len() >= MAX_MEMORIES
            || self.memories.iter().any(|m| (m.pos - entry.pos).abs() < 1.0)
        {
            return None;
        }
        self.memories.push(entry);
        self.memories.sort_by(|a, b| a.pos.total_cmp(&b.pos));
        self.memories.iter().position(|m| *m == entry).map(|i| i + 1)
    }

    /// Calls the next (dir > 0) or previous memory point: it becomes the cue
    /// point and the deck waits there, paused, with any stored loop armed.
    pub fn call_memory(&mut self, dir: i32) -> Option<usize> {
        let i = if dir > 0 {
            self.memories.iter().position(|m| m.pos > self.pos + 1.0)?
        } else {
            self.memories.iter().rposition(|m| m.pos < self.pos - 1.0)?
        };
        let m = self.memories[i];
        self.cue_preview = false;
        self.jump(m.pos);
        self.stop_now();
        self.cue = m.pos;
        if let Some(out) = m.loop_out {
            self.loop_in = Some(m.pos);
            self.loop_out = Some(out);
            self.looping = true;
            self.loop_adjust = false;
        }
        Some(i + 1)
    }

    /// Deletes the memory point at the current cue point.
    pub fn delete_memory(&mut self) -> bool {
        let before = self.memories.len();
        let cue = self.cue;
        self.memories.retain(|m| (m.pos - cue).abs() >= 1.0);
        self.memories.len() != before
    }

    // --- Loops ---------------------------------------------------------------

    pub fn loop_in(&mut self) {
        if self.track.is_none() {
            return;
        }
        self.loop_in = Some(self.pos);
        self.loop_out = None;
        self.looping = false;
        self.loop_adjust = false;
        self.cue = self.pos;
    }

    /// Sets the loop out point, or while looping toggles out-point adjustment.
    pub fn loop_out(&mut self) {
        if self.looping && self.loop_out.is_some() {
            self.loop_adjust = !self.loop_adjust;
            return;
        }
        let Some(start) = self.loop_in else { return };
        if self.pos > start + self.sample_rate() * 0.01 {
            self.loop_out = Some(self.pos);
            self.looping = true;
            self.jump(start);
        }
    }

    pub fn reloop_exit(&mut self) {
        self.loop_adjust = false;
        if self.looping {
            self.looping = false;
        } else if let (Some(start), Some(_)) = (self.loop_in, self.loop_out) {
            self.looping = true;
            self.jump(start);
        }
    }

    pub fn auto_loop(&mut self, beats: f64) -> bool {
        let Some(beat) = self.beat_frames() else { return false };
        if self.track.is_none() {
            return false;
        }
        let end = (self.pos + beats * beat).min(self.last_frame());
        self.loop_in = Some(self.pos);
        self.loop_out = Some(end);
        self.looping = true;
        self.cue = self.pos;
        true
    }

    /// Halves (0.5) or doubles (2.0) the current loop from its in point.
    pub fn scale_loop(&mut self, factor: f64) {
        let (Some(start), Some(end)) = (self.loop_in, self.loop_out) else { return };
        let len = ((end - start) * factor).max(self.sample_rate() * 0.01);
        if start + len > self.last_frame() {
            return;
        }
        self.loop_out = Some(start + len);
        if self.looping && self.pos >= start + len {
            self.jump(start + (self.pos - start) % len);
        }
    }

    pub fn is_adjusting_loop(&self) -> bool {
        self.loop_adjust && self.looping
    }

    /// Moves the loop out point while adjusting, like turning the jog.
    pub fn adjust_loop_out(&mut self, cd_frames: f64) {
        let (Some(start), Some(end)) = (self.loop_in, self.loop_out) else { return };
        if !self.is_adjusting_loop() {
            return;
        }
        let sr = self.sample_rate();
        let end = (end + cd_frames * sr / CD_FRAMES).clamp(start + sr * 0.01, self.last_frame());
        self.loop_out = Some(end);
        if self.pos >= end {
            self.jump(start + (self.pos - start) % (end - start));
        }
    }

    pub fn loop_beats(&self) -> Option<f64> {
        let (start, end) = (self.loop_in?, self.loop_out?);
        Some((end - start) / self.beat_frames()?)
    }

    // --- Tempo ---------------------------------------------------------------

    pub fn move_tempo(&mut self, steps: f64) {
        let step = self.range.step();
        let max = self.range.percent();
        self.tempo = ((self.tempo + steps * step) / step).round() * step;
        self.tempo = self.tempo.clamp(-max, max);
    }

    pub fn reset_tempo(&mut self) {
        self.tempo = 0.0;
    }

    pub fn cycle_range(&mut self) {
        self.range = self.range.next();
        let max = self.range.percent();
        self.tempo = self.tempo.clamp(-max, max);
    }

    pub fn toggle_master_tempo(&mut self) -> bool {
        if self.stretch.is_some() {
            self.master_tempo = !self.master_tempo;
        }
        self.master_tempo
    }

    // --- Audio -----------------------------------------------------------------

    /// Fills `out` (interleaved stereo) with the next block of audio.
    pub fn render(&mut self, out: &mut [f32]) {
        let n = out.len() / 2;
        if self.out_l.len() < n {
            self.out_l.resize(n, 0.0);
            self.out_r.resize(n, 0.0);
        }
        let mut l = std::mem::take(&mut self.out_l);
        let mut r = std::mem::take(&mut self.out_r);
        if self.master_tempo && self.stretch.is_some() {
            self.render_stretched(&mut l[..n], &mut r[..n]);
        } else {
            self.stretch_primed = false;
            self.generate(&mut l[..n], &mut r[..n]);
        }
        for (i, frame) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            frame[0] = l[i];
            frame[1] = r[i];
        }
        self.out_l = l;
        self.out_r = r;
    }

    /// Varispeed playback, then Rubber Band shifts the key back to the original.
    fn render_stretched(&mut self, l: &mut [f32], r: &mut [f32]) {
        let Some(mut st) = self.stretch.take() else { return };
        let mut gl = std::mem::take(&mut self.gen_l);
        let mut gr = std::mem::take(&mut self.gen_r);

        if !self.stretch_primed {
            st.reset();
            gl.fill(0.0);
            gr.fill(0.0);
            let mut pad = st.start_pad();
            while pad > 0 {
                let k = pad.min(MAX_BLOCK);
                st.process(&gl[..k], &gr[..k]);
                pad -= k;
            }
            self.discard = st.start_delay();
            self.stretch_primed = true;
        }
        st.set_pitch_scale(1.0 / self.tempo_factor());

        let n = l.len();
        let mut done = 0;
        while done < n {
            let available = st.available();
            if available == 0 {
                let k = st.samples_required().clamp(64, MAX_BLOCK);
                self.generate(&mut gl[..k], &mut gr[..k]);
                st.process(&gl[..k], &gr[..k]);
            } else if self.discard > 0 {
                let k = available.min(self.discard).min(MAX_BLOCK);
                let got = st.retrieve(&mut gl[..k], &mut gr[..k]);
                self.discard -= got.min(self.discard);
            } else {
                let k = available.min(n - done);
                let got = st.retrieve(&mut l[done..done + k], &mut r[done..done + k]);
                if got == 0 {
                    break;
                }
                done += got;
            }
        }
        l[done..].fill(0.0);
        r[done..].fill(0.0);

        self.gen_l = gl;
        self.gen_r = gr;
        self.stretch = Some(st);
    }

    /// Reads the track at the current speed with cubic interpolation, running
    /// the motor ramp, loops and end-of-track handling frame by frame.
    fn generate(&mut self, l: &mut [f32], r: &mut [f32]) {
        let Some(track) = self.track.clone() else {
            l.fill(0.0);
            r.fill(0.0);
            return;
        };
        let samples = &track.samples;
        let frames = track.frames();
        let last = self.last_frame();
        let sr_ratio = track.sample_rate as f64 / self.out_rate as f64;
        let pitch = self.tempo_factor() * (1.0 + self.bend / 100.0);
        let target = if self.playing { self.direction() } else { 0.0 };
        let ramp = |seconds: f64| {
            if seconds <= 0.0 { f64::INFINITY } else { 1.0 / (seconds * self.out_rate as f64) }
        };
        let (up, down) = (ramp(self.start_time), ramp(self.brake_time));

        for i in 0..l.len() {
            if self.speed != target {
                let slowing = target.abs() < self.speed.abs() || target * self.speed < 0.0;
                let d = if slowing { down } else { up };
                if (target - self.speed).abs() <= d {
                    self.speed = target;
                } else {
                    self.speed += d * (target - self.speed).signum();
                }
            }
            let rate = self.speed * pitch + self.search;
            let step = rate * sr_ratio;
            let gate = (rate.abs() / 0.05).min(1.0) as f32;
            self.gain += (gate - self.gain) * 0.02;

            let (mut a, mut b) = sample(samples, frames, self.pos);
            if let Some((old, left)) = self.xfade.as_mut() {
                let g = *left as f32 / XFADE as f32;
                let (oa, ob) = sample(samples, frames, *old);
                a = a * (1.0 - g) + oa * g;
                b = b * (1.0 - g) + ob * g;
                *old += step;
                *left -= 1;
                if *left == 0 {
                    self.xfade = None;
                }
            }
            l[i] = a * self.gain;
            r[i] = b * self.gain;

            let prev = self.pos;
            self.pos += step;
            if self.looping
                && let (Some(start), Some(end)) = (self.loop_in, self.loop_out)
            {
                let len = end - start;
                if step > 0.0 && prev < end && self.pos >= end {
                    self.xfade = Some((self.pos, XFADE));
                    self.pos -= len;
                } else if step < 0.0 && prev >= start && self.pos < start {
                    self.xfade = Some((self.pos, XFADE));
                    self.pos += len;
                }
            }
            if self.pos >= last {
                self.pos = last;
                if step > 0.0 {
                    self.stop_now();
                }
            } else if self.pos < 0.0 {
                self.pos = 0.0;
                if step < 0.0 {
                    self.stop_now();
                }
            }
        }
    }
}

fn sample(s: &[f32], frames: usize, pos: f64) -> (f32, f32) {
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

impl Source for Deck {
    fn render(&mut self, out: &mut [f32]) {
        Deck::render(self, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use odj_core::track::test_util::click_track;

    const SR: u32 = 48_000;

    fn deck() -> Deck {
        let samples = click_track(120.0, 20.0, SR);
        let track = Track::from_samples("t".into(), "t".into(), None, SR, samples);
        let mut d = Deck::new(SR);
        d.load(Arc::new(track), &TrackMemory::default(), false);
        d
    }

    fn run(d: &mut Deck, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        for chunk in out.chunks_mut(1024) {
            d.render(chunk);
        }
        out
    }

    #[test]
    fn auto_cue_picks_first_sound_or_start() {
        let track = Arc::new(Track::from_samples("t".into(), "t".into(), None, SR, click_track(120.0, 5.0, SR)));
        let mut d = Deck::new(SR);
        d.load(track.clone(), &TrackMemory::default(), true);
        assert!(d.cue > 0.2 * SR as f64 && d.pos == d.cue, "cue {}", d.cue);
        d.load(track, &TrackMemory::default(), false);
        assert_eq!((d.cue, d.pos), (0.0, 0.0));
    }

    #[test]
    fn hold_cue_previews_and_returns() {
        let mut d = deck();
        d.cue_down(true);
        assert!(d.playing && d.cue_preview);
        run(&mut d, 4800);
        assert!((d.pos - 4800.0).abs() < 2.0);
        d.cue_up();
        assert!(!d.playing);
        assert_eq!(d.pos, 0.0);
    }

    #[test]
    fn play_while_holding_cue_keeps_playing() {
        let mut d = deck();
        d.cue_down(true);
        d.play_pause();
        d.cue_up();
        assert!(d.playing);
    }

    #[test]
    fn cue_while_playing_returns_to_cue_and_pauses() {
        let mut d = deck();
        d.play_pause();
        run(&mut d, 10_000);
        d.cue_down(true);
        assert!(!d.playing);
        assert_eq!(d.pos, 0.0);
    }

    #[test]
    fn cue_while_paused_elsewhere_sets_cue() {
        let mut d = deck();
        d.step_paused(75.0);
        d.cue_down(true);
        assert_eq!(d.cue, SR as f64);
        assert!(!d.playing);
    }

    #[test]
    fn manual_loop_wraps() {
        let mut d = deck();
        d.play_pause();
        d.loop_in();
        run(&mut d, 24_000);
        d.loop_out();
        assert_eq!(d.pos, 0.0);
        run(&mut d, 100_000);
        assert!(d.pos >= 0.0 && d.pos < 24_000.0, "pos {}", d.pos);
        d.reloop_exit();
        run(&mut d, 30_000);
        assert!(d.pos > 24_000.0);
    }

    #[test]
    fn loop_out_adjust_moves_out_point() {
        let mut d = deck();
        d.play_pause();
        d.loop_in();
        run(&mut d, 24_000);
        d.loop_out();
        assert!(!d.is_adjusting_loop());
        d.adjust_loop_out(75.0); // ignored outside adjust mode
        assert_eq!(d.loop_out, Some(24_000.0));
        d.loop_out();
        assert!(d.is_adjusting_loop());
        d.adjust_loop_out(75.0);
        assert_eq!(d.loop_out, Some(24_000.0 + SR as f64));
        d.adjust_loop_out(-1000.0); // can't go before the in point
        assert!(d.loop_out.unwrap() > 0.0);
        d.loop_out();
        assert!(!d.is_adjusting_loop());
        d.loop_out();
        d.reloop_exit();
        assert!(!d.is_adjusting_loop());
    }

    #[test]
    fn memory_cues_store_call_and_delete() {
        let mut d = deck();
        d.store_memory(); // cue at 0
        d.step_paused(75.0);
        d.cue_down(true); // cue at 1 s
        d.store_memory();
        assert!(d.store_memory().is_none(), "duplicates are refused");
        d.play_pause();
        run(&mut d, SR as usize);
        d.loop_in(); // 2 s
        run(&mut d, 24_000);
        d.loop_out();
        assert_eq!(d.store_memory(), Some(3), "the loop is stored at its in point");
        assert_eq!(d.memories[2].loop_out, Some(2.5 * SR as f64));

        // Calling while playing jumps there and waits in pause.
        d.reloop_exit();
        run(&mut d, 200_000);
        assert_eq!(d.call_memory(-1), Some(3));
        assert!(!d.playing && d.looping);
        assert_eq!(d.pos, 2.0 * SR as f64);
        assert_eq!(d.call_memory(-1), Some(2));
        assert_eq!(d.cue, SR as f64);
        assert_eq!(d.call_memory(-1), Some(1));
        assert_eq!(d.call_memory(-1), None);
        assert_eq!(d.call_memory(1), Some(2));

        assert!(d.delete_memory());
        assert_eq!(d.memories.len(), 2);
        assert!(!d.delete_memory());
    }

    #[test]
    fn auto_loop_uses_bpm() {
        let mut d = deck();
        assert!(d.auto_loop(4.0));
        let beats = d.loop_beats().unwrap();
        assert!((beats - 4.0).abs() < 1e-6);
        d.scale_loop(0.5);
        assert!((d.loop_beats().unwrap() - 2.0).abs() < 1e-6);
    }

    #[test]
    fn hot_cue_stores_then_jumps_and_plays() {
        let mut d = deck();
        assert!(!d.hot_cue(0), "empty slots do nothing");
        d.step_paused(150.0);
        d.set_hot_cue(0);
        d.step_paused(-150.0);
        assert!(d.hot_cue(0));
        assert!(d.playing);
        assert_eq!(d.pos, 2.0 * SR as f64);
    }

    #[test]
    fn tempo_changes_speed() {
        let mut d = deck();
        d.move_tempo(200.0); // 200 x 0.05% = +10%
        assert!((d.tempo - 10.0).abs() < 1e-9);
        d.play_pause();
        run(&mut d, SR as usize);
        assert!((d.pos - 1.1 * SR as f64).abs() < 2.0, "pos {}", d.pos);
    }

    #[test]
    fn master_tempo_produces_audio() {
        let mut d = deck();
        assert!(d.toggle_master_tempo());
        d.move_tempo(100.0);
        d.play_pause();
        let out = run(&mut d, SR as usize);
        let energy: f32 = out.iter().map(|x| x * x).sum();
        assert!(energy > 1.0);
        assert!(d.pos > 1.05 * SR as f64);
    }

    #[test]
    fn brake_time_slows_gradually() {
        let mut d = deck();
        d.brake_time = 1.0;
        d.play_pause();
        run(&mut d, 4800);
        d.play_pause();
        run(&mut d, SR as usize / 2);
        assert!(d.speed > 0.4 && d.speed < 0.6, "speed {}", d.speed);
        run(&mut d, SR as usize);
        assert_eq!(d.speed, 0.0);
    }

    #[test]
    fn reverse_plays_backwards() {
        let mut d = deck();
        d.step_paused(150.0);
        d.toggle_reverse();
        d.play_pause();
        run(&mut d, 4800);
        assert!((d.pos - (2.0 * SR as f64 - 4800.0)).abs() < 2.0);
    }
}
