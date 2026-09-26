use std::io::IsTerminal as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use boombox_core::api::player::{DeviceMatch, PlayOptions, resolve_device};
use boombox_core::api::{Device, PlaybackState, PlayerApi, PlayingItem, RepeatState};
use boombox_core::{Client, Config, Error};
use clap::{Args, Subcommand};

use crate::fmt;

/// Spotify needs a moment to settle after a transport command before
/// `GET /me/player` reflects it.
const SETTLE: Duration = Duration::from_millis(400);

/// A device transfer takes noticeably longer to land than a skip does.
const TRANSFER_POLL: Duration = Duration::from_millis(300);
const TRANSFER_ATTEMPTS: usize = 10;

#[derive(Args)]
pub struct NowArgs {
    /// Emit the full player state as JSON
    #[arg(long)]
    pub json: bool,

    /// Template with {title} {artist} {album} {position} {duration} {remaining}
    /// {pct} {bar} {bar:N} {status} {state} {device} {volume} {shuffle} {repeat} {uri}
    #[arg(long, short)]
    pub format: Option<String>,
}

#[derive(Subcommand)]
pub enum QueueCommand {
    /// Show what is playing next
    List {
        #[arg(long)]
        json: bool,
        /// How many upcoming items to show
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Append a track or episode URI to the queue
    Add { uri: String },
}

pub enum PlayerCommand {
    Next,
    Previous,
    Pause,
    Toggle,
    Play { uri: Option<String> },
    Now(NowArgs),
    Seek { position: String },
    Volume { level: Option<String> },
    Shuffle { mode: Option<String> },
    Repeat { mode: Option<String> },
    Devices { json: bool },
    Connect { name: String },
    Queue(QueueCommand),
}

pub async fn run(cmd: PlayerCommand, direct: bool) -> Result<()> {
    let config = Config::load()?;

    // A running daemon answers `now` from cache without touching the network.
    // Everything still works identically when there isn't one -- that fallback
    // is the whole reason the transport sits behind a trait.
    if !direct && let Some(ipc) = crate::daemon::try_connect(&config).await {
        tracing::debug!("using the daemon");
        return dispatch(&ipc, cmd).await;
    }

    let auth = Arc::new(boombox_core::Auth::from_config(&config)?);
    let client = Client::new(auth);
    dispatch(&client, cmd).await
}

async fn dispatch<P: PlayerApi>(client: &P, cmd: PlayerCommand) -> Result<()> {
    match cmd {
        PlayerCommand::Next => {
            client.next().await?;
            report_track(client).await
        }
        PlayerCommand::Previous => {
            client.previous().await?;
            report_track(client).await
        }
        PlayerCommand::Pause => {
            client.pause().await?;
            say("\u{23f8}  paused");
            Ok(())
        }
        PlayerCommand::Toggle => {
            let state = require_state(client).await?;
            if state.is_playing {
                client.pause().await?;
                say("\u{23f8}  paused");
            } else {
                client.play(PlayOptions::resume()).await?;
                say(&format!("\u{25b6}  {}", headline(state.item.as_ref())));
            }
            Ok(())
        }
        PlayerCommand::Play { uri } => {
            let opts = match uri.as_deref() {
                None => {
                    // Resume needs something loaded. When a queue runs out the
                    // player is left active but empty, and Spotify accepts a
                    // resume for it and does nothing -- which is silent both
                    // from the API and from here.
                    let state = require_state(client).await?;
                    if state.item.is_none() {
                        bail!(
                            "nothing to resume: the player is empty, which is what \
                             happens when a queue runs out.\n       \
                             Start something, e.g. `boombox play spotify:album:...`"
                        );
                    }
                    PlayOptions::resume()
                }
                Some(u) => play_options_for(u)?,
            };
            client.play(opts).await?;
            report_track(client).await
        }
        PlayerCommand::Now(args) => now(client, args).await,
        PlayerCommand::Seek { position } => {
            let state = require_state(client).await?;
            let target = fmt::parse_position(&position, state.progress(), state.duration())?;
            client.seek(target).await?;
            say(&format!("\u{2192}  {} / {}", fmt::clock(target), fmt::clock(state.duration())));
            Ok(())
        }
        PlayerCommand::Volume { level } => volume(client, level).await,
        PlayerCommand::Shuffle { mode } => shuffle(client, mode).await,
        PlayerCommand::Repeat { mode } => repeat(client, mode).await,
        PlayerCommand::Devices { json } => devices(client, json).await,
        PlayerCommand::Connect { name } => connect(client, &name).await,
        PlayerCommand::Queue(q) => queue(client, q).await,
    }
}

async fn now<P: PlayerApi>(client: &P, args: NowArgs) -> Result<()> {
    let Some(state) = client.playback_state().await? else {
        if args.json {
            println!("{}", serde_json::json!({"playing": false, "device": null}));
            return Ok(());
        }
        return Err(Error::NoActiveDevice.into());
    };

    if args.json {
        println!("{}", serde_json::to_string(&as_json(&state))?);
    } else if let Some(template) = args.format {
        println!("{}", fmt::render_template(&template, &state));
    } else {
        println!("{}", fmt::now_line(&state));
    }
    Ok(())
}

async fn volume<P: PlayerApi>(client: &P, level: Option<String>) -> Result<()> {
    let state = require_state(client).await?;
    let current = state.volume().unwrap_or(0);

    let Some(level) = level else {
        println!("{current}%");
        return Ok(());
    };

    if let Some(device) = &state.device
        && !device.supports_volume
    {
        bail!("{} does not support volume control", device.name);
    }

    let target = fmt::parse_volume(&level, current)?;
    client.set_volume(target).await?;
    say(&format!("\u{266a}  {target}%"));
    Ok(())
}

async fn shuffle<P: PlayerApi>(client: &P, mode: Option<String>) -> Result<()> {
    let state = require_state(client).await?;
    let target = match mode.as_deref() {
        None | Some("toggle") => !state.shuffle_state,
        Some("on" | "true" | "yes") => true,
        Some("off" | "false" | "no") => false,
        Some(other) => bail!("expected on, off or toggle, got `{other}`"),
    };
    client.set_shuffle(target).await?;
    say(&format!("\u{21c4}  shuffle {}", if target { "on" } else { "off" }));
    Ok(())
}

async fn repeat<P: PlayerApi>(client: &P, mode: Option<String>) -> Result<()> {
    let state = require_state(client).await?;
    let target = match mode.as_deref() {
        None | Some("toggle" | "cycle") => state.repeat_state.next(),
        Some(other) => other.parse::<RepeatState>().map_err(|e| anyhow::anyhow!(e))?,
    };
    client.set_repeat(target).await?;
    say(&format!("\u{21bb}  repeat {target}"));
    Ok(())
}

async fn devices<P: PlayerApi>(client: &P, json: bool) -> Result<()> {
    let devices = client.devices().await?;

    if json {
        println!("{}", serde_json::to_string(&devices)?);
        return Ok(());
    }
    if devices.is_empty() {
        eprintln!("boombox: no devices available. Open Spotify somewhere first.");
        return Err(Error::NoActiveDevice.into());
    }

    let width = devices.iter().map(|d| d.name.chars().count()).max().unwrap_or(0);
    for d in &devices {
        println!(
            "{} {:width$}  {:10} {}",
            if d.is_active { "\u{25cf}" } else { "\u{25cb}" },
            d.name,
            d.device_type,
            volume_cell(d),
            width = width,
        );
    }
    Ok(())
}

fn volume_cell(d: &Device) -> String {
    match (d.supports_volume, d.volume_percent) {
        (true, Some(v)) => format!("{v}%"),
        _ => "\u{2014}".into(),
    }
}

async fn connect<P: PlayerApi>(client: &P, name: &str) -> Result<()> {
    let devices = client.devices().await?;
    match resolve_device(&devices, name) {
        DeviceMatch::One(device) => {
            let Some(id) = &device.id else {
                bail!("{} cannot be targeted: Spotify reported no device id", device.name);
            };
            // Preserve whatever was happening rather than forcing playback on.
            let was_playing = client.playback_state().await?.map(|s| s.is_playing).unwrap_or(false);
            client.transfer(id, was_playing).await?;

            // Spotify answers 204 the moment it accepts the request, well
            // before the target device picks it up, so confirm rather than
            // claim. Naming the wrong device is worse than a short wait.
            match await_takeover(client, id, was_playing).await {
                Takeover::Playing(name) | Takeover::Idle(name) => say(&format!("\u{25b8}  {name}")),
                Takeover::Stalled(name) => {
                    // Reported on stderr and not as a failure: the transfer
                    // itself did happen. But saying nothing here is how this
                    // ends up looking like boombox silently doing nothing.
                    eprintln!(
                        "boombox: playback moved to {name}, but it could not load the \
                         current context and is silent.\n      \
                         Start something explicitly, e.g. `boombox play <uri>`."
                    );
                }
                Takeover::NotTaken => eprintln!(
                    "boombox: {} accepted the transfer but has not taken over yet",
                    device.name
                ),
            }
            Ok(())
        }
        DeviceMatch::None => {
            let available: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
            if available.is_empty() {
                bail!("no devices available. Open Spotify somewhere first.");
            }
            bail!("no device matching `{name}`. Available: {}", available.join(", "));
        }
        DeviceMatch::Ambiguous(names) => {
            bail!("`{name}` matches {} devices: {}", names.len(), names.join(", "));
        }
    }
}

async fn queue<P: PlayerApi>(client: &P, cmd: QueueCommand) -> Result<()> {
    match cmd {
        QueueCommand::Add { uri } => {
            let uri = boombox_core::uri::normalise(&uri).ok_or_else(|| {
                anyhow::anyhow!(
                    "expected a Spotify URI or share link, got `{uri}`\n\
                     e.g. spotify:track:... or https://open.spotify.com/track/..."
                )
            })?;
            client.add_to_queue(&uri).await?;
            say("\u{002b}  queued");
            Ok(())
        }
        QueueCommand::List { json, limit } => {
            let q = client.queue().await?;
            if json {
                let items: Vec<_> = q.queue.iter().take(limit).map(item_json).collect();
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "currently_playing": q.currently_playing.as_ref().map(item_json),
                        "queue": items,
                    }))?
                );
                return Ok(());
            }
            if let Some(current) = &q.currently_playing {
                println!("\u{25b6}  {}", headline(Some(current)));
            }
            if q.queue.is_empty() {
                println!("   queue is empty");
            }
            for (i, item) in q.queue.iter().take(limit).enumerate() {
                println!("{:>2}  {}", i + 1, headline(Some(item)));
            }
            Ok(())
        }
    }
}

