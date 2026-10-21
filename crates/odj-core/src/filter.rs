//! A 3-band boost/cut EQ (low shelf, mid peak, high shelf), built from the
//! standard RBJ "Audio EQ Cookbook" biquad formulas. Used by odj-player's
//! Mixer overlay to shape a deck's tone.

use std::f64::consts::PI;

/// A direct-form-I biquad filter: `y[n] = b0·x[n] + b1·x[n-1] + b2·x[n-2]
/// - a1·y[n-1] - a2·y[n-2]`, coefficients already normalized by `a0`.
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Changes this filter's shape without touching its history (`x1,x2,y1,y2`),
    /// so adjusting the EQ live doesn't click the way rebuilding the filter
    /// from scratch would.
    fn set_coeffs(&mut self, b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) {
        self.b0 = (b0 / a0) as f32;
        self.b1 = (b1 / a0) as f32;
        self.b2 = (b2 / a0) as f32;
        self.a1 = (a1 / a0) as f32;
        self.a2 = (a2 / a0) as f32;
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Low shelf coefficients (unnormalized), cookbook shelf slope `S = 1` — at
/// which `alpha` reduces to `sin(w0)/sqrt(2)`, independent of the gain `A`
/// (the `(A + 1/A)·(1/S - 1)` term vanishes when `S = 1`).
fn low_shelf(rate: f64, f0: f64, db: f64) -> (f64, f64, f64, f64, f64, f64) {
    let a = 10f64.powf(db / 40.0);
    let w0 = 2.0 * PI * f0 / rate;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / 2.0_f64.sqrt();
    let sqrt_a2 = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + sqrt_a2);
    let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
    let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - sqrt_a2);
    let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + sqrt_a2;
    let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
    let a2 = (a + 1.0) + (a - 1.0) * cos_w0 - sqrt_a2;
    (b0, b1, b2, a0, a1, a2)
}

/// High shelf coefficients (unnormalized) — see [`low_shelf`] for `alpha`.
fn high_shelf(rate: f64, f0: f64, db: f64) -> (f64, f64, f64, f64, f64, f64) {
    let a = 10f64.powf(db / 40.0);
    let w0 = 2.0 * PI * f0 / rate;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / 2.0_f64.sqrt();
    let sqrt_a2 = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + sqrt_a2);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - sqrt_a2);
    let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + sqrt_a2;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
    let a2 = (a + 1.0) - (a - 1.0) * cos_w0 - sqrt_a2;
    (b0, b1, b2, a0, a1, a2)
}

/// Peaking (bell) EQ coefficients (unnormalized).
fn peaking(rate: f64, f0: f64, q: f64, db: f64) -> (f64, f64, f64, f64, f64, f64) {
    let a = 10f64.powf(db / 40.0);
    let w0 = 2.0 * PI * f0 / rate;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha = sin_w0 / (2.0 * q);
    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cos_w0;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cos_w0;
    let a2 = 1.0 - alpha / a;
    (b0, b1, b2, a0, a1, a2)
}

const LOW_HZ: f64 = 300.0;
const MID_HZ: f64 = 1000.0;
const MID_Q: f64 = 0.7;
const HIGH_HZ: f64 = 3000.0;

/// Each band's gain range: a fully boost/cut EQ, not a cut-only "kill" EQ —
/// 0 dB (centered, flat) is neutral.
pub const MIN_DB: f32 = -26.0;
pub const MAX_DB: f32 = 6.0;

/// A 3-band EQ (low shelf / mid peak / high shelf in series), one instance
/// per deck, applied to the whole stereo mix after it's generated — see
/// `Deck::render_at`.
pub struct ThreeBandEq {
    rate: u32,
    low_db: f32,
    mid_db: f32,
    high_db: f32,
    low: [Biquad; 2],
    mid: [Biquad; 2],
    high: [Biquad; 2],
}

impl ThreeBandEq {
    /// Neutral (0 dB on every band) at `rate` (the deck's output rate).
    pub fn new(rate: u32) -> Self {
        let mut eq = Self {
            rate,
            low_db: 0.0,
            mid_db: 0.0,
            high_db: 0.0,
            low: Default::default(),
            mid: Default::default(),
            high: Default::default(),
        };
        eq.set_low_db(0.0);
        eq.set_mid_db(0.0);
        eq.set_high_db(0.0);
        eq
    }

    pub fn low_db(&self) -> f32 {
        self.low_db
    }

    pub fn mid_db(&self) -> f32 {
        self.mid_db
    }

    pub fn high_db(&self) -> f32 {
        self.high_db
    }

    pub fn set_low_db(&mut self, db: f32) {
        self.low_db = db.clamp(MIN_DB, MAX_DB);
        let (b0, b1, b2, a0, a1, a2) = low_shelf(self.rate as f64, LOW_HZ, self.low_db as f64);
        for ch in &mut self.low {
            ch.set_coeffs(b0, b1, b2, a0, a1, a2);
        }
    }

    pub fn set_mid_db(&mut self, db: f32) {
        self.mid_db = db.clamp(MIN_DB, MAX_DB);
        let (b0, b1, b2, a0, a1, a2) = peaking(self.rate as f64, MID_HZ, MID_Q, self.mid_db as f64);
        for ch in &mut self.mid {
            ch.set_coeffs(b0, b1, b2, a0, a1, a2);
        }
    }

