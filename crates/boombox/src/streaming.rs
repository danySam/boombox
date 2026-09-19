//! Optional Spotify Connect device, backed by librespot.
//!
//! Compiled in only with `--features streaming`, and inert until
//! `[streaming] enabled = true`. When it is running, the daemon advertises
//! itself in Spotify's device list and decodes audio itself, rather than only
//! driving other devices over the Web API.
//!
//! Spotify permits one active stream per account, so this is a handoff: when
//! playback transfers here, whatever was playing elsewhere stops.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use boombox_core::Config;
use librespot::connect::{ConnectConfig, Spirc};
use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::DeviceType;
use librespot::core::{Session, SessionConfig};
use librespot::oauth::OAuthClientBuilder;
use librespot::playback::audio_backend::{self, Sink, SinkResult};
use librespot::playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot::playback::convert::Converter;
use librespot::playback::decoder::AudioPacket;
use librespot::playback::mixer::{self, MixerConfig};
use librespot::playback::player::Player;
use librespot::playback::{NUM_CHANNELS, SAMPLE_RATE};

use crate::spectrum::SpectrumTap;

/// Loopback redirect for the streaming login. Any loopback port is accepted
/// by Spotify's own client; this one matches librespot's example.
const OAUTH_REDIRECT: &str = "http://127.0.0.1:8898/login";

/// Where librespot keeps its own credentials, separate from the Web API token.
fn cache(config: &Config) -> Result<Cache> {
    let _ = config;
    let dir = boombox_core::private::librespot_dir()?;
    // Private: librespot writes its reusable login here with the process's
    // default permissions, which left it readable by other users.
    boombox_core::private::ensure_private_dir(&dir)?;
    // credentials and volume are cached; audio is not, so nothing large is
    // written and there is no cache size to manage.
    Cache::new(Some(dir.clone()), Some(dir), None::<std::path::PathBuf>, None)
        .map_err(|e| anyhow::anyhow!("could not open the librespot cache: {e}"))
}

/// Authorises the streaming session. This is a *second* login, distinct from
/// `boombox auth login`, and it cannot be avoided:
///
/// The Connect protocol first asks clienttoken.spotify.com for a client token,
/// and that endpoint only issues them to Spotify's own first-party client IDs.
/// A Development Mode client ID is rejected with 400, so the streaming session
/// has to be established under Spotify's public "keymaster" client rather than
/// the one the Web API half of boombox uses.
pub async fn authorize(config: &Config) -> Result<()> {
    let cache = cache(config)?;
    let session_config = SessionConfig::default();

    let client =
        OAuthClientBuilder::new(&session_config.client_id, OAUTH_REDIRECT, vec!["streaming"])
            .open_in_browser()
            .build()
            .map_err(|e| anyhow::anyhow!("could not start the streaming login: {e}"))?;

    let token = client
        .get_access_token_async()
        .await
        .map_err(|e| anyhow::anyhow!("streaming authorization failed: {e}"))?;

    // store_credentials = true writes reusable credentials into the cache, so
    // the daemon never needs this interactive flow again.
    let session = Session::new(session_config, Some(cache));
    session
        .connect(Credentials::with_access_token(token.access_token), true)
        .await
        .map_err(|e| anyhow::anyhow!("could not establish the streaming session: {e}"))?;
    // The directory is private already; this makes the login itself so too.
    boombox_core::private::restrict(&boombox_core::private::librespot_credentials()?)?;

    Ok(())
}

/// The name this machine's Connect device registers under.
///
/// A configured name is used exactly as written. Otherwise it is built from
/// the machine's own name, because the alternative -- every install calling
/// itself "boombox" -- puts two identical rows in the device picker as soon
/// as someone runs it on a second computer.
pub fn device_name(config: &Config) -> String {
    config
        .streaming
        .device_name
        .clone()
        .unwrap_or_else(|| default_device_name(sysinfo::System::host_name()))
}

