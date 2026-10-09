//! Audio output through cpal: PipeWire when it runs, so several odj apps can share
//! a sound card; otherwise straight to the ALSA hardware.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::platform::PipeWireHost;
use cpal::{
    BufferSize, Device, DeviceDirection, ErrorKind, FromSample, HostId, SampleFormat, SizedSample, Stream,
    StreamConfig,
};

pub use cpal::Host;

/// Whatever produces the sound: the audio callback asks it for each block.
pub trait Source: Send + 'static {
    /// Fills `out` with interleaved stereo samples.
    fn render(&mut self, out: &mut [f32]);
}

/// Requested buffer size; small enough for cue/hot-cue response to feel immediate.
const BUFFER_FRAMES: u32 = 512;

/// Which output channels (0-based) of a card the deck plays on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Route {
    Stereo(u16, u16),
    /// Both sides of the track summed into one channel.
    Mono(u16),
}

/// A playback device: a PipeWire sink, or with plain ALSA a card opened through
/// `plughw` so ALSA converts sample formats while we still address every channel.
pub struct Card {
    device: Device,
    pub name: String,
    pub channels: u16,
    rate: u32,
    /// Why the card can't be used right now (ALSA only; PipeWire shares cards).
    pub unavailable: Option<&'static str>,
}

/// One entry of the output picker.
pub struct Choice {
    pub card: usize,
    pub route: Route,
}

impl Choice {
    pub fn available(&self, cards: &[Card]) -> bool {
        cards[self.card].unavailable.is_none()
    }

    pub fn label(&self, cards: &[Card]) -> String {
        let card = &cards[self.card];
        let name = &card.name;
        if let Some(why) = card.unavailable {
            return format!("{name} · {why}");
        }
        match (card.channels, self.route) {
            (2, Route::Stereo(..)) => format!("{name} · stereo"),
            (2, Route::Mono(0)) => format!("{name} · left only (mono)"),
            (2, Route::Mono(_)) => format!("{name} · right only (mono)"),
            (_, Route::Stereo(l, r)) => format!("{name} · Out {}–{} (stereo)", l + 1, r + 1),
            (_, Route::Mono(c)) => format!("{name} · Out {} (mono)", c + 1),
        }
    }
}

/// PipeWire when it is running, plain ALSA otherwise. Keep it alive while streams play.
pub fn host() -> Host {
    cpal::default_host()
}

/// Playback devices to choose from, without HDMI/DisplayPort.
///
/// Fails when PipeWire runs but offers nothing: going around it to the hardware would
/// lock cards away from PipeWire and other odj instances, and hide the real problem.
pub fn cards(host: &Host) -> Result<Vec<Card>> {
    let pipewire = <PipeWireHost as HostTrait>::is_available();
    let cards = match host.id() {
        HostId::PipeWire => pipewire_cards(host),
        _ => alsa_cards(host),
    };
    if pipewire && (host.id() != HostId::PipeWire || cards.is_empty()) {
        bail!(
            "PipeWire is running but has no usable audio outputs (HDMI is not used).\n\
             Check `wpctl status`. If it lists no devices, PipeWire probably can't open the \
             sound cards: make sure your user may use /dev/snd (e.g. is in the `audio` group), \
             then log in again or reboot so PipeWire restarts with that access."
        );
    }
    Ok(cards)
}

fn is_hdmi(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("hdmi") || name.contains("displayport")
}

/// PipeWire sinks. Other programs' playback streams (including other odj instances)
/// and the "follow the default" placeholders are left out.
fn pipewire_cards(host: &Host) -> Vec<Card> {
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    let mut cards = Vec::new();
    for device in devices {
        let node = device.id().map(|id| id.id().to_string()).unwrap_or_default();
        let Ok(desc) = device.description() else { continue };
        // Sinks are listed as duplex (their monitor can be recorded); streams as output only.
        if desc.direction() != DeviceDirection::Duplex || node.ends_with("_default") {
            continue;
        }
        // The whole-card node that split sinks (e.g. "Line A", "Line B") are carved out of.
        if node.starts_with("alsa_output.hw_") {
            continue;
        }
        let name = desc.name().to_string();
        if is_hdmi(&node) || is_hdmi(&name) {
            continue;
        }
        let Ok(config) = device.default_output_config() else { continue };
        cards.push(Card {
            device,
            name,
            channels: config.channels(),
            rate: config.sample_rate(),
            unavailable: None,
        });
    }
    // PipeWire lists nodes in creation order, which can put "Line B" before "Line A".
    cards.sort_by(|a, b| a.name.cmp(&b.name));
    cards
}

/// ALSA hardware cards. Cards that are busy or off-limits are listed as unavailable.
fn alsa_cards(host: &Host) -> Vec<Card> {
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    let devices: Vec<Device> = devices.collect();
    let pcm_id = |d: &Device| d.id().map(|id| id.id().to_string()).unwrap_or_default();
    let mut cards = Vec::new();
    for hw in &devices {
        // Each physical device is listed once as hw:CARD=<index>,DEV=<index>.
        let id = pcm_id(hw);
        let Some(rest) = id.strip_prefix("hw:CARD=") else { continue };
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let Ok(desc) = hw.description() else { continue };
        let name = desc.name().to_string();
        if is_hdmi(&name) {
            continue;
        }
        let Some(plug) = devices.iter().find(|d| pcm_id(d) == format!("plug{id}")) else { continue };
        // The raw hw device tells the real channel count and native rate.
        let configs = match hw.supported_output_configs() {
            Ok(configs) => configs,
            Err(e) => {
                let why = match e.kind() {
                    ErrorKind::DeviceBusy => "in use by another program",
                    ErrorKind::PermissionDenied => "no permission (are you in the audio group?)",
                    _ => continue,
                };
                cards.push(Card { device: plug.clone(), name, channels: 0, rate: 0, unavailable: Some(why) });
                continue;
            }
        };
        let Some(best) = configs.max_by(|a, b| {
            a.channels().cmp(&b.channels()).then(a.cmp_default_heuristics(b))
        }) else {
            continue;
        };
        let channels = best.channels();
        let rate = best.try_with_standard_sample_rate().unwrap_or_else(|| best.with_max_sample_rate());
        cards.push(Card { device: plug.clone(), name, channels, rate: rate.sample_rate(), unavailable: None });
    }
    cards
}

