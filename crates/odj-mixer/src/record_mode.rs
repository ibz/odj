//! The record-mode screen's own state: 8 fixed track slots, each
//! freely assignable to any currently-routed input and independently
//! toggled Play / Mute / Record (`Play` is the default — a fresh
//! track, or one just loaded from a previous session, plays back
//! whatever it already has rather than sitting idle). Owns nothing
//! about keys or drawing — `ui::run` calls these in response to them
//! and shows the result through the existing status popup, same
//! convention as the old `Recorder` it replaces.
//!
//! The real-time half (the shared playhead, each input's own capture
//! tap, mixing a Play track's decoded buffer back into the live
//! output) lives on `routing::MixedSource` — this module only starts
//! and stops [`crate::recorder::Recording`] writer threads (reused
//! as-is, one per actively-capturing track) and decodes a track's own
//! file into memory for Play (`odj_core::track::decode`, the same
//! "whole file up front" approach odj-player's `Deck` and
//! odj-sampler's `Sampler` already use — these recordings are bounded
//! in length, same class as any other). Every track's own state is
//! never persisted across a restart — only its input assignment and
//! `start_offset` are — so a restart never silently resumes live
//! *recording* (`Record` is never the state a track comes back in);
//! playback is different on purpose: since `Play` is the default,
//! [`RecordMode::enter`] re-installs it automatically for any track
//! that already has content the moment record mode opens, and
//! previously-recorded tracks stay aligned with anything recorded
//! after the restart either way.
//!
//! Mixer mode and record mode are kept independent: on the ordinary
//! live screen, every routed input is audible exactly as its own
//! Volume fader says, whether or not record mode even exists. Record
//! mode only starts influencing that the moment its own screen
//! actually opens — `ui::run` calls [`RecordMode::enter`] right then,
//! which mutes (via `routing::Routing::set_input_muted`) every
//! track's assigned input unless it's armed to `Record` (only a
//! Record-armed track monitors its own live input; `Mute` and `Play`
//! both suppress the live pass-through) — and stops the moment it
//! closes again ([`RecordMode::leave`] un-mutes everything), so
//! leaving record mode never leaves the live screen silently muted by
//! a decision made somewhere else.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail, ensure};
use ringbuf::HeapRb;
use ringbuf::traits::Split;
use serde::{Deserialize, Serialize};

use crate::devices::{DeviceKey, device_key_string, parse_device_key};
use crate::recorder::Recording;
use crate::routing::{PlayTrack, Routing, TRACKS};

/// Ring buffer capacity for a track's own capture tap, in stereo
/// samples (~1 s at 48 kHz) — same generous slack
/// `recorder::Recording`'s old single-file tap used, for the same
/// reason: the consumer is a disk-writing thread, not another
/// realtime callback, so a moment of disk latency shouldn't drop
/// frames. The push side (`MixedSource::render`) never blocks
/// regardless — on overrun it simply drops whatever doesn't fit.
const CAPTURE_RING_STEREO_SAMPLES: usize = 96_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrackState {
    #[default]
    Play,
    Mute,
    Record,
}

impl TrackState {
    /// `S`'s own cycle: Play → Mute → Record → Play.
    pub fn next(self) -> Self {
        match self {
            TrackState::Play => TrackState::Mute,
            TrackState::Mute => TrackState::Record,
            TrackState::Record => TrackState::Play,
        }
    }

    /// `Shift+S`'s own cycle: the reverse of [`next`](Self::next).
    pub fn prev(self) -> Self {
        match self {
            TrackState::Play => TrackState::Record,
            TrackState::Mute => TrackState::Play,
            TrackState::Record => TrackState::Mute,
        }
    }
}

/// One of the 8 fixed track slots.
#[derive(Default)]
pub struct Track {
    pub input: Option<DeviceKey>,
    pub state: TrackState,
    /// The playhead frame this track's own file's first frame
    /// corresponds to — set fresh each time a new recording pass
    /// starts on it (see `RecordMode::start_track_capture`), so a
    /// track re-recorded after a gap doesn't misalign against
    /// anything else still armed to Play. `pub` read-only: the UI
    /// needs it (converted to seconds, at the live output rate) to
    /// draw an in-progress capture's own growing block on the shared
    /// timeline.
    pub start_offset: u64,
    /// The writer thread currently draining this track's own capture
    /// tap, if it's actively recording right now.
    capture: Option<Recording>,
    /// This track's own cached waveform (`odj_core::track::Track::
    /// load`'s own analysis, the same one odj-player's editor view
    /// already uses — reused as-is rather than writing a second
    /// decoder) — refreshed only when the file on disk actually
    /// changes (`RecordMode::refresh_wave`, called on load and right
    /// after a capture stops), never on every redraw; empty while
    /// there's no file yet, or while a fresh pass is overwriting it.
    pub wave: Vec<odj_core::track::WaveBin>,
    pub wave_scale: f32,
    /// Where on the shared playhead timeline, in seconds, `wave`'s
    /// own first bin sits — `start_offset` converted from frames (at
    /// whatever rate the file was recorded at) to seconds.
    pub wave_start_secs: f64,
    /// `start_offset` plus this track's own recorded length, in
    /// frames at the same rate `start_offset` already is — i.e.
    /// exactly where this track's content ends on the shared playhead
    /// timeline. Used to find "the end" a looping Play track restarts
    /// from (see `RecordMode::poll`'s own auto-restart check); 0 while
    /// `wave` is empty.
    pub wave_end_offset: u64,
}