/// "boombox on studio", or plain "boombox" from a machine that will not say
/// what it is called.
fn default_device_name(host: Option<String>) -> String {
    // "studio.local" and "studio.lan" are the same machine as "studio", and
    // the domain is noise in a picker.
    let host = host.unwrap_or_default();
    match host.split('.').next().unwrap_or_default().trim() {
        "" => "boombox".to_string(),
        machine => format!("boombox on {machine}"),
    }
}

/// Whether the streaming sign-in has been done on this machine.
pub fn authorized() -> bool {
    boombox_core::private::librespot_credentials().is_ok_and(|path| path.exists())
}

/// `boombox auth login --streaming`: signs in, and turns streaming on.
pub async fn login(config: &Config) -> Result<()> {
    println!("Opening your browser to authorize streaming.");
    println!("This is separate from `boombox auth login` -- see the note in README.md.");
    authorize(config).await?;
    let path = boombox_core::config::save_streaming_enabled(true)?;
    println!();
    println!("Streaming authorized, and turned on in {}.", path.display());
    println!();
    println!("The Connect device lives in the daemon, which `boombox` starts for you.");
    println!("If one is already running, restart it to pick this up:");
    println!("  boombox daemon --stop");
    println!();
    println!("Spotify allows one stream per account, so moving playback here");
    println!("stops whatever is playing elsewhere.");
    Ok(())
}

/// Wraps the real audio sink so every packet is copied into the spectrum tap
/// on its way to the speakers. librespot needs no patching for this: the sink
/// is supplied by us in the first place.
struct TappedSink {
    inner: Box<dyn Sink>,
    tap: Arc<SpectrumTap>,
}

impl Sink for TappedSink {
    fn start(&mut self) -> SinkResult<()> {
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.inner.stop()
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        // Raw packets are passthrough-encoded and carry no samples to analyse.
        if let Ok(samples) = packet.samples() {
            self.tap.push_interleaved(samples, NUM_CHANNELS as usize);
        }
        // The tap must never be able to interrupt playback, so the real write
        // happens regardless of what the analysis did.
        self.inner.write(packet, converter)
    }
}

