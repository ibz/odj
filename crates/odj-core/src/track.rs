//! Decoding a file fully into memory, plus the analysis the player's display
//! shows: waveform, beat grid and the auto-cue point.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

use crate::grid::{BAR, BeatGrid};

/// Waveform bins per second of audio.
pub const WAVE_RATE: f64 = 100.0;

/// Auto-cue threshold, -48 dB.
const AUTO_CUE_LEVEL: f32 = 0.004;

#[derive(Clone, Copy, Default, Debug)]
pub struct WaveBin {
    /// Peak level of the full signal.
    pub peak: f32,
    /// Peak level of the low (bass) band, used for colouring.
    pub low: f32,
}

pub struct Track {
    /// Fingerprint of the encoded audio, independent of tags, file name and folder.
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub sample_rate: u32,
    /// Interleaved stereo.
    pub samples: Vec<f32>,
    pub wave: Vec<WaveBin>,
    /// Detected beat grid; the user's corrections live in the track's memory.
    pub grid: Option<BeatGrid>,
    /// The beats keep to the grid, so quantizing makes sense.
    pub steady: bool,
    /// First audible frame, used as the default cue point.
    pub first_sound: f64,
}

/// A file's audio and tags, without any analysis.
pub struct Decoded {
    /// Fingerprint of the encoded audio, see `Track::id`.
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub sample_rate: u32,
    /// Interleaved stereo.
    pub samples: Vec<f32>,
}

impl Track {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }

    pub fn bpm(&self) -> Option<f64> {
        self.grid.map(|g| g.bpm)
    }

    pub fn load(path: &Path) -> Result<Track> {
        let d = decode(path)?;
        let mut track = Track::from_samples(path.to_path_buf(), d.title, d.artist, d.sample_rate, d.samples);
        track.id = d.id;
        Ok(track)
    }

    pub fn from_samples(
        path: PathBuf,
        title: String,
        artist: Option<String>,
        sample_rate: u32,
        samples: Vec<f32>,
    ) -> Track {
        let mono: Vec<f32> = samples.as_chunks::<2>().0.iter().map(|f| 0.5 * (f[0] + f[1])).collect();
        let first_sound = samples
            .as_chunks::<2>().0.iter()
            .position(|f| f[0].abs().max(f[1].abs()) > AUTO_CUE_LEVEL)
            .unwrap_or(0) as f64;
        let (grid, steady) = detect_grid(&mono, sample_rate).map_or((None, false), |(g, s)| (Some(g), s));
        Track {
            // Set by `load`; tracks built from bare samples have no file to fingerprint.
            id: String::new(),
            wave: waveform(&mono, sample_rate),
            grid,
            steady,
            path,
            title,
            artist,
            sample_rate,
            samples,
            first_sound,
        }
    }
}

/// Decodes a whole file to interleaved stereo at its own sample rate.
pub fn decode(path: &Path) -> Result<Decoded> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| anyhow!("unsupported file: {e}"))?;

    let (mut title, mut artist) = (None, None);
    if let Some(rev) = format.metadata().skip_to_latest() {
        let tags = rev
            .media
            .tags
            .iter()
            .chain(rev.per_track.iter().flat_map(|t| t.metadata.tags.iter()));
        for tag in tags {
            match &tag.std {
                Some(StandardTag::TrackTitle(s)) => title = Some(s.to_string()),
                Some(StandardTag::Artist(s)) => artist = Some(s.to_string()),
                _ => {}
            }
        }
    }

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio track"))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("no audio codec parameters"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| anyhow!("unsupported codec: {e}"))?;
    let track_id = track.id;

    let mut samples = Vec::new();
    let mut packet_buf: Vec<f32> = Vec::new();
    // Hashing the packets rather than the file keeps the ID stable across tag edits,
    // and rather than decoded samples, across decoder changes.
    let mut hasher = blake3::Hasher::new();
    let mut sample_rate = 0;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // A broken tail shouldn't make the whole track unplayable.
            Err(_) if !samples.is_empty() => break,
            Err(e) => return Err(anyhow!("read error: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        hasher.update(&packet.data);
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("decode error: {e}")),
        };
        sample_rate = buf.spec().rate();
        let channels = buf.spec().channels().count().max(1);
        packet_buf.resize(buf.samples_interleaved(), 0.0);
        buf.copy_to_slice_interleaved(&mut packet_buf);
        for frame in packet_buf.chunks_exact(channels) {
            let l = frame[0];
            let r = if channels > 1 { frame[1] } else { l };
            samples.push(l);
            samples.push(r);
        }
    }
    if samples.is_empty() || sample_rate == 0 {
        return Err(anyhow!("no audio decoded"));
    }

    let title = title.unwrap_or_else(|| {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    });
    Ok(Decoded { id: fingerprint(hasher), title, artist, sample_rate, samples })
}

