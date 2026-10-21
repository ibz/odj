//! The deck: transport, cue, loops, tempo and the audio render path.
//!
//! The UI thread calls the control methods and the audio callback calls
//! `render`, both behind the same mutex. Positions are in source frames.

use std::sync::Arc;

use odj_core::audio::Source;
use odj_core::filter::ThreeBandEq;
use odj_core::grid::BeatGrid;
use odj_core::key::Key;
use odj_core::link::{self, Packet};
use odj_core::memory::{Cue, MAX_MEMORIES, TrackMemory};
use odj_core::track::Track;

use crate::stretch::Stretcher;

/// Length of the crossfade used to hide clicks on jumps and loop wraps.
const XFADE: usize = 96;
/// Largest block fed to the time stretcher.
const MAX_BLOCK: usize = 4096;
/// CD frames per second, the unit of the paused jog and the time display.
pub const CD_FRAMES: f64 = 75.0;
/// A quantized jump pressed this soon after a beat goes at once, as if on it.
const LATE_SECONDS: f64 = 0.02;
/// Beatjump sizes, in beats; whole beats keep the jump in phase.
pub const JUMP_SIZES: [f64; 7] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0];
/// Range of beatloop sizes, in beats.
pub const MIN_LOOP_BEATS: f64 = 1.0 / 32.0;
pub const MAX_LOOP_BEATS: f64 = 64.0;
/// The phase lock changes a follower's speed by at most this much…
const MAX_TRIM: f64 = 0.003;
/// …aiming to correct an error within about this many seconds.
const TRIM_TIME: f64 = 1.0;
/// A follower further out of phase than this, in seconds, jumps back in…
const JUMP_ERROR: f64 = 0.025;
/// …once it has been out that far for this long.
const JUMP_AFTER: f64 = 0.05;
/// How much of each new phase measurement goes into the smoothed error.
const SMOOTHING: f64 = 0.3;

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

/// A jump waiting for the next beat, so a quantized deck stays in phase.
#[derive(Clone, Copy)]
struct Pending {
    to: f64,
    /// Loops from `to` to here once there.
    loop_out: Option<f64>,
    /// The hot cue that asked for it.
    hot: Option<usize>,
}

/// Slip mode: while a slipped action runs, a ghost position carries on where
/// the track would be, and the deck goes back to it when the last one ends.
#[derive(Clone, Copy, Default)]
struct Slip {
    ghost: Option<f64>,
    looping: bool,
    reverse: bool,
    /// A hot cue held down.
    hot: Option<usize>,
    paused: bool,
}

impl Slip {
    fn active(&self) -> bool {
        self.looping || self.reverse || self.hot.is_some() || self.paused
    }
}

/// A loop roll held down. It always slips, with its own ghost, which keeps
/// looping in the loop that was on before the roll.
#[derive(Clone, Copy)]
struct Roll {
    beats: f64,
    ghost: f64,
    /// Loop in, loop out and looping before the roll.
    saved: (Option<f64>, Option<f64>, bool),
}

/// Where the deck is heard: the block being rendered starts coming out of the
/// speakers at `time`, with `pos` heard then, playing at `rate` times normal.
#[derive(Clone, Copy)]
struct Heard {
    time: f64,
    pos: f64,
    rate: f64,
}

/// The other decks, as the sync thread last heard from them.
#[derive(Clone, Default)]
struct LinkState {
    /// This deck's number; None while sync is unavailable.
    deck: Option<u8>,
    master: bool,
    master_since: f64,
    others: Vec<Packet>,
}

/// Sync as the UI shows it.
#[derive(Clone, Default)]
pub struct SyncView {
    pub deck: Option<u8>,
    pub synced: bool,
    pub master: bool,
    /// The master whose tempo this deck follows.
    pub leader: Option<u8>,
    /// Beats are locked too, not only the tempo.
    pub phase_lock: bool,
    /// Play was pressed: the deck starts on the master's beat.
    pub waiting: bool,
    /// The deck beatmatched against, and how far off this deck is, in beats.
    pub phase: Option<(u8, f64)>,
    /// Every other deck, by number, with its beats laid along this deck's
    /// track for the waveform.
    pub lanes: Vec<Lane>,
    pub others: Vec<Packet>,
}

#[derive(Clone, Copy, Debug)]
pub struct Lane {
    pub deck: u8,
    pub master: bool,
    pub playing: bool,
    /// While both decks run.
    pub beats: Option<LaneBeats>,
}

/// Another deck's beats in this deck's track time: its beat `beat` is heard
/// with this deck's second `at`, and each beat after it `period` seconds on.
#[derive(Clone, Copy, Debug)]
pub struct LaneBeats {
    pub beat: f64,
    pub at: f64,
    pub period: f64,
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
    /// The key as heard, and the semitones the tempo moves it.
    pub key: Option<(Key, i32)>,
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
    pub grid: Option<BeatGrid>,
    /// The grid was set by hand rather than detected.
    pub grid_edited: bool,
    /// The beats are worth drawing: steady, or the user cares about the grid.
    pub show_grid: bool,
    pub grid_adjust: bool,
    pub quantize: bool,
    /// A hot cue waiting for the next beat.
    pub pending_hot: Option<usize>,
    pub loop_beats: Option<f64>,
    pub slip: bool,
    /// Where the track would be while slipping or rolling.
    pub ghost: Option<f64>,
    /// The size of the loop roll held down.
    pub roll: Option<f64>,
    pub jump_beats: f64,
    pub loop_size: f64,
    pub sync: SyncView,
    /// The Mixer overlay's live values — see `Deck::volume`/`Deck::low_db`.
    pub volume: f32,
    pub low_db: f32,
    pub mid_db: f32,
    pub high_db: f32,
}

/// Which memory point to call: always the nearest one in that direction,
/// including one the deck is already sitting exactly on — so landing on a
/// memory and immediately calling the same direction again doesn't re-find
/// it, but instead steps on to the next one that way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MemoryCall {
    Left,
    Right,
}

