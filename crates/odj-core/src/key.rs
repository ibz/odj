//! The musical key of a track: its notes folded into the 12 pitch classes
//! (a chromagram), against the 24 major and minor key profiles.

use std::f32::consts::PI;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// A key: the tonic's pitch class (0 is C) and major or minor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub tonic: u8,
    pub minor: bool,
}

const MAJOR_NAMES: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];
const MINOR_NAMES: [&str; 12] = ["Cm", "C#m", "Dm", "Ebm", "Em", "Fm", "F#m", "Gm", "G#m", "Am", "Bbm", "Bm"];

impl Key {
    /// The usual name, like `Am` or `F#`.
    pub fn name(self) -> &'static str {
        if self.minor { MINOR_NAMES[self.tonic as usize] } else { MAJOR_NAMES[self.tonic as usize] }
    }

    /// The Camelot wheel's code, like `8A` for A minor: keys a step apart on
    /// the wheel, and A and B of the same number, mix well.
    pub fn camelot(self) -> String {
        let (number, minor) = self.wheel();
        format!("{}{}", number + 1, if minor { 'A' } else { 'B' })
    }

    /// Whether the two mix well: the same number on the Camelot wheel, or
    /// the same letter a step either way.
    pub fn compatible(self, other: Key) -> bool {
        let (a, b) = (self.wheel(), other.wheel());
        a.0 == b.0 || (a.1 == b.1 && matches!((a.0 + 12 - b.0) % 12, 1 | 11))
    }

    /// Place on the Camelot wheel: the number less one (0 to 11), and minor.
    /// Minor keys sit with their relative major, three semitones up, and each
    /// step round is a fifth up; C major is 8B.
    fn wheel(self) -> (u32, bool) {
        let major = if self.minor { (self.tonic + 3) % 12 } else { self.tonic };
        ((major as u32 * 7 + 7) % 12, self.minor)
    }

    /// One byte for the deck link: 0 for no key.
    pub fn to_byte(key: Option<Key>) -> u8 {
        key.map_or(0, |k| 1 + k.tonic + 12 * k.minor as u8)
    }

    pub fn from_byte(b: u8) -> Option<Key> {
        (1..=24).contains(&b).then(|| Key { tonic: (b - 1) % 12, minor: b > 12 })
    }

    /// The key heard `semitones` higher, as when the tempo changes without
    /// Master Tempo.
    pub fn shifted(self, semitones: i32) -> Key {
        Key { tonic: (self.tonic as i32 + semitones).rem_euclid(12) as u8, ..self }
    }
}

/// Lowest and highest notes looked at, as MIDI numbers: C2 to C7.
const LOW_NOTE: f32 = 36.0;
const HIGH_NOTE: f32 = 96.0;
/// Frames quieter than this (RMS, about -50 dB) say nothing about the key.
const QUIET: f32 = 0.003;
/// Spectral peaks this far below a frame's loudest are left out (-40 dB).
const PEAK_FLOOR: f32 = 0.01;
/// Below this correlation with the best key's profile, there's no key to speak of.
const MIN_FIT: f32 = 0.5;

/// Temperley's key profiles (Kostka–Payne): how much each scale degree is
/// heard in a major or a minor key, from the tonic up.
const MAJOR: [f32; 12] = [0.748, 0.060, 0.488, 0.082, 0.670, 0.460, 0.096, 0.715, 0.104, 0.366, 0.057, 0.400];
const MINOR: [f32; 12] = [0.712, 0.084, 0.474, 0.618, 0.049, 0.460, 0.105, 0.747, 0.404, 0.067, 0.133, 0.330];

/// The track's key, or `None` when it has too little pitched sound, like a
/// drum track.
pub fn detect_key(mono: &[f32], sample_rate: u32) -> Option<Key> {
    best_key(&chromagram(mono, sample_rate)?)
}