impl Track {
    /// Whether this track is actively writing to disk right now —
    /// `capture` itself stays private (the UI has no business
    /// touching the writer directly), but needs to know this much to
    /// show a track mid-take differently from one with settled,
    /// already-analyzed content.
    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct SavedTrack {
    input: Option<String>,
    start_offset: u64,
}

#[derive(Serialize, Deserialize)]
struct SavedState {
    version: u32,
    playhead: u64,
    tracks: [SavedTrack; TRACKS],
}

impl Default for SavedState {
    fn default() -> Self {
        Self { version: 1, playhead: 0, tracks: std::array::from_fn(|_| SavedTrack::default()) }
    }
}

fn state_dir() -> PathBuf {
    odj_core::memory::data_dir().join("multitrack-recorder")
}

fn state_file() -> PathBuf {
    state_dir().join("state.json")
}

/// Where track `slot`'s own recording lives (1-indexed in the file
/// name, matching how the UI numbers the rows) — fixed, not
/// timestamped like the old single-file `Recorder`'s own
/// `recording_path`, since starting a new pass always overwrites it
/// (see `start_track_capture`'s own doc comment).
pub fn track_path(slot: usize) -> PathBuf {
    state_dir().join(format!("track-{}.wav", slot + 1))
}

/// Where a `B`-triggered bounce lands — timestamped (same
/// `%Y-%m-%d-%H%M%S` naming, and the same `-2`/`-3`… disambiguation
/// on a same-second collision, as the old single-file `Recorder`'s
/// own `recording_path` used), alongside the 8 track files, so a
/// bounce never collides with — or gets overwritten by — a later
/// recording pass on any particular track.
fn bounce_path() -> Result<PathBuf> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).context("creating the multitrack-recorder folder")?;
    let base = chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string();
    let mut path = dir.join(format!("bounce-{base}.wav"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("bounce-{base}-{n}.wav"));
    }
    Ok(path)
}

pub struct RecordMode {
    tracks: [Track; TRACKS],
    running: bool,
    /// This session's own playhead, as last known — either just
    /// loaded from disk (before `poll` has had a chance to restore it
    /// onto a live engine) or mirrored from the live one afterward;
    /// used as `save`'s own fallback when nothing's open to read it
    /// from live.
    saved_playhead: u64,
    /// Set once `poll` has restored `saved_playhead` onto a live
    /// engine — after that, `saved_playhead` just tracks the live
    /// value instead.
    playhead_restored: bool,
}

impl RecordMode {
    /// Reads `state.json`, if there is one — every track's own
    /// `state` is forced to [`TrackState::default`] regardless of
    /// what (if anything) was ever written there, since track state
    /// is never persisted.
    pub fn load() -> Self {
        let saved: SavedState = odj_core::memory::read_json(&state_file()).unwrap_or_default();
        let tracks = std::array::from_fn(|i| Track {
            input: saved.tracks[i].input.as_deref().and_then(parse_device_key),
            state: TrackState::default(),
            start_offset: saved.tracks[i].start_offset,
            capture: None,
            ..Default::default()
        });
        let mut mode = Self { tracks, running: false, saved_playhead: saved.playhead, playhead_restored: false };
        for slot in 0..TRACKS {
            mode.refresh_wave(slot);
        }
        mode
    }

    /// Re-decodes track `slot`'s own file and caches its waveform
    /// (empties the cache if there's no file) — called whenever the
    /// file on disk actually changed: once per track on `load`, and
    /// right after a capture starts (clearing the *previous* pass's
    /// now-stale wave immediately, before its decoded replacement
    /// exists) or stops (installing the new one). Never called on
    /// every redraw — decoding a whole file is too expensive for
    /// that, and nothing about a settled file changes between
    /// captures anyway.
    fn refresh_wave(&mut self, slot: usize) {
        let track = &mut self.tracks[slot];
        let path = track_path(slot);
        let Ok(loaded) = odj_core::track::Track::load(&path) else {
            track.wave = Vec::new();
            track.wave_scale = 0.0;
            track.wave_start_secs = 0.0;
            track.wave_end_offset = 0;
            return;
        };
        track.wave_start_secs = track.start_offset as f64 / loaded.sample_rate as f64;
        track.wave_end_offset = track.start_offset + loaded.frames() as u64;
        track.wave = loaded.wave;
        track.wave_scale = loaded.wave_scale;
    }

    fn save(&self, routing: &Mutex<Routing>) {
        let playhead = routing.lock().unwrap_or_else(|e| e.into_inner()).playhead().unwrap_or(self.saved_playhead);
        let tracks = std::array::from_fn(|i| SavedTrack {
            input: self.tracks[i].input.as_ref().map(device_key_string),
            start_offset: self.tracks[i].start_offset,
        });
        if let Some(dir) = state_file().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = odj_core::memory::write_json(&state_file(), &SavedState { version: 1, playhead, tracks });
    }