/// `spotify:` URIs for albums, artists and playlists are contexts; tracks and
/// episodes are played directly.
fn play_options_for(uri: &str) -> Result<PlayOptions> {
    // A share link is what the Spotify apps put on the clipboard, and for
    // a playlist the API will not let us list -- a Daily Mix, say -- it is
    // the only way to name the thing at all.
    let uri = boombox_core::uri::normalise(uri).ok_or_else(|| {
        anyhow::anyhow!(
            "expected a Spotify URI or share link, got `{uri}`\n\
             e.g. spotify:album:... or https://open.spotify.com/album/..."
        )
    })?;
    let kind = uri.split(':').nth(1).unwrap_or_default();
    Ok(match kind {
        "album" | "artist" | "playlist" | "show" => PlayOptions::context(uri),
        "track" | "episode" => PlayOptions::tracks(vec![uri]),
        other => bail!("don't know how to play a `{other}` URI"),
    })
}

async fn require_state<P: PlayerApi>(client: &P) -> Result<PlaybackState> {
    client.playback_state().await?.ok_or_else(|| Error::NoActiveDevice.into())
}

/// Re-reads the state after a transport command so the user sees what they got.
/// Skipped when stdout is not a terminal, which keeps keybindings instant.
async fn report_track<P: PlayerApi>(client: &P) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return Ok(());
    }
    tokio::time::sleep(SETTLE).await;
    match client.playback_state().await {
        Ok(Some(state)) => println!("\u{266a}  {}", headline(state.item.as_ref())),
        // The command already succeeded; a failed confirmation read is not
        // worth failing the whole invocation over.
        Ok(None) => {}
        Err(e) => tracing::debug!("could not read back playback state: {e}"),
    }
    Ok(())
}