    pub fn set_high_db(&mut self, db: f32) {
        self.high_db = db.clamp(MIN_DB, MAX_DB);
        let (b0, b1, b2, a0, a1, a2) = high_shelf(self.rate as f64, HIGH_HZ, self.high_db as f64);
        for ch in &mut self.high {
            ch.set_coeffs(b0, b1, b2, a0, a1, a2);
        }
    }

    /// Runs low → mid → high in series, in place, on a block of stereo audio.
    pub fn process_stereo(&mut self, l: &mut [f32], r: &mut [f32]) {
        for x in l.iter_mut() {
            *x = self.high[0].process(self.mid[0].process(self.low[0].process(*x)));
        }
        for x in r.iter_mut() {
            *x = self.high[1].process(self.mid[1].process(self.low[1].process(*x)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    /// A sine test tone at `hz`, long enough that the first quarter (settling
    /// out any filter transient) can be discarded before measuring.
    fn tone(hz: f64, len: usize) -> Vec<f32> {
        (0..len).map(|i| (2.0 * PI * hz * i as f64 / RATE as f64).sin() as f32).collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Past the filters' settling transient, so a steady-state RMS
    /// comparison isn't skewed by the first few cycles.
    fn steady(samples: &[f32]) -> &[f32] {
        &samples[samples.len() / 4..]
    }

    #[test]
    fn flat_eq_is_an_identity_filter() {
        let mut eq = ThreeBandEq::new(RATE);
        let input = tone(440.0, 1000);
        let mut l = input.clone();
        let mut r = input.clone();
        eq.process_stereo(&mut l, &mut r);
        for (x, y) in input.iter().zip(l.iter()) {
            assert!((x - y).abs() < 1e-4, "0 dB on every band should pass the signal through unchanged");
        }
        assert_eq!(l, r);
    }

    #[test]
    fn boosting_low_raises_a_low_tone_more_than_a_high_one() {
        let low_in = tone(100.0, 2000);
        let high_in = tone(8000.0, 2000);

        let mut eq = ThreeBandEq::new(RATE);
        eq.set_low_db(MAX_DB);
        let mut low_out = low_in.clone();
        let mut dummy = low_in.clone();
        eq.process_stereo(&mut low_out, &mut dummy);

        let mut eq2 = ThreeBandEq::new(RATE);
        eq2.set_low_db(MAX_DB);
        let mut high_out = high_in.clone();
        let mut dummy2 = high_in.clone();
        eq2.process_stereo(&mut high_out, &mut dummy2);

        let low_ratio = rms(steady(&low_out)) / rms(steady(&low_in));
        let high_ratio = rms(steady(&high_out)) / rms(steady(&high_in));
        assert!(low_ratio > 1.3, "boosting the low band should noticeably raise a low tone: {low_ratio}");
        assert!((0.9..=1.1).contains(&high_ratio), "a high tone should pass through the low shelf mostly unaffected: {high_ratio}");
    }

    #[test]
    fn cutting_low_lowers_a_low_tone_more_than_a_high_one() {
        let low_in = tone(100.0, 2000);
        let high_in = tone(8000.0, 2000);

        let mut eq = ThreeBandEq::new(RATE);
        eq.set_low_db(MIN_DB);
        let mut low_out = low_in.clone();
        let mut dummy = low_in.clone();
        eq.process_stereo(&mut low_out, &mut dummy);

        let mut eq2 = ThreeBandEq::new(RATE);
        eq2.set_low_db(MIN_DB);
        let mut high_out = high_in.clone();
        let mut dummy2 = high_in.clone();
        eq2.process_stereo(&mut high_out, &mut dummy2);

        let low_ratio = rms(steady(&low_out)) / rms(steady(&low_in));
        let high_ratio = rms(steady(&high_out)) / rms(steady(&high_in));
        assert!(low_ratio < 0.3, "cutting the low band hard should quiet a low tone: {low_ratio}");
        assert!((0.9..=1.1).contains(&high_ratio), "a high tone should pass through the low shelf mostly unaffected: {high_ratio}");
    }

    #[test]
    fn boosting_mid_favors_the_center_frequency() {
        let center = tone(MID_HZ, 2000);
        let low = tone(100.0, 2000);
        let high = tone(8000.0, 2000);

        let ratio = |input: &[f32]| {
            let mut eq = ThreeBandEq::new(RATE);
            eq.set_mid_db(MAX_DB);
            let mut out = input.to_vec();
            let mut dummy = input.to_vec();
            eq.process_stereo(&mut out, &mut dummy);
            rms(steady(&out)) / rms(steady(input))
        };

        let center_ratio = ratio(&center);
        let low_ratio = ratio(&low);
        let high_ratio = ratio(&high);
        assert!(center_ratio > low_ratio, "the center frequency should be boosted more than a low one: {center_ratio} vs {low_ratio}");
        assert!(center_ratio > high_ratio, "the center frequency should be boosted more than a high one: {center_ratio} vs {high_ratio}");
    }

    #[test]
    fn coefficients_stay_finite_across_the_whole_range() {
        let input = tone(440.0, 200);
        for db in [MIN_DB, MIN_DB / 2.0, 0.0, MAX_DB / 2.0, MAX_DB] {
            let mut eq = ThreeBandEq::new(RATE);
            eq.set_low_db(db);
            eq.set_mid_db(db);
            eq.set_high_db(db);
            let mut l = input.clone();
            let mut r = input.clone();
            eq.process_stereo(&mut l, &mut r);
            assert!(l.iter().all(|x| x.is_finite()), "low/mid/high at {db} dB produced non-finite output");
        }
    }
}
