//! Writing a plain PCM16 WAV file — the write-side counterpart to
//! [`crate::track::decode`].

use std::io;
use std::path::Path;

/// Writes `samples` (interleaved, `channels` channels, in roughly `-1.0..=1.0`)
/// to `path` as a 16-bit PCM WAV at `rate`. Values outside range are clamped
/// rather than wrapping.
pub fn write_wav(path: &Path, samples: &[f32], channels: u16, rate: u32) -> io::Result<()> {
    let data: Vec<u8> =
        samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes()).collect();
    let block_align = channels * 2;
    let mut riff = b"WAVE".to_vec();
    riff.extend_from_slice(b"fmt ");
    riff.extend_from_slice(&16u32.to_le_bytes());
    riff.extend_from_slice(&1u16.to_le_bytes()); // PCM
    riff.extend_from_slice(&channels.to_le_bytes());
    riff.extend_from_slice(&rate.to_le_bytes());
    riff.extend_from_slice(&(rate * block_align as u32).to_le_bytes()); // byte rate
    riff.extend_from_slice(&block_align.to_le_bytes());
    riff.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    riff.extend_from_slice(b"data");
    riff.extend_from_slice(&(data.len() as u32).to_le_bytes());
    riff.extend_from_slice(&data);
    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(riff.len() as u32).to_le_bytes());
    file.extend_from_slice(&riff);
    std::fs::write(path, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::decode;

    #[test]
    fn round_trips_through_decode() {
        let dir = std::env::temp_dir().join(format!("odj-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");

        let samples: Vec<f32> = (0..2000).map(|i| ((i as f32) * 0.1).sin() * 0.5).collect();
        write_wav(&path, &samples, 1, 22_050).unwrap();

        let decoded = decode(&path).unwrap();
        assert_eq!(decoded.sample_rate, 22_050);
        // Written mono, decode() upmixes to interleaved stereo.
        assert_eq!(decoded.samples.len(), samples.len() * 2);
        for (i, &s) in samples.iter().enumerate() {
            assert!((decoded.samples[i * 2] - s).abs() < 0.01, "sample {i}: {} vs {s}", decoded.samples[i * 2]);
        }

        std::fs::remove_dir_all(dir).unwrap();
    }
}
