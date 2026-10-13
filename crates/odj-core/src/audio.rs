//! Audio output through cpal: PipeWire when it runs, so several odj apps can share
//! a sound card; otherwise straight to the ALSA hardware.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::link;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::platform::PipeWireHost;
use cpal::{
    BufferSize, Error as CpalError, OutputCallbackInfo, Device, DeviceDirection, ErrorKind, FromSample, HostId,
    SampleFormat, SizedSample, Stream, StreamConfig,
};
use pipewire as pw;

pub use cpal::Host;

/// Whatever produces the sound: the audio callback asks it for each block.
pub trait Source: Send + 'static {
    /// Fills `out` with interleaved stereo samples, the first of which comes
    /// out of the speakers at `heard_at` on the [`link::now`] clock.
    fn render(&mut self, out: &mut [f32], heard_at: f64);
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
    /// Stable handle for this card: the PipeWire node name, or the ALSA `hw:CARD=` id.
    /// Used to re-resolve the card at open time, since this whole snapshot can be stale by
    /// then — a USB card that was unplugged and replugged gets a new PipeWire node (and can
    /// land on a new ALSA card index too), even though this `id` stays the same.
    id: String,
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
        if let Some(why) = card.unavailable {
            return format!("{} · {why}", card.name);
        }
        route_label(&card.name, card.channels, self.route)
    }
}

/// Builds the choice label text from a card's name and channel count directly, so
/// [`Output::open`] can label the card it actually resolved rather than the possibly
/// stale one the picker showed.
fn route_label(name: &str, channels: u16, route: Route) -> String {
    match (channels, route) {
        (2, Route::Stereo(..)) => format!("{name} · stereo"),
        (2, Route::Mono(0)) => format!("{name} · left only (mono)"),
        (2, Route::Mono(_)) => format!("{name} · right only (mono)"),
        (_, Route::Stereo(l, r)) => format!("{name} · Out {}–{} (stereo)", l + 1, r + 1),
        (_, Route::Mono(c)) => format!("{name} · Out {} (mono)", c + 1),
    }
}

/// PipeWire when it is running, plain ALSA otherwise. Keep it alive while streams play.
pub fn host() -> Host {
    cpal::default_host()
}

/// Playback devices to choose from, including HDMI/DisplayPort — the picker is always
/// shown, so there's no need to hide outputs nobody would want; the user just skips them.
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
            "PipeWire is running but has no usable audio outputs.\n\
             Check `wpctl status`. If it lists no devices, PipeWire probably can't open the \
             sound cards: make sure your user may use /dev/snd (e.g. is in the `audio` group), \
             then log in again or reboot so PipeWire restarts with that access."
        );
    }
    Ok(cards)
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
        let Ok(config) = device.default_output_config() else { continue };
        cards.push(Card {
            device,
            id: node,
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
                cards.push(Card { device: plug.clone(), id: id.clone(), name, channels: 0, rate: 0, unavailable: Some(why) });
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
        cards.push(Card { device: plug.clone(), id: id.clone(), name, channels, rate: rate.sample_rate(), unavailable: None });
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

/// Carries a cpal stream error from the audio callback thread (where printing would
/// corrupt the TUI) out to the app's event loop, so a problem that shows up after the
/// stream was already built — the device disappearing, PipeWire losing it — reaches the
/// status line instead of vanishing silently.
#[derive(Clone, Default)]
pub struct Errors(Arc<Mutex<Option<String>>>);

impl Errors {
    fn report(&self, msg: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
    }

    /// The latest error, if one arrived since the last call.
    pub fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

pub struct Output {
    device: Device,
    config: StreamConfig,
    format: SampleFormat,
    route: Route,
    pub name: String,
    /// The card [`verify_route`] should find this stream linked to; `None` for
    /// [`Output::open_default`], which has no specific target to check against.
    target_id: Option<String>,
    /// Errors reported by the stream's callback once it's running.
    pub errors: Errors,
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
            target_id: None,
            errors: Errors::default(),
        })
    }

    /// Opens `choice` on one of `cards`. Re-enumerates right before connecting: the list
    /// the picker showed can be stale by the time the user picks from it (a USB card
    /// replugged in between gets a new PipeWire node even though its name didn't change),
    /// and opening the stale `Device` would silently reconnect to the system default
    /// instead of erroring — the exact failure this re-check exists to catch.
    pub fn open(host: &Host, cards: &[Card], choice: &Choice) -> Result<Self> {
        let wanted = &cards[choice.card];
        if let Some(why) = wanted.unavailable {
            bail!("{} is unavailable: {why}", wanted.name);
        }
        let fresh = self::cards(host)?;
        let card = fresh
            .iter()
            .find(|c| c.id == wanted.id)
            .ok_or_else(|| anyhow!("{} is no longer available", wanted.name))?;
        if let Some(why) = card.unavailable {
            bail!("{} is unavailable: {why}", card.name);
        }
        if card.channels != wanted.channels {
            bail!(
                "{} changed ({} channels now, {} when it was picked)",
                card.name,
                card.channels,
                wanted.channels
            );
        }
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
            name: route_label(&card.name, card.channels, choice.route),
            target_id: Some(card.id.clone()),
            errors: Errors::default(),
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
        let errors = self.errors.clone();
        match self.format {
            SampleFormat::F32 => build::<f32, S>(&self.device, config, route, source, errors),
            SampleFormat::F64 => build::<f64, S>(&self.device, config, route, source, errors),
            SampleFormat::I16 => build::<i16, S>(&self.device, config, route, source, errors),
            SampleFormat::I32 => build::<i32, S>(&self.device, config, route, source, errors),
            SampleFormat::U16 => build::<u16, S>(&self.device, config, route, source, errors),
            other => Err(anyhow!("unsupported sample format {other}")),
        }
    }
}