/// 128 bits of the hash as hex: plenty to tell tracks apart, short as a file name.
fn fingerprint(hasher: blake3::Hasher) -> String {
    hasher.finalize().to_hex()[..32].to_string()
}

fn lowpass_coef(cutoff: f64, sample_rate: u32) -> f32 {
    (1.0 - (-2.0 * std::f64::consts::PI * cutoff / sample_rate as f64).exp()) as f32
}

fn waveform(mono: &[f32], sample_rate: u32) -> Vec<WaveBin> {
    let bin = ((sample_rate as f64 / WAVE_RATE) as usize).max(1);
    let a = lowpass_coef(150.0, sample_rate);
    let mut lp = 0.0f32;
    mono.chunks(bin)
        .map(|chunk| {
            let mut b = WaveBin::default();
            for &x in chunk {
                lp += a * (x - lp);
                b.peak = b.peak.max(x.abs());
                b.low = b.low.max(lp.abs());
            }
            b
        })
        .collect()
}

/// Bins per second of the energy envelopes the beat analysis works on.
const ENERGY_RATE: f64 = 1000.0;
/// Energy bins per bin of the tempo search's envelope (200 Hz).
const COARSE: usize = 5;
/// Beats whose onsets stray further than this from the fitted grid make a
/// track unsteady; so does finding onsets on too few of its beats.
const STEADY_MS: f64 = 8.0;
const STEADY_SHARE: f64 = 0.5;

/// Signal energy, full band and bass, in bins of `hop` frames.
struct Energy {
    hop: usize,
    full: Vec<f32>,
    low: Vec<f32>,
}

fn energy(mono: &[f32], sample_rate: u32) -> Energy {
    let hop = ((sample_rate as f64 / ENERGY_RATE) as usize).max(1);
    let a = lowpass_coef(150.0, sample_rate);
    let mut lp = 0.0f32;
    let (mut full, mut low) = (Vec::new(), Vec::new());
    for chunk in mono.chunks(hop) {
        let (mut ef, mut el) = (0.0f32, 0.0f32);
        for &x in chunk {
            lp += a * (x - lp);
            ef += x * x;
            el += lp * lp;
        }
        full.push(ef);
        low.push(el);
    }
    Energy { hop, full, low }
}

/// The detected grid, and whether the beats keep to it closely enough to quantize.
pub fn detect_grid(mono: &[f32], sample_rate: u32) -> Option<(BeatGrid, bool)> {
    let e = energy(mono, sample_rate);
    let bpm = rough_bpm(&e, sample_rate)?;
    let rate = sample_rate as f64 / e.hop as f64;
    let onset = fine_onsets(&e);
    let fit = fit_beats(&onset, rate * 60.0 / bpm, rate)?;
    let hop = e.hop as f64;
    let bpm = 60.0 * rate / fit.period;
    let bar = BAR as f64 * fit.period * hop;
    let grid = BeatGrid { bpm, anchor: ((fit.phase + fit.downbeat as f64 * fit.period) * hop).rem_euclid(bar) };
    Some((grid, fit.steady))
}