/// Polls until `device_id` is the active device, returning its name. `None`
/// means Spotify accepted the transfer but the device never took over.
/// How a transfer turned out.
#[derive(Debug, PartialEq)]
enum Takeover {
    /// The device took over and audio is running.
    Playing(String),
    /// The device took over and is idle, which is correct if it was not
    /// playing to begin with.
    Idle(String),
    /// The device took over but never loaded a track, despite being asked to
    /// play. This is what a context librespot cannot resolve looks like from
    /// the outside: everything reports success and nothing makes a sound.
    Stalled(String),
    /// Spotify accepted the request but the device never became active.
    NotTaken,
}

/// Waits for `device_id` to become the active device, then — if playback was
/// meant to resume — for it to actually load a track.
async fn await_takeover<P: PlayerApi>(
    client: &P,
    device_id: &str,
    expect_playing: bool,
) -> Takeover {
    let mut name = None;

    for _ in 0..TRANSFER_ATTEMPTS {
        tokio::time::sleep(TRANSFER_POLL).await;
        let state = match client.playback_state().await {
            Ok(Some(state)) => state,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("could not confirm transfer: {e}");
                return Takeover::NotTaken;
            }
        };

        let active = state.device.as_ref().filter(|d| d.id.as_deref() == Some(device_id));
        let Some(active) = active else {
            continue;
        };
        name = Some(active.name.clone());

        if !expect_playing {
            return Takeover::Idle(active.name.clone());
        }
        // An item with a name is the signal that the device resolved whatever
        // it was handed and has something queued up.
        if state.is_playing && state.item.is_some() {
            return Takeover::Playing(active.name.clone());
        }
    }

    match name {
        // Active for the whole wait but never started: it has nothing to play.
        Some(name) if expect_playing => Takeover::Stalled(name),
        Some(name) => Takeover::Idle(name),
        None => Takeover::NotTaken,
    }
}

