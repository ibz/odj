//! Writing a plain PCM16 WAV file — the write-side counterpart to
//! [`crate::track::decode`]. [`write_wav`] writes a whole buffer at once;
//! [`WavWriter`] writes incrementally, for a recording whose length isn't
//! known upfront and whose memory use needs to stay flat regardless of how
//! long it runs.

use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

/// `-1.0..=1.0` (clamped rather than wrapped) to little-endian 16-bit PCM
/// bytes — the one place either writer below converts samples, so they
/// can't drift apart.
fn pcm16_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes()).collect()
}

/// The fixed 44-byte canonical PCM16 WAV header for `data_len` bytes of
/// data that follow it. [`write_wav`] knows `data_len` upfront; writing
/// it incrementally instead means writing this with `data_len: 0` first
/// and patching the two size fields once the real length is known (see
/// [`WavWriter::finish`]) — offset 4 is the RIFF chunk size
/// (`36 + data_len`), offset 40 is the data chunk size (`data_len`).
fn wav_header(channels: u16, rate: u32, data_len: u32) -> [u8; 44] {
    let block_align = channels * 2;
    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    header[28..32].copy_from_slice(&(rate * block_align as u32).to_le_bytes()); // byte rate
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_len.to_le_bytes());
    header
}

/// Writes `samples` (interleaved, `channels` channels, in roughly `-1.0..=1.0`)
/// to `path` as a 16-bit PCM WAV at `rate`. Values outside range are clamped
/// rather than wrapping.
pub fn write_wav(path: &Path, samples: &[f32], channels: u16, rate: u32) -> io::Result<()> {
    let data = pcm16_bytes(samples);
    let mut file = wav_header(channels, rate, data.len() as u32).to_vec();
    file.extend_from_slice(&data);
    std::fs::write(path, file)
}

/// A WAV file written incrementally: [`create`](Self::create) writes a
/// placeholder header, [`write`](Self::write) appends PCM16 data as it
/// arrives (memory use stays flat — nothing buffered beyond one call's
/// own slice), [`finish`](Self::finish) seeks back and patches the RIFF
/// and data chunk sizes now that the real length is known.
pub struct WavWriter {
    file: std::fs::File,
    data_len: u32,
}

impl WavWriter {
    pub fn create(path: &Path, channels: u16, rate: u32) -> io::Result<Self> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(&wav_header(channels, rate, 0))?;
        Ok(Self { file, data_len: 0 })
    }

    /// Appends `samples` as 16-bit PCM. `data_len` saturates rather than
    /// wrapping on overflow — WAV's own 32-bit size field caps a single
    /// file around 4 GiB (~6 hours at 48 kHz stereo) regardless; a
    /// recording that long hits the format's own limit, not a bug here.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        let bytes = pcm16_bytes(samples);
        self.file.write_all(&bytes)?;
        self.data_len = self.data_len.saturating_add(bytes.len() as u32);
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&(36u32 + self.data_len).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&self.data_len.to_le_bytes())?;
        self.file.flush()
    }
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

    #[test]
    fn wav_writer_round_trips_through_decode() {
        let dir = std::env::temp_dir().join(format!("odj-wavwriter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");

        let samples: Vec<f32> = (0..2000).map(|i| ((i as f32) * 0.1).sin() * 0.5).collect();
        let mut w = WavWriter::create(&path, 1, 22_050).unwrap();
        // Several uneven writes, not one big one, to exercise incremental appends.
        for chunk in [&samples[..300], &samples[300..301], &samples[301..1500], &samples[1500..]] {
            w.write(chunk).unwrap();
        }
        w.finish().unwrap();

        let decoded = decode(&path).unwrap();
        assert_eq!(decoded.sample_rate, 22_050);
        assert_eq!(decoded.samples.len(), samples.len() * 2);
        for (i, &s) in samples.iter().enumerate() {
            assert!((decoded.samples[i * 2] - s).abs() < 0.01, "sample {i}: {} vs {s}", decoded.samples[i * 2]);
        }

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wav_writer_matches_write_wav_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("odj-wavwriter-bytes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (one_shot, incremental) = (dir.join("one_shot.wav"), dir.join("incremental.wav"));

        let samples: Vec<f32> = (0..1000).map(|i| ((i as f32) * 0.05).cos() * 0.8).collect();
        write_wav(&one_shot, &samples, 2, 48_000).unwrap();

        let mut w = WavWriter::create(&incremental, 2, 48_000).unwrap();
        w.write(&samples[..400]).unwrap();
        w.write(&samples[400..]).unwrap();
        w.finish().unwrap();

        assert_eq!(std::fs::read(&one_shot).unwrap(), std::fs::read(&incremental).unwrap());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wav_writer_create_fails_when_the_parent_directory_is_missing() {
        let path = std::env::temp_dir().join(format!("odj-wavwriter-missing-{}/tone.wav", std::process::id()));
        assert!(WavWriter::create(&path, 2, 48_000).is_err());
    }
}