/// Tempo estimate from the autocorrelation of an onset-strength envelope, in
/// 78..180 BPM.
fn rough_bpm(e: &Energy, sample_rate: u32) -> Option<f64> {
    const MIN_BPM: f64 = 60.0;
    const MAX_BPM: f64 = 200.0;

    let hop = e.hop * COARSE;
    let env_rate = sample_rate as f64 / hop as f64;
    if e.full.len() < COARSE * (env_rate as usize) * 4 {
        return None;
    }

    // Log energy of the full band and the bass band (kicks).
    let log = |bins: &[f32]| -> Vec<f32> {
        bins.chunks(COARSE)
            .map(|c| (1.0 + 1000.0 * c.iter().sum::<f32>() / (c.len() * e.hop) as f32).ln())
            .collect()
    };
    let (full, low) = (log(&e.full), log(&e.low));

    // Onset strength: rectified rise in log energy, minus its local mean.
    let mut onset: Vec<f32> = (1..full.len())
        .map(|k| (full[k] - full[k - 1]).max(0.0) + 2.0 * (low[k] - low[k - 1]).max(0.0))
        .collect();
    let w = 16;
    let mut prefix = vec![0.0f32; onset.len() + 1];
    for (i, &v) in onset.iter().enumerate() {
        prefix[i + 1] = prefix[i] + v;
    }
    let raw = onset.clone();
    for i in 0..onset.len() {
        let (lo, hi) = (i.saturating_sub(w), (i + w + 1).min(raw.len()));
        let mean = (prefix[hi] - prefix[lo]) / (hi - lo) as f32;
        onset[i] = (raw[i] - mean).max(0.0);
    }
    if onset.iter().sum::<f32>() < 1e-3 {
        return None;
    }

    let max_multiple = 8.0;
    let max_lag = ((env_rate * 60.0 / MIN_BPM) * max_multiple).ceil() as usize + 2;
    let max_lag = max_lag.min(onset.len() - 1);
    let acf: Vec<f64> = (0..=max_lag)
        .map(|lag| {
            onset[lag..]
                .iter()
                .zip(&onset)
                .map(|(&x, &y)| (x * y) as f64)
                .sum::<f64>()
                / (onset.len() - lag) as f64
        })
        .collect();
    let acf_at = |lag: f64| -> f64 {
        let i = lag.floor() as usize;
        if i + 1 >= acf.len() {
            return 0.0;
        }
        let t = lag - i as f64;
        acf[i] * (1.0 - t) + acf[i + 1] * t
    };
    let score = |bpm: f64, multiples: usize| -> f64 {
        let lag = env_rate * 60.0 / bpm;
        (1..=multiples).map(|k| acf_at(lag * k as f64)).sum::<f64>() / multiples as f64
    };

    // Coarse search with a mild preference for typical dance tempos, then refine.
    let mut best = (0.0, f64::MIN);
    let mut bpm = MIN_BPM;
    while bpm <= MAX_BPM {
        let octave = (bpm / 125.0).log2() / 1.0;
        let s = score(bpm, 4) * (-0.5 * octave * octave).exp();
        if s > best.1 {
            best = (bpm, s);
        }
        bpm += 0.1;
    }
    let mut refined = (best.0, f64::MIN);
    let mut bpm = best.0 - 0.3;
    while bpm <= best.0 + 0.3 {
        let s = score(bpm, 8);
        if s > refined.1 {
            refined = (bpm, s);
        }
        bpm += 0.005;
    }

    let mut bpm = refined.0;
    while bpm < 78.0 {
        bpm *= 2.0;
    }
    while bpm >= 180.0 {
        bpm /= 2.0;
    }
    Some(bpm)
}

/// Onset strength at every energy bin: how much louder the next 10 ms are than
/// the last 10 ms, bass counting double. It peaks at the bin an attack starts in.
fn fine_onsets(e: &Energy) -> Vec<f32> {
    const W: usize = 10;
    let prefix = |bins: &[f32]| -> Vec<f64> {
        let mut p = vec![0.0f64; bins.len() + 1];
        for (i, &v) in bins.iter().enumerate() {
            p[i + 1] = p[i] + v as f64;
        }
        p
    };
    let (pf, pl) = (prefix(&e.full), prefix(&e.low));
    let n = e.full.len();
    let scale = 1000.0 / (W * e.hop) as f64;
    let level = |p: &[f64], from: usize| (1.0 + scale * (p[from + W] - p[from])).ln();
    (0..n)
        .map(|i| {
            if i < W || i + W > n {
                return 0.0;
            }
            let rise = |p: &[f64]| (level(p, i) - level(p, i - W)).max(0.0);
            (rise(&pf) + 2.0 * rise(&pl)) as f32
        })
        .collect()
}

struct BeatFit {
    /// Bin of beat 0 and bins per beat, fractional.
    phase: f64,
    period: f64,
    /// Which of the first four beats starts a bar.
    downbeat: usize,
    steady: bool,
}