fn build<T, S>(
    device: &Device,
    config: &StreamConfig,
    route: Route,
    source: Arc<Mutex<S>>,
    errors: Errors,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
    S: Source,
{
    let channels = config.channels as usize;
    let mut stereo = vec![0.0f32; 8192];
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], info: &OutputCallbackInfo| {
            let ts = info.timestamp();
            let heard_at = link::now() + ts.playback.duration_since(ts.callback).as_secs_f64();
            let frames = data.len() / channels;
            if stereo.len() < frames * 2 {
                stereo.resize(frames * 2, 0.0);
            }
            source.lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(&mut stereo[..frames * 2], heard_at);
            for (frame, lr) in data.chunks_exact_mut(channels).zip(stereo.chunks_exact(2)) {
                route_frame(frame, lr, route);
            }
        },
        // Printing would corrupt the TUI; reported through `Errors` instead for the app
        // to show. Xruns are dropped here too — they're audible already, and reporting
        // every one would spam the status line.
        move |err: CpalError| {
            if err.kind() != ErrorKind::Xrun {
                errors.report(match err.message() {
                    Some(m) => format!("Audio error: {m}"),
                    None => format!("Audio error: {}", err.kind()),
                });
            }
        },
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

/// Confirms a PipeWire-routed [`Output`] actually connected to the card it was opened on,
/// rather than PipeWire silently falling back to the default sink because the node we
/// asked for had disappeared by the time the stream connected (the race [`Output::open`]'s
/// re-resolve narrows but can't fully close). Call this right after [`Output::start`]; on
/// a mismatch, drop the returned stream and report it instead of letting playback continue
/// on the wrong device.
///
/// A no-op for [`Output::open_default`] (nothing specific to check against) and for the
/// plain ALSA backend (no silent rerouting is possible there). Also a no-op if PipeWire
/// doesn't answer quickly or the stream has no sink link yet — neither is evidence of a
/// wrong route, just of a slow or busy session.
pub fn verify_route(host: &Host, output: &Output) -> Result<()> {
    let Some(want) = output.target_id.as_deref() else { return Ok(()) };
    if host.id() != HostId::PipeWire {
        return Ok(());
    }
    match pipewire_peer_of_own_stream() {
        PeerCheck::Linked(got) if got == want => Ok(()),
        PeerCheck::Linked(got) => bail!(
            "expected to play on {} but PipeWire linked the stream to {got} instead \
             (the device likely disappeared and PipeWire fell back to its default)",
            output.name
        ),
        PeerCheck::Unconfirmed => Ok(()),
    }
}

#[derive(Debug)]
enum PeerCheck {
    /// The stream is linked to exactly this PipeWire node name.
    Linked(String),
    /// Couldn't tell in time, or the stream has no sink link yet — not treated as a mismatch.
    Unconfirmed,
}

/// Looks up, via a short-lived PipeWire registry roundtrip (the same technique `pw-dump`
/// and `pw-mon` use), which sink node our own stream is actually linked to. Our stream is
/// identified by the `node.group` cpal tags it with — `cpal-<pid>` — since cpal doesn't
/// otherwise expose the node id it ends up with.
///
/// Each discovered node and link is bound individually to read its full properties: the
/// registry's own `global` event carries only a reduced prop set that doesn't include
/// ad-hoc stream properties like `node.group`. Those binds are reactive (sent while
/// dispatching the first sync's replies), so a second sync/wait round trip follows to be
/// sure their `info` replies have also come back before giving up and reading what was
/// collected.
fn pipewire_peer_of_own_stream() -> PeerCheck {
    pw::init();
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else { return PeerCheck::Unconfirmed };
    let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else { return PeerCheck::Unconfirmed };
    let Ok(core) = context.connect_rc(None) else { return PeerCheck::Unconfirmed };
    let Ok(registry) = core.get_registry_rc() else { return PeerCheck::Unconfirmed };

    // node id -> (node.name, is this our stream's own node?)
    let nodes: Rc<RefCell<HashMap<u32, (String, bool)>>> = Rc::default();
    // (output node id, input node id) per PipeWire link
    let links: Rc<RefCell<Vec<(u32, u32)>>> = Rc::default();
    // Keeps every bound Node/Link proxy and its info listener alive for the round trips;
    // PipeWire only delivers a bound object's info while its proxy is still held.
    type Proxy = (Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>);
    let proxies: Rc<RefCell<Vec<Proxy>>> = Rc::default();
    let our_group = format!("cpal-{}", std::process::id());

    let registry_for_cb = registry.clone();
    let nodes_for_cb = nodes.clone();
    let links_for_cb = links.clone();
    let proxies_for_cb = proxies.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| match g.type_ {
            pw::types::ObjectType::Node => {
                let Ok(node): Result<pw::node::Node, _> = registry_for_cb.bind(g) else { return };
                let nodes_for_info = nodes_for_cb.clone();
                let group = our_group.clone();
                let id = g.id;
                let listener = node
                    .add_listener_local()
                    .info(move |info| {
                        let props = info.props();
                        let name = props.and_then(|p| p.get("node.name")).unwrap_or_default().to_string();
                        let ours = props.and_then(|p| p.get("node.group")) == Some(group.as_str());
                        nodes_for_info.borrow_mut().insert(id, (name, ours));
                    })
                    .register();
                proxies_for_cb.borrow_mut().push((Box::new(node), Box::new(listener)));
            }
            pw::types::ObjectType::Link => {
                let Ok(link): Result<pw::link::Link, _> = registry_for_cb.bind(g) else { return };
                let links_for_info = links_for_cb.clone();
                let listener = link
                    .add_listener_local()
                    .info(move |info| {
                        links_for_info.borrow_mut().push((info.output_node_id(), info.input_node_id()));
                    })
                    .register();
                proxies_for_cb.borrow_mut().push((Box::new(link), Box::new(listener)));
            }
            _ => {}
        })
        .register();

    let stage = Rc::new(Cell::new(0u8));
    let done = Rc::new(Cell::new(false));
    let core_for_done = core.clone();
    let stage_for_done = stage.clone();
    let done_for_cb = done.clone();
    let loop_for_done = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, _seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            if stage_for_done.get() == 0 {
                stage_for_done.set(1);
                let _ = core_for_done.sync(0);
            } else {
                done_for_cb.set(true);
                loop_for_done.quit();
            }
        })
        .register();
    if core.sync(0).is_err() {
        return PeerCheck::Unconfirmed;
    }

    // Bounds the round trips: without this, a PipeWire session that never answers would
    // hang the caller forever instead of falling back to `Unconfirmed`.
    let loop_for_timeout = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| loop_for_timeout.quit());
    let _ = timer.update_timer(Some(Duration::from_millis(1500)), None);

    mainloop.run();
    if !done.get() {
        return PeerCheck::Unconfirmed; // the timer fired first
    }

    let nodes = nodes.borrow();
    let Some((&our_id, _)) = nodes.iter().find(|(_, (_, ours))| *ours) else {
        return PeerCheck::Unconfirmed;
    };
    links
        .borrow()
        .iter()
        .find(|(output, _)| *output == our_id)
        .and_then(|(_, input)| nodes.get(input))
        .map(|(name, _)| PeerCheck::Linked(name.clone()))
        .unwrap_or(PeerCheck::Unconfirmed)
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

    struct Silence;
    impl Source for Silence {
        fn render(&mut self, out: &mut [f32], _heard_at: f64) {
            out.fill(0.0);
        }
    }

    /// Picks a usable stereo choice so the two tests below don't duplicate it.
    fn any_stereo_choice(host: &Host) -> (Vec<Card>, Choice) {
        let cards = cards(host).unwrap();
        let choice = choices(&cards)
            .into_iter()
            .find(|c| matches!(c.route, Route::Stereo(..)) && c.available(&cards))
            .expect("this machine has no usable stereo output");
        (cards, choice)
    }

    #[test]
    #[ignore = "opens and plays silence on this machine's default output"]
    fn verify_route_confirms_the_real_target() {
        let host = host();
        let (cards, choice) = any_stereo_choice(&host);
        let output = Output::open(&host, &cards, &choice).unwrap();
        let stream = output.start(Arc::new(Mutex::new(Silence))).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            matches!(pipewire_peer_of_own_stream(), PeerCheck::Linked(_)),
            "should have actually found the link, not fallen back to Unconfirmed"
        );
        verify_route(&host, &output).unwrap();
        drop(stream);
    }

    #[test]
    #[ignore = "opens and plays silence on this machine's default output"]
    fn verify_route_rejects_a_mismatched_target() {
        let host = host();
        let (cards, choice) = any_stereo_choice(&host);
        let mut output = Output::open(&host, &cards, &choice).unwrap();
        output.target_id = Some("not-a-real-pipewire-node".to_string());
        let stream = output.start(Arc::new(Mutex::new(Silence))).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let err = verify_route(&host, &output).unwrap_err();
        assert!(err.to_string().contains("PipeWire linked"), "{err}");
        drop(stream);
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