/// How much of each pitch class the track has, every frame counting the same.
fn chromagram(mono: &[f32], sample_rate: u32) -> Option<[f32; 12]> {
    // About half a second, for bins close enough to tell low notes apart.
    let size = ((sample_rate as usize) / 2).next_power_of_two();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(size);
    let window: Vec<f32> = (0..size).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / size as f32).cos()).collect();
    let hz_per_bin = sample_rate as f32 / size as f32;
    let bin_of = |note: f32| (440.0 * 2f32.powf((note - 69.0) / 12.0) / hz_per_bin) as usize;
    let (low, high) = (bin_of(LOW_NOTE).max(1), bin_of(HIGH_NOTE).min(size / 2 - 1));

    // Each frame's peaks, as (note, level): a note's fraction is how far off
    // A = 440 Hz tuning it is.
    let mut frames: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut buf = vec![Complex::default(); size];
    let mut mags = vec![0f32; size / 2];
    for chunk in mono.chunks_exact(size) {
        let rms = (chunk.iter().map(|x| x * x).sum::<f32>() / size as f32).sqrt();
        if rms < QUIET {
            continue;
        }
        for ((b, x), w) in buf.iter_mut().zip(chunk).zip(&window) {
            *b = Complex::new(x * w, 0.0);
        }
        fft.process(&mut buf);
        for (m, b) in mags.iter_mut().zip(&buf) {
            *m = b.norm();
        }
        let loudest = mags[low..=high].iter().fold(0f32, |a, &m| a.max(m));
        let mut peaks = Vec::new();
        for k in low..=high {
            let (a, b, c) = (mags[k - 1], mags[k], mags[k + 1]);
            if b > a && b >= c && b > loudest * PEAK_FLOOR {
                // Between the bins, where a parabola through the three peaks.
                let offset = 0.5 * (a - c) / (a - 2.0 * b + c);
                let hz = (k as f32 + offset) * hz_per_bin;
                peaks.push((69.0 + 12.0 * (hz / 440.0).log2(), b));
            }
        }
        frames.push(peaks);
    }

    let tuning = tuning(&frames);
    let mut chroma = [0f32; 12];
    for peaks in &frames {
        let mut frame = [0f32; 12];
        for &(note, level) in peaks {
            let note = note - tuning;
            let nearest = note.round();
            // Full weight on a note, none halfway between two.
            let w = (PI * (note - nearest)).cos().powi(2);
            frame[nearest.rem_euclid(12.0) as usize] += w * level;
        }
        let max = frame.iter().fold(0f32, |a, &x| a.max(x));
        if max > 0.0 {
            for (c, x) in chroma.iter_mut().zip(frame) {
                *c += x / max;
            }
        }
    }
    chroma.iter().any(|&c| c > 0.0).then_some(chroma)
}

/// How far the track is tuned from A = 440 Hz, in semitones (-0.5 to 0.5):
/// the most common offset of its peaks from the nearest note.
fn tuning(frames: &[Vec<(f32, f32)>]) -> f32 {
    const BINS: usize = 50;
    let mut hist = [0f32; BINS];
    for &(note, level) in frames.iter().flatten() {
        let off = note - note.round() + 0.5;
        hist[((off * BINS as f32) as usize).min(BINS - 1)] += level;
    }
    // Smoothed round the circle: -0.5 and 0.5 are the same tuning.
    let smooth = |i: usize| (0..5).map(|d| hist[(i + BINS + d - 2) % BINS]).sum::<f32>();
    let best = (0..BINS).max_by(|&a, &b| smooth(a).total_cmp(&smooth(b))).unwrap_or(BINS / 2);
    (best as f32 + 0.5) / BINS as f32 - 0.5
}

/// The key whose profile the chromagram follows best, if it follows it well.
fn best_key(chroma: &[f32; 12]) -> Option<Key> {
    let mut best = (f32::MIN, Key { tonic: 0, minor: false });
    for tonic in 0..12u8 {
        for minor in [false, true] {
            let profile = if minor { &MINOR } else { &MAJOR };
            let rotated: [f32; 12] = std::array::from_fn(|pc| profile[(pc + 12 - tonic as usize) % 12]);
            let r = correlation(chroma, &rotated);
            if r > best.0 {
                best = (r, Key { tonic, minor });
            }
        }
    }
    (best.0 >= MIN_FIT).then_some(best.1)
}

