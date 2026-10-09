//! Beat grids: where a track's beats and bars fall, at a constant tempo.

use serde::{Deserialize, Serialize};

/// Beats per bar; odj assumes 4/4.
pub const BAR: i64 = 4;

/// A constant-tempo grid. Positions are frames at the track's sample rate,
/// which every method takes as `rate`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeatGrid {
    pub bpm: f64,
    /// Frame of a downbeat (beat 1 of a bar). Beat `n` is at `anchor + n` beats.
    pub anchor: f64,
}

impl BeatGrid {
    /// Frames per beat.
    pub fn period(&self, rate: f64) -> f64 {
        60.0 / self.bpm * rate
    }

    /// The beat index at `pos`, fractional between beats.
    pub fn beat_at(&self, pos: f64, rate: f64) -> f64 {
        (pos - self.anchor) / self.period(rate)
    }

    /// The frame of beat `n`.
    pub fn beat_pos(&self, n: f64, rate: f64) -> f64 {
        self.anchor + n * self.period(rate)
    }

    pub fn nearest(&self, pos: f64, rate: f64) -> f64 {
        self.beat_pos(self.beat_at(pos, rate).round(), rate)
    }

    /// `n` beats on from `pos` along the grid: from a beat it moves by whole
    /// beats, from between beats the first step lands on the next beat that way.
    pub fn step(&self, pos: f64, n: i64, rate: f64) -> f64 {
        let b = self.beat_at(pos, rate);
        let on_beat = (pos - self.nearest(pos, rate)).abs() < 1.0;
        let from = match n {
            _ if on_beat => b.round(),
            n if n > 0 => b.ceil() - 1.0,
            _ => b.floor() + 1.0,
        };
        self.beat_pos(from + n as f64, rate)
    }

    /// The same grid at another tempo, with the beat at `pos` kept in place.
    pub fn with_bpm(&self, bpm: f64, pos: f64, rate: f64) -> Self {
        let fixed = self.nearest(pos, rate);
        let n = self.beat_at(fixed, rate).round();
        let bar_phase = n.rem_euclid(BAR as f64);
        let grid = Self { bpm, anchor: fixed };
        Self { anchor: fixed - bar_phase * grid.period(rate), ..grid }
    }

    pub fn shifted(&self, frames: f64) -> Self {
        Self { anchor: self.anchor + frames, ..*self }
    }

    /// Bar and beat at `pos`, both from 1, counting bar 1 from the track's
    /// first downbeat; a pickup before it is bar 0.
    pub fn bar_beat(&self, pos: f64, rate: f64) -> (i64, i64) {
        let period = self.period(rate);
        let bar = BAR as f64 * period;
        // The first downbeat, allowing it to sit a little before the start.
        let mut first = self.anchor.rem_euclid(bar);
        if first > bar - period / 2.0 {
            first -= bar;
        }
        let n = ((pos - first + 1.0) / period).floor() as i64;
        (n.div_euclid(BAR) + 1, n.rem_euclid(BAR) + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;
    // 120 BPM: 24 000 frames a beat.
    const GRID: BeatGrid = BeatGrid { bpm: 120.0, anchor: 1_000.0 };

    #[test]
    fn beats_and_nearest() {
        assert_eq!(GRID.period(RATE), 24_000.0);
        assert_eq!(GRID.beat_pos(2.0, RATE), 49_000.0);
        assert_eq!(GRID.beat_at(13_000.0, RATE), 0.5);
        assert_eq!(GRID.nearest(12_000.0, RATE), 1_000.0);
        assert_eq!(GRID.nearest(14_000.0, RATE), 25_000.0);
        assert_eq!(GRID.nearest(-30_000.0, RATE), -23_000.0);
    }

    #[test]
    fn steps_land_on_the_grid() {
        assert_eq!(GRID.step(25_000.0, 1, RATE), 49_000.0);
        assert_eq!(GRID.step(25_000.0, -2, RATE), -23_000.0);
        // From between beats the first step goes to the next beat that way.
        assert_eq!(GRID.step(30_000.0, 1, RATE), 49_000.0);
        assert_eq!(GRID.step(30_000.0, -1, RATE), 25_000.0);
        assert_eq!(GRID.step(30_000.0, 4, RATE), 121_000.0);
        assert_eq!(GRID.step(30_000.0, -4, RATE), -47_000.0);
    }

    #[test]
    fn tempo_change_keeps_the_beat_at_the_playhead_and_the_bars() {
        let g = GRID.with_bpm(100.0, 73_500.0, RATE);
        // Beat 3 of the old grid (73 000) stays, and stays beat 3 of its bar.
        assert!((g.nearest(73_000.0, RATE) - 73_000.0).abs() < 1e-6);
        assert_eq!(g.beat_at(73_000.0, RATE).round().rem_euclid(4.0), 3.0);
        assert_eq!(g.bpm, 100.0);
    }

    #[test]
    fn bars_count_from_the_first_downbeat() {
        let g = BeatGrid { anchor: 1_000.0 + 8.0 * 24_000.0, ..GRID };
        assert_eq!(g.bar_beat(1_000.0, RATE), (1, 1));
        assert_eq!(g.bar_beat(25_000.0, RATE), (1, 2));
        assert_eq!(g.bar_beat(24_999.5, RATE), (1, 2), "a frame early still counts");
        assert_eq!(g.bar_beat(1_000.0 + 4.0 * 24_000.0 + 10.0, RATE), (2, 1));
        assert_eq!(g.bar_beat(0.0, RATE), (0, 4), "just before beat 1");
        // A first beat just before the start is still bar 1.
        let g = BeatGrid { anchor: -500.0, ..GRID };
        assert_eq!(g.bar_beat(0.0, RATE), (1, 1));
        // A pickup before the first downbeat is bar 0.
        let g = BeatGrid { anchor: 50_000.0, ..GRID };
        assert_eq!(g.bar_beat(30_000.0, RATE), (0, 4));
        assert_eq!(g.shifted(-1_000.0).anchor, 49_000.0);
    }
}