/// Every stereo pair (1–2, 3–4, …) of every card, then every single channel.
/// An unavailable card gets one entry saying why.
pub fn choices(cards: &[Card]) -> Vec<Choice> {
    let mut out = Vec::new();
    for (i, card) in cards.iter().enumerate() {
        if card.unavailable.is_some() {
            out.push(Choice { card: i, route: Route::Stereo(0, 1) });
            continue;
        }
        for l in (0..card.channels.saturating_sub(1)).step_by(2) {
            out.push(Choice { card: i, route: Route::Stereo(l, l + 1) });
        }
        for c in 0..card.channels {
            out.push(Choice { card: i, route: Route::Mono(c) });
        }
    }
    out
}

pub struct Output {
    device: Device,
    config: StreamConfig,
    format: SampleFormat,
    route: Route,
    pub name: String,
}

impl Output {
    /// The system default device, playing on its first two channels.
    pub fn open_default(host: &Host) -> Result<Self> {
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no audio output device"))?;
        let supported = device.default_output_config().context("querying output config")?;
        let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
        let route = if supported.channels() >= 2 { Route::Stereo(0, 1) } else { Route::Mono(0) };
        Ok(Self {
            format: supported.sample_format(),
            config: supported.into(),
            device,
            route,
            name,
        })
    }

    pub fn open(cards: &[Card], choice: &Choice) -> Result<Self> {
        let card = &cards[choice.card];
        let configs: Vec<_> = card
            .device
            .supported_output_configs()
            .with_context(|| format!("opening {}", card.name))?
            .filter(|c| c.channels() == card.channels && c.contains_rate(card.rate))
            .collect();
        let supported = configs
            .iter()
            .find(|c| c.sample_format() == SampleFormat::F32)
            .or_else(|| configs.iter().max_by(|a, b| a.cmp_default_heuristics(b)))
            .ok_or_else(|| anyhow!("{} has no usable output format", card.name))?
            .with_sample_rate(card.rate);
        Ok(Self {
            format: supported.sample_format(),
            config: supported.into(),
            device: card.device.clone(),
            route: choice.route,
            name: choice.label(cards),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    pub fn start<S: Source>(&self, source: Arc<Mutex<S>>) -> Result<Stream> {
        let mut config = self.config;
        config.buffer_size = BufferSize::Fixed(BUFFER_FRAMES);
        let stream = self.build(&config, source.clone()).or_else(|_| {
            config.buffer_size = BufferSize::Default;
            self.build(&config, source)
        })?;
        stream.play().context("starting audio stream")?;
        Ok(stream)
    }

    fn build<S: Source>(&self, config: &StreamConfig, source: Arc<Mutex<S>>) -> Result<Stream> {
        let route = self.route;
        match self.format {
            SampleFormat::F32 => build::<f32, S>(&self.device, config, route, source),
            SampleFormat::F64 => build::<f64, S>(&self.device, config, route, source),
            SampleFormat::I16 => build::<i16, S>(&self.device, config, route, source),
            SampleFormat::I32 => build::<i32, S>(&self.device, config, route, source),
            SampleFormat::U16 => build::<u16, S>(&self.device, config, route, source),
            other => Err(anyhow!("unsupported sample format {other}")),
        }
    }
}

fn build<T, S>(device: &Device, config: &StreamConfig, route: Route, source: Arc<Mutex<S>>) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
    S: Source,
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
            source.lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(&mut stereo[..frames * 2]);
            for (frame, lr) in data.chunks_exact_mut(channels).zip(stereo.chunks_exact(2)) {
                route_frame(frame, lr, route);
            }
        },
        // Printing would corrupt the TUI; xruns are audible anyway.
        |_| {},
        None,
    )?;
    Ok(stream)
}

/// Writes one stereo frame to the routed channels and silences the rest.
fn route_frame<T: SizedSample + FromSample<f32>>(frame: &mut [T], lr: &[f32], route: Route) {
    frame.fill(T::EQUILIBRIUM);
    match route {
        Route::Stereo(l, r) => {
            frame[l as usize] = T::from_sample(lr[0]);
            frame[r as usize] = T::from_sample(lr[1]);
        }
        Route::Mono(c) => frame[c as usize] = T::from_sample(0.5 * (lr[0] + lr[1])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_route_writes_pair_and_silences_rest() {
        let mut frame = [9.0f32; 4];
        route_frame(&mut frame, &[0.25, -0.5], Route::Stereo(2, 3));
        assert_eq!(frame, [0.0, 0.0, 0.25, -0.5]);
    }

    #[test]
    fn mono_route_sums_to_one_channel() {
        let mut frame = [9.0f32; 4];
        route_frame(&mut frame, &[0.25, -0.75], Route::Mono(1));
        assert_eq!(frame, [0.0, -0.25, 0.0, 0.0]);
    }

    #[test]
    #[ignore = "lists this machine's sound cards"]
    fn list_cards() {
        let cards = cards(&host()).unwrap();
        for c in &cards {
            println!("{} · {} ch · {} Hz", c.name, c.channels, c.rate);
        }
        for c in choices(&cards) {
            println!("  {}", c.label(&cards));
        }
    }
}