/// Places a grid on the onsets: finds the phase for the rough period, then
/// fits a line through the onset nearest each beat, narrowing in.
fn fit_beats(onset: &[f32], period: f64, rate: f64) -> Option<BeatFit> {
    let n = onset.len();
    // The phase where the beats line up with the most onset strength. The
    // onsets are widened first so a slightly wrong period still lines up.
    let wide: Vec<f32> = (0..n)
        .map(|i| onset[i.saturating_sub(8)..(i + 9).min(n)].iter().fold(0.0f32, |m, &v| m.max(v)))
        .collect();
    let mut best = (0.0, f32::MIN);
    let mut phase = 0.0;
    while phase < period {
        let mut s = 0.0;
        let mut t = phase;
        while (t as usize) < n {
            s += wide[t as usize];
            t += period;
        }
        if s > best.1 {
            best = (phase, s);
        }
        phase += 1.0;
    }

    let (mut phase, mut period) = (best.0, period);
    let mut beats = Vec::new();
    for window in [0.2 * period, 0.08 * period, 0.025 * rate] {
        beats = match_beats(onset, phase, period, window);
        let (p, q) = fit_line(&beats)?;
        (phase, period) = (p, q);
    }

    // A whole BPM is likely meant; take it when it fits about as well.
    let bpm = 60.0 * rate / period;
    let whole = 60.0 * rate / bpm.round();
    if (bpm - bpm.round()).abs() < 0.05 {
        let w: f64 = beats.iter().map(|b| b.2).sum();
        let whole_phase = beats.iter().map(|&(k, t, s)| (t - k * whole) * s).sum::<f64>() / w;
        if spread(&beats, whole_phase, whole) <= spread(&beats, phase, period) + 0.5 {
            (phase, period) = (whole_phase, whole);
        }
    }

    // Downbeat: the beat of the bar with the strongest onsets, or when that is
    // a close call, the first beat found, as tracks tend to start on a bar.
    let mut bar = [0.0f64; BAR as usize];
    for &(k, _, s) in &beats {
        bar[(k as i64).rem_euclid(BAR) as usize] += s;
    }
    let strongest = (0..bar.len()).max_by(|&a, &b| bar[a].total_cmp(&bar[b])).unwrap_or(0);
    let first = beats.first().map_or(0, |b| (b.0 as i64).rem_euclid(BAR) as usize);
    let downbeat = if bar[first] >= 0.9 * bar[strongest] { first } else { strongest };

    let (first, last) = (beats.first().map_or(0.0, |b| b.0), beats.last().map_or(0.0, |b| b.0));
    let share = beats.len() as f64 / (last - first + 1.0);
    let steady = share >= STEADY_SHARE && spread(&beats, phase, period) <= STEADY_MS * rate / 1000.0;
    Some(BeatFit { phase, period, downbeat, steady })
}

/// For each beat of the grid, the strongest onset within `window` bins of it,
/// as (beat, bin, strength); weak ones, where there is no real onset, are left out.
fn match_beats(onset: &[f32], phase: f64, period: f64, window: f64) -> Vec<(f64, f64, f64)> {
    let mut found = Vec::new();
    let mut k = (-phase / period).ceil();
    loop {
        let t = phase + k * period;
        let (lo, hi) = ((t - window).ceil().max(0.0) as usize, (t + window).floor() as usize);
        if lo >= onset.len() {
            break;
        }
        let hi = hi.min(onset.len() - 1);
        if let Some(i) = (lo..=hi).max_by(|&a, &b| onset[a].total_cmp(&onset[b])) {
            found.push((k, i as f64, onset[i] as f64));
        }
        k += 1.0;
    }
    let mut strengths: Vec<f64> = found.iter().map(|b| b.2).collect();
    strengths.sort_by(f64::total_cmp);
    let strong = strengths.get(strengths.len() * 3 / 4).copied().unwrap_or(0.0);
    found.retain(|b| b.2 > 0.0 && b.2 >= 0.3 * strong);
    found
}

/// Weighted least squares of bin against beat: the phase and period.
fn fit_line(beats: &[(f64, f64, f64)]) -> Option<(f64, f64)> {
    let w: f64 = beats.iter().map(|b| b.2).sum();
    if beats.len() < 8 || w <= 0.0 {
        return None;
    }
    let mk = beats.iter().map(|b| b.0 * b.2).sum::<f64>() / w;
    let mt = beats.iter().map(|b| b.1 * b.2).sum::<f64>() / w;
    let cov: f64 = beats.iter().map(|&(k, t, s)| s * (k - mk) * (t - mt)).sum();
    let var: f64 = beats.iter().map(|&(k, _, s)| s * (k - mk) * (k - mk)).sum();
    if var <= 0.0 {
        return None;
    }
    let period = cov / var;
    Some((mt - period * mk, period))
}