/// Whether `pos` counts as "at" this memory point: for a loop, anywhere
/// inside it, not just exactly at its start — the playhead wanders the
/// whole loop while it plays, so a tight distance check would only ever
/// match for an instant right as it wraps.
fn memory_covers(m: &Cue, pos: f64) -> bool {
    match m.loop_out {
        Some(out) => pos >= m.pos - 1.0 && pos < out + 1.0,
        None => (m.pos - pos).abs() < 1.0,
    }
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
    /// The user's grid, overriding detection.
    grid_override: Option<BeatGrid>,
    /// Quantize as the user set it for this track, overriding the default.
    quantize_override: Option<bool>,
    /// The jog keys shift the grid and the tempo keys change its BPM.
    grid_adjust: bool,
    pending: Option<Pending>,
    slip_mode: bool,
    slip: Slip,
    roll: Option<Roll>,
    /// Beatjump size, in beats.
    pub jump_beats: f64,
    /// Size of the next beatloop, in beats.
    pub loop_size: f64,
    synced: bool,
    link: LinkState,
    /// Following a master: the multiple of its tempo played, 1, 2 or ½.
    sync_ratio: Option<f64>,
    /// The phase lock's speed correction, a factor near 1.
    trim: f64,
    /// How far ahead of the master, in beats, smoothed.
    phase_error: Option<f64>,
    /// When the phase error went over `JUMP_ERROR`.
    far_since: Option<f64>,
    /// Play was pressed on a follower: it starts on the master's beat…
    start_wait: bool,
    /// …heard at this time.
    start_at: Option<f64>,
    heard: Option<Heard>,
    /// Output frames fed to the time stretcher and not yet heard out of it.
    mt_lag: usize,
    xfade: Option<(f64, usize)>,
    gain: f32,
    /// The Mixer overlay's channel fader, 0.0..=1.0 — a live performance
    /// control like tempo/speed, not a saved preference; never touched by
    /// `load()`.
    volume: f32,
    /// Likewise live, not saved, and not reset by `load()`.
    eq: ThreeBandEq,
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
            grid_override: None,
            quantize_override: None,
            grid_adjust: false,
            pending: None,
            slip_mode: false,
            slip: Slip::default(),
            roll: None,
            jump_beats: 4.0,
            loop_size: 4.0,
            synced: false,
            link: LinkState::default(),
            sync_ratio: None,
            trim: 1.0,
            phase_error: None,
            far_since: None,
            start_wait: false,
            start_at: None,
            heard: None,
            mt_lag: 0,
            xfade: None,
            gain: 0.0,
            volume: 1.0,
            eq: ThreeBandEq::new(out_rate),
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
            key: self.heard_key(),
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
            grid: self.grid(),
            grid_edited: self.grid_override.is_some(),
            show_grid: self.grid().is_some()
                && (self.track.as_ref().is_some_and(|t| t.steady)
                    || self.grid_override.is_some()
                    || self.quantize()
                    || self.grid_adjust),
            grid_adjust: self.grid_adjust,
            quantize: self.quantize(),
            pending_hot: self.pending.and_then(|p| p.hot),
            slip: self.slip_mode,
            ghost: self.roll.map(|r| r.ghost).or(self.slip.ghost),
            roll: self.roll.map(|r| r.beats),
            jump_beats: self.jump_beats,
            loop_size: self.loop_size,
            loop_beats: self.loop_beats(),
            sync: SyncView {
                deck: self.link.deck,
                synced: self.synced,
                master: self.link.master,
                leader: self.follows(),
                phase_lock: self.lock_target().is_some(),
                waiting: self.start_wait,
                phase: self.phase(),
                lanes: self.lanes(),
                others: self.link.others.clone(),
            },
            volume: self.volume,
            low_db: self.eq.low_db(),
            mid_db: self.eq.mid_db(),
            high_db: self.eq.high_db(),
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
        self.grid_override = mem.grid(&track);
        self.quantize_override = mem.quantize;
        self.grid_adjust = false;
        self.pending = None;
        self.cancel_slips();
        self.looping = false;
        self.loop_adjust = false;
        self.playing = false;
        self.speed = 0.0;
        self.reverse = false;
        self.cue_preview = false;
        self.start_wait = false;
        self.start_at = None;
        self.unfollow();
        self.xfade = None;
        self.stretch_primed = false;
        let old = self.track.replace(track);
        self.match_leader();
        old
    }

    pub fn memory(&self) -> TrackMemory {
        TrackMemory {
            hot: self.hot,
            memories: self.memories.clone(),
            loop_in: self.loop_in,
            loop_out: self.loop_out,
            grid: self.grid_override,
            bpm: None,
            quantize: self.quantize_override,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn is_reverse(&self) -> bool {
        self.reverse
    }

    pub fn position(&self) -> f64 {
        self.pos
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

    /// The track's key as heard, and how many semitones the tempo moves it:
    /// without Master Tempo, the pitch goes with the speed.
    pub fn heard_key(&self) -> Option<(Key, i32)> {
        let key = self.track.as_ref()?.key?;
        let shift = if self.master_tempo { 0 } else { (12.0 * self.tempo_factor().log2()).round() as i32 };
        Some((key.shifted(shift), shift))
    }

    fn tempo_factor(&self) -> f64 {
        1.0 + self.tempo / 100.0
    }

    /// Playing speed relative to the track's own: tempo, jog and phase trim.
    fn pitch(&self) -> f64 {
        self.tempo_factor() * (1.0 + self.bend / 100.0) * self.trim
    }

    /// Source frames per output frame at normal speed.
    fn sr_ratio(&self) -> f64 {
        self.sample_rate() / self.out_rate as f64
    }

    /// The beat grid: the user's, else the detected one.
    pub fn grid(&self) -> Option<BeatGrid> {
        self.grid_override.or(self.track.as_ref()?.grid)
    }

    /// The track's own BPM, before the pitch fader.
    pub fn bpm(&self) -> Option<f64> {
        self.grid().map(|g| g.bpm)
    }

    fn beat_frames(&self) -> Option<f64> {
        self.grid().map(|g| g.period(self.sample_rate()))
    }

    pub fn stretch_latency_ms(&self) -> Option<f64> {
        self.stretch.as_ref().map(|s| s.latency() as f64 * 1000.0 / self.out_rate as f64)
    }

    /// Where a quantized action lands: the nearest beat, or `pos` itself.
    fn snap(&self, pos: f64) -> f64 {
        match self.grid() {
            Some(g) if self.quantize() => g.nearest(pos, self.sample_rate()).clamp(0.0, self.last_frame()),
            _ => pos,
        }
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
        self.start_wait = false;
        self.start_at = None;
    }

    fn stop_now(&mut self) {
        self.playing = false;
        self.speed = 0.0;
        self.start_wait = false;
        self.start_at = None;
    }

    /// Starts at once, or a follower, on the master's next beat.
    fn start_on_beat(&mut self) {
        if self.lock_target().is_some() && !self.reverse {
            self.start_wait = true;
        } else {
            self.start_now();
        }
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
        if self.start_wait {
            // Again before the beat: stays paused.
            self.start_wait = false;
            self.start_at = None;
        } else if self.playing {
            self.playing = false;
            self.pending = None;
            self.begin_slip(|s| s.paused = true);
        } else if self.slip.paused {
            self.playing = true;
            self.end_slip(|s| s.paused = false, true);
        } else if self.lock_target().is_some() && !self.reverse {
            self.start_wait = true;
        } else {
            self.playing = true;
        }
    }

    /// The cue button. `can_hold` is false when key releases can't be seen,
    /// which disables the hold-to-preview behaviour.
    pub fn cue_down(&mut self, can_hold: bool) {
        if self.track.is_none() || self.cue_preview {
            return;
        }
        self.pending = None;
        self.start_wait = false;
        self.start_at = None;
        self.cancel_slips();
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
            self.cue = self.snap(self.pos);
            self.pos = self.cue;
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

    /// Reverse; in slip mode, going forward again goes back to the ghost.
    pub fn toggle_reverse(&mut self) {
        self.reverse = !self.reverse;
        if self.reverse && self.playing {
            self.begin_slip(|s| s.reverse = true);
        } else if !self.reverse {
            self.end_slip(|s| s.reverse = false, true);
        }
    }

    /// Moves the paused playhead, like turning the jog in pause mode.
    pub fn step_paused(&mut self, cd_frames: f64) {
        if !self.playing {
            let delta = cd_frames * self.sample_rate() / CD_FRAMES;
            self.pos = (self.pos + delta).clamp(0.0, self.last_frame());
        }
    }

    /// Moves the paused playhead by beats: along the grid when quantizing.
    pub fn step_beats_paused(&mut self, beats: i64) {
        match self.grid() {
            Some(g) if self.quantize() && !self.playing => {
                self.pos = g.step(self.pos, beats, self.sample_rate()).clamp(0.0, self.last_frame());
            }
            _ => {
                let frames = self.beat_frames().unwrap_or(self.sample_rate());
                self.step_paused(beats as f64 * frames * CD_FRAMES / self.sample_rate());
            }
        }
    }

    // --- Hot cues ----------------------------------------------------------

    /// Stores (or overwrites) a hot cue: the active loop, else the playhead.
    pub fn set_hot_cue(&mut self, i: usize) {
        if self.track.is_none() {
            return;
        }
        self.hot[i] = Some(match (self.looping, self.loop_in, self.loop_out) {
            (true, Some(pos), Some(out)) => Cue { pos, loop_out: Some(out) },
            _ => Cue { pos: self.snap(self.pos), loop_out: None },
        });
    }

    /// Jumps to a hot cue and plays. Returns false if the slot is empty.
    /// Quantized while playing, the jump waits for the next beat. In slip
    /// mode a `held` hot cue plays until `hot_cue_up`, then back to the ghost.
    pub fn hot_cue(&mut self, i: usize, held: bool) -> bool {
        let Some(h) = self.hot[i] else { return false };
        self.cue_preview = false;
        self.loop_adjust = false;
        self.roll = None;
        if self.playing {
            if held {
                self.begin_slip(|s| s.hot = Some(i));
            }
            self.jump_in_phase(h.pos, h.loop_out, Some(i));
        } else {
            self.pending = None;
            self.enter(h.pos, h.loop_out, 0.0);
            self.start_on_beat();
        }
        true
    }

    /// Jumps now, or when quantizing, on the next beat; a beat that has only
    /// just gone counts as now, the jump landing as far past `to`.
    fn jump_in_phase(&mut self, to: f64, loop_out: Option<f64>, hot: Option<usize>) {
        self.pending = None;
        if let Some(g) = self.grid().filter(|_| self.quantize()) {
            let sr = self.sample_rate();
            let late = self.pos - g.beat_pos(g.beat_at(self.pos, sr).floor(), sr);
            if self.reverse || late >= LATE_SECONDS * sr {
                self.pending = Some(Pending { to, loop_out, hot });
                return;
            }
            return self.enter(to, loop_out, late);
        }
        self.enter(to, loop_out, 0.0);
    }

    /// The hot cue key let go: in slip, back to where the track would be.
    pub fn hot_cue_up(&mut self, i: usize) {
        if self.slip.hot != Some(i) {
            return;
        }
        if self.pending.is_some_and(|p| p.hot == Some(i)) {
            // Let go before the beat: nothing happened yet.
            self.pending = None;
            return self.end_slip(|s| s.hot = None, false);
        }
        // Leave a loop the hot cue started too.
        self.looping = false;
        self.end_slip(
            |s| {
                s.hot = None;
                s.looping = false;
            },
            true,
        );
    }

    /// Goes to `to` plus `offset`, looping to `loop_out` if given.
    fn enter(&mut self, to: f64, loop_out: Option<f64>, offset: f64) {
        match loop_out {
            Some(out) => {
                self.loop_in = Some(to);
                self.loop_out = Some(out);
                self.set_looping(true, false);
            }
            None => self.set_looping(false, false),
        }
        self.jump(to + offset);
    }

    // --- Slip --------------------------------------------------------------------

    pub fn toggle_slip(&mut self) -> bool {
        self.slip_mode = !self.slip_mode;
        if !self.slip_mode {
            self.slip = Slip::default();
        }
        self.slip_mode
    }

    /// Starts a slipped action, the ghost starting here unless already running.
    fn begin_slip(&mut self, mark: impl FnOnce(&mut Slip)) {
        if !self.slip_mode {
            return;
        }
        self.slip.ghost.get_or_insert(self.pos);
        mark(&mut self.slip);
    }

    /// Ends a slipped action; once none is left, back to the ghost if `back`.
    fn end_slip(&mut self, clear: impl FnOnce(&mut Slip), back: bool) {
        clear(&mut self.slip);
        if self.slip.active() {
            return;
        }
        if let Some(ghost) = self.slip.ghost.take()
            && back
        {
            self.jump(ghost);
        }
    }

    /// Drops any slip and roll without going back.
    fn cancel_slips(&mut self) {
        self.slip = Slip::default();
        self.roll = None;
    }

    /// Turns the loop on or off; in slip mode a loop slips, and leaving it
    /// goes back to the ghost if `back`.
    fn set_looping(&mut self, on: bool, back: bool) {
        if on && !self.looping && self.playing {
            self.begin_slip(|s| s.looping = true);
        } else if !on && self.looping {
            self.end_slip(|s| s.looping = false, back);
        }
        self.looping = on;
    }

    /// Holds a loop roll of `beats` from the last step of that size before
    /// where the track is; another size while held switches to it.
    /// False without a grid.
    pub fn roll_down(&mut self, beats: f64) -> bool {
        let Some(g) = self.grid() else { return false };
        if !self.playing {
            return true;
        }
        self.pending = None;
        let ghost = match &mut self.roll {
            Some(r) => {
                r.beats = beats;
                r.ghost
            }
            None => {
                let saved = (self.loop_in, self.loop_out, self.looping);
                self.roll = Some(Roll { beats, ghost: self.pos, saved });
                self.pos
            }
        };
        let step = g.period(self.sample_rate()) * beats;
        let start = g.anchor + ((ghost - g.anchor) / step).floor() * step;
        self.loop_in = Some(start);
        self.loop_out = Some(start + step);
        self.looping = true;
        if self.pos < start || self.pos >= start + step {
            self.jump(ghost);
        }
        true
    }

    /// Lets go of the roll: the loop from before comes back and the deck goes
    /// to where the track would be.
    pub fn roll_up(&mut self) {
        let Some(roll) = self.roll.take() else { return };
        (self.loop_in, self.loop_out, self.looping) = roll.saved;
        self.jump(roll.ghost);
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
        self.push_memory(entry)
    }

    /// Promotes the last loop set (by `I`/`O`/`L`), in or out of it, to a
    /// memory point — unlike `store_memory`, this works even once you've
    /// left it, since `loop_in`/`loop_out` (what Reloop-Exit reuses) hold
    /// only the one most recent loop.
    pub fn store_last_loop(&mut self) -> Option<usize> {
        self.track.as_ref()?;
        let (pos, out) = (self.loop_in?, self.loop_out?);
        self.push_memory(Cue { pos, loop_out: Some(out) })
    }

    fn push_memory(&mut self, entry: Cue) -> Option<usize> {
        if self.memories.len() >= MAX_MEMORIES
            || self.memories.iter().any(|m| (m.pos - entry.pos).abs() < 1.0)
        {
            return None;
        }
        self.memories.push(entry);
        self.memories.sort_by(|a, b| a.pos.total_cmp(&b.pos));
        self.memories.iter().position(|m| *m == entry).map(|i| i + 1)
    }

    /// Calls a memory point. Playing, the deck jumps there and plays on, on
    /// the next beat when quantizing; paused, it becomes the cue point and the
    /// deck waits there. A stored loop loops.
    pub fn call_memory(&mut self, which: MemoryCall) -> Option<usize> {
        // From a jump still waiting for its beat, so presses add up.
        let from = self.pending.map_or(self.pos, |p| p.to);
        let i = match which {
            // Strictly before: excludes one already sat on (within a
            // frame), so a repeat press steps to the next one back
            // instead of re-finding the same one.
            MemoryCall::Left => self.memories.iter().rposition(|m| m.pos < from - 1.0)?,
            MemoryCall::Right => self.memories.iter().position(|m| m.pos > from + 1.0)?,
        };
        let m = self.memories[i];
        self.loop_adjust = false;
        self.cancel_slips();
        if self.playing && !self.cue_preview {
            self.jump_in_phase(m.pos, m.loop_out, None);
            return Some(i + 1);
        }
        // Paused, or previewing the cue: that ends here.
        self.cue_preview = false;
        self.pending = None;
        self.jump(m.pos);
        self.stop_now();
        self.cue = m.pos;
        if let Some(out) = m.loop_out {
            self.loop_in = Some(m.pos);
            self.loop_out = Some(out);
            self.looping = true;
        }
        Some(i + 1)
    }

    /// The memory point at the current position, if any — what
    /// `delete_memory` would remove, without removing it. Keyed off `pos`,
    /// not `cue`: calling a memory while playing jumps `pos` there without
    /// moving `cue` (see `call_memory`), so using `cue` here would make
    /// deleting a loop you just jumped to silently fail while playing.
    pub fn memory_at_pos(&self) -> Option<Cue> {
        let pos = self.pos;
        self.memories.iter().copied().find(|m| memory_covers(m, pos))
    }

    /// Deletes the memory point at the current position.
    pub fn delete_memory(&mut self) -> bool {
        let before = self.memories.len();
        let pos = self.pos;
        self.memories.retain(|m| !memory_covers(m, pos));
        self.memories.len() != before
    }

    // --- Loops ---------------------------------------------------------------

    pub fn loop_in(&mut self) {
        if self.track.is_none() {
            return;
        }
        let at = self.snap(self.pos);
        self.loop_in = Some(at);
        self.loop_out = None;
        self.set_looping(false, false);
        self.loop_adjust = false;
        self.cue = at;
    }

    /// Sets the loop out point, or while looping toggles out-point adjustment.
    /// Quantized, it goes on the nearest beat at least a beat after the in point.
    pub fn loop_out(&mut self) {
        if self.looping && self.loop_out.is_some() {
            self.loop_adjust = !self.loop_adjust;
            self.grid_adjust &= !self.loop_adjust;
            return;
        }
        let Some(start) = self.loop_in else { return };
        match self.beat_frames().filter(|_| self.quantize()) {
            Some(beat) => {
                if self.pos + beat / 2.0 < start || start + beat > self.last_frame() {
                    return;
                }
                let end = self.snap(self.pos).max(start + beat);
                self.loop_out = Some(end);
                self.set_looping(true, false);
                // Past the out point already: carry on from as far into the loop.
                if self.pos >= end {
                    self.jump(start + (self.pos - end));
                }
            }
            None if self.pos > start + self.sample_rate() * 0.01 => {
                self.loop_out = Some(self.pos);
                self.set_looping(true, false);
                self.jump(start);
            }
            None => {}
        }
    }

    /// Exits the loop, or goes back into the last one (on the beat when quantizing).
    pub fn reloop_exit(&mut self) {
        self.loop_adjust = false;
        if self.looping {
            self.set_looping(false, true);
            self.pending = None;
        } else if let (Some(start), Some(end)) = (self.loop_in, self.loop_out) {
            if self.playing {
                self.jump_in_phase(start, Some(end), None);
            } else {
                self.enter(start, Some(end), 0.0);
            }
        }
    }

    /// A loop of `beats` from the playhead, or quantized, from the nearest
    /// beat, or for loops under a beat, the nearest step of their size.
    pub fn auto_loop(&mut self, beats: f64) -> bool {
        let Some(beat) = self.beat_frames() else { return false };
        let start = match self.grid() {
            Some(g) if self.quantize() => {
                let step = beat * beats.min(1.0);
                (g.anchor + ((self.pos - g.anchor) / step).round() * step).max(0.0)
            }
            _ => self.pos,
        };
        let end = (start + beats * beat).min(self.last_frame());
        self.loop_in = Some(start);
        self.loop_out = Some(end);
        self.set_looping(true, false);
        self.cue = start;
        true
    }

    /// The L key: a beatloop of the loop size, or out of the loop.
    pub fn beat_loop(&mut self) -> bool {
        if self.looping {
            self.set_looping(false, true);
            self.pending = None;
            return true;
        }
        self.auto_loop(self.loop_size)
    }

    /// Halves (0.5) or doubles (2.0) the roll held down or the running loop,
    /// else the next beatloop. The loop size follows. `from_left` extends
    /// or shrinks from the in point (keeping the out point fixed) instead
    /// of the usual the other way around — meaningless for a roll or the
    /// next beatloop's size, since neither has an in/out point yet.
    pub fn resize_loop(&mut self, factor: f64, from_left: bool) {
        if let Some(r) = self.roll {
            self.loop_size = (r.beats * factor).clamp(MIN_LOOP_BEATS, MAX_LOOP_BEATS);
            self.roll_down(self.loop_size);
            return;
        }
        if !self.looping {
            self.loop_size = (self.loop_size * factor).clamp(MIN_LOOP_BEATS, MAX_LOOP_BEATS);
            return;
        }
        if from_left {
            self.scale_loop_left(factor);
        } else {
            self.scale_loop(factor);
        }
        // A loop of a power-of-two beats becomes the size of the next one.
        if let Some(beats) = self.loop_beats() {
            let p = beats.log2().round();
            if (beats - p.exp2()).abs() < 1e-3 && (MIN_LOOP_BEATS..=MAX_LOOP_BEATS).contains(&p.exp2()) {
                self.loop_size = p.exp2();
            }
        }
    }

    /// Jumps `dir` (±1) times the beatjump size; while looping the loop moves
    /// along. False without a grid.
    pub fn beat_jump(&mut self, dir: f64) -> bool {
        let Some(beat) = self.beat_frames() else { return false };
        let d = dir * self.jump_beats * beat;
        if self.looping
            && let (Some(a), Some(b)) = (self.loop_in, self.loop_out)
        {
            if a + d < 0.0 || b + d > self.last_frame() {
                return true;
            }
            self.loop_in = Some(a + d);
            self.loop_out = Some(b + d);
        }
        self.jump(self.pos + d);
        true
    }

    /// Next (up) or previous beatjump size.
    pub fn resize_jump(&mut self, up: bool) {
        let i = JUMP_SIZES.iter().position(|&s| s == self.jump_beats).unwrap_or(2);
        let i = if up { (i + 1).min(JUMP_SIZES.len() - 1) } else { i.saturating_sub(1) };
        self.jump_beats = JUMP_SIZES[i];
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

    /// Like `scale_loop`, but keeps the out point fixed and moves the in
    /// point instead — extends or shrinks the loop to the left rather than
    /// the right.
    pub fn scale_loop_left(&mut self, factor: f64) {
        let (Some(start), Some(end)) = (self.loop_in, self.loop_out) else { return };
        let len = ((end - start) * factor).max(self.sample_rate() * 0.01);
        let new_start = end - len;
        if new_start < 0.0 {
            return;
        }
        self.loop_in = Some(new_start);
        if self.looping && self.pos < new_start {
            self.jump(end - (end - self.pos) % len);
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

    // --- Beat grid and quantize ------------------------------------------------

    /// Quantizing: as set for the track, and always while synced, so loops
    /// and jumps keep the deck in phase.
    pub fn quantize(&self) -> bool {
        let steady = self.track.as_ref().is_some_and(|t| t.steady);
        self.grid().is_some() && (self.synced || self.quantize_override.unwrap_or(steady))
    }

    /// Turns quantize on or off for this track; None without a grid.
    pub fn toggle_quantize(&mut self) -> Option<bool> {
        self.grid()?;
        let on = !self.quantize();
        self.quantize_override = Some(on);
        if !on {
            self.pending = None;
        }
        Some(on)
    }

    /// Sets the BPM from a tempo tapped while paused, heard after the pitch
    /// fader; the beat nearest the playhead stays where it is.
    pub fn set_tapped_bpm(&mut self, heard: f64) {
        let bpm = heard / self.tempo_factor();
        let sr = self.sample_rate();
        let grid = match self.grid() {
            Some(g) => g.with_bpm(bpm, self.pos, sr),
            None => BeatGrid { bpm, anchor: self.pos },
        };
        self.grid_override = Some(grid);
    }

    /// Sets the grid from taps along the track, at least two, the last on a
    /// beat; the bars keep their place. Returns the BPM as heard.
    pub fn set_tapped_beats(&mut self, taps: &[f64]) -> Option<f64> {
        let n = taps.len() as f64;
        let mk = (n - 1.0) / 2.0;
        let mt = taps.iter().sum::<f64>() / n;
        let var: f64 = (0..taps.len()).map(|k| (k as f64 - mk).powi(2)).sum();
        let cov: f64 = taps.iter().enumerate().map(|(k, t)| (k as f64 - mk) * (t - mt)).sum();
        let period = cov / var;
        if !period.is_finite() || period <= 0.0 {
            return None;
        }
        let sr = self.sample_rate();
        let last = mt + period * (n - 1.0 - mk);
        let bpm = 60.0 * sr / period;
        let bar_phase = self.grid().map_or(0.0, |g| g.beat_at(last, sr).round().rem_euclid(4.0));
        self.grid_override = Some(BeatGrid { bpm, anchor: last - bar_phase * period });
        Some(bpm * self.tempo_factor())
    }

    /// Makes the playhead beat 1 of a bar. False without a grid.
    pub fn set_downbeat(&mut self) -> bool {
        let Some(g) = self.grid() else { return false };
        self.grid_override = Some(BeatGrid { anchor: self.pos, ..g });
        true
    }

    /// Back to the detected grid.
    pub fn reset_grid(&mut self) {
        self.grid_override = None;
    }

    /// Enters or leaves grid adjustment; false without a grid.
    pub fn toggle_grid_adjust(&mut self) -> bool {
        if self.grid().is_none() {
            self.grid_adjust = false;
            return false;
        }
        self.grid_adjust = !self.grid_adjust;
        self.loop_adjust &= !self.grid_adjust;
        true
    }

    pub fn is_adjusting_grid(&self) -> bool {
        self.grid_adjust
    }

    /// Moves the grid later (positive) or earlier.
    pub fn shift_grid(&mut self, ms: f64) {
        if let Some(g) = self.grid() {
            self.grid_override = Some(g.shifted(ms * self.sample_rate() / 1000.0));
        }
    }

    /// Changes the grid's BPM, keeping the beat nearest the playhead in place.
    pub fn nudge_grid_bpm(&mut self, delta: f64) {
        if let Some(g) = self.grid() {
            let bpm = ((g.bpm + delta) * 1000.0).round() / 1000.0;
            if bpm > 20.0 {
                self.grid_override = Some(g.with_bpm(bpm, self.pos, self.sample_rate()));
            }
        }
    }

    // --- Sync ------------------------------------------------------------------

    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Turns sync on or off; None without a grid. Off, the tempo stays where
    /// sync put it, on a range wide enough to hold it.
    pub fn toggle_sync(&mut self) -> Option<bool> {
        if self.synced {
            self.synced = false;
            self.unfollow();
            if self.start_wait {
                self.start_now();
            }
            while self.tempo.abs() > self.range.percent() {
                self.range = self.range.next();
            }
            return Some(false);
        }
        self.grid()?;
        self.synced = true;
        self.match_leader();
        Some(true)
    }

    /// Makes this deck the master; false without a grid.
    pub fn claim_master(&mut self, now: f64) -> bool {
        if self.grid().is_none() {
            return false;
        }
        if !self.link.master {
            self.link.master = true;
            self.link.master_since = now;
            self.unfollow();
        }
        true
    }

    /// The deck whose tempo this one follows.
    pub fn follows(&self) -> Option<u8> {
        self.sync_ratio.and(self.leader()).map(|p| p.deck)
    }

    /// The master, when synced and another deck is it.
    fn leader(&self) -> Option<&Packet> {
        if !self.synced || self.link.master {
            return None;
        }
        link::leader(&self.link.others)
    }

    /// The master to lock beats to, and the multiple of its tempo played:
    /// running, with steady grids on both decks.
    fn lock_target(&self) -> Option<(Packet, f64)> {
        let lead = *self.leader()?;
        let ratio = self.sync_ratio?;
        (lead.running && lead.steady && self.steady() && self.grid().is_some()).then_some((lead, ratio))
    }

    /// The beats follow the music closely enough to lock to: detected so, or
    /// set by hand.
    fn steady(&self) -> bool {
        self.grid_override.is_some() || self.track.as_ref().is_some_and(|t| t.steady)
    }

    /// Playing forward at speed through whole beats: a beat clock to lock to.
    fn running(&self) -> bool {
        let whole = |b: f64| b > 0.999 && (b - b.round()).abs() < 1e-3;
        self.playing
            && !self.reverse
            && self.speed == 1.0
            && self.search == 0.0
            && self.grid().is_some()
            && (!self.looping || self.loop_beats().is_some_and(whole))
    }

    /// One round of sync, from the sync thread: takes in the other decks,
    /// settles who is master, follows the master's tempo, and returns what to
    /// tell the others.
    pub fn update_link(&mut self, deck: u8, others: &[Packet], now: f64) -> Packet {
        self.link.deck = Some(deck);
        self.link.others = others.to_vec();
        let master = link::elect(&self.packet(now), others);
        if master != self.link.master {
            self.link.master = master;
            self.link.master_since = now;
        }
        self.match_leader();
        self.packet(now)
    }

    /// Sets the tempo so this deck is heard at the master's, or half or
    /// double that, whichever is nearer.
    fn match_leader(&mut self) {
        let (Some(target), Some(bpm)) = (self.leader().and_then(|p| p.bpm), self.bpm()) else {
            return self.unfollow();
        };
        let (factor, ratio) = link::match_tempo(bpm, target);
        self.tempo = (factor - 1.0) * 100.0;
        self.sync_ratio = Some(ratio);
    }

    fn unfollow(&mut self) {
        self.sync_ratio = None;
        self.trim = 1.0;
        self.phase_error = None;
        self.far_since = None;
    }

    /// What this deck tells the others.
    fn packet(&self, now: f64) -> Packet {
        let h = self.heard.unwrap_or(Heard { time: now, pos: self.pos, rate: 0.0 });
        let grid = self.grid();
        Packet {
            deck: self.link.deck.unwrap_or(0),
            playing: self.playing,
            synced: self.synced,
            master: self.link.master,
            steady: self.steady(),
            running: self.running(),
            master_since: self.link.master_since,
            bpm: self.bpm().map(|b| b * self.tempo_factor()),
            beat: grid.map_or(0.0, |g| g.beat_at(h.pos, self.sample_rate())),
            rate: grid.map_or(0.0, |g| g.bpm / 60.0 * h.rate),
            time: h.time,
            key: self.heard_key().map(|(k, _)| k),
        }
    }

    /// How far this deck's beat is from the master's, or else another
    /// playing deck's, in beats: the phase meter, also for beatmatching by ear.
    fn phase(&self) -> Option<(u8, f64)> {
        let (h, g, other) = (self.heard?, self.grid()?, self.beatmatched()?);
        let ratio = match self.sync_ratio {
            Some(ratio) => ratio,
            None => link::match_tempo(g.bpm * self.tempo_factor(), other.bpm?).1,
        };
        let beat = g.beat_at(h.pos, self.sample_rate());
        Some((other.deck, link::wrap(beat - ratio * other.beat_at(h.time))))
    }

    /// The deck to beatmatch against: the leader, else the playing master,
    /// else any deck playing; only while both play.
    fn beatmatched(&self) -> Option<&Packet> {
        let others = &self.link.others;
        self.leader()
            .or_else(|| others.iter().find(|p| p.master && p.running))
            .or_else(|| others.iter().find(|p| p.running))
            .filter(|p| p.running && self.playing)
    }

    fn lanes(&self) -> Vec<Lane> {
        let heard = self.heard.filter(|h| self.playing && h.rate > 0.0);
        let mut lanes: Vec<Lane> = self
            .link
            .others
            .iter()
            .map(|p| Lane {
                deck: p.deck,
                master: p.master,
                playing: p.playing,
                beats: heard.filter(|_| p.running && p.rate > 0.0).map(|h| LaneBeats {
                    beat: p.beat_at(h.time),
                    at: h.pos / self.sample_rate(),
                    period: h.rate / p.rate,
                }),
            })
            .collect();
        lanes.sort_by_key(|l| l.deck);
        lanes
    }

    /// Where the deck is heard as the block starting at `t` is: behind the
    /// playhead by what is still inside the time stretcher.
    fn note_heard(&mut self, t: f64) {
        let rate = self.speed * self.pitch() + self.search;
        let lag = self.mt_lag as f64 * rate * self.sr_ratio();
        self.heard = Some(Heard { time: t, pos: self.pos - lag, rate });
    }

    /// Keeps a follower in phase with the master, by trimming its speed or,
    /// far out, by jumping; and times a start waiting for the beat.
    fn follow(&mut self, t: f64) {
        if self.start_wait {
            // A frame generated now is heard after what is in the stretcher.
            self.start_at = self.beat_start(t + self.mt_lag as f64 / self.out_rate as f64);
            if self.start_at.is_none() {
                self.start_now();
            }
        }
        let locked = self.lock_target().filter(|_| self.running() && self.bend == 0.0 && !self.cue_preview);
        let (Some((lead, ratio)), Some(h), Some(g)) = (locked, self.heard, self.grid()) else {
            self.trim = 1.0;
            self.phase_error = None;
            self.far_since = None;
            return;
        };
        let sr = self.sample_rate();
        let measured = link::wrap(g.beat_at(h.pos, sr) - ratio * lead.beat_at(t));
        let error = match self.phase_error {
            Some(e) => link::wrap(e + link::wrap(measured - e) * SMOOTHING),
            None => measured,
        };
        self.phase_error = Some(error);
        let seconds = error * 60.0 / (g.bpm * self.tempo_factor());
        if seconds.abs() <= JUMP_ERROR {
            self.far_since = None;
        } else if t - *self.far_since.get_or_insert(t) >= JUMP_AFTER {
            self.shift_by(-measured * g.period(sr));
            self.trim = 1.0;
            self.phase_error = None;
            self.far_since = None;
            return;
        }
        self.trim = 1.0 - (seconds / TRIM_TIME).clamp(-MAX_TRIM, MAX_TRIM);
    }

    /// When a follower waiting at its playhead should be heard starting so
    /// its beat meets the master's, at the earliest at `t`; a beat just gone
    /// still counts, so a start due at the end of a block isn't put off.
    fn beat_start(&self, t: f64) -> Option<f64> {
        let (lead, ratio) = self.lock_target().filter(|(l, _)| l.rate > 0.0)?;
        let b = self.grid()?.beat_at(self.pos, self.sample_rate());
        let n = (ratio * lead.beat_at(t - 0.001) - b).ceil();
        Some(lead.time + ((b + n) / ratio - lead.beat) / lead.rate)
    }

    /// Moves the playhead by `d` frames to get back in phase, staying in the
    /// loop, the slip ghosts moving along.
    fn shift_by(&mut self, d: f64) {
        let mut to = self.pos + d;
        if self.looping
            && let (Some(a), Some(b)) = (self.loop_in, self.loop_out)
        {
            if to >= b {
                to -= b - a;
            } else if to < a {
                to += b - a;
            }
        }
        self.jump(to);
        if let Some(g) = self.slip.ghost.as_mut() {
            *g += d;
        }
        if let Some(r) = self.roll.as_mut() {
            r.ghost += d;
        }
    }

    // --- Tempo ---------------------------------------------------------------

    /// Moves the tempo fader; false while following a master's tempo.
    pub fn move_tempo(&mut self, steps: f64) -> bool {
        if self.follows().is_some() {
            return false;
        }
        let step = self.range.step();
        let max = self.range.percent();
        self.tempo = ((self.tempo + steps * step) / step).round() * step;
        self.tempo = self.tempo.clamp(-max, max);
        true
    }

    pub fn reset_tempo(&mut self) -> bool {
        if self.follows().is_some() {
            return false;
        }
        self.tempo = 0.0;
        true
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

    // --- Mixer: volume and 3-band EQ --------------------------------------------

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
    }

    pub fn low_db(&self) -> f32 {
        self.eq.low_db()
    }

    pub fn set_low_db(&mut self, db: f32) {
        self.eq.set_low_db(db);
    }

    pub fn mid_db(&self) -> f32 {
        self.eq.mid_db()
    }

    pub fn set_mid_db(&mut self, db: f32) {
        self.eq.set_mid_db(db);
    }

    pub fn high_db(&self) -> f32 {
        self.eq.high_db()
    }

    pub fn set_high_db(&mut self, db: f32) {
        self.eq.set_high_db(db);
    }

    // --- Audio -----------------------------------------------------------------

    /// Fills `out` (interleaved stereo) with the next block of audio.
    #[cfg(test)]
    pub fn render(&mut self, out: &mut [f32]) {
        self.render_at(out, None);
    }

    /// Like `render`, knowing when the block will be heard, which sync needs.
    pub fn render_at(&mut self, out: &mut [f32], heard_at: Option<f64>) {
        let stretched = self.master_tempo && self.stretch.is_some();
        if !stretched {
            self.mt_lag = 0;
        }
        match heard_at {
            Some(t) => {
                self.note_heard(t);
                self.follow(t);
            }
            None if self.start_wait => self.start_now(),
            None => {}
        }
        let n = out.len() / 2;
        if self.out_l.len() < n {
            self.out_l.resize(n, 0.0);
            self.out_r.resize(n, 0.0);
        }
        let mut l = std::mem::take(&mut self.out_l);
        let mut r = std::mem::take(&mut self.out_r);
        if stretched {
            self.render_stretched(&mut l[..n], &mut r[..n], heard_at);
        } else {
            self.stretch_primed = false;
            self.generate(&mut l[..n], &mut r[..n], heard_at);
        }
        self.eq.process_stereo(&mut l[..n], &mut r[..n]);
        if self.volume != 1.0 {
            for x in &mut l[..n] {
                *x *= self.volume;
            }
            for x in &mut r[..n] {
                *x *= self.volume;
            }
        }
        for (i, frame) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            frame[0] = l[i];
            frame[1] = r[i];
        }
        self.out_l = l;
        self.out_r = r;
    }

    /// Varispeed playback, then Rubber Band shifts the key back to the original.
    fn render_stretched(&mut self, l: &mut [f32], r: &mut [f32], heard_at: Option<f64>) {
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
            self.mt_lag = 0;
            self.stretch_primed = true;
        }
        st.set_pitch_scale(1.0 / self.tempo_factor());

        let n = l.len();
        let mut done = 0;
        while done < n {
            let available = st.available();
            if available == 0 {
                let k = st.samples_required().clamp(64, MAX_BLOCK);
                let heard = heard_at.map(|t| t + (done + self.mt_lag) as f64 / self.out_rate as f64);
                self.generate(&mut gl[..k], &mut gr[..k], heard);
                st.process(&gl[..k], &gr[..k]);
                self.mt_lag += k;
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
                self.mt_lag = self.mt_lag.saturating_sub(got);
            }
        }
        l[done..].fill(0.0);
        r[done..].fill(0.0);

        self.gen_l = gl;
        self.gen_r = gr;
        self.stretch = Some(st);
    }

    /// Reads the track at the current speed with cubic interpolation, running
    /// the motor ramp, loops and end-of-track handling frame by frame. The
    /// first frame is heard at `heard`, if known.
    fn generate(&mut self, l: &mut [f32], r: &mut [f32], heard: Option<f64>) {
        let Some(track) = self.track.clone() else {
            l.fill(0.0);
            r.fill(0.0);
            return;
        };
        let samples = &track.samples;
        let frames = track.frames();
        let last = self.last_frame();
        let sr_ratio = track.sample_rate as f64 / self.out_rate as f64;
        let pitch = self.pitch();
        let mut target = if self.playing { self.direction() } else { 0.0 };
        let out_rate = self.out_rate as f64;
        let ramp = |seconds: f64| {
            if seconds <= 0.0 { f64::INFINITY } else { 1.0 / (seconds * self.out_rate as f64) }
        };
        let (up, down) = (ramp(self.start_time), ramp(self.brake_time));
        let grid = self.grid();
        let track_rate = track.sample_rate as f64;
        // The ghosts move on at playing speed, forward unless the deck was
        // already in reverse before the slip.
        let ghost_step = pitch * sr_ratio;
        let dir = self.direction();
        let slip_dir = if self.slip.reverse { 1.0 } else { dir };

        for i in 0..l.len() {
            if let (Some(at), Some(t)) = (self.start_at, heard)
                && t + i as f64 / out_rate >= at
            {
                self.start_now();
                target = self.direction();
            }
            if (self.playing || self.slip.paused)
                && let Some(g) = self.slip.ghost.as_mut()
            {
                *g = (*g + slip_dir * ghost_step).clamp(0.0, last);
            }
            if self.playing
                && let Some(r) = self.roll.as_mut()
            {
                let prev = r.ghost;
                r.ghost += dir * ghost_step;
                if let (Some(a), Some(b), true) = r.saved
                    && prev < b
                    && r.ghost >= b
                {
                    r.ghost -= b - a;
                }
                r.ghost = r.ghost.clamp(0.0, last);
            }
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
            if let (Some(p), Some(g)) = (self.pending, grid) {
                let (b0, b1) = (g.beat_at(prev, track_rate).floor(), g.beat_at(self.pos, track_rate).floor());
                if b0 != b1 {
                    let beat = g.beat_pos(b0.max(b1), track_rate);
                    self.pending = None;
                    self.enter(p.to, p.loop_out, self.pos - beat);
                    continue;
                }
            }
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
    fn render(&mut self, out: &mut [f32], heard_at: f64) {
        self.render_at(out, Some(heard_at));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use odj_core::track::test_util::click_track;

    const SR: u32 = 48_000;

    /// A steady 120 BPM track, beats at 0.25 s + n × 0.5 s, unquantized.
    fn deck() -> Deck {
        deck_with(Some(false))
    }

    fn deck_with(quantize: Option<bool>) -> Deck {
        let samples = click_track(120.0, 20.0, SR);
        let track = Track::from_samples("t".into(), "t".into(), None, SR, samples);
        let mut d = Deck::new(SR);
        d.load(Arc::new(track), &TrackMemory { quantize, ..TrackMemory::default() }, false);
        d
    }

    /// Frame of beat `n` of the test track.
    fn beat(n: f64) -> f64 {
        (0.25 + 0.5 * n) * SR as f64
    }

    /// Distance from `pos` to the nearest beat, in ms.
    fn off_beat(pos: f64) -> f64 {
        let b = ((pos / SR as f64 - 0.25) / 0.5).round();
        (pos - beat(b)) / SR as f64 * 1000.0
    }

    #[test]
    fn beatjump_moves_whole_beats() {
        let mut d = deck();
        d.play_pause();
        run(&mut d, 30_000);
        assert!(d.beat_jump(1.0));
        assert_eq!(d.pos, 30_000.0 + 4.0 * 24_000.0);
        d.jump_beats = 1.0;
        d.beat_jump(-1.0);
        assert_eq!(d.pos, 30_000.0 + 3.0 * 24_000.0);
        d.play_pause();
        d.beat_jump(-1.0);
        assert_eq!(d.pos, 30_000.0 + 2.0 * 24_000.0, "paused too");

        let silent = Track::from_samples("t".into(), "t".into(), None, SR, vec![0.0; SR as usize * 10]);
        let mut d = Deck::new(SR);
        d.load(Arc::new(silent), &TrackMemory::default(), false);
        assert!(!d.beat_jump(1.0), "no grid, no beatjump");
    }

    #[test]
    fn beatjump_moves_the_loop_along() {
        let mut d = deck();
        d.auto_loop(4.0); // 0 .. 96 000
        d.play_pause();
        run(&mut d, 30_000);
        d.beat_jump(1.0);
        assert_eq!((d.loop_in, d.loop_out), (Some(96_000.0), Some(192_000.0)));
        assert_eq!(d.pos, 126_000.0);
        d.beat_jump(-1.0);
        d.beat_jump(-1.0);
        assert_eq!(d.loop_in, Some(0.0), "not past the start");
    }

    #[test]
    fn beatjump_sizes() {
        let mut d = deck();
        d.resize_jump(true);
        assert_eq!(d.jump_beats, 8.0);
        for _ in 0..9 {
            d.resize_jump(true);
        }
        assert_eq!(d.jump_beats, 64.0);
        for _ in 0..9 {
            d.resize_jump(false);
        }
        assert_eq!(d.jump_beats, 1.0);
    }

    #[test]
    fn beat_loop_sizes_and_toggle() {
        let mut d = deck_with(Some(true));
        d.resize_loop(0.5, false);
        d.resize_loop(0.5, false);
        d.resize_loop(0.5, false);
        assert_eq!(d.loop_size, 0.5);
        d.resize_loop(0.5, false);
        d.step_paused(41.0); // between steps of a quarter beat
        assert!(d.beat_loop());
        assert!(d.looping);
        assert!((d.loop_beats().unwrap() - 0.25).abs() < 1e-9);
        let steps = (d.loop_in.unwrap() - beat(0.0)) / 6_000.0;
        assert!((steps - steps.round()).abs() < 1e-6, "on a quarter beat: {steps}");
        // While looping, [ ] change the loop and the size follows.
        d.resize_loop(2.0, false);
        assert_eq!((d.loop_size, d.loop_beats().map(|b| (b * 1e6).round() / 1e6)), (0.5, Some(0.5)));
        assert!(d.beat_loop());
        assert!(!d.looping, "L again leaves the loop");
        for _ in 0..12 {
            d.resize_loop(2.0, false);
        }
        assert_eq!(d.loop_size, MAX_LOOP_BEATS);
    }

    #[test]
    fn resize_loop_left_keeps_the_out_point_and_moves_the_in_point() {
        let mut d = deck();
        d.play_pause();
        run(&mut d, SR as usize); // 1 s
        d.loop_in();
        run(&mut d, 24_000); // +0.5 s
        d.loop_out();
        let (start, end) = (d.loop_in.unwrap(), d.loop_out.unwrap());
        let len = end - start;

        // Double from the left: the out point stays, the in point moves
        // earlier by the loop's length — the mirror of plain `[`/`]`,
        // which always keeps the in point fixed instead.
        d.resize_loop(2.0, true);
        assert_eq!(d.loop_out, Some(end), "the out point doesn't move");
        assert_eq!(d.loop_in, Some(start - len));

        // Halve from the left: back toward the out point, still anchored there.
        d.resize_loop(0.5, true);
        assert_eq!(d.loop_out, Some(end));
        assert!((d.loop_in.unwrap() - start).abs() < 1.0);
    }

    #[test]
    fn resize_loop_left_wont_extend_past_the_start_of_the_track() {
        let mut d = deck();
        d.play_pause();
        run(&mut d, 12_000);
        d.loop_in();
        run(&mut d, 6_000);
        d.loop_out();
        for _ in 0..20 {
            d.resize_loop(2.0, true);
        }
        assert!(d.loop_in.unwrap() >= 0.0);
    }

    #[test]
    fn slip_loop_comes_back_where_the_track_would_be() {
        let mut d = deck();
        assert!(d.toggle_slip());
        d.play_pause();
        run(&mut d, 24_000);
        d.auto_loop(1.0); // 24 000 .. 48 000
        run(&mut d, 72_000);
        assert!(d.pos < 48_000.0);
        assert_eq!(d.snapshot().ghost, Some(96_000.0));
        d.reloop_exit();
        assert_eq!(d.pos, 96_000.0);
        assert_eq!(d.snapshot().ghost, None);
    }

    #[test]
    fn slip_reverse_and_pause_come_back() {
        let mut d = deck();
        d.toggle_slip();
        d.play_pause();
        run(&mut d, 24_000);
        d.toggle_reverse();
        run(&mut d, 12_000);
        d.toggle_reverse();
        assert_eq!(d.pos, 36_000.0);

        d.play_pause();
        run(&mut d, 24_000);
        assert!(d.pos < 48_000.0, "braked");
        d.play_pause();
        assert_eq!(d.pos, 60_000.0);
        assert!(d.playing);
    }

    #[test]
    fn held_hot_cue_slips_only_in_slip_mode() {
        let mut d = deck();
        d.step_paused(150.0);
        d.set_hot_cue(0); // 96 000
        d.step_paused(-150.0);
        d.toggle_slip();
        d.play_pause();
        run(&mut d, 24_000);
        d.hot_cue(0, true);
        assert_eq!(d.pos, 96_000.0);
        run(&mut d, 12_000);
        d.hot_cue_up(0);
        assert_eq!(d.pos, 36_000.0);

        d.toggle_slip();
        d.hot_cue(0, true);
        run(&mut d, 12_000);
        d.hot_cue_up(0);
        assert_eq!(d.pos, 108_000.0, "no slip, no going back");
    }

    #[test]
    fn roll_comes_back_even_without_slip_mode() {
        let mut d = deck();
        d.play_pause();
        run(&mut d, 30_000);
        assert!(d.roll_down(0.5));
        // Half-beat steps from the beat at 12 000: the roll is 24 000 .. 36 000.
        assert_eq!((d.loop_in, d.loop_out), (Some(24_000.0), Some(36_000.0)));
        run(&mut d, 24_000);
        assert!(d.pos >= 24_000.0 && d.pos < 36_000.0);
        // Halving while rolling keeps the one ghost, and sets the loop size.
        d.resize_loop(0.5, false);
        assert_eq!((d.snapshot().roll, d.loop_size), (Some(0.25), 0.25));
        assert!((d.loop_beats().unwrap() - 0.25).abs() < 1e-9);
        d.roll_up();
        assert_eq!(d.pos, 54_000.0);
        assert!(!d.looping && d.loop_in.is_none());
    }

    #[test]
    fn roll_inside_a_slip_loop_returns_into_the_loop() {
        let mut d = deck();
        d.toggle_slip();
        d.play_pause();
        run(&mut d, 24_000);
        d.auto_loop(2.0); // 24 000 .. 72 000
        run(&mut d, 30_000); // at 54 000
        d.roll_down(0.25);
        run(&mut d, 24_000); // the roll's ghost wraps in the loop: 78 000 → 30 000
        d.roll_up();
        assert_eq!(d.pos, 30_000.0);
        assert_eq!((d.loop_in, d.loop_out, d.looping), (Some(24_000.0), Some(72_000.0), true));
        d.reloop_exit();
        assert_eq!(d.pos, 24_000.0 + 54_000.0, "the slip's ghost ran on through it all");
    }

    #[test]
    fn steady_tracks_quantize_by_default() {
        let mut d = deck_with(None);
        assert!(d.track.as_ref().unwrap().steady && d.quantize());
        assert_eq!(d.toggle_quantize(), Some(false));
        assert_eq!(d.memory().quantize, Some(false), "the choice is remembered");
        let mut d = Deck::new(SR);
        assert_eq!(d.toggle_quantize(), None, "nothing to quantize to");
    }

    #[test]
    fn quantized_cues_and_loops_land_on_beats() {
        let mut d = deck_with(Some(true));
        d.step_paused(80.0); // 1.067 s, nearest beat 1.25 s
        d.cue_down(true);
        assert!(off_beat(d.cue).abs() < 1.0 && d.pos == d.cue, "cue {}", d.cue);
        d.step_paused(-10.0);
        d.set_hot_cue(0);
        assert!(off_beat(d.hot[0].unwrap().pos).abs() < 1.0);

        d.step_paused(37.0);
        d.play_pause();
        d.loop_in();
        assert!(off_beat(d.loop_in.unwrap()).abs() < 1.0);
        run(&mut d, 30_000); // less than a beat on: the loop is still a beat long
        d.loop_out();
        assert!((d.loop_beats().unwrap() - 1.0).abs() < 1e-3, "{:?}", d.loop_beats());
        d.reloop_exit();
        d.step_paused(0.0);
        assert!(d.auto_loop(4.0));
        assert!(off_beat(d.loop_in.unwrap()).abs() < 1.0);
        assert!((d.loop_beats().unwrap() - 4.0).abs() < 1e-3);
    }

    #[test]
    fn quantized_beat_steps_follow_the_grid() {
        let mut d = deck_with(Some(true));
        d.step_beats_paused(1);
        assert!(off_beat(d.pos).abs() < 1.0 && (d.pos - beat(0.0)).abs() < 50.0, "pos {}", d.pos);
        d.step_beats_paused(2);
        assert!((d.pos - beat(2.0)).abs() < 50.0);
        d.step_beats_paused(-1);
        assert!((d.pos - beat(1.0)).abs() < 50.0);
    }

    #[test]
    fn quantized_hot_cue_waits_for_the_beat() {
        let mut d = deck_with(Some(true));
        d.step_paused(150.0);
        d.set_hot_cue(0); // 2.25 s
        let cue = d.hot[0].unwrap().pos;
        d.step_paused(-150.0);
        d.play_pause();
        run(&mut d, 30_000); // 0.625 s: past beat 0.25 s, before 0.75 s
        assert!(d.hot_cue(0, false));
        assert_eq!(d.snapshot().pending_hot, Some(0));
        assert!(d.pos < beat(1.0), "nothing happens before the beat");
        run(&mut d, 12_000); // 0.875 s: the beat at 0.75 s went by
        assert_eq!(d.snapshot().pending_hot, None);
        // Landed on the cue at the beat and played on in phase.
        let expected = cue + (0.875 - 0.75) * SR as f64;
        assert!((d.pos - expected).abs() < 2.0, "pos {} expected {expected}", d.pos);
        assert!(off_beat(d.pos - (expected - cue)).abs() < 1.0);
    }

    #[test]
    fn quantized_memory_call_waits_for_the_beat() {
        let mut d = deck_with(Some(true));
        d.step_paused(150.0);
        d.cue_down(true); // 2.25 s
        let at = d.cue;
        d.store_memory();
        d.step_paused(150.0);
        d.cue_down(true); // 4.25 s
        d.store_memory();
        d.step_paused(-300.0);
        d.cue_down(true); // snaps to the beat at 0.25 s
        d.play_pause();
        run(&mut d, 18_000); // 0.625 s: past beat 0.25 s, before 0.75 s
        assert_eq!(d.call_memory(MemoryCall::Right), Some(1));
        assert!(d.pos < beat(1.0), "nothing happens before the beat");
        assert_eq!(d.call_memory(MemoryCall::Right), Some(2), "a second press goes on from the first");
        assert_eq!(d.call_memory(MemoryCall::Left), Some(1));
        run(&mut d, 12_000); // 0.875 s: the beat at 0.75 s went by
        let expected = at + (0.875 - 0.75) * SR as f64;
        assert!(d.playing);
        assert!((d.pos - expected).abs() < 2.0, "pos {} expected {expected}", d.pos);
    }

    #[test]
    fn quantized_hot_cue_just_after_a_beat_goes_at_once() {
        let mut d = deck_with(Some(true));
        d.step_paused(150.0);
        d.set_hot_cue(0);
        let cue = d.hot[0].unwrap().pos;
        d.step_paused(-150.0);
        d.play_pause();
        run(&mut d, 12_480); // 0.26 s: 10 ms after the beat at 0.25 s
        assert!(d.hot_cue(0, false));
        assert!(d.pending.is_none());
        assert!((d.pos - (cue + 480.0)).abs() < 2.0, "pos {}", d.pos);
    }

    #[test]
    fn unquantized_hot_cue_jumps_at_once() {
        let mut d = deck();
        d.step_paused(150.0);
        d.set_hot_cue(0);
        d.step_paused(-150.0);
        d.play_pause();
        run(&mut d, 30_000);
        d.hot_cue(0, false);
        assert_eq!(d.pos, 2.0 * SR as f64);
    }

    #[test]
    fn pending_jump_is_dropped_by_pause_and_cue() {
        let mut d = deck_with(Some(true));
        d.step_paused(150.0);
        d.set_hot_cue(0);
        d.step_paused(-150.0);
        d.play_pause();
        run(&mut d, 30_000);
        d.hot_cue(0, false);
        d.play_pause();
        assert!(d.pending.is_none());
        d.play_pause();
        d.hot_cue(0, false);
        d.cue_down(true);
        assert!(d.pending.is_none());
    }

    #[test]
    fn grid_edits_move_the_beats() {
        let mut d = deck();
        let detected = d.grid().unwrap();
        d.step_paused(100.0);
        assert!(d.set_downbeat());
        let g = d.grid().unwrap();
        assert_eq!(g.bar_beat(d.pos, SR as f64), (1, 1));
        assert_eq!(g.bpm, detected.bpm);
        assert_eq!(d.memory().grid, Some(g), "edits are saved");

        assert!(d.toggle_grid_adjust());
        d.shift_grid(10.0);
        assert!((d.grid().unwrap().anchor - (g.anchor + 480.0)).abs() < 1e-6);
        d.nudge_grid_bpm(0.5);
        assert_eq!(d.grid().unwrap().bpm, detected.bpm + 0.5);
        d.reset_grid();
        assert_eq!(d.grid(), Some(detected));
        assert_eq!(d.memory().grid, None);
    }

    #[test]
    fn taps_while_playing_place_the_beats() {
        let mut d = deck();
        // Taps at 100 BPM, 20 ms after the beats of the 120 BPM track would be.
        let taps: Vec<f64> = (0..6).map(|k| 1.0 * SR as f64 + k as f64 * 0.6 * SR as f64).collect();
        d.move_tempo(200.0); // heard 10% faster
        let heard = d.set_tapped_beats(&taps).unwrap();
        assert!((heard - 110.0).abs() < 1e-6);
        let g = d.grid().unwrap();
        assert!((g.bpm - 100.0).abs() < 1e-6);
        assert!((g.nearest(taps[5], SR as f64) - taps[5]).abs() < 1e-6);
    }

    #[test]
    fn old_tapped_bpm_keeps_the_detected_phase() {
        let samples = click_track(120.0, 20.0, SR);
        let track = Arc::new(Track::from_samples("t".into(), "t".into(), None, SR, samples));
        let mut d = Deck::new(SR);
        d.load(track.clone(), &TrackMemory { bpm: Some(121.0), ..TrackMemory::default() }, false);
        let g = d.grid().unwrap();
        assert_eq!((g.bpm, g.anchor), (121.0, track.grid.unwrap().anchor));
        assert_eq!((d.memory().grid, d.memory().bpm), (Some(g), None));
    }

    // --- Sync ---

    /// 10 ms blocks.
    const BLOCK: usize = 480;

    /// A master heard at `bpm`, its beat `beat0` heard at time 0.
    fn master(bpm: f64, beat0: f64) -> impl Fn(f64) -> Packet {
        move |t| Packet {
            deck: 1,
            playing: true,
            synced: true,
            master: true,
            steady: true,
            running: true,
            master_since: 0.0,
            bpm: Some(bpm),
            beat: beat0 + t * bpm / 60.0,
            rate: bpm / 60.0,
            time: t,
            key: None,
        }
    }

    /// Runs `d` against the master from time `*t` for `seconds`, the way the
    /// audio and sync threads would; returns the output.
    fn follow(d: &mut Deck, t: &mut f64, seconds: f64, lead: &impl Fn(f64) -> Packet) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = [0.0; BLOCK * 2];
        for _ in 0..(seconds * SR as f64 / BLOCK as f64).round() as usize {
            d.update_link(2, &[lead(*t)], *t);
            d.render_at(&mut buf, Some(*t));
            out.extend_from_slice(&buf);
            *t += BLOCK as f64 / SR as f64;
        }
        out
    }

    /// The follower's phase against the master, in ms.
    fn phase_ms(d: &Deck) -> f64 {
        let (_, beats) = d.phase().expect("a phase");
        beats * 60_000.0 / (d.bpm().unwrap() * d.tempo_factor())
    }

    /// A synced deck that started in phase with `lead` and played a second.
    fn following(lead: &impl Fn(f64) -> Packet, t: &mut f64) -> Deck {
        following_on(deck(), lead, t)
    }

    fn following_on(mut d: Deck, lead: &impl Fn(f64) -> Packet, t: &mut f64) -> Deck {
        d.pos = beat(2.0);
        assert_eq!(d.toggle_sync(), Some(true));
        follow(&mut d, t, 0.02, lead);
        d.play_pause();
        follow(&mut d, t, 1.0, lead);
        assert!(d.playing);
        d
    }

    #[test]
    fn followers_take_the_masters_tempo() {
        let mut d = deck();
        let mut t = 100.0;
        d.toggle_sync();
        follow(&mut d, &mut t, 0.02, &master(135.0, 0.0));
        assert_eq!(d.follows(), Some(1));
        assert!((d.tempo - 12.5).abs() < 1e-9);
        assert!(!d.move_tempo(1.0) && !d.reset_tempo(), "the tempo keys are locked");
        assert!(d.quantize(), "quantize is on while synced");
        // Half or double, whichever is nearer.
        follow(&mut d, &mut t, 0.02, &master(250.0, 0.0));
        assert_eq!(d.sync_ratio, Some(0.5));
        assert!((d.tempo - 125.0 / 1.2 + 100.0).abs() < 1e-9);
        follow(&mut d, &mut t, 0.02, &master(67.5, 0.0));
        assert_eq!(d.sync_ratio, Some(2.0));
        assert!((d.tempo - 12.5).abs() < 1e-9);
        // Unsynced, the tempo stays, on a range that holds it.
        assert_eq!(d.toggle_sync(), Some(false));
        assert!((d.tempo - 12.5).abs() < 1e-9);
        assert_eq!(d.range, TempoRange::R16);
        assert!(!d.quantize());
        assert!(d.move_tempo(1.0));
    }

    #[test]
    fn a_synced_deck_playing_alone_becomes_master() {
        let mut d = deck();
        d.toggle_sync();
        d.play_pause();
        let me = d.update_link(2, &[], 5.0);
        assert!(me.master && d.snapshot().sync.master);
        assert!(d.playing, "nothing to wait for");
        // A newer claim takes over, and this deck follows it.
        let lead = Packet { master_since: 6.0, ..master(128.0, 0.0)(7.0) };
        d.update_link(2, &[lead], 7.0);
        assert!(!d.snapshot().sync.master);
        assert_eq!(d.follows(), Some(1));
        // Claiming it back.
        assert!(d.claim_master(8.0));
        assert_eq!(d.follows(), None);
        assert!(d.update_link(2, &[lead], 8.0).master);
    }

    #[test]
    fn play_waits_for_the_masters_beat() {
        let lead = master(120.0, 0.3);
        let mut t = 100.0;
        let mut d = deck();
        d.pos = beat(2.0);
        d.toggle_sync();
        follow(&mut d, &mut t, 0.02, &lead);
        d.play_pause();
        assert!(!d.playing && d.snapshot().sync.waiting);
        let pressed = t;
        while !d.playing {
            follow(&mut d, &mut t, 0.01, &lead);
            assert!(t - pressed < 0.52, "at most a beat");
        }
        assert!(t - pressed > 0.3, "the master's beat was 0.35 s off");
        follow(&mut d, &mut t, 0.02, &lead);
        assert!(phase_ms(&d).abs() < 0.1, "in phase from the start: {} ms", phase_ms(&d));
        assert!((d.trim - 1.0).abs() < 1e-6);

        // Play again before the beat: still paused.
        d.play_pause();
        d.play_pause();
        assert!(d.snapshot().sync.waiting);
        d.play_pause();
        follow(&mut d, &mut t, 1.0, &lead);
        assert!(!d.playing && !d.snapshot().sync.waiting);
    }

    #[test]
    fn the_lane_puts_the_masters_beats_on_ours() {
        let mut t = 100.0;
        let mut d = following(&master(120.0, 0.3), &mut t);
        let lanes = d.snapshot().sync.lanes;
        assert_eq!(lanes.len(), 1);
        assert!(lanes[0].deck == 1 && lanes[0].master);
        let lane = lanes[0].beats.expect("beats while both play");
        let (g, sr) = (d.grid().unwrap(), d.sample_rate());
        assert!((lane.period - g.period(sr) / sr).abs() < 1e-6, "{} s a beat", lane.period);
        for k in 0..4 {
            let n = lane.beat.ceil() + k as f64;
            let ours = g.beat_at((lane.at + (n - lane.beat) * lane.period) * sr, sr);
            assert!((ours - ours.round()).abs() < 1e-3, "beat {n} lands on {ours}");
        }
        d.play_pause();
        assert!(d.snapshot().sync.lanes[0].beats.is_none(), "no beats while paused");
    }

    #[test]
    fn every_other_deck_gets_a_lane() {
        let mut t = 100.0;
        let lead = master(120.0, 0.3);
        let mut d = following(&lead, &mut t);
        let stopped = Packet { deck: 4, master: false, playing: false, running: false, ..lead(t) };
        let other = Packet { deck: 2, master: false, ..lead(t) };
        d.update_link(3, &[stopped, lead(t), other], t);
        let lanes = d.snapshot().sync.lanes;
        assert_eq!(lanes.iter().map(|l| l.deck).collect::<Vec<_>>(), [1, 2, 4]);
        assert!(lanes[0].master && !lanes[1].master);
        assert!(lanes[0].beats.is_some() && lanes[1].beats.is_some());
        assert!(!lanes[2].playing && lanes[2].beats.is_none());
    }

    #[test]
    fn small_phase_errors_are_trimmed_out_without_a_jump() {
        let mut t = 100.0;
        let mut d = following(&master(120.0, 0.3), &mut t);
        // The master moves 10 ms.
        let lead = master(120.0, 0.32);
        let mut buf = [0.0; BLOCK * 2];
        for _ in 0..800 {
            let before = d.pos;
            d.update_link(2, &[lead(t)], t);
            d.render_at(&mut buf, Some(t));
            t += BLOCK as f64 / SR as f64;
            let moved = d.pos - before;
            assert!((moved / BLOCK as f64 - 1.0).abs() <= MAX_TRIM + 1e-9, "no jump: {moved}");
        }
        assert!(phase_ms(&d).abs() < 0.5, "{} ms", phase_ms(&d));
    }

    #[test]
    fn big_phase_errors_jump_back_in() {
        let mut t = 100.0;
        let mut d = following(&master(120.0, 0.3), &mut t);
        let lead = master(120.0, 0.5);
        follow(&mut d, &mut t, 0.1, &lead);
        assert!(phase_ms(&d).abs() < 0.5, "{} ms", phase_ms(&d));
    }

    #[test]
    fn followers_keep_up_with_the_master() {
        let mut t = 100.0;
        let track = Track::from_samples("t".into(), "t".into(), None, SR, click_track(120.0, 150.0, SR));
        let mut d = Deck::new(SR);
        d.load(Arc::new(track), &TrackMemory::default(), false);
        let mut d = following_on(d, &master(120.0, 0.3), &mut t);
        // A tempo change, live.
        let lead = |t: f64| Packet { beat: 0.3 + 101.0 * 2.0 + (t - 101.0) * 2.1, ..master(126.0, 0.0)(t) };
        follow(&mut d, &mut t, 4.0, &lead);
        assert!((d.tempo - 5.0).abs() < 1e-9);
        assert!(phase_ms(&d).abs() < 0.5, "{} ms", phase_ms(&d));
        // The master's sound card runs 100 ppm fast, for minutes.
        let at = t;
        let drifting = |t: f64| Packet { beat: lead(at).beat + (t - at) * 2.1 * 1.0001, ..lead(t) };
        let mut buf = [0.0; BLOCK * 2];
        while t < at + 120.0 {
            let before = d.pos;
            d.update_link(2, &[drifting(t)], t);
            d.render_at(&mut buf, Some(t));
            t += BLOCK as f64 / SR as f64;
            assert!(((d.pos - before) / (BLOCK as f64 * 1.05) - 1.0).abs() <= MAX_TRIM + 1e-9, "no jump");
        }
        assert!(phase_ms(&d).abs() < 0.5, "{} ms", phase_ms(&d));
    }

    #[test]
    fn unsteady_grids_sync_the_tempo_only() {
        let mut track = Track::from_samples("t".into(), "t".into(), None, SR, click_track(120.0, 20.0, SR));
        track.steady = false;
        let mut d = Deck::new(SR);
        d.load(Arc::new(track), &TrackMemory::default(), false);
        let lead = master(126.0, 0.3);
        let mut t = 100.0;
        d.toggle_sync();
        follow(&mut d, &mut t, 0.02, &lead);
        let sync = d.snapshot().sync;
        assert_eq!((sync.leader, sync.phase_lock), (Some(1), false));
        assert!((d.tempo - 5.0).abs() < 1e-9);
        d.play_pause();
        assert!(d.playing, "no waiting for a beat");
        follow(&mut d, &mut t, 1.0, &lead);
        assert_eq!(d.trim, 1.0);
        // A grid set by hand is trusted.
        d.set_downbeat();
        assert!(d.snapshot().sync.phase_lock);
    }

    /// Finds where the clicks start in `out` (stereo), in frames.
    fn onsets(out: &[f32]) -> Vec<usize> {
        let mut quiet = 0;
        let mut found = Vec::new();
        for (i, f) in out.chunks(2).enumerate() {
            if f[0].abs() < 0.01 {
                quiet += 1;
                continue;
            }
            if quiet > SR as usize / 50 {
                found.push(i);
            }
            quiet = 0;
        }
        found
    }

    /// The deck says what it plays is heard when it actually comes out, with
    /// Master Tempo's latency too.
    fn check_heard_positions(master_tempo: bool) {
        let mut d = deck();
        if master_tempo && !d.toggle_master_tempo() {
            return;
        }
        d.play_pause();
        let mut buf = [0.0; BLOCK * 2];
        let mut out = Vec::new();
        let mut said = Vec::new();
        let mut t = 50.0;
        for _ in 0..300 {
            d.render_at(&mut buf, Some(t));
            said.push(d.heard.unwrap());
            out.extend_from_slice(&buf);
            t += BLOCK as f64 / SR as f64;
        }
        let found = onsets(&out);
        assert!(found.len() > 8, "{found:?}");
        for i in found {
            let h = said[i / BLOCK];
            let pos = h.pos + (i % BLOCK) as f64 * h.rate;
            // Kicks on the beats, hats halfway.
            let off = ((pos - beat(0.0)) / 12_000.0 - ((pos - beat(0.0)) / 12_000.0).round()) * 12_000.0;
            assert!(off.abs() < 0.002 * SR as f64, "heard {} ms off at frame {i}", off / 48.0);
        }
    }

    #[test]
    fn heard_positions_match_the_output() {
        check_heard_positions(false);
    }

    #[test]
    fn heard_positions_match_the_output_with_master_tempo() {
        check_heard_positions(true);
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
    fn j_and_k_jump_straight_to_the_nearest_memory() {
        let mut d = deck();
        for _ in 0..3 {
            d.store_memory();
            d.step_paused(75.0);
            d.cue_down(true);
        } // memories at 0, 1 and 2 s; the deck ends up at 3 s, paused

        // Lands exactly on the nearest one each press, so — unlike a tight
        // "which region am I in" lookup — it never has to skip past a
        // memory that's clearly the nearest one just to reach it.
        assert_eq!(d.call_memory(MemoryCall::Left), Some(3));
        assert_eq!(d.pos, 2.0 * SR as f64);
        assert_eq!(d.call_memory(MemoryCall::Left), Some(2), "pressing again steps further back");
        assert_eq!(d.pos, SR as f64);
        assert_eq!(d.call_memory(MemoryCall::Left), Some(1));
        assert_eq!(d.pos, 0.0);
        assert_eq!(d.call_memory(MemoryCall::Left), None, "nothing before the first");

        assert_eq!(d.call_memory(MemoryCall::Right), Some(2), "and K steps forward the same way");
        assert_eq!(d.pos, SR as f64);
    }

    #[test]
    fn j_finds_a_memory_further_left_after_the_nearer_one_is_deleted() {
        let mut d = deck();
        d.store_memory(); // A at 0 s
        d.step_paused(150.0); // 2 s
        d.cue_down(true);
        d.store_memory(); // B at 2 s, nearer the playhead
        assert!(d.delete_memory(), "delete B");
        assert_eq!(d.memories.len(), 1);

        assert_eq!(d.call_memory(MemoryCall::Left), Some(1), "finds A directly, doesn't skip past it");
        assert_eq!(d.pos, 0.0);
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

        // Calling while playing jumps there and plays on, looping a stored loop.
        d.reloop_exit();
        run(&mut d, 200_000);
        let cue = d.cue;
        assert_eq!(d.call_memory(MemoryCall::Left), Some(3));
        assert!(d.playing && d.looping);
        assert_eq!(d.pos, 2.0 * SR as f64);
        assert_eq!(d.cue, cue, "the cue point stays");
        assert_eq!(d.call_memory(MemoryCall::Left), Some(2));
        assert!(d.playing && !d.looping);
        assert_eq!(d.pos, SR as f64);

        // Paused, it becomes the cue point and the deck waits there.
        d.play_pause();
        assert_eq!(d.call_memory(MemoryCall::Left), Some(1));
        assert!(!d.playing);
        assert_eq!((d.pos, d.cue), (0.0, 0.0));
        assert_eq!(d.call_memory(MemoryCall::Left), None, "nothing before the first");
        assert_eq!(d.call_memory(MemoryCall::Right), Some(2));
        assert_eq!(d.cue, SR as f64);

        assert!(d.delete_memory());
        assert_eq!(d.memories.len(), 2);
        assert!(!d.delete_memory());
    }

    #[test]
    fn delete_memory_works_after_calling_one_while_playing() {
        let mut d = deck();
        d.store_memory(); // a memory at pos 0
        d.step_paused(150.0); // 2 s, still paused
        d.cue_down(true); // sets a new cue here, at 2 s — away from the memory
        d.play_pause();
        run(&mut d, SR as usize); // +1 s, still playing; cue stays at 2 s

        // Jumps back to the memory, but — the actual bug report — stays
        // playing, so `cue` doesn't follow (only a paused call moves it).
        assert_eq!(d.call_memory(MemoryCall::Left), Some(1));
        assert!((d.pos - 0.0).abs() < 1.0, "pos is back at the memory");
        assert!((d.cue - 2.0 * SR as f64).abs() < 1.0, "cue is still where it was, unmoved");
        assert!(d.delete_memory(), "delete still finds it via pos, not the stale cue");
        assert!(d.memories.is_empty());
    }

    #[test]
    fn delete_memory_works_anywhere_inside_a_playing_loop() {
        let mut d = deck();
        d.play_pause();
        run(&mut d, SR as usize); // 1 s
        d.loop_in(); // loop_in = 1 s
        run(&mut d, 24_000); // +0.5 s
        d.loop_out(); // loop_out = 1.5 s, now looping
        assert!(d.store_memory().is_some(), "promotes the active loop");

        // Deep into the loop body, not sitting at its start point — the
        // actual bug report: a tight distance check to the start only
        // ever matched for an instant right as the loop wrapped.
        run(&mut d, 10_000);
        assert!((d.pos - SR as f64).abs() > 1.0, "well past the loop's start point");
        assert!(d.memory_at_pos().is_some(), "still finds it — pos is inside the loop");
        assert!(d.delete_memory());
        assert!(d.memories.is_empty());
    }

    #[test]
    fn store_last_loop_promotes_a_dormant_loop() {
        let mut d = deck();
        assert_eq!(d.store_memory(), Some(1)); // cue at 0
        d.play_pause();
        run(&mut d, SR as usize);
        d.loop_in(); // 1 s — also moves the cue here, unlike store_last_loop
        run(&mut d, 24_000);
        d.loop_out(); // 1.5 s
        d.reloop_exit(); // leave the loop — loop_in/out stay as "the last loop"
        assert!(!d.looping);

        // store_last_loop grabs the dormant loop even though we're not in it.
        assert_eq!(d.store_last_loop(), Some(2));
        assert_eq!(d.memories[1].pos, SR as f64);
        assert_eq!(d.memories[1].loop_out, Some(1.5 * SR as f64));

        // Promoting the same loop again is a duplicate.
        assert!(d.store_last_loop().is_none());
    }

    #[test]
    fn store_last_loop_is_none_without_a_loop() {
        let mut d = deck();
        assert!(d.store_last_loop().is_none());
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
        assert!(!d.hot_cue(0, false), "empty slots do nothing");
        d.step_paused(150.0);
        d.set_hot_cue(0);
        d.step_paused(-150.0);
        assert!(d.hot_cue(0, false));
        assert!(d.playing);
        assert_eq!(d.pos, 2.0 * SR as f64);
    }

    #[test]
    fn detected_grid_matches_the_track() {
        let d = deck();
        assert!(off_beat(d.grid().unwrap().anchor).abs() < 2.0);
        assert_eq!(d.bpm(), Some(120.0));
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

    #[test]
    fn reverse_in_a_loop_plays_the_loop_backwards() {
        let mut d = deck();
        d.play_pause();
        run(&mut d, 24_000);
        d.loop_in();
        run(&mut d, 24_000);
        d.loop_out();
        let (a, b) = (d.loop_in.unwrap(), d.loop_out.unwrap());
        d.toggle_reverse();
        let mut wrapped = false;
        let mut last = d.pos;
        for _ in 0..40 {
            run(&mut d, 4800);
            assert!(d.looping && d.pos >= a && d.pos < b, "{} outside {a}..{b}", d.pos);
            wrapped |= d.pos > last;
            last = d.pos;
        }
        assert!(wrapped, "going back past the loop in comes round to the loop out");
    }

    #[test]
    fn the_key_moves_with_the_tempo_unless_master_tempo() {
        let mut track = Track::from_samples("t".into(), "t".into(), None, SR, click_track(120.0, 5.0, SR));
        let am = Key { tonic: 9, minor: true };
        track.key = Some(am);
        let mut d = Deck::new(SR);
        d.load(Arc::new(track), &TrackMemory::default(), false);
        assert_eq!(d.heard_key(), Some((am, 0)));
        d.tempo = 2.0;
        assert_eq!(d.heard_key(), Some((am, 0)), "2% is a third of a semitone");
        d.tempo = 6.0;
        let up = (Key { tonic: 10, minor: true }, 1);
        assert_eq!(d.heard_key(), Some(up), "6% is a semitone");
        assert_eq!(d.packet(0.0).key, Some(up.0), "the other decks hear it too");
        d.master_tempo = true;
        assert_eq!(d.heard_key(), Some((am, 0)));
    }

    /// A sine test tone, for the mixer tests below — distinct from
    /// `click_track`'s broadband clicks, so low/high content is controlled.
    fn sine_track(hz: f64, seconds: f64) -> Track {
        let n = (SR as f64 * seconds) as usize;
        let samples: Vec<f32> = (0..n)
            .flat_map(|i| {
                let x = (2.0 * std::f64::consts::PI * hz * i as f64 / SR as f64).sin() as f32;
                [x, x]
            })
            .collect();
        Track::from_samples("t".into(), "t".into(), None, SR, samples)
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn mixer_defaults_are_neutral() {
        let d = Deck::new(SR);
        assert_eq!(d.volume(), 1.0);
        assert_eq!((d.low_db(), d.mid_db(), d.high_db()), (0.0, 0.0, 0.0));
    }

    #[test]
    fn set_volume_zero_silences_the_deck() {
        let mut d = Deck::new(SR);
        d.load(Arc::new(sine_track(440.0, 1.0)), &TrackMemory::default(), false);
        d.play_pause();
        d.set_volume(0.0);
        let out = run(&mut d, 4800);
        assert!(out.iter().all(|&x| x == 0.0), "volume 0 should silence the deck");
    }

    #[test]
    fn eq_bands_shape_the_rendered_mix() {
        let low = || Arc::new(sine_track(100.0, 1.0));
        let high = || Arc::new(sine_track(8000.0, 1.0));

        let render = |track: Arc<Track>, set: fn(&mut Deck)| {
            let mut d = Deck::new(SR);
            d.load(track, &TrackMemory::default(), false);
            d.play_pause();
            set(&mut d);
            run(&mut d, 20_000)
        };

        let low_flat = render(low(), |_| {});
        let low_boosted = render(low(), |d| d.set_low_db(odj_core::filter::MAX_DB));
        assert!(rms(&low_boosted) > rms(&low_flat) * 1.3, "boosting the low band should raise a low tone");

        let high_flat = render(high(), |_| {});
        let high_cut = render(high(), |d| d.set_high_db(odj_core::filter::MIN_DB));
        assert!(rms(&high_cut) < rms(&high_flat) * 0.3, "cutting the high band hard should quiet a high tone");
    }

    #[test]
    fn mixer_values_are_deck_level_not_track_level() {
        let mut d = deck();
        d.set_volume(0.5);
        d.set_low_db(3.0);
        d.load(Arc::new(sine_track(440.0, 1.0)), &TrackMemory::default(), false);
        assert_eq!(d.volume(), 0.5, "loading a track doesn't reset the mixer");
        assert_eq!(d.low_db(), 3.0);
    }
}