    pub fn tracks(&self) -> &[Track; TRACKS] {
        &self.tracks
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Whether any track is actively writing to disk right now — the
    /// live screen's own "still recording" badge stays visible by
    /// this even while looking at a different screen.
    pub fn is_capturing(&self) -> bool {
        self.tracks.iter().any(|t| t.capture.is_some())
    }

    /// The shared playhead, live if there's an output open to read it
    /// from, else this session's own last-known value.
    pub fn playhead(&self, routing: &Mutex<Routing>) -> u64 {
        routing.lock().unwrap_or_else(|e| e.into_inner()).playhead().unwrap_or(self.saved_playhead)
    }

    /// Assigns (or clears, with `None`) track `slot`'s input — stops
    /// its capture first if one happens to be active (reassigning out
    /// from under a running recording would otherwise strand the tap
    /// on the old input). Un-mutes the old input (it's no longer this
    /// track's business) and mutes the new one if the track's own
    /// state isn't `Record` (see `apply_mute`).
    pub fn set_input(&mut self, slot: usize, input: Option<DeviceKey>, routing: &Mutex<Routing>) {
        self.stop_track_capture(slot, routing);
        if let Some(old) = &self.tracks[slot].input {
            routing.lock().unwrap_or_else(|e| e.into_inner()).set_input_muted(old, false);
        }
        self.tracks[slot].input = input;
        self.apply_mute(slot, routing);
        self.save(routing);
    }

    /// `S`/`Shift+S`: cycles the selected track's state, installing or
    /// tearing down whatever that transition needs on the live
    /// engine. Errors (a sample-rate mismatch, a decode failure) are
    /// returned rather than silently swallowed, for the caller's own
    /// status line — the state itself still doesn't change on one.
    pub fn cycle_state(&mut self, slot: usize, forward: bool, routing: &Mutex<Routing>) -> Result<()> {
        let next = if forward { self.tracks[slot].state.next() } else { self.tracks[slot].state.prev() };
        self.set_state(slot, next, routing)?;
        self.tracks[slot].state = next;
        self.apply_mute(slot, routing);
        self.save(routing);
        Ok(())
    }

    /// Mutes track `slot`'s own assigned input unless it's armed to
    /// `Record` — only a Record-armed track monitors its live input;
    /// `Mute` and `Play` both suppress the live pass-through (`Play`
    /// especially: otherwise you'd hear the live input *and* this
    /// track's own recorded take layered together). A no-op if the
    /// track has no input assigned.
    fn apply_mute(&self, slot: usize, routing: &Mutex<Routing>) {
        let track = &self.tracks[slot];
        if let Some(input) = &track.input {
            let muted = track.state != TrackState::Record;
            routing.lock().unwrap_or_else(|e| e.into_inner()).set_input_muted(input, muted);
        }
    }

    /// Called once, right when record mode's own screen opens —
    /// applies every track's current state for real: muting its
    /// assigned input's live pass-through unless it's `Record` (see
    /// `apply_mute`), and — since `Play` is the default a track
    /// starts in, both on a fresh load and after `stop_everything`'s
    /// own engine teardown — (re)installing a `Play` track's own
    /// decoded content, exactly as if it had just been cycled into
    /// that state. Nothing in `RecordMode` touches an input's mute
    /// (or a Play track's own buffer) before this runs, so opening
    /// record mode is the one moment mixer mode's own, independent
    /// behavior (every routed input audible, same as if record mode
    /// didn't exist) starts being overridden by record mode's own
    /// track states. Returns one message per track whose own file
    /// couldn't be loaded (a sample-rate mismatch, say) — silence, not
    /// an error, for a track with nothing recorded yet.
    pub fn enter(&self, routing: &Mutex<Routing>) -> Vec<String> {
        let mut messages = Vec::new();
        for slot in 0..TRACKS {
            self.apply_mute(slot, routing);
            if self.tracks[slot].state == TrackState::Play
                && let Err(e) = self.load_track_playback(slot, routing)
            {
                messages.push(format!("Track {}: {e}", slot + 1));
            }
        }
        messages
    }

    /// The inverse of [`enter`](Self::enter) — called when record
    /// mode's screen closes: restores every track's own input to
    /// normal, unmuted pass-through, so mixer mode is never left
    /// silently muted by a decision made on a different screen (see
    /// this module's own doc comment on why the two are kept
    /// independent).
    pub fn leave(&self, routing: &Mutex<Routing>) {
        for track in &self.tracks {
            if let Some(input) = &track.input {
                routing.lock().unwrap_or_else(|e| e.into_inner()).set_input_muted(input, false);
            }
        }
    }

    fn set_state(&mut self, slot: usize, next: TrackState, routing: &Mutex<Routing>) -> Result<()> {
        let prev = self.tracks[slot].state;
        if prev == TrackState::Record && next != TrackState::Record {
            self.stop_track_capture(slot, routing);
        }
        if prev == TrackState::Play && next != TrackState::Play {
            routing.lock().unwrap_or_else(|e| e.into_inner()).set_play_track(slot, None);
        }
        if next == TrackState::Play {
            self.load_track_playback(slot, routing)?;
        }
        if next == TrackState::Record && self.running {
            self.start_track_capture(slot, routing)?;
        }
        Ok(())
    }

    /// Decodes track `slot`'s own file (if it has one yet — silence,
    /// not an error, if it doesn't) and installs it on the live
    /// engine at its own `start_offset`. Refuses a track recorded at
    /// a different rate than the current output, rather than
    /// silently playing it back at the wrong speed (mirrors
    /// `Routing::try_set_route`'s own socket-link rate guard).
    fn load_track_playback(&self, slot: usize, routing: &Mutex<Routing>) -> Result<()> {
        let path = track_path(slot);
        if !path.is_file() {
            routing.lock().unwrap_or_else(|e| e.into_inner()).set_play_track(slot, None);
            return Ok(());
        }
        let decoded = odj_core::track::decode(&path).with_context(|| format!("decoding track {}'s own recording", slot + 1))?;
        if let Some(rate) = routing.lock().unwrap_or_else(|e| e.into_inner()).main_output_rate() {
            ensure!(
                rate == decoded.sample_rate,
                "track {} was recorded at {} Hz but the output is at {rate} Hz — playback doesn't resample, so they must match",
                slot + 1,
                decoded.sample_rate
            );
        }
        let start_offset = self.tracks[slot].start_offset;
        routing.lock().unwrap_or_else(|e| e.into_inner()).set_play_track(slot, Some(PlayTrack { samples: decoded.samples, start_offset }));
        Ok(())
    }

    /// Starts a fresh recording pass on track `slot`: a new writer
    /// (reusing [`Recording`] as-is), tapped onto its assigned input
    /// at the engine's own current playhead. `Recording::start`
    /// truncates the file (`WavWriter::create` always does) — so this
    /// is also how a track gets "re-recorded": its previous take is
    /// simply gone the moment a new pass begins, no append/resume
    /// support in this version. A no-op if the track has no input
    /// assigned yet (armed to Record with nothing to capture).
    fn start_track_capture(&mut self, slot: usize, routing: &Mutex<Routing>) -> Result<()> {
        let Some(input) = self.tracks[slot].input.clone() else { return Ok(()) };
        let rate = routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .main_output_rate()
            .ok_or_else(|| anyhow!("nothing's playing yet — add a channel before recording"))?;
        let playhead = routing.lock().unwrap_or_else(|e| e.into_inner()).playhead().unwrap_or(0);
        let (prod, cons) = HeapRb::<f32>::new(CAPTURE_RING_STEREO_SAMPLES).split();
        let rec = Recording::start(track_path(slot), 2, rate, cons)?;
        if !routing.lock().unwrap_or_else(|e| e.into_inner()).set_input_tap(&input, Some(prod)) {
            let _ = rec.stop();
            bail!("{} isn't routed anywhere right now", input.0);
        }
        self.tracks[slot].start_offset = playhead;
        self.tracks[slot].capture = Some(rec);
        // The file about to be overwritten is gone the moment this
        // starts (`Recording::start` truncates it) — drop its now-stale
        // cached waveform immediately rather than showing it a beat
        // longer than the file it came from actually exists.
        self.tracks[slot].wave = Vec::new();
        self.tracks[slot].wave_end_offset = 0;
        Ok(())
    }

    /// Stops track `slot`'s own capture, if one's active — tears the
    /// tap down first, same order the old `Recorder` used, so the
    /// writer thread isn't left draining a tap that no longer exists.
    fn stop_track_capture(&mut self, slot: usize, routing: &Mutex<Routing>) {
        let Some(rec) = self.tracks[slot].capture.take() else { return };
        if let Some(input) = &self.tracks[slot].input {
            routing.lock().unwrap_or_else(|e| e.into_inner()).set_input_tap(input, None);
        }
        let _ = rec.stop();
        self.refresh_wave(slot); // the file just finished — cache its real waveform
    }

    /// `R`: if anything's actively capturing right now, stops just
    /// those captures (leaving `running` — and any Play tracks —
    /// untouched). Otherwise starts the transport (if it wasn't
    /// already) and a fresh capture on every track currently armed to
    /// Record, returning one message per track that failed to start.
    pub fn toggle_record(&mut self, routing: &Mutex<Routing>) -> Vec<String> {
        if self.tracks.iter().any(|t| t.capture.is_some()) {
            for slot in 0..TRACKS {
                self.stop_track_capture(slot, routing);
            }
            self.save(routing);
            return Vec::new();
        }
        self.set_running(true, routing);
        let mut messages = Vec::new();
        for slot in 0..TRACKS {
            if self.tracks[slot].state == TrackState::Record
                && let Err(e) = self.start_track_capture(slot, routing)
            {
                messages.push(format!("Track {}: {e}", slot + 1));
            }
        }
        self.save(routing);
        messages
    }

    /// `P`: toggles the transport without touching any track's own
    /// state — turning it off also stops every active capture (see
    /// this module's own doc comment on why: capturing while the
    /// playhead is frozen would corrupt the alignment every Play
    /// track relies on), but a track armed to Play stays armed, just
    /// frozen along with the playhead.
    pub fn toggle_play(&mut self, routing: &Mutex<Routing>) {
        if self.running {
            self.stop_all_captures(routing);
            self.set_running(false, routing);
        } else {
            self.set_running(true, routing);
        }
        self.save(routing);
    }

    /// `X`: stops the transport if it's running (same effect as
    /// [`toggle_play`](Self::toggle_play) turning it off); rewinds the
    /// playhead to 0 instead if it was already stopped.
    pub fn stop_or_rewind(&mut self, routing: &Mutex<Routing>) {
        if self.running {
            self.stop_all_captures(routing);
            self.set_running(false, routing);
        } else {
            routing.lock().unwrap_or_else(|e| e.into_inner()).rewind_playhead();
            self.saved_playhead = 0;
        }
        self.save(routing);
    }

    /// Tears down every active capture and Play track, and stops the
    /// transport — called before the main output itself changes (see
    /// `Routing::set_main_output`'s own doc comment: switching outputs
    /// drops the whole engine these are installed on, taps and all),
    /// same guard the old `Recorder` needed for its own single tap.
    pub fn stop_everything(&mut self, routing: &Mutex<Routing>) {
        self.stop_all_captures(routing);
        self.set_running(false, routing);
        for slot in 0..TRACKS {
            routing.lock().unwrap_or_else(|e| e.into_inner()).set_play_track(slot, None);
            // Nothing's actually armed on the engine side any more
            // after this — resetting each track's own state too
            // avoids a row that still *looks* armed to Record/Play
            // but silently isn't, once the swap finishes.
            self.tracks[slot].state = TrackState::Mute;
        }
        self.save(routing);
    }

    fn stop_all_captures(&mut self, routing: &Mutex<Routing>) {
        for slot in 0..TRACKS {
            self.stop_track_capture(slot, routing);
        }
    }

    fn set_running(&mut self, running: bool, routing: &Mutex<Routing>) {
        self.running = running;
        routing.lock().unwrap_or_else(|e| e.into_inner()).set_running(running);
    }

    /// Called every tick, unconditionally — not just while the record
    /// screen is open, same as the old `Recorder::poll_error` wasn't
    /// either. Restores this session's own saved playhead onto the
    /// live engine the first time one exists (so a track recorded
    /// before a restart stays aligned with one recorded after it),
    /// then stops any track whose input has since stopped being
    /// routed (unplugged, forgotten — `is_routed` is the live
    /// screen's own freshly-scanned device set, same check its
    /// `forget_input` reconciliation already makes) and surfaces any
    /// writer-thread error.
    pub fn poll(&mut self, routing: &Mutex<Routing>, is_routed: impl Fn(&DeviceKey) -> bool) -> Vec<String> {
        {
            let mut r = routing.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(live) = r.playhead() {
                if self.playhead_restored {
                    self.saved_playhead = live;
                } else {
                    r.set_playhead(self.saved_playhead);
                    self.playhead_restored = true;
                }
            }
        }

        // Auto-restart: once running (and nothing's actively
        // capturing — wrapping mid-capture would corrupt that track's
        // own `start_offset`-based alignment, the same invariant
        // `toggle_play`'s own teardown-on-stop already protects)
        // reaches the end of whatever's currently armed to Play, loop
        // back to 0 rather than running on into near-silence, same as
        // a hardware looper's own behavior. "The end" is the furthest
        // `wave_end_offset` among tracks actually armed to Play right
        // now — a Mute or Record track (open-ended, still growing)
        // doesn't count toward it.
        if self.running && !self.is_capturing() {
            let loop_end =
                self.tracks.iter().filter(|t| t.state == TrackState::Play && !t.wave.is_empty()).map(|t| t.wave_end_offset).max();
            if let Some(end) = loop_end
                && routing.lock().unwrap_or_else(|e| e.into_inner()).playhead().is_some_and(|p| p >= end)
            {
                routing.lock().unwrap_or_else(|e| e.into_inner()).rewind_playhead();
                self.saved_playhead = 0;
            }
        }

        let mut messages = Vec::new();
        for slot in 0..TRACKS {
            if self.tracks[slot].capture.is_none() {
                continue;
            }
            let still_routed = self.tracks[slot].input.as_ref().is_some_and(&is_routed);
            if !still_routed {
                self.stop_track_capture(slot, routing);
                messages.push(format!("Track {}: recording stopped — input disconnected", slot + 1));
                continue;
            }
            if let Some(e) = self.tracks[slot].capture.as_ref().and_then(Recording::take_error) {
                self.stop_track_capture(slot, routing);
                messages.push(format!("Track {}: {e}", slot + 1));
            }
        }
        if !messages.is_empty() {
            self.save(routing);
        }
        messages
    }

    /// `B`: downmixes every track currently armed to `Play` into one
    /// new stereo WAV file (see `bounce_path`'s own doc comment for
    /// where it lands) — a sample-accurate sum of each contributing
    /// track at its own `start_offset`, decoded fresh from disk (the
    /// cached `wave` only keeps peaks for the waveform display, not
    /// full-resolution samples). Pure file post-processing: unlike
    /// everything else in this module, this never touches the live
    /// engine or `routing` at all, so it works the same whether the
    /// transport is running or not, and doesn't depend on which
    /// output (if any) is currently open. Refuses if the contributing
    /// tracks don't all share the same sample rate — mixing them
    /// without resampling would be wrong, same reasoning as the
    /// rate guard `Play` itself already enforces.
    pub fn bounce(&self) -> Result<PathBuf> {
        let mut rate = None;
        let mut contributions: Vec<(u64, Vec<f32>)> = Vec::new();
        for (i, track) in self.tracks.iter().enumerate() {
            if track.state != TrackState::Play {
                continue;
            }
            let path = track_path(i);
            if !path.is_file() {
                continue;
            }
            let decoded = odj_core::track::decode(&path).with_context(|| format!("decoding track {}'s own recording", i + 1))?;
            match rate {
                None => rate = Some(decoded.sample_rate),
                Some(r) => ensure!(
                    r == decoded.sample_rate,
                    "track {} is at {} Hz but another Play track is at {r} Hz — bouncing doesn't resample, so they must match",
                    i + 1,
                    decoded.sample_rate
                ),
            }
            contributions.push((track.start_offset, decoded.samples));
        }
        let rate = rate.ok_or_else(|| anyhow!("nothing's armed to Play — nothing to bounce"))?;

        let frames = contributions.iter().map(|(offset, samples)| offset + (samples.len() / 2) as u64).max().unwrap_or(0);
        let mut mix = vec![0.0f32; frames as usize * 2];
        for (offset, samples) in &contributions {
            let start = *offset as usize * 2;
            for (i, &s) in samples.iter().enumerate() {
                mix[start + i] += s;
            }
        }

        let path = bounce_path()?;
        let mut writer = odj_core::wav::WavWriter::create(&path, 2, rate)?;
        writer.write(&mix)?;
        writer.finish()?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use odj_core::audio::Route;
    use odj_core::audio_link::Listener;

    use super::*;
    use crate::devices::{Devices, Kind};

    /// Points `XDG_DATA_HOME` at a fresh temp directory for the span
    /// of one test — tests across the crate share this process's
    /// environment, same isolation trick `recorder.rs`'s own tests
    /// use. Returns it for cleanup.
    fn isolate() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("odj-mixer-record-mode-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &root);
        }
        root
    }

    fn key(name: &str) -> DeviceKey {
        (name.to_string(), Route::Stereo(0, 1))
    }

    /// `Routing::set_main_output` alone never actually opens anything
    /// — `self.outputs` only gets an entry once some input is really
    /// routed to it (see `try_set_route`). These tests only exercise
    /// data-level bookkeeping (load/save, state cycling, arming) that
    /// doesn't need a live engine at all — every `Routing` call they
    /// make against a bare `Routing::default()` is a documented no-op
    /// (`false`/`None`), never a panic, which is itself part of what's
    /// being tested (e.g. "no channel yet" is a status message, not a
    /// crash).
    fn bare_routing() -> Mutex<Routing> {
        Mutex::new(Routing::default())
    }

    #[test]
    fn load_always_resets_every_track_to_its_default_state() {
        let root = isolate();
        let mode = RecordMode::load();
        for t in mode.tracks() {
            assert_eq!(t.state, TrackState::default());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn save_and_load_round_trips_input_assignment_and_start_offset() {
        let root = isolate();
        let routing = bare_routing();
        let mut mode = RecordMode::load();
        mode.set_input(0, Some(key("mic")), &routing);
        mode.tracks[2].start_offset = 12_345;
        mode.save(&routing);

        let reloaded = RecordMode::load();
        assert_eq!(reloaded.tracks()[0].input, Some(key("mic")));
        assert_eq!(reloaded.tracks()[2].start_offset, 12_345);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cycle_state_goes_play_mute_record_play() {
        let root = isolate();
        let routing = bare_routing();
        let mut mode = RecordMode::load();
        assert_eq!(mode.tracks()[0].state, TrackState::Play, "no file yet, but that's silence, not an error");
        mode.cycle_state(0, true, &routing).unwrap();
        assert_eq!(mode.tracks()[0].state, TrackState::Mute);
        mode.cycle_state(0, true, &routing).unwrap();
        assert_eq!(mode.tracks()[0].state, TrackState::Record);
        mode.cycle_state(0, true, &routing).unwrap();
        assert_eq!(mode.tracks()[0].state, TrackState::Play);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn toggle_record_arms_running_if_it_wasnt_already() {
        let root = isolate();
        let routing = bare_routing();
        let mut mode = RecordMode::load();
        assert!(!mode.is_running());
        mode.toggle_record(&routing);
        assert!(mode.is_running(), "R always starts the transport if it wasn't running");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn toggle_play_flips_running_and_back() {
        let root = isolate();
        let routing = bare_routing();
        let mut mode = RecordMode::load();
        mode.toggle_play(&routing);
        assert!(mode.is_running());
        mode.toggle_play(&routing);
        assert!(!mode.is_running());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn entering_play_on_a_track_with_no_recorded_file_yet_is_silence_not_an_error() {
        let root = isolate();
        let routing = bare_routing();
        let mut mode = RecordMode::load();
        assert!(mode.set_state(0, TrackState::Play, &routing).is_ok());
        std::fs::remove_dir_all(&root).unwrap();
    }

    // --- `bounce` is pure file post-processing — it never touches
    // `routing` at all, so none of these need the hardware fixture.

    fn write_track(slot: usize, rate: u32, samples: &[f32]) {
        std::fs::create_dir_all(track_path(slot).parent().unwrap()).unwrap();
        let mut w = odj_core::wav::WavWriter::create(&track_path(slot), 2, rate).unwrap();
        w.write(samples).unwrap();
        w.finish().unwrap();
    }

    #[test]
    fn bounce_sums_every_play_armed_track_at_its_own_start_offset() {
        let root = isolate();
        let mut mode = RecordMode::load();

        write_track(0, 48_000, &[0.1, 0.1, 0.2, 0.2]); // 2 frames, starting at 0
        mode.tracks[0].state = TrackState::Play;
        mode.tracks[0].start_offset = 0;

        write_track(1, 48_000, &[0.05, 0.05]); // 1 frame, starting at frame 1 — overlaps track 0's 2nd frame
        mode.tracks[1].state = TrackState::Play;
        mode.tracks[1].start_offset = 1;

        let path = mode.bounce().unwrap();
        let decoded = odj_core::track::decode(&path).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.samples.len(), 4, "exactly 2 frames total");
        assert!((decoded.samples[0] - 0.1).abs() < 0.01, "{:?}", decoded.samples);
        assert!((decoded.samples[1] - 0.1).abs() < 0.01, "{:?}", decoded.samples);
        assert!((decoded.samples[2] - 0.25).abs() < 0.01, "0.2 + 0.05 overlapping: {:?}", decoded.samples);
        assert!((decoded.samples[3] - 0.25).abs() < 0.01, "{:?}", decoded.samples);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bounce_skips_tracks_not_armed_to_play() {
        let root = isolate();
        let mut mode = RecordMode::load();

        write_track(0, 48_000, &[0.5, 0.5]);
        mode.tracks[0].state = TrackState::Mute; // has a file, but not armed to Play

        write_track(1, 48_000, &[0.2, 0.2]);
        mode.tracks[1].state = TrackState::Play;
        mode.tracks[1].start_offset = 0;

        let path = mode.bounce().unwrap();
        let decoded = odj_core::track::decode(&path).unwrap();
        assert!((decoded.samples[0] - 0.2).abs() < 0.01, "only the Play track contributed: {:?}", decoded.samples);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bounce_errors_when_nothing_is_armed_to_play() {
        let root = isolate();
        // Fresh load: every track defaults to `Play`, but none has a file yet.
        let mode = RecordMode::load();
        let err = mode.bounce().unwrap_err();
        assert!(err.to_string().contains("nothing"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bounce_refuses_mismatched_sample_rates() {
        let root = isolate();
        let mut mode = RecordMode::load();

        write_track(0, 48_000, &[0.1, 0.1]);
        mode.tracks[0].state = TrackState::Play;
        write_track(1, 44_100, &[0.1, 0.1]);
        mode.tracks[1].state = TrackState::Play;

        let err = mode.bounce().unwrap_err();
        assert!(err.to_string().contains("Hz"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bounce_path_is_timestamped_and_lands_alongside_the_tracks() {
        let root = isolate();
        let mut mode = RecordMode::load();
        write_track(0, 48_000, &[0.1, 0.1]);
        mode.tracks[0].state = TrackState::Play;

        let path = mode.bounce().unwrap();
        assert_eq!(path.parent(), track_path(0).parent());
        assert!(path.file_name().unwrap().to_str().unwrap().starts_with("bounce-"), "{path:?}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    // --- Tests below need a genuinely open output (so `main_output_rate`/
    // `playhead`/`set_input_tap` have something real to act on) — same
    // hardware dependency, and the same socket-link trick to avoid
    // needing a real *input* device too, as `routing.rs`'s own
    // `#[ignore = "opens a real output on this machine"]` tests.

    fn write_raw_handshake(stream: &mut std::os::unix::net::UnixStream, name: &str, sample_rate: u32) {
        // A longer name would overwrite the sample-rate bytes right
        // after this fixed 32-byte field instead of failing loudly —
        // confirmed the hard way (several `#[ignore]`d tests below
        // silently got a corrupted rate before this was caught).
        assert!(name.len() <= 32, "fake deck name {name:?} is longer than the handshake's 32-byte name field");
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

    /// A `Routing` with one fake deck (no real input hardware needed,
    /// just `odj_core::audio_link`'s own socket transport) actually
    /// routed to one real output device — a genuinely open
    /// `MixedSource`, so `main_output_rate`/`playhead`/`set_input_tap`
    /// all work for real. Returns the routing, the fake deck's own
    /// `DeviceKey` (to assign to a track), the client socket (keep
    /// alive — dropping it disconnects), and the runtime dir to clean
    /// up afterward.
    fn fake_routed_input(tag: &str) -> (Mutex<Routing>, DeviceKey, std::os::unix::net::UnixStream, PathBuf) {
        let dir = std::env::temp_dir().join(format!("odj-mixer-record-mode-link-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
        }
        let listener = Arc::new(Listener::start().unwrap());
        // Kept short: the raw handshake's own name field is a fixed
        // 32 bytes (see `write_raw_handshake`) — a longer one would
        // silently corrupt the sample-rate bytes right after it
        // rather than failing loudly.
        let name = format!("rm-{tag}");
        let mut client = std::os::unix::net::UnixStream::connect(dir.join("odj/audio/mixer")).unwrap();
        write_raw_handshake(&mut client, &name, 48_000);

        let devices_in = Devices::start(Kind::Input);
        let devices_out = Devices::start(Kind::Output);
        let output_key = any_output_key(&devices_out);

        let mut routing = Routing::default();
        routing.set_socket_listener(listener.clone());
        // `set_route` alone opens `output_key`'s own `MixedSource` but
        // never marks it as *main* — every `main_output`-gated method
        // (`playhead`, `set_input_tap`, …) silently no-ops without
        // this, same as it would against a real app that never picked
        // an output yet.
        routing.set_main_output(Some(output_key.clone()));
        let input_key = (name, Route::Stereo(0, 1));
        routing.set_route(&devices_in, &devices_out, &input_key, Some(&output_key)).unwrap();

        (Mutex::new(routing), input_key, client, dir)
    }

    /// Like [`fake_routed_input`], but with `count` fake decks all
    /// routed to the *same* real output through one shared listener —
    /// for a test that needs two genuinely-distinct routed inputs at
    /// once (e.g. reassigning a track from one to the other).
    fn fake_routed_inputs(
        tag: &str,
        count: usize,
    ) -> (Mutex<Routing>, Vec<DeviceKey>, Vec<std::os::unix::net::UnixStream>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("odj-mixer-record-mode-link-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
        }
        let listener = Arc::new(Listener::start().unwrap());
        let devices_in = Devices::start(Kind::Input);
        let devices_out = Devices::start(Kind::Output);
        let output_key = any_output_key(&devices_out);

        let mut routing = Routing::default();
        routing.set_socket_listener(listener.clone());
        routing.set_main_output(Some(output_key.clone()));

        let (mut keys, mut clients) = (Vec::new(), Vec::new());
        for i in 0..count {
            let name = format!("rm-{tag}-{i}"); // kept short — see `write_raw_handshake`'s own doc comment
            let mut client = std::os::unix::net::UnixStream::connect(dir.join("odj/audio/mixer")).unwrap();
            write_raw_handshake(&mut client, &name, 48_000);
            let input_key = (name, Route::Stereo(0, 1));
            routing.set_route(&devices_in, &devices_out, &input_key, Some(&output_key)).unwrap();
            keys.push(input_key);
            clients.push(client);
        }
        (Mutex::new(routing), keys, clients, dir)
    }

    /// Tears the fake deck's own route down explicitly (stopping and
    /// joining its socket-reader thread, same as `clear_route` always
    /// does) rather than just letting `Mutex<Routing>` drop silently
    /// — nothing drops *that* thread on its own, and it holds its own
    /// clone of the `Listener`, so skipping this would leak both past
    /// the end of the test.
    fn cleanup(routing: &Mutex<Routing>, input: &DeviceKey, dir: PathBuf) {
        routing.lock().unwrap_or_else(|e| e.into_inner()).forget_input(input);
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn entering_record_mode_mutes_a_track_not_armed_to_record() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("enter-mutes-non-record");
        let mut mode = RecordMode::load();
        // Simulates a track whose input assignment came from a
        // previous session's saved state.json (so `set_input`, which
        // applies mute immediately, was never called this session) —
        // the one case `enter` itself actually has to do anything.
        mode.tracks[0].input = Some(input.clone());
        assert!(!routing.lock().unwrap().is_input_muted(&input), "nothing's touched this input's mute yet");

        mode.enter(&routing);
        assert!(routing.lock().unwrap().is_input_muted(&input), "not Record (the default is Play), so it's muted once record mode is open");

        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn entering_record_mode_installs_playback_for_a_track_already_set_to_play() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("enter-loads-play");
        let rate = routing.lock().unwrap().main_output_rate().unwrap();
        std::fs::create_dir_all(track_path(0).parent().unwrap()).unwrap();
        // A rate mismatch is the simplest way to observe, from outside,
        // that `enter` genuinely tried to load this track's own file —
        // there's no public getter for "is a play track installed".
        let mut writer = odj_core::wav::WavWriter::create(&track_path(0), 2, rate + 1).unwrap();
        writer.write(&[0.0, 0.0]).unwrap();
        writer.finish().unwrap();

        // Track 0 is `Play` by default — `cycle_state`/`set_state` is
        // never called this session, so if anything surfaces this
        // error, it can only be `enter` itself doing the loading.
        let mode = RecordMode::load();
        assert_eq!(mode.tracks()[0].state, TrackState::Play);
        let messages = mode.enter(&routing);
        assert!(messages.iter().any(|m| m.contains("Hz")), "{messages:?}");

        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn set_input_applies_the_tracks_current_mute_state_immediately() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("set-input-applies-mute");
        let mut mode = RecordMode::load();
        // No explicit `enter()` call: in practice `set_input` is only
        // ever reachable through the UI after record mode's already
        // open (and `enter`'s already run), so it has to apply the
        // track's own current state right away, not wait for a
        // separate step.
        mode.set_input(0, Some(input.clone()), &routing);
        assert!(routing.lock().unwrap().is_input_muted(&input), "default state (Play) isn't Record either, so it's still muted");

        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn cycling_to_record_unmutes_the_assigned_input() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("cycle-unmutes");
        let mut mode = RecordMode::load();
        mode.set_input(0, Some(input.clone()), &routing);
        mode.enter(&routing);
        assert!(routing.lock().unwrap().is_input_muted(&input), "Play (the default): suppressed");

        mode.cycle_state(0, true, &routing).unwrap(); // Play -> Mute
        assert!(routing.lock().unwrap().is_input_muted(&input), "Mute: still suppressed");

        mode.cycle_state(0, true, &routing).unwrap(); // Mute -> Record
        assert!(!routing.lock().unwrap().is_input_muted(&input), "Record: monitors the live input");

        mode.cycle_state(0, true, &routing).unwrap(); // Record -> Play
        assert!(routing.lock().unwrap().is_input_muted(&input), "Play: suppressed again");

        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn leaving_record_mode_restores_normal_pass_through() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("leave-restores");
        let mut mode = RecordMode::load();
        mode.set_input(0, Some(input.clone()), &routing);
        mode.enter(&routing);
        assert!(routing.lock().unwrap().is_input_muted(&input));

        mode.leave(&routing);
        assert!(!routing.lock().unwrap().is_input_muted(&input), "mixer mode is never left muted by a record-mode decision");

        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn reassigning_a_tracks_input_unmutes_the_old_one_and_mutes_the_new_one() {
        let root = isolate();
        let (routing, inputs, _clients, dir) = fake_routed_inputs("reassign", 2);
        let (old_input, new_input) = (inputs[0].clone(), inputs[1].clone());

        let mut mode = RecordMode::load();
        mode.set_input(0, Some(old_input.clone()), &routing);
        mode.enter(&routing);
        assert!(routing.lock().unwrap().is_input_muted(&old_input));

        mode.set_input(0, Some(new_input.clone()), &routing);
        assert!(!routing.lock().unwrap().is_input_muted(&old_input), "no longer this track's business");
        assert!(routing.lock().unwrap().is_input_muted(&new_input), "the new assignment inherits the track's own (still not Record) state");

        routing.lock().unwrap().forget_input(&old_input);
        cleanup(&routing, &new_input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn stop_or_rewind_rewinds_only_once_already_stopped() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("rewind");
        let mut mode = RecordMode::load();
        mode.toggle_play(&routing); // start running
        routing.lock().unwrap().set_playhead(100);
        mode.stop_or_rewind(&routing); // first press: stop, not rewind
        assert!(!mode.is_running());
        assert_eq!(routing.lock().unwrap().playhead(), Some(100), "stopping doesn't also rewind");
        mode.stop_or_rewind(&routing); // second press, already stopped: rewind
        assert_eq!(routing.lock().unwrap().playhead(), Some(0));
        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn poll_restores_the_saved_playhead_onto_a_freshly_opened_output() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("restore");
        {
            let mut mode = RecordMode::load();
            mode.toggle_play(&routing);
            routing.lock().unwrap().set_playhead(500);
            mode.save(&routing);
        }
        let mut mode = RecordMode::load();
        mode.poll(&routing, |_| false);
        assert_eq!(routing.lock().unwrap().playhead(), Some(500), "restored from the saved session, not left at the engine's fresh 0");
        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn starting_a_new_recording_pass_truncates_the_tracks_existing_file() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("truncate");
        let mut mode = RecordMode::load();
        mode.set_input(0, Some(input.clone()), &routing);
        mode.cycle_state(0, false, &routing).unwrap(); // Play -> Record (one step back)

        mode.toggle_record(&routing);
        std::thread::sleep(Duration::from_millis(80));
        mode.toggle_record(&routing);
        let first_len = std::fs::metadata(track_path(0)).unwrap().len();

        mode.toggle_record(&routing); // a fresh pass on the same track
        std::thread::sleep(Duration::from_millis(80));
        mode.toggle_record(&routing);
        let second_len = std::fs::metadata(track_path(0)).unwrap().len();

        assert!(second_len < first_len * 3, "overwritten by the second pass, not appended onto the first: {first_len} vs {second_len}");
        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn toggle_play_off_stops_an_active_capture() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("stop-capture");
        let mut mode = RecordMode::load();
        mode.set_input(0, Some(input.clone()), &routing);
        mode.cycle_state(0, false, &routing).unwrap(); // Play -> Record
        mode.toggle_record(&routing);
        assert!(mode.tracks()[0].capture.is_some());
        mode.toggle_play(&routing); // turning the transport off
        assert!(mode.tracks()[0].capture.is_none(), "capture torn down, not left writing with a frozen playhead");
        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn poll_stops_a_tracks_capture_when_its_input_is_no_longer_routed() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("poll-disconnect");
        let mut mode = RecordMode::load();
        mode.set_input(0, Some(input.clone()), &routing);
        mode.cycle_state(0, false, &routing).unwrap(); // Play -> Record
        mode.toggle_record(&routing);
        assert!(mode.tracks()[0].capture.is_some());
        let messages = mode.poll(&routing, |_| false); // nothing known as routed any more
        assert!(!messages.is_empty());
        assert!(mode.tracks()[0].capture.is_none());
        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn stop_everything_tears_down_every_active_capture_and_play_track() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("stop-everything");
        let mut mode = RecordMode::load();
        mode.set_input(0, Some(input.clone()), &routing);
        mode.cycle_state(0, false, &routing).unwrap(); // Play -> Record
        mode.toggle_record(&routing);
        assert!(mode.tracks()[0].capture.is_some());
        mode.stop_everything(&routing);
        assert!(mode.tracks()[0].capture.is_none());
        assert!(!mode.is_running());
        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn poll_auto_restarts_the_playhead_once_a_play_armed_track_reaches_its_end() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("auto-restart");
        let rate = routing.lock().unwrap().main_output_rate().unwrap();
        std::fs::create_dir_all(track_path(0).parent().unwrap()).unwrap();
        let mut writer = odj_core::wav::WavWriter::create(&track_path(0), 2, rate).unwrap();
        writer.write(&vec![0.1f32; 200]).unwrap(); // 100 frames
        writer.finish().unwrap();

        let mut mode = RecordMode::load();
        mode.set_state(0, TrackState::Play, &routing).unwrap();
        mode.tracks[0].state = TrackState::Play;
        mode.toggle_play(&routing); // start running
        mode.poll(&routing, |_| true); // consumes the one-time playhead restore (to 0), before setting it ourselves below

        routing.lock().unwrap().set_playhead(50); // still inside the 100-frame track
        mode.poll(&routing, |_| true);
        assert_eq!(routing.lock().unwrap().playhead(), Some(50), "hasn't reached the end yet");

        routing.lock().unwrap().set_playhead(150); // past the end
        mode.poll(&routing, |_| true);
        assert_eq!(routing.lock().unwrap().playhead(), Some(0), "restarted on its own, like a hardware looper");

        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn poll_does_not_auto_restart_while_a_track_is_actively_capturing() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("no-restart-while-capturing");
        let rate = routing.lock().unwrap().main_output_rate().unwrap();
        std::fs::create_dir_all(track_path(0).parent().unwrap()).unwrap();
        let mut writer = odj_core::wav::WavWriter::create(&track_path(0), 2, rate).unwrap();
        writer.write(&vec![0.1f32; 200]).unwrap(); // 100 frames
        writer.finish().unwrap();

        let mut mode = RecordMode::load();
        mode.set_state(0, TrackState::Play, &routing).unwrap();
        mode.tracks[0].state = TrackState::Play;
        // Track 2 is actively capturing — wrapping the playhead now
        // would corrupt its own `start_offset`-based alignment, so
        // the auto-restart must stay off until it stops.
        mode.set_input(1, Some(input.clone()), &routing);
        mode.cycle_state(1, false, &routing).unwrap(); // Play -> Record
        mode.toggle_record(&routing);
        assert!(mode.tracks()[1].is_capturing());
        mode.poll(&routing, |_| true); // consumes the one-time playhead restore (to 0), before setting it ourselves below

        routing.lock().unwrap().set_playhead(150); // past track 1's own end
        mode.poll(&routing, |_| true);
        assert_eq!(routing.lock().unwrap().playhead(), Some(150), "left running free while something's still capturing");

        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[ignore = "opens a real output on this machine"]
    fn entering_play_refuses_a_sample_rate_mismatch() {
        let root = isolate();
        let (routing, input, _client, dir) = fake_routed_input("rate-mismatch");
        let rate = routing.lock().unwrap().main_output_rate().unwrap();
        std::fs::create_dir_all(track_path(0).parent().unwrap()).unwrap();
        let mut writer = odj_core::wav::WavWriter::create(&track_path(0), 2, rate + 1).unwrap();
        writer.write(&[0.0, 0.0]).unwrap();
        writer.finish().unwrap();

        let mut mode = RecordMode::load();
        let err = mode.set_state(0, TrackState::Play, &routing).unwrap_err();
        assert!(err.to_string().contains("Hz"), "{err}");
        cleanup(&routing, &input, dir);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