fn correlation(a: &[f32; 12], b: &[f32; 12]) -> f32 {
    let mean = |v: &[f32; 12]| v.iter().sum::<f32>() / 12.0;
    let (ma, mb) = (mean(a), mean(b));
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        ab += (x - ma) * (y - mb);
        aa += (x - ma) * (x - ma);
        bb += (y - mb) * (y - mb);
    }
    if aa == 0.0 || bb == 0.0 { 0.0 } else { ab / (aa * bb).sqrt() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 44_100;

    /// A tone rich in harmonics, like a synth or a piano, at a MIDI note.
    fn note(out: &mut [f32], midi: f32, level: f32) {
        let hz = 440.0 * 2f64.powf((midi as f64 - 69.0) / 12.0);
        for h in 1..=6 {
            // A phasor turned by the same angle each sample: no sin per sample.
            let step = Complex::from_polar(1.0, std::f64::consts::TAU * hz * h as f64 / SR as f64);
            let mut z = Complex::new(1.0f64, 0.0);
            let amp = level / h as f32;
            for s in out.iter_mut() {
                *s += amp * z.im as f32;
                z *= step;
            }
        }
    }

    /// Chords of (bass note, other notes), one per second, the progression
    /// played `times` over.
    fn progression(chords: &[(f32, &[f32])], times: usize, detune: f32) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..times {
            for (bass, notes) in chords {
                let mut bar = vec![0f32; SR as usize];
                note(&mut bar, bass + detune, 0.2);
                for n in *notes {
                    note(&mut bar, n + detune, 0.1);
                }
                out.extend(bar);
            }
        }
        out
    }

    /// i–iv–V–i in a minor key, or I–IV–V–I in a major one, on `tonic`.
    fn cadence(tonic: u8, minor: bool, detune: f32) -> Vec<f32> {
        let t = 48.0 + tonic as f32;
        let third = if minor { 3.0 } else { 4.0 };
        let chords: [(f32, &[f32]); 4] = [
            (t - 12.0, &[t, t + third, t + 7.0]),
            (t - 7.0, &[t + 5.0, t + 5.0 + third, t + 12.0]),
            // V is major in both: the leading tone is what makes a minor key.
            (t - 5.0, &[t + 7.0, t + 11.0, t + 14.0]),
            (t - 12.0, &[t, t + third, t + 7.0]),
        ];
        progression(&chords, 2, detune)
    }

    #[test]
    fn finds_every_major_and_minor_key() {
        for tonic in 0..12 {
            for minor in [false, true] {
                let want = Key { tonic, minor };
                assert_eq!(detect_key(&cadence(tonic, minor, 0.0), SR), Some(want), "{}", want.name());
            }
        }
    }

    #[test]
    fn allows_for_tracks_not_tuned_to_440() {
        // A quarter of a semitone sharp is closer to the next key up than
        // a tuning-blind detector can tell.
        let want = Key { tonic: 9, minor: true };
        assert_eq!(detect_key(&cadence(9, true, 0.3), SR), Some(want));
        assert_eq!(detect_key(&cadence(9, true, -0.3), SR), Some(want));
    }

    #[test]
    fn drums_and_silence_have_no_key() {
        assert_eq!(detect_key(&vec![0.0; SR as usize * 10], SR), None);
        // Noise bursts, like hi-hats and snares.
        let mut seed = 1u32;
        let drums: Vec<f32> = (0..SR as usize * 10)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
                if i % 11_025 < 2_000 { noise } else { 0.0 }
            })
            .collect();
        assert_eq!(detect_key(&drums, SR), None);
    }

    #[test]
    fn names_and_camelot_codes() {
        let k = |tonic, minor| Key { tonic, minor };
        assert_eq!((k(9, true).name(), k(9, true).camelot().as_str()), ("Am", "8A"));
        assert_eq!((k(0, false).name(), k(0, false).camelot().as_str()), ("C", "8B"));
        assert_eq!(k(11, false).camelot(), "1B");
        assert_eq!(k(8, true).camelot(), "1A", "G#m");
        assert_eq!(k(4, false).camelot(), "12B");
        assert_eq!(k(7, false).camelot(), "9B");
        assert_eq!(k(9, true).shifted(1), k(10, true));
        assert_eq!(k(0, false).shifted(-1), k(11, false));
    }

    #[test]
    fn compatible_keys_are_neighbours_on_the_wheel() {
        let am = Key { tonic: 9, minor: true }; // 8A
        let mixes = |tonic, minor| am.compatible(Key { tonic, minor });
        assert!(mixes(9, true), "8A itself");
        assert!(mixes(0, false), "8B, C: the relative major");
        assert!(mixes(4, true), "9A, Em");
        assert!(mixes(2, true), "7A, Dm");
        assert!(!mixes(7, false), "9B, G: a step and a letter away");
        assert!(!mixes(10, true), "3A, Bbm: a semitone up");
        let gsm = Key { tonic: 8, minor: true }; // 1A
        assert!(gsm.compatible(Key { tonic: 1, minor: true }), "12A, C#m: round the wheel");
        for b in 0..=24 {
            assert_eq!(Key::to_byte(Key::from_byte(b)), b);
        }
    }

    /// Runs the detector over real tracks listed in `ODJ_KEY_LABELS`, a file
    /// of `path<TAB>tonic<TAB>minor` lines, and counts the kinds of misses.
    #[test]
    #[ignore]
    fn labelled_tracks() {
        let Ok(list) = std::env::var("ODJ_KEY_LABELS") else { return };
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for line in std::fs::read_to_string(list).unwrap().lines() {
            let mut f = line.split('\t');
            let (path, tonic, minor) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
            let want = Key { tonic: tonic.parse().unwrap(), minor: minor == "1" };
            let Ok(d) = crate::track::decode(std::path::Path::new(path)) else { continue };
            let mono: Vec<f32> = d.samples.as_chunks::<2>().0.iter().map(|f| 0.5 * (f[0] + f[1])).collect();
            let chroma = chromagram(&mono, d.sample_rate);
            if let Some(c) = chroma {
                println!("chroma\t{tonic}\t{minor}\t{}", c.map(|x| x.to_string()).join(","));
            }
            let got = chroma.and_then(|c| best_key(&c));
            let kind = match got {
                None => "none",
                Some(g) if g == want => "exact",
                Some(g) if g.camelot()[..g.camelot().len() - 1] == want.camelot()[..want.camelot().len() - 1] => "relative",
                Some(g) if g.minor == want.minor && (g.tonic + 12 - want.tonic) % 12 == 7 => "fifth up",
                Some(g) if g.minor == want.minor && (want.tonic + 12 - g.tonic) % 12 == 7 => "fifth down",
                Some(g) if g.tonic == want.tonic => "parallel",
                Some(_) => "other",
            };
            *counts.entry(kind).or_default() += 1;
            let name = path.rsplit('/').next().unwrap();
            println!("{kind:<10} want {:<4} got {:<4} {name}", want.name(), got.map_or("-", |g| g.name()));
        }
        println!("{counts:?}");
    }
}