fn headline(item: Option<&PlayingItem>) -> String {
    let Some(item) = item else { return "nothing playing".into() };
    let byline = item.byline();
    if byline.is_empty() {
        item.name().to_string()
    } else {
        format!("{} \u{b7} {}", item.name(), byline)
    }
}

fn say(message: &str) {
    if std::io::stdout().is_terminal() {
        println!("{message}");
    }
}

fn item_json(item: &PlayingItem) -> serde_json::Value {
    serde_json::json!({
        "name": item.name(),
        "artist": item.byline(),
        "album": item.collection(),
        "uri": item.uri(),
        "duration_ms": item.duration_ms(),
    })
}

fn as_json(state: &PlaybackState) -> serde_json::Value {
    serde_json::json!({
        "playing": state.is_playing,
        "track": state.item.as_ref().map(item_json),
        "progress_ms": state.progress(),
        "duration_ms": state.duration(),
        "device": state.device.as_ref().map(|d| serde_json::json!({
            "id": d.id,
            "name": d.name,
            "type": d.device_type,
            "volume": d.volume_percent,
        })),
        "shuffle": state.shuffle_state,
        "repeat": state.repeat_state,
        "context": state.context.as_ref().map(|c| c.uri.clone()),
        // True while a change of ours has not come back from Spotify yet,
        // so a script can tell "what was asked for" from "what is so".
        "pending": state.pending.any(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays a scripted sequence of playback states, so the transfer
    /// confirmation can be tested without a network or a device.
    struct ScriptedPlayer {
        states: std::sync::Mutex<std::collections::VecDeque<Option<PlaybackState>>>,
        last: std::sync::Mutex<Option<PlaybackState>>,
    }

    impl ScriptedPlayer {
        fn new(states: Vec<Option<PlaybackState>>) -> Self {
            Self { states: std::sync::Mutex::new(states.into()), last: std::sync::Mutex::new(None) }
        }
    }

    /// Only `playback_state` is exercised; the rest exist to satisfy the trait.
    impl PlayerApi for ScriptedPlayer {
        async fn playback_state(&self) -> boombox_core::Result<Option<PlaybackState>> {
            let next = self.states.lock().unwrap().pop_front();
            match next {
                // Past the end of the script, hold the final state.
                None => Ok(self.last.lock().unwrap().clone()),
                Some(state) => {
                    *self.last.lock().unwrap() = state.clone();
                    Ok(state)
                }
            }
        }
        async fn devices(&self) -> boombox_core::Result<Vec<Device>> {
            Ok(Vec::new())
        }
        async fn play(&self, _: PlayOptions) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn pause(&self) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn next(&self) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn previous(&self) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn seek(&self, _: u64) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn set_volume(&self, _: u32) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn set_shuffle(&self, _: bool) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn set_repeat(&self, _: RepeatState) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn transfer(&self, _: &str, _: bool) -> boombox_core::Result<()> {
            Ok(())
        }
        async fn queue(&self) -> boombox_core::Result<boombox_core::api::Queue> {
            Ok(serde_json::from_str("{}").unwrap())
        }
        async fn add_to_queue(&self, _: &str) -> boombox_core::Result<()> {
            Ok(())
        }
    }

    /// A state on device `d1`, optionally playing, optionally with a track.
    fn on_target(playing: bool, with_item: bool) -> Option<PlaybackState> {
        let item = if with_item {
            r#","item":{"type":"track","name":"x","uri":"spotify:track:x",
                 "duration_ms":1000,"artists":[],"album":{}}"#
        } else {
            ""
        };
        Some(
            serde_json::from_str(&format!(
                r#"{{"is_playing":{playing},"progress_ms":0,
                    "device":{{"id":"d1","name":"boombox","type":"Computer",
                              "volume_percent":50,"supports_volume":true}}{item}}}"#
            ))
            .unwrap(),
        )
    }

    fn elsewhere() -> Option<PlaybackState> {
        Some(
            serde_json::from_str(
                r#"{"is_playing":true,"progress_ms":0,
                    "device":{"id":"other","name":"Phone","type":"Smartphone",
                              "volume_percent":50,"supports_volume":true}}"#,
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn resuming_an_empty_player_says_so_instead_of_doing_nothing() {
        // An active device with no item: the state a finished queue leaves
        // behind. Spotify accepts a resume for it and nothing happens.
        let player = ScriptedPlayer::new(vec![on_target(false, false)]);
        let err = dispatch(&player, PlayerCommand::Play { uri: None }).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("nothing to resume"), "{text}");
        assert!(text.contains("boombox play"), "should say what to do instead: {text}");
    }

    #[tokio::test]
    async fn resuming_a_paused_track_is_allowed() {
        let player = ScriptedPlayer::new(vec![on_target(false, true)]);
        assert!(dispatch(&player, PlayerCommand::Play { uri: None }).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_that_starts_playing_is_reported_as_playing() {
        let player =
            ScriptedPlayer::new(vec![elsewhere(), on_target(false, false), on_target(true, true)]);
        assert_eq!(await_takeover(&player, "d1", true).await, Takeover::Playing("boombox".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_that_never_loads_a_track_is_reported_as_stalled() {
        // This is the shape of the bug: active throughout, never plays. The
        // Web API reports the transfer succeeded and nothing makes a sound.
        let player = ScriptedPlayer::new(vec![on_target(false, false)]);
        assert_eq!(await_takeover(&player, "d1", true).await, Takeover::Stalled("boombox".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn transferring_while_paused_is_idle_not_stalled() {
        // Nothing was playing, so silence afterwards is correct.
        let player = ScriptedPlayer::new(vec![on_target(false, false)]);
        assert_eq!(await_takeover(&player, "d1", false).await, Takeover::Idle("boombox".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_that_never_becomes_active_is_not_taken() {
        let player = ScriptedPlayer::new(vec![elsewhere()]);
        assert_eq!(await_takeover(&player, "d1", true).await, Takeover::NotTaken);
    }

    #[tokio::test(start_paused = true)]
    async fn playing_without_a_track_still_counts_as_stalled() {
        // is_playing true but no item is the other face of the same failure:
        // it claims to be playing something it never resolved.
        let player = ScriptedPlayer::new(vec![on_target(true, false)]);
        assert_eq!(await_takeover(&player, "d1", true).await, Takeover::Stalled("boombox".into()));
    }

    #[test]
    fn albums_and_playlists_play_as_contexts() {
        let o = play_options_for("spotify:album:abc").unwrap();
        assert_eq!(o.context_uri.as_deref(), Some("spotify:album:abc"));
        assert!(o.uris.is_empty());

        let o = play_options_for("spotify:playlist:xyz").unwrap();
        assert_eq!(o.context_uri.as_deref(), Some("spotify:playlist:xyz"));
    }

    #[test]
    fn tracks_play_as_explicit_uris() {
        let o = play_options_for("spotify:track:abc").unwrap();
        assert!(o.context_uri.is_none());
        assert_eq!(o.uris, vec!["spotify:track:abc"]);
    }

    #[test]
    fn non_uris_and_unknown_kinds_are_rejected() {
        assert!(play_options_for("lowtide").is_err());
        assert!(play_options_for("https://example.com/album/x").is_err());
        assert!(play_options_for("spotify:user:someone").is_err());
    }

    /// A share link is what the Spotify apps copy, and for a playlist the
    /// API will not let us list it is the only way to name the thing.
    #[test]
    fn a_share_link_is_played_like_the_uri_it_stands_for() {
        let from_link =
            play_options_for("https://open.spotify.com/playlist/37i9dQZF1EExampleMix02?si=x")
                .unwrap();
        let from_uri = play_options_for("spotify:playlist:37i9dQZF1EExampleMix02").unwrap();
        assert_eq!(from_link.context_uri, from_uri.context_uri);
        assert_eq!(
            from_link.context_uri.as_deref(),
            Some("spotify:playlist:37i9dQZF1EExampleMix02")
        );
    }

    #[test]
    fn a_track_share_link_plays_as_a_track_not_a_context() {
        let opts =
            play_options_for("https://open.spotify.com/track/ExampleTrack0000000001").unwrap();
        assert!(opts.context_uri.is_none());
        assert_eq!(opts.uris, vec!["spotify:track:ExampleTrack0000000001"]);
    }

    #[test]
    fn headline_falls_back_when_there_is_no_item() {
        assert_eq!(headline(None), "nothing playing");
    }
}
