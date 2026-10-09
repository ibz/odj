//! Decoding a file fully into memory, plus the analysis the player's display
//! shows: waveform, BPM and the auto-cue point.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

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
    pub bpm: Option<f64>,
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
        Track {
            // Set by `load`; tracks built from bare samples have no file to fingerprint.
            id: String::new(),
            wave: waveform(&mono, sample_rate),
            bpm: detect_bpm(&mono, sample_rate),
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

/// Tempo estimate from the autocorrelation of an onset-strength envelope.
pub fn detect_bpm(mono: &[f32], sample_rate: u32) -> Option<f64> {
    const ENV_RATE: f64 = 200.0;
    const MIN_BPM: f64 = 60.0;
    const MAX_BPM: f64 = 200.0;

    let hop = ((sample_rate as f64 / ENV_RATE) as usize).max(1);
    let env_rate = sample_rate as f64 / hop as f64;
    if mono.len() < hop * (env_rate as usize) * 4 {
        return None;
    }

    // Energy envelopes of the full band and the bass band (kicks).
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
        let n = chunk.len() as f32;
        full.push((1.0 + 1000.0 * ef / n).ln());
        low.push((1.0 + 1000.0 * el / n).ln());
    }

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
    if (bpm - bpm.round()).abs() < 0.1 {
        bpm = bpm.round();
    }
    Some((bpm * 10.0).round() / 10.0)
}

#[cfg(any(test, feature = "test-util"))]
pub mod test_util {
    /// A synthetic four-on-the-floor kick pattern with off-beat hats.
    pub fn click_track(bpm: f64, seconds: f64, sample_rate: u32) -> Vec<f32> {
        let frames = (seconds * sample_rate as f64) as usize;
        let beat = 60.0 / bpm * sample_rate as f64;
        let mut out = vec![0.0f32; frames * 2];
        let mut t = beat * 0.5;
        while (t as usize) < frames {
            let start = t as usize;
            for i in 0..(sample_rate as usize / 8) {
                let s = start + i;
                if s >= frames {
                    break;
                }
                let x = i as f32 / sample_rate as f32;
                let kick = (2.0 * std::f32::consts::PI * 55.0 * x).sin() * (-x * 25.0).exp();
                out[s * 2] += 0.8 * kick;
                out[s * 2 + 1] += 0.8 * kick;
            }
            let hat = (t + beat / 2.0) as usize;
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
            t += beat;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::click_track;
    use super::*;

    #[test]
    fn detects_common_tempos() {
        for &bpm in &[90.0, 124.0, 128.0, 140.0, 174.0] {
            let s = click_track(bpm, 40.0, 44_100);
            let mono: Vec<f32> = s.chunks_exact(2).map(|f| f[0]).collect();
            let got = detect_bpm(&mono, 44_100).expect("bpm");
            assert!((got - bpm).abs() < 0.15, "expected {bpm}, got {got}");
        }
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
        eprintln!("title={} artist={:?} sr={} dur={:.2} bpm={:?} first={}", t.title, t.artist, t.sample_rate, t.duration(), t.bpm, t.first_sound);
    }
}