/// A running Connect device. Dropping this does not stop it; call
/// [`Streaming::shutdown`] so librespot deregisters cleanly and Spotify does
/// not leave a ghost device in the picker.
pub struct Streaming {
    spirc: Spirc,
    device_name: String,
    tap: Arc<SpectrumTap>,
    /// The protocol loop. Finishing is how a dropped session announces
    /// itself: librespot logs the disconnection and the task returns, and
    /// if nobody is holding this the device simply vanishes while the
    /// process carries on looking healthy.
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Streaming {
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Shared handle to the audio tap, so the daemon can serve spectrum
    /// requests without holding the whole Streaming value.
    pub fn tap(&self) -> Arc<SpectrumTap> {
        Arc::clone(&self.tap)
    }

    pub fn shutdown(&self) {
        if let Err(e) = self.spirc.shutdown() {
            tracing::warn!("librespot shutdown failed: {e}");
        }
    }

    /// Waits for the Connect session to end.
    ///
    /// Returns as soon as librespot stops driving the protocol, whether
    /// that was a clean shutdown or the server closing the connection.
    /// Callers cannot tell the two apart from here, and should not need
    /// to: either way there is no device any more.
    pub async fn ended(&mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

fn bitrate_from(kbps: u16) -> Result<Bitrate> {
    Ok(match kbps {
        96 => Bitrate::Bitrate96,
        160 => Bitrate::Bitrate160,
        320 => Bitrate::Bitrate320,
        other => bail!("bitrate must be 96, 160 or 320, got {other}"),
    })
}

/// Builds the session, player and Connect state machine, and spawns the task
/// that drives them. Returns once the device is registered.
pub async fn start(config: &Config) -> Result<Streaming> {
    let settings = &config.streaming;

    let cache = cache(config)?;
    let credentials = cache.credentials().context(
        "no streaming credentials cached. Run `boombox auth login --streaming` once; \
         the Connect protocol needs its own session and cannot reuse the Web API token",
    )?;

    // Deliberately NOT our Web API client_id -- see login() for why.
    let session = Session::new(SessionConfig::default(), Some(cache));

    let player_config = PlayerConfig {
        bitrate: bitrate_from(settings.bitrate)?,
        normalisation: settings.normalisation,
        ..PlayerConfig::default()
    };

    let backend =
        audio_backend::find(settings.backend.clone()).with_context(|| match &settings.backend {
            Some(name) => format!("no audio backend named `{name}` in this build"),
            None => "this build has no audio backend compiled in".to_string(),
        })?;

    let mixer_builder = mixer::find(None).context("no mixer available in this build")?;
    let mixer = mixer_builder(MixerConfig::default())
        .map_err(|e| anyhow::anyhow!("could not open the mixer: {e}"))?;

    let tap = Arc::new(SpectrumTap::new(SAMPLE_RATE));
    let sink_tap = Arc::clone(&tap);
    let player = Player::new(player_config, session.clone(), mixer.get_soft_volume(), move || {
        Box::new(TappedSink { inner: backend(None, AudioFormat::default()), tap: sink_tap })
    });

    let name = device_name(config);
    let connect_config = ConnectConfig {
        name: name.clone(),
        // Computer rather than Speaker: this is a machine you sit at, and the
        // icon in the picker should say so.
        device_type: DeviceType::Computer,
        initial_volume: settings.initial_volume_u16(),
        ..ConnectConfig::default()
    };

    let (spirc, spirc_task) =
        Spirc::new(connect_config, session, credentials, player, Arc::clone(&mixer))
            .await
            .map_err(|e| anyhow::anyhow!("could not register as a Connect device: {e}"))?;

    // The task owns the protocol loop; without it the device appears and then
    // goes silent. The handle is kept so its ending can be noticed.
    let task = tokio::spawn(spirc_task);

    Ok(Streaming { spirc, device_name: name, tap, task: Some(task) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two machines with one name each, rather than two rows both called
    /// "boombox".
    #[test]
    fn the_default_name_carries_the_machine() {
        assert_eq!(default_device_name(Some("studio".into())), "boombox on studio");
    }

    /// mDNS and DHCP hand out names with a domain attached; the machine is
    /// the part worth showing.
    #[test]
    fn the_domain_is_dropped_from_the_machine_name() {
        assert_eq!(default_device_name(Some("studio.local".into())), "boombox on studio");
        assert_eq!(default_device_name(Some("studio.lan.example".into())), "boombox on studio");
    }

    /// A machine that will not say must still get a usable name.
    #[test]
    fn a_nameless_machine_falls_back_to_the_bare_name() {
        assert_eq!(default_device_name(None), "boombox");
        assert_eq!(default_device_name(Some(String::new())), "boombox");
        assert_eq!(default_device_name(Some("   ".into())), "boombox");
        assert_eq!(default_device_name(Some(".".into())), "boombox");
    }

    /// A name in the config is a decision, not a suggestion.
    #[test]
    fn a_configured_name_is_used_exactly_as_written() {
        let mut config = Config::default();
        config.streaming.device_name = Some("Kitchen".into());
        assert_eq!(device_name(&config), "Kitchen");
    }

    /// The name is per machine, so it must not be baked into the config file
    /// that setup writes -- one copied to another computer would carry it.
    #[test]
    fn nothing_is_configured_by_default() {
        assert_eq!(Config::default().streaming.device_name, None);
    }

    #[test]
    fn bitrates_map_to_the_three_spotify_offers() {
        assert!(matches!(bitrate_from(96).unwrap(), Bitrate::Bitrate96));
        assert!(matches!(bitrate_from(160).unwrap(), Bitrate::Bitrate160));
        assert!(matches!(bitrate_from(320).unwrap(), Bitrate::Bitrate320));
    }

    #[test]
    fn an_unsupported_bitrate_is_rejected_rather_than_rounded() {
        let err = bitrate_from(256).unwrap_err().to_string();
        assert!(err.contains("96, 160 or 320"), "{err}");
    }
}