/// Median distance, in bins, of the onsets from the grid.
fn spread(beats: &[(f64, f64, f64)], phase: f64, period: f64) -> f64 {
    let mut d: Vec<f64> = beats.iter().map(|&(k, t, _)| (t - phase - k * period).abs()).collect();
    d.sort_by(f64::total_cmp);
    d.get(d.len() / 2).copied().unwrap_or(f64::MAX)
}

#[cfg(any(test, feature = "test-util"))]
pub mod test_util {
    /// A synthetic four-on-the-floor kick pattern with off-beat hats.
    pub fn click_track(bpm: f64, seconds: f64, sample_rate: u32) -> Vec<f32> {
        let beat = 60.0 / bpm;
        clicks((0..).map(|k| (beat * (k as f64 + 0.5), 0.8)), beat, seconds, sample_rate)
    }

    /// A kick at each (time in seconds, level), each followed by a hat half a
    /// `beat` later.
    pub fn clicks(beats: impl IntoIterator<Item = (f64, f32)>, beat: f64, seconds: f64, sample_rate: u32) -> Vec<f32> {
        let frames = (seconds * sample_rate as f64) as usize;
        let sr = sample_rate as f64;
        let mut out = vec![0.0f32; frames * 2];
        for (t, level) in beats {
            let start = (t * sr) as usize;
            if start >= frames {
                break;
            }
            for i in 0..(sample_rate as usize / 8) {
                let s = start + i;
                if s >= frames {
                    break;
                }
                let x = i as f32 / sample_rate as f32;
                let kick = (2.0 * std::f32::consts::PI * 55.0 * x).sin() * (-x * 25.0).exp();
                out[s * 2] += level * kick;
                out[s * 2 + 1] += level * kick;
            }
            let hat = ((t + beat / 2.0) * sr) as usize;
            for i in 0..(sample_rate as usize / 50) {
                let s = hat + i;
                if s >= frames {
                    break;
                }
                let noise = ((s as u32).wrapping_mul(2_654_435_761) >> 16) as f32 / 65536.0 - 0.5;
                let x = i as f32 / sample_rate as f32;
                out[s * 2] += 0.2 * noise * (-x * 300.0).exp();
                out[s * 2 + 1] += 0.2 * noise * (-x * 300.0).exp();
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{click_track, clicks};
    use super::*;

    fn grid_of(samples: &[f32]) -> Option<(BeatGrid, bool)> {
        let mono: Vec<f32> = samples.as_chunks::<2>().0.iter().map(|f| f[0]).collect();
        detect_grid(&mono, 44_100)
    }

    #[test]
    fn detects_common_tempos_and_their_phase() {
        for &bpm in &[90.0, 124.0, 128.0, 140.0, 174.0] {
            let (grid, steady) = grid_of(&click_track(bpm, 40.0, 44_100)).expect("grid");
            assert!((grid.bpm - bpm).abs() < 0.02, "expected {bpm}, got {}", grid.bpm);
            // The kicks are half a beat in.
            let kick = 0.5 * 60.0 / bpm * 44_100.0;
            let off = (grid.nearest(kick, 44_100.0) - kick) / 44.1;
            assert!(off.abs() < 2.0, "{bpm} BPM: first beat {off:.2} ms off");
            assert!(steady);
            // All beats alike: the first one starts bar 1.
            assert_eq!(grid.bar_beat(kick, 44_100.0), (1, 1), "{bpm} BPM");
        }
    }

    #[test]
    fn grid_holds_over_a_whole_track() {
        // An odd tempo, a first beat 37 ms in, and accented downbeats from beat 2 on.
        let (bpm, first) = (123.45, 0.037);
        let beat = 60.0 / bpm;
        let at = |k: f64| (first + k * beat) * 44_100.0;
        let beats = (0..).map(|k| (first + k as f64 * beat, if k % 4 == 2 { 0.9 } else { 0.5 }));
        let (grid, steady) = grid_of(&clicks(beats, beat, 300.0, 44_100)).expect("grid");
        assert!((grid.bpm - bpm).abs() < 0.005, "bpm {}", grid.bpm);
        assert!(steady);
        for k in [0.0, 300.0, 610.0] {
            let off = (grid.nearest(at(k), 44_100.0) - at(k)) / 44.1;
            assert!(off.abs() < 2.0, "beat {k}: {off:.2} ms off");
        }
        assert_eq!(grid.bar_beat(at(2.0), 44_100.0), (1, 1));
        assert_eq!(grid.bar_beat(at(0.0), 44_100.0), (0, 3));
    }

    #[test]
    fn whole_tempos_come_out_exact() {
        let (grid, _) = grid_of(&click_track(128.0, 60.0, 44_100)).expect("grid");
        assert_eq!(grid.bpm, 128.0);
    }

    #[test]
    fn loose_timing_is_not_steady() {
        // A player drifting around 120 BPM.
        let mut t = 0.3;
        let drifting = (0..400).map(|k| {
            let at = t;
            t += 0.5 / (1.0 + 0.04 * (k as f64 / 30.0).sin());
            (at, 0.8)
        });
        let grid = grid_of(&clicks(drifting, 0.5, 200.0, 44_100));
        assert!(grid.is_none_or(|(_, steady)| !steady), "{grid:?}");
        // Steady on average, but each beat up to 30 ms off.
        let mut seed = 1u32;
        let jittered = (0..400).map(|k| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let jitter = (seed >> 8) as f64 / (1u32 << 24) as f64 * 0.06 - 0.03;
            (0.3 + k as f64 * 0.5 + jitter, 0.8)
        });
        let grid = grid_of(&clicks(jittered, 0.5, 200.0, 44_100));
        assert!(grid.is_none_or(|(_, steady)| !steady), "{grid:?}");
    }

    /// A 16-bit stereo WAV, optionally with a LIST/INFO title tag before the audio.
    fn write_wav(path: &Path, samples: &[i16], title: Option<&str>) {
        let mut info = Vec::new();
        if let Some(t) = title {
            let mut text = t.as_bytes().to_vec();
            text.push(0);
            if text.len() % 2 == 1 {
                text.push(0);
            }
            info.extend_from_slice(b"INFOINAM");
            info.extend_from_slice(&(text.len() as u32).to_le_bytes());
            info.extend_from_slice(&text);
        }
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut riff = b"WAVE".to_vec();
        riff.extend_from_slice(b"fmt ");
        riff.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 2] {
            riff.extend_from_slice(&v.to_le_bytes());
        }
        riff.extend_from_slice(&44_100u32.to_le_bytes());
        riff.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        for v in [4u16, 16] {
            riff.extend_from_slice(&v.to_le_bytes());
        }
        if !info.is_empty() {
            riff.extend_from_slice(b"LIST");
            riff.extend_from_slice(&(info.len() as u32).to_le_bytes());
            riff.extend_from_slice(&info);
        }
        riff.extend_from_slice(b"data");
        riff.extend_from_slice(&(data.len() as u32).to_le_bytes());
        riff.extend_from_slice(&data);
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(riff.len() as u32).to_le_bytes());
        file.extend_from_slice(&riff);
        std::fs::write(path, file).unwrap();
    }

