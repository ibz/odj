//! Audio output through cpal (ALSA, which PipeWire serves on most desktops).

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::engine::Deck;

/// Requested buffer size; small enough for cue/hot-cue response to feel immediate.
const BUFFER_FRAMES: u32 = 512;

pub struct Output {
    device: Device,
    config: StreamConfig,
    format: SampleFormat,
    pub name: String,
}

impl Output {
    pub fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("no audio output device"))?;
        let supported = device.default_output_config().context("querying output config")?;
        let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
        Ok(Self {
            format: supported.sample_format(),
            config: supported.into(),
            device,
            name,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    pub fn start(&self, deck: Arc<Mutex<Deck>>) -> Result<Stream> {
        let mut config = self.config;
        config.buffer_size = BufferSize::Fixed(BUFFER_FRAMES);
        let stream = self.build(&config, deck.clone()).or_else(|_| {
            config.buffer_size = BufferSize::Default;
            self.build(&config, deck)
        })?;
        stream.play().context("starting audio stream")?;
        Ok(stream)
    }

    fn build(&self, config: &StreamConfig, deck: Arc<Mutex<Deck>>) -> Result<Stream> {
        match self.format {
            SampleFormat::F32 => build::<f32>(&self.device, config, deck),
            SampleFormat::F64 => build::<f64>(&self.device, config, deck),
            SampleFormat::I16 => build::<i16>(&self.device, config, deck),
            SampleFormat::I32 => build::<i32>(&self.device, config, deck),
            SampleFormat::U16 => build::<u16>(&self.device, config, deck),
            other => Err(anyhow!("unsupported sample format {other}")),
        }
    }
}

fn build<T>(device: &Device, config: &StreamConfig, deck: Arc<Mutex<Deck>>) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut stereo = vec![0.0f32; 8192];
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            let frames = data.len() / channels;
            if stereo.len() < frames * 2 {
                stereo.resize(frames * 2, 0.0);
            }
            deck.lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(&mut stereo[..frames * 2]);
            for (frame, lr) in data.chunks_exact_mut(channels).zip(stereo.chunks_exact(2)) {
                if channels == 1 {
                    frame[0] = T::from_sample(0.5 * (lr[0] + lr[1]));
                } else {
                    frame[0] = T::from_sample(lr[0]);
                    frame[1] = T::from_sample(lr[1]);
                    for s in &mut frame[2..] {
                        *s = T::EQUILIBRIUM;
                    }
                }
            }
        },
        // Printing would corrupt the TUI; xruns are audible anyway.
        |_| {},
        None,
    )?;
    Ok(stream)
}