    #[test]
    fn fingerprint_ignores_tags_and_name_but_not_audio() {
        let dir = std::env::temp_dir().join(format!("odj-fingerprint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let audio: Vec<i16> = (0..44_100 * 2).map(|i| ((i * 37) % 20_000) as i16 - 10_000).collect();
        let mut other = audio.clone();
        other[1000] += 1;
        write_wav(&dir.join("plain.wav"), &audio, None);
        write_wav(&dir.join("tagged copy.wav"), &audio, Some("Some Title"));
        write_wav(&dir.join("changed.wav"), &other, None);
        let id = |name: &str| Track::load(&dir.join(name)).unwrap().id;
        assert_eq!(id("plain.wav").len(), 32);
        assert_eq!(id("plain.wav"), id("tagged copy.wav"));
        assert_ne!(id("plain.wav"), id("changed.wav"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn auto_cue_skips_leading_silence() {
        let mut s = vec![0.0f32; 44_100 * 2];
        s.extend(click_track(128.0, 5.0, 44_100));
        let t = Track::from_samples("x".into(), "x".into(), None, 44_100, s);
        assert!(t.first_sound >= 44_100.0);
    }
}

#[cfg(test)]
mod file_test {
    #[test]
    #[ignore]
    fn load_real_file() {
        let p = std::env::var("ODJ_TEST_FILE").unwrap();
        let t = super::Track::load(std::path::Path::new(&p)).unwrap();
        eprintln!("title={} artist={:?} sr={} dur={:.2} bpm={:?} first={}", t.title, t.artist, t.sample_rate, t.duration(), t.grid, t.first_sound);
    }
}
