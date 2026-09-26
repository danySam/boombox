use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use boombox_core::api::library::LibraryApi;
use boombox_core::api::player::PlayerApi;
use boombox_core::api::{Pendings, PlaybackState};
use boombox_core::intent::Pending;
use boombox_core::{Client, Config, Error};
use boombox_ipc::protocol::{PROTOCOL_VERSION, StreamingState};
use boombox_ipc::{DaemonStatus, IpcClient, Request, Response, WireError};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, RwLock, watch};

/// Ceiling for the exponential backoff applied after a rate limit.
const MAX_BACKOFF: Duration = Duration::from_secs(120);

/// Resolution of the per-track waveform. Wider than any terminal, so the seek
/// bar can be resampled down to whatever the pane happens to be.
#[cfg(feature = "streaming")]
const ENVELOPE_BUCKETS: usize = 600;

/// How often the envelope samples the audio peak. Ten times a second fills
/// every bucket of a three-minute track with room to spare, and costs a mutex
/// lock and a float compare.
#[cfg(feature = "streaming")]
const ENVELOPE_INTERVAL: Duration = Duration::from_millis(100);

/// How long a written value is shown in place of the reported one, when
/// the player never comes to agree with it.
///
/// Measured against the real thing: a volume change takes about eleven
/// seconds to appear in the API, so anything shorter makes the figure snap
/// back to a stale reading and then jump forward again. Pausing and
/// seeking land within a poll or two, so they give up sooner.
const VOLUME_SETTLE: Duration = Duration::from_secs(20);
const POSITION_SETTLE: Duration = Duration::from_secs(10);
const PLAYING_SETTLE: Duration = Duration::from_secs(8);

/// Devices round a volume -- ask for 55 and it reads back 54 -- so an
/// exact match would never arrive.
const VOLUME_TOLERANCE: u32 = 2;

/// A position is never reported exactly: the track has played on between
/// the write landing and the poll observing it.
const POSITION_TOLERANCE_MS: u64 = 2_500;

/// How often to resolve one outstanding context name. Slow on purpose:
/// nothing is waiting on it, and the list settles within a minute.
const NAME_INTERVAL: Duration = Duration::from_secs(2);

/// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux, including
/// the trailing NUL. Binding past it fails with a bare EINVAL that says
/// nothing about length, so check it ourselves and explain.
#[cfg(target_os = "macos")]
const MAX_SOCKET_PATH: usize = 103;
#[cfg(not(target_os = "macos"))]
const MAX_SOCKET_PATH: usize = 107;

pub struct Daemon {
    client: Arc<Client>,
    cache: Arc<RwLock<Cache>>,
    /// Rung after a write so the poller refreshes immediately instead of
    /// serving state the user's own command just invalidated.
    nudge: Arc<Notify>,
    stats: Arc<Stats>,
    /// Set once the Connect device is up; absent in non-streaming builds.
    #[cfg(feature = "streaming")]
    /// Swapped rather than set once: a dropped Connect session is
    /// reconnected with a fresh tap, and a `OnceLock` would leave the
    /// daemon serving spectrum data from the session that died.
    spectrum: Arc<RwLock<Option<Arc<crate::spectrum::SpectrumTap>>>>,
    /// Whether a Connect device is registered right now, and if not, why.
    /// The daemon can be perfectly healthy while there is none.
    streaming_state: Arc<RwLock<StreamingState>>,
    #[cfg(feature = "streaming")]
    envelope: Arc<RwLock<crate::spectrum::Envelope>>,
    active_interval: Duration,
    idle_interval: Duration,
    started: Instant,
    /// Contexts seen playing, so the app can offer them again. Spotify
    /// does not expose its own playlists to third parties -- watching what
    /// goes past is the only way to learn they exist.
    recents: crate::recents::Store,
    /// Spotify saying this device is playing while no audio reaches it.
    #[cfg(feature = "streaming")]
    stall: Arc<RwLock<crate::stall::StallWatch>>,
    /// The name this machine's Connect device registers under. For saying
    /// which device a log line is about, and nothing else.
    #[cfg(feature = "streaming")]
    device_name: String,
    /// The id Spotify knows our Connect device by, while one is registered.
    /// What "playing here" and adopting are decided on, because a name can
    /// belong to two machines and this cannot.
    #[cfg(feature = "streaming")]
    device_id: Arc<RwLock<Option<String>>>,
}

#[derive(Default)]
struct Cache {
    playback: Option<PlaybackState>,
    fetched_at: Option<Instant>,
    /// Kept beside the state it overrides, under the same lock, so a read
    /// can never catch one without the other.
    intents: Intents,
}

/// What has been asked for and not yet confirmed.
///
/// Spotify reports a write seconds after applying it, and the poll the
/// daemon fires straight after writing is the one most likely to still
/// carry the old value. Without this, every command the user gives is
/// answered with the state it was meant to change.
#[derive(Default)]
struct Intents {
    volume: Option<Pending<u32>>,
    position: Option<Pending<u64>>,
    playing: Option<Pending<bool>>,
}

fn volume_agrees(wanted: u32, seen: u32) -> bool {
    wanted.abs_diff(seen) <= VOLUME_TOLERANCE
}

impl Intents {
    /// Drops whatever the player has now agreed with, or waited out.
    fn settle_against(&mut self, observed: Option<&PlaybackState>, now: Instant) {
        let playing_now = observed.is_some_and(|s| s.is_playing);
        self.volume =
            self.volume.filter(|p| p.holds(observed.and_then(|s| s.volume()), volume_agrees, now));
        self.position = self.position.filter(|p| {
            // Compared against where the asked-for position has travelled
            // to, not where it started: a track seeked two seconds ago is
            // two seconds further on, and that is agreement, not drift.
            let projected = projected_position(p, playing_now, now);
            p.holds(
                observed.and_then(|s| s.progress_ms),
                |_, seen| projected.abs_diff(seen) <= POSITION_TOLERANCE_MS,
                now,
            )
        });
        self.playing =
            self.playing.filter(|p| p.holds(observed.map(|s| s.is_playing), |a, b| a == b, now));
    }
}

/// Replaces an outstanding value, or starts holding one.
fn renew<T: Copy>(slot: &mut Option<Pending<T>>, wanted: T, settle: Duration, now: Instant) {
    match slot {
        Some(pending) => pending.renew(wanted, now),
        None => *slot = Some(Pending::at(wanted, settle, now)),
    }
}

/// Where a position asked for at some point in the past has reached by now.
fn projected_position(pending: &Pending<u64>, playing: bool, now: Instant) -> u64 {
    let travelled = if playing { pending.age(now).as_millis() as u64 } else { 0 };
    pending.wanted().saturating_add(travelled)
}

/// What a write means for the state being held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrote {
    Volume(u32),
    Position(u64),
    Playing(bool),
    /// A different track: any position asked for belongs to the one being
    /// left behind.
    Track,
}

fn wrote(request: &Request) -> Option<Wrote> {
    match request {
        Request::SetVolume { percent } => Some(Wrote::Volume(*percent)),
        Request::Seek { position_ms } => Some(Wrote::Position(*position_ms)),
        Request::Pause => Some(Wrote::Playing(false)),
        Request::Play(_) => Some(Wrote::Playing(true)),
        Request::Next | Request::Previous => Some(Wrote::Track),
        _ => None,
    }
}

/// The cached state with anything outstanding shown in its place.
fn overlay(
    polled: &PlaybackState,
    intents: &Intents,
    since_poll: Duration,
    now: Instant,
) -> PlaybackState {
    let mut state = polled.clone();
    let mut pending = Pendings::default();

    // First, because it decides whether the clock below runs at all.
    if let Some(p) = &intents.playing
        && p.holds(Some(polled.is_playing), |a, b| a == b, now)
    {
        state.is_playing = p.wanted();
        pending.playing = true;
    }

    match &intents.position {
        Some(p)
            if p.holds(
                polled.progress_ms,
                |_, seen| {
                    projected_position(p, state.is_playing, now).abs_diff(seen)
                        <= POSITION_TOLERANCE_MS
                },
                now,
            ) =>
        {
            // Advanced from when it was asked for, not from the last poll:
            // the seek happened after that poll, not before it.
            let target = projected_position(p, state.is_playing, now);
            let duration = state.duration();
            state.progress_ms = Some(if duration > 0 { target.min(duration) } else { target });
            pending.position = true;
        }
        // Nothing outstanding: the ordinary smooth clock between polls.
        _ => state = state.advanced_by(since_poll),
    }

    if let Some(p) = &intents.volume
        && p.holds(polled.volume(), volume_agrees, now)
        && let Some(device) = state.device.as_mut()
    {
        device.volume_percent = Some(p.wanted());
        pending.volume = true;
    }

    state.pending = pending;
    state
}

#[derive(Default)]
struct Stats {
    requests: AtomicU64,
    api_calls: AtomicU64,
    polling_millis: AtomicU64,
}

/// Removes the socket file on the way out, including on a panic. A leftover
/// file makes the next start look like a daemon is already running.
struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn run(config: &Config) -> Result<()> {
    // First, before anything that can block. Reading the keychain can stall
    // indefinitely waiting on a dialog, and a daemon hung there is exactly
    // when you most want to know which build it is.
    println!("boombox daemon {} (protocol v{PROTOCOL_VERSION})", boombox_core::build_info::long());
    tracing::info!(
        version = boombox_core::build_info::long(),
        protocol = PROTOCOL_VERSION,
        "daemon starting"
    );

    let path = config.socket_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    check_socket_path(&path)?;
    ensure_not_already_running(&path).await?;

    let auth = Arc::new(boombox_core::Auth::from_config(config)?);
    let client = Arc::new(Client::new(Arc::clone(&auth)));

    let listener =
        UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    let _guard = SocketGuard(path.clone());
    // Private to its owner. It normally sits in the state directory, which is
    // private already, but the path is configurable -- and the advice for an
    // over-long one is to move it to /tmp.
    if let Err(e) = boombox_core::private::restrict(&path) {
        tracing::warn!("could not restrict {}: {e}", path.display());
    }

    let daemon = Arc::new(Daemon {
        client,
        cache: Arc::new(RwLock::new(Cache::default())),
        nudge: Arc::new(Notify::new()),
        stats: Arc::new(Stats::default()),
        #[cfg(feature = "streaming")]
        spectrum: Arc::new(RwLock::new(None)),
        streaming_state: Arc::new(RwLock::new(initial_streaming_state())),
        #[cfg(feature = "streaming")]
        envelope: Arc::new(RwLock::new(crate::spectrum::Envelope::new(ENVELOPE_BUCKETS))),
        active_interval: Duration::from_millis(config.daemon.poll_active_ms),
        idle_interval: Duration::from_millis(config.daemon.poll_idle_ms),
        started: Instant::now(),
        recents: crate::recents::Store::load(),
        #[cfg(feature = "streaming")]
        stall: Arc::new(RwLock::new(crate::stall::StallWatch::default())),
        #[cfg(feature = "streaming")]
        device_name: crate::streaming::device_name(config),
        #[cfg(feature = "streaming")]
        device_id: Arc::new(RwLock::new(None)),
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    println!("listening on {}", path.display());
    tracing::info!(socket = %path.display(), "daemon listening");

    let accept = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        let shutdown_tx = shutdown_tx.clone();
        let mut shutdown = shutdown_rx.clone();
        async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _)) => {
                                let daemon = Arc::clone(&daemon);
                                let shutdown_tx = shutdown_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = daemon.serve(stream, &shutdown_tx).await {
                                        tracing::debug!("client connection ended: {e}");
                                    }
                                });
                            }
                            Err(e) => tracing::warn!("accept failed: {e}"),
                        }
                    }
                    _ = shutdown.changed() => break,
                }
            }
        }
    });

    // Serving starts before librespot, not just binding. Binding alone let
    // the kernel queue connections that nothing would answer for as long as
    // librespot took to come up -- a client could connect and then wait
    // seconds for a reply to a ping. Everything except audio works without
    // the streaming session, so there is no reason to make callers wait
    // for it.
    #[cfg(feature = "streaming")]
    let streaming = if let StreamingPlan::Idle(state) =
        streaming_plan(config.streaming.enabled, crate::streaming::authorized())
    {
        println!("{}", idle_streaming_note(&state));
        *daemon.streaming_state.write().await = state;
        None
    } else {
        // Supervised rather than started once. A Connect session can be
        // dropped by the server at any time -- an expired token, a network
        // blip, Spotify closing the websocket -- and when that happens
        // librespot's protocol task simply ends. The process stays up and
        // keeps answering the CLI, so the daemon looks perfectly healthy
        // while the device it exists to provide has gone.
        Some(tokio::spawn({
            let daemon = Arc::clone(&daemon);
            let config = config.clone();
            let mut shutdown = shutdown_rx.clone();
            async move {
                supervise_streaming(&daemon, &config, &mut shutdown).await;
            }
        }))
    };

    println!(
        "polling every {}s while playing, {}s when idle",
        daemon.active_interval.as_secs_f32(),
        daemon.idle_interval.as_secs_f32()
    );

    #[cfg(feature = "streaming")]
    let envelope_task = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        let mut shutdown = shutdown_rx.clone();
        async move {
            let mut ticker = tokio::time::interval(ENVELOPE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => daemon.sample_envelope().await,
                    _ = shutdown.changed() => break,
                }
            }
        }
    });

    // Names are resolved one at a time on a slow timer rather than in a
    // burst: a list restored from disk with forty unnamed contexts would
    // otherwise fire forty requests the moment the daemon starts.
    let namer = tokio::spawn({
        let recents = daemon.recents.clone();
        let client = Arc::clone(&daemon.client);
        let mut shutdown = shutdown_rx.clone();
        async move {
            // Off the startup path deliberately: a slow or failing seed
            // must not delay the socket becoming useful.
            tokio::select! {
                _ = recents.seed(&client) => {}
                // A daemon stopped during its first seconds should stop,
                // not sit waiting on a network call nobody needs.
                _ = shutdown.changed() => return,
            }
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(NAME_INTERVAL) => {
                        recents.resolve_one(&client).await;
                    }
                    _ = shutdown.changed() => break,
                }
            }
        }
    });

    let poller = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        let shutdown = shutdown_rx.clone();
        async move { daemon.poll_loop(shutdown).await }
    });

    // `daemon --stop` signals through the same channel the listeners watch, so
    // waiting only on a signal here would leave the process alive but deaf.
    let mut exit = shutdown_rx.clone();
    tokio::select! {
        _ = wait_for_signal() => {}
        _ = exit.changed() => {}
    }
    let _ = shutdown_tx.send(true);
    let _ = poller.await;
    let _ = namer.await;
    accept.abort();
    #[cfg(feature = "streaming")]
    let _ = envelope_task.await;

    // The supervisor deregisters before it returns, or Spotify leaves a
    // ghost device in the picker until it times out.
    #[cfg(feature = "streaming")]
    if let Some(task) = streaming {
        let _ = task.await;
    }

    println!("boombox daemon stopped");
    Ok(())
}

fn check_socket_path(path: &Path) -> Result<()> {
    let len = path.as_os_str().as_encoded_bytes().len();
    if len <= MAX_SOCKET_PATH {
        return Ok(());
    }
    bail!(
        "socket path is {len} bytes, but this platform allows at most \
         {MAX_SOCKET_PATH}:\n  {}\n\nSet a shorter path in your config:\n  \
         [daemon]\n  socket = \"/tmp/boombox.sock\"",
        path.display()
    )
}

/// A socket file left behind by a crashed daemon is indistinguishable from a
/// live one until you try to talk to it.
async fn ensure_not_already_running(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match probe(path).await {
        Probe::Answering(status) => {
            bail!("a boombox daemon is already running (pid {})", status.pid);
        }
        // Deleting the socket here would strand it: still running, still
        // the Connect device, unreachable -- and with this one beside it.
        Probe::Unresponsive => {
            let pid = end_unresponsive(path).await?;
            println!("ended a daemon (pid {pid}) that had stopped answering");
            Ok(())
        }
        Probe::Absent => {
            tracing::debug!("removing stale socket at {}", path.display());
            std::fs::remove_file(path)?;
            Ok(())
        }
    }
}

/// How long to wait before reconnecting a dropped Connect session, and the
/// ceiling that backoff climbs to.
///
/// Short at first because most drops are a blip and reconnect immediately;
/// backed off because a session that cannot be established -- expired
/// credentials, no network -- should not be retried in a tight loop for
/// however many days the daemon is up.
#[cfg(feature = "streaming")]
const RECONNECT_FIRST: Duration = Duration::from_secs(2);
#[cfg(feature = "streaming")]
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// Keeps a Connect device registered for as long as the daemon runs.
///
/// librespot's protocol task ending *is* the notification that the session
/// died: there is no error to catch and nothing is logged above a warning.
/// Before this, that task was spawned and forgotten, so a dropped session
/// left the daemon serving IPC perfectly while the device it exists to
/// provide had silently gone.
#[cfg(feature = "streaming")]
/// Whether to run a Connect device, and what to report when not.
///
/// Both halves are needed: the switch says the user wants one, the sign-in is
/// what Spotify needs to give them one. A missing sign-in is a standing state
/// to report, not a failure to retry -- nothing about it changes while the
/// daemon runs.
#[cfg(feature = "streaming")]
fn streaming_plan(enabled: bool, signed_in: bool) -> StreamingPlan {
    match (enabled, signed_in) {
        (false, _) => StreamingPlan::Idle(StreamingState::Disabled),
        (true, false) => StreamingPlan::Idle(StreamingState::NotSignedIn),
        (true, true) => StreamingPlan::Supervise,
    }
}

#[cfg(feature = "streaming")]
#[derive(Debug, PartialEq, Eq)]
enum StreamingPlan {
    Supervise,
    Idle(StreamingState),
}

/// The one line a daemon says at startup when there will be no device.
#[cfg(feature = "streaming")]
fn idle_streaming_note(state: &StreamingState) -> String {
    match state {
        StreamingState::NotSignedIn => "streaming is on, but this machine has not done the \
             streaming sign-in: run `boombox auth login --streaming`. Everything else works."
            .into(),
        _ => "streaming is off; boombox will drive your other devices".into(),
    }
}

/// What a daemon reports before it has tried anything.
fn initial_streaming_state() -> StreamingState {
    #[cfg(feature = "streaming")]
    {
        StreamingState::Disabled
    }
    #[cfg(not(feature = "streaming"))]
    {
        StreamingState::NotCompiled
    }
}

#[cfg(feature = "streaming")]
async fn supervise_streaming(
    daemon: &Arc<Daemon>,
    config: &Config,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut backoff = RECONNECT_FIRST;
    let mut first = true;
    loop {
        match crate::streaming::start(config).await {
            Ok(mut handle) => {
                if first {
                    println!("advertising as a Connect device: {}", handle.device_name());
                    first = false;
                } else {
                    tracing::info!("reconnected as {}", handle.device_name());
                }
                *daemon.spectrum.write().await = Some(handle.tap());
                *daemon.device_id.write().await = Some(handle.device_id().to_string());
                *daemon.streaming_state.write().await = StreamingState::Live;
                backoff = RECONNECT_FIRST;

                // Here rather than in the TUI: the daemon owns the device,
                // so it knows the moment one exists to adopt. A front end
                // can only guess and poll, and blocking a UI on that guess
                // is what made the TUI slow to appear.
                tokio::spawn({
                    let daemon = Arc::clone(daemon);
                    let id = handle.device_id().to_string();
                    let name = handle.device_name().to_string();
                    let adopt = config.daemon.adopt_playback;
                    async move { daemon.adopt_if_idle(&id, &name, adopt).await }
                });

                tokio::select! {
                    () = handle.ended() => {
                        *daemon.streaming_state.write().await =
                            StreamingState::Unavailable("the session ended; reconnecting".into());
                        *daemon.spectrum.write().await = None;
                        // The next session registers a new id, and until it
                        // does we have no device: an id left behind here
                        // would be another machine's to match.
                        *daemon.device_id.write().await = None;
                        tracing::warn!("Connect session ended; reconnecting");
                    }
                    _ = shutdown.changed() => {
                        handle.shutdown();
                        *daemon.streaming_state.write().await = StreamingState::Disabled;
                        *daemon.device_id.write().await = None;
                        return;
                    }
                }
            }
            Err(e) => {
                *daemon.streaming_state.write().await =
                    StreamingState::Unavailable(format!("{e:#}"));
                // Once at startup, then quietly: a machine with no audio
                // output would otherwise put the same line in the log every
                // minute for as long as the daemon runs.
                if first {
                    eprintln!("boombox: no Connect device: {e:#}");
                    eprintln!(
                        "boombox: everything else works; `boombox daemon --status` says more"
                    );
                    first = false;
                } else {
                    tracing::debug!("still no Connect device: {e:#}");
                }
            }
        }

        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => return,
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("cannot listen for SIGTERM: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

impl Daemon {
    async fn poll_loop(&self, mut shutdown: watch::Receiver<bool>) {
        let mut backoff: Option<Duration> = None;

        loop {
            let interval = match self.refresh().await {
                Ok(is_playing) => {
                    backoff = None;
                    if is_playing { self.active_interval } else { self.idle_interval }
                }
                Err(e) => {
                    // Keep serving cached state; a transient API failure is not
                    // a reason to take the daemon down.
                    tracing::warn!("poll failed: {e}");
                    let next =
                        backoff.map(|b| (b * 2).min(MAX_BACKOFF)).unwrap_or(self.idle_interval);
                    backoff = Some(next);
                    next
                }
            };
            self.stats.polling_millis.store(interval.as_millis() as u64, Ordering::Relaxed);

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.nudge.notified() => {}
                _ = shutdown.changed() => break,
            }
        }
    }

    /// Pairs the audio peak with where the track currently is. Both halves
    /// are already being maintained; this just writes them down together.
    #[cfg(feature = "streaming")]
    async fn sample_envelope(&self) {
        let Some(tap) = self.spectrum.read().await.clone() else {
            return;
        };
        let peak = tap.take_peak();
        if peak <= 0.0 {
            return; // Silence carries no shape worth recording.
        }

        let Some(state) = self.cached_playback().await else {
            return;
        };
        if !state.is_playing {
            return;
        }
        let Some(uri) = state.item.as_ref().and_then(|i| i.uri()) else {
            return;
        };
        self.envelope.write().await.record(uri, state.progress(), state.duration(), peak);
    }

    async fn refresh(&self) -> Result<bool, Error> {
        self.stats.api_calls.fetch_add(1, Ordering::Relaxed);
        let state = self.client.playback_state().await?;
        let is_playing = state.as_ref().is_some_and(|s| s.is_playing);

        // Only while it is actually playing: a paused player keeps
        // reporting its last context indefinitely, and something left
        // paused overnight is not something you listened to.
        if is_playing && let Some(context) = state.as_ref().and_then(|s| s.context.as_ref()) {
            self.recents.touch(&context.uri).await;
        }

        // The envelope follows the item on every poll, not only when a sample
        // is recorded. Recording needs sound, and an emptied player makes
        // none, so the last track's shape used to stay on the seek bar
        // against 0:00 / 0:00 for as long as nothing played.
        //
        // Only when there is a player state to read. No state at all is also
        // what Spotify reports for a moment during a device transfer, and
        // clearing then would wipe the shape of a track that carries on.
        #[cfg(feature = "streaming")]
        if let Some(current) = state.as_ref() {
            let item = current.item.as_ref().and_then(|i| i.uri());
            self.envelope.write().await.follow(item);
        }

        #[cfg(feature = "streaming")]
        self.watch_for_silence(state.as_ref()).await;

        let mut cache = self.cache.write().await;
        let now = Instant::now();
        let track_changed = {
            let was = cache.playback.as_ref().and_then(|s| s.item.as_ref()).and_then(|i| i.uri());
            let is = state.as_ref().and_then(|s| s.item.as_ref()).and_then(|i| i.uri());
            was != is
        };
        if track_changed {
            // A position asked for in the track that ended says nothing
            // about the one that replaced it.
            cache.intents.position = None;
        }
        cache.intents.settle_against(state.as_ref(), now);
        cache.playback = state;
        cache.fetched_at = Some(now);
        Ok(is_playing)
    }

    /// Cached state with the progress clock advanced to now, so a status bar
    /// polling the daemon reads a smooth timer between API calls.
    async fn cached_playback(&self) -> Option<PlaybackState> {
        let cache = self.cache.read().await;
        let state = cache.playback.as_ref()?;
        let now = Instant::now();
        let elapsed =
            cache.fetched_at.map(|t| now.saturating_duration_since(t)).unwrap_or_default();
        Some(overlay(state, &cache.intents, elapsed, now))
    }

    /// Holds what a write asked for until the player reports it.
    async fn remember(&self, what: Wrote) {
        let now = Instant::now();
        let mut cache = self.cache.write().await;
        match what {
            Wrote::Volume(percent) => renew(&mut cache.intents.volume, percent, VOLUME_SETTLE, now),
            Wrote::Position(ms) => renew(&mut cache.intents.position, ms, POSITION_SETTLE, now),
            Wrote::Playing(on) => renew(&mut cache.intents.playing, on, PLAYING_SETTLE, now),
            // Skipping abandons a position rather than asking for one.
            Wrote::Track => cache.intents.position = None,
        }
    }

    async fn serve(&self, stream: UnixStream, shutdown_tx: &watch::Sender<bool>) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        if reader.read_line(&mut line).await? == 0 {
            // A bare connect with no request: that is the liveness probe.
            return Ok(());
        }
        self.stats.requests.fetch_add(1, Ordering::Relaxed);

        let (response, stop) = match serde_json::from_str::<Request>(&line) {
            Ok(request) => self.dispatch(request).await,
            Err(e) => (
                Response::Error(WireError {
                    kind: boombox_ipc::WireErrorKind::Other,
                    message: format!(
                        "malformed request: {e} \
                         (if this followed an upgrade, this daemon is an older build \
                         -- run `boombox daemon --stop` and start it again)"
                    ),
                    status: 0,
                }),
                false,
            ),
        };

        let mut encoded = serde_json::to_string(&response)?;
        encoded.push('\n');
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.flush().await?;

        // Only now, so `boombox daemon --stop` gets its acknowledgement.
        if stop {
            let _ = shutdown_tx.send(true);
        }
        Ok(())
    }

    /// Returns the response and whether the daemon should exit afterwards.
    async fn dispatch(&self, request: Request) -> (Response, bool) {
        match request {
            Request::Ping => return (Response::Pong(self.status().await), false),
            Request::Shutdown => return (Response::Unit, true),
            // The whole point of the daemon: answered without touching the network.
            Request::PlaybackState => {
                return (Response::PlaybackState(self.cached_playback().await), false);
            }
            // Answered from the local audio tap: no network, no API budget.
            Request::Spectrum { bands } => {
                return (Response::Spectrum(self.bands(bands).await), false);
            }
            Request::Waveform { points } => {
                return (Response::Waveform(self.waveform(points).await), false);
            }
            Request::Envelope { points } => {
                return (Response::Envelope(self.envelope(points).await), false);
            }
            // The daemon's own memory, not Spotify's -- no API call.
            Request::Recents => return (Response::Recents(self.recents.list().await), false),
            Request::Remember { ref uri } => {
                self.recents.remember_now(&self.client, uri).await;
                return (Response::Unit, false);
            }
            _ => {}
        }

        let recorded = wrote(&request);
        let response = match self.forward(request).await {
            Ok(response) => {
                // Only once the write is accepted. A change Spotify refused
                // must not be shown as though it had worked.
                if let Some(what) = recorded {
                    self.remember(what).await;
                }
                response
            }
            Err(e) => Response::Error(WireError::from(&e)),
        };
        (response, false)
    }

    async fn forward(&self, request: Request) -> Result<Response, Error> {
        self.stats.api_calls.fetch_add(1, Ordering::Relaxed);
        let client = &*self.client;

        let response = match request {
            Request::Devices => Response::Devices(client.devices().await?),
            Request::Queue => Response::Queue(client.queue().await?),
            Request::Play(opts) => {
                // Recorded here as well as from polling. Polling would
                // catch it a moment later anyway, but a context reached by
                // pasting its link is the one case where getting it wrong
                // loses something unrecoverable -- there is no other way
                // back to a Spotify-owned playlist.
                if let Some(uri) = opts.context_uri.clone() {
                    self.recents.touch(&uri).await;
                }
                unit(client.play(opts).await)?
            }
            Request::Pause => unit(client.pause().await)?,
            Request::Next => unit(client.next().await)?,
            Request::Previous => unit(client.previous().await)?,
            Request::Seek { position_ms } => unit(client.seek(position_ms).await)?,
            Request::SetVolume { percent } => unit(client.set_volume(percent).await)?,
            Request::SetShuffle { on } => unit(client.set_shuffle(on).await)?,
            Request::SetRepeat { state } => unit(client.set_repeat(state).await)?,
            Request::Transfer { device_id, play } => unit(client.transfer(&device_id, play).await)?,
            Request::AddToQueue { uri } => unit(client.add_to_queue(&uri).await)?,

            Request::Search { query, types, limit, offset } => {
                Response::Search(Box::new(client.search(&query, &types, limit, offset).await?))
            }
            Request::MyPlaylists { limit, offset } => {
                Response::Playlists(client.my_playlists(limit, offset).await?)
            }
            Request::PlaylistItems { id, limit, offset } => {
                Response::PlaylistItems(client.playlist_items(&id, limit, offset).await?)
            }
            Request::SavedTracks { limit, offset } => {
                Response::SavedTracks(client.saved_tracks(limit, offset).await?)
            }
            Request::SavedAlbums { limit, offset } => {
                Response::SavedAlbums(client.saved_albums(limit, offset).await?)
            }
            Request::LibraryContains { uris } => {
                Response::Contains(client.library_contains(&uris).await?)
            }
            Request::LibraryAdd { uris } => unit(client.library_add(&uris).await)?,
            Request::LibraryRemove { uris } => unit(client.library_remove(&uris).await)?,
            Request::PlaybackState
            | Request::Ping
            | Request::Shutdown
            | Request::Spectrum { .. }
            | Request::Waveform { .. }
            | Request::Envelope { .. }
            | Request::Recents
            | Request::Remember { .. } => unreachable!("handled before forwarding"),
        };

        // A write just changed what the cache describes. Re-poll now rather
        // than let the next reader see stale state.
        let is_read = matches!(
            response,
            Response::Devices(_)
                | Response::Queue(_)
                | Response::Search(_)
                | Response::Playlists(_)
                | Response::PlaylistItems(_)
                | Response::SavedTracks(_)
                | Response::SavedAlbums(_)
                | Response::Contains(_)
        );
        if !is_read {
            self.nudge.notify_one();
        }
        Ok(response)
    }

    // Async, because these are called from the request handler, which
    // runs on a worker thread. Reading the lock by blocking there does not
    // wait -- it panics outright, taking the connection with it and
    // answering nothing, which looks from the outside exactly like a
    // daemon that has no audio to report.
    #[cfg(feature = "streaming")]
    async fn bands(&self, count: u16) -> Vec<f32> {
        match self.spectrum.read().await.clone() {
            Some(tap) => tap.bands(count as usize),
            // Streaming compiled in but not running.
            None => Vec::new(),
        }
    }

    #[cfg(feature = "streaming")]
    async fn waveform(&self, points: u16) -> Vec<f32> {
        match self.spectrum.read().await.clone() {
            Some(tap) => tap.waveform(points as usize),
            None => Vec::new(),
        }
    }

    #[cfg(feature = "streaming")]
    async fn envelope(&self, points: u16) -> Vec<f32> {
        self.envelope.read().await.sample(points as usize)
    }

    #[cfg(not(feature = "streaming"))]
    async fn envelope(&self, _points: u16) -> Vec<f32> {
        Vec::new()
    }

    #[cfg(not(feature = "streaming"))]
    async fn bands(&self, _count: u16) -> Vec<f32> {
        Vec::new()
    }

    #[cfg(not(feature = "streaming"))]
    async fn waveform(&self, _points: u16) -> Vec<f32> {
        Vec::new()
    }

    /// Makes our own Connect device the active one when nothing else is
    /// playing.
    ///
    /// Never takes playback from a device that is already going: music
    /// coming out of a phone is not something a daemon starting up should
    /// interfere with. Transfers with `play = false`, because adopting
    /// decides where sound would come from and is not a licence to start
    /// making some.
    ///
    /// Waits for the device to show up, because librespot returning a
    /// handle is not the same as Spotify having listed it -- the first
    /// attempt reliably finds nothing. This runs in the background, so the
    /// wait costs no one anything.
    ///
    /// Found by id: matching on the name would let a second machine called
    /// the same thing be adopted instead, moving playback to the wrong
    /// computer. The name is only for saying what happened.
    #[cfg(feature = "streaming")]
    async fn adopt_if_idle(&self, device_id: &str, device_name: &str, enabled: bool) {
        if !enabled {
            return;
        }
        let deadline = Instant::now() + ADOPT_WAIT;
        loop {
            // Re-checked every pass rather than once: the user may well
            // start playing something during the wait, and adopting then
            // would take it straight back off them.
            match self.client.playback_state().await {
                Ok(Some(state)) if state.device.is_some() => return,
                Err(e) => {
                    tracing::debug!("cannot tell whether anything is playing: {e}");
                    return;
                }
                _ => {}
            }

            if let Ok(devices) = self.client.devices().await
                && let Some(device) = devices.iter().find(|d| d.id.as_deref() == Some(device_id))
                && let Some(id) = device.id.as_deref()
            {
                match self.client.transfer(id, false).await {
                    Ok(()) => {
                        tracing::info!("nothing was playing, so {device_name} is now active");
                    }
                    Err(e) => tracing::debug!("could not adopt {device_name}: {e}"),
                }
                return;
            }

            if Instant::now() >= deadline {
                tracing::debug!("{device_name} never appeared; not adopting");
                return;
            }
            tokio::time::sleep(ADOPT_POLL).await;
        }
    }

    /// Notices Spotify reporting this device as playing while no audio
    /// reaches it, and says so in the log.
    ///
    /// Reported rather than acted on. It has not been caught happening yet,
    /// and an automatic restart would re-register the device under a new id
    /// with playback to move back to it -- so any restart waits until a real
    /// case shows what it should do.
    #[cfg(feature = "streaming")]
    async fn watch_for_silence(&self, state: Option<&PlaybackState>) {
        use crate::stall::{Change, playing_here};
        let tap = self.spectrum.read().await.clone();
        let device_id = self.device_id.read().await.clone();
        let here = tap.is_some() && playing_here(state, device_id.as_deref());
        let since_audio = tap.map(|t| t.since_audio()).unwrap_or_default();
        let name = &self.device_name;
        match self.stall.write().await.observe(here, since_audio, Instant::now()) {
            Some(Change::Stalled) => tracing::warn!(
                "Spotify reports {name} playing, but no audio has reached it for {}s",
                since_audio.as_secs()
            ),
            Some(Change::Recovered) => tracing::warn!("audio is reaching {name} again"),
            Some(Change::Cleared) => {
                tracing::warn!("playback stopped or moved away while {name} was silent")
            }
            None => {}
        }
    }

    /// Seconds without audio while Spotify reports playback here, when that
    /// is happening right now.
    #[cfg(feature = "streaming")]
    async fn audio_stalled_secs(&self) -> Option<u64> {
        if !self.stall.read().await.is_stalled() {
            return None;
        }
        let tap = self.spectrum.read().await.clone()?;
        Some(tap.since_audio().as_secs())
    }

    #[cfg(not(feature = "streaming"))]
    async fn audio_stalled_secs(&self) -> Option<u64> {
        None
    }

    async fn status(&self) -> DaemonStatus {
        let audio_stalled_secs = self.audio_stalled_secs().await;
        let streaming_state = self.streaming_state.read().await.clone();
        let cache = self.cache.read().await;
        DaemonStatus {
            protocol_version: PROTOCOL_VERSION,
            version: boombox_core::build_info::short(),
            streaming: streaming_state == StreamingState::Live,
            streaming_state: Some(streaming_state),
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            requests_served: self.stats.requests.load(Ordering::Relaxed),
            api_calls: self.stats.api_calls.load(Ordering::Relaxed),
            cache_age_secs: cache.fetched_at.map(|t| t.elapsed().as_secs()).unwrap_or(u64::MAX),
            polling_secs: self.stats.polling_millis.load(Ordering::Relaxed) / 1000,
            audio_stalled_secs,
        }
    }
}

fn unit(result: Result<(), Error>) -> Result<Response, Error> {
    result.map(|()| Response::Unit)
}

/// `boombox daemon --status`
pub async fn status(config: &Config) -> Result<()> {
    let path = config.socket_path()?;
    match probe(&path).await {
        Probe::Answering(s) => {
            println!("running   pid {}", s.pid);
            println!("socket    {}", path.display());
            println!("uptime    {}s", s.uptime_secs);
            println!(
                "version   {}",
                if s.version.is_empty() { "unknown (predates reporting)" } else { &s.version }
            );
            println!("streaming {}", s.streaming_summary());
            if let Some(secs) = s.audio_stalled_secs {
                println!("audio     silent for {secs}s, though Spotify says it is playing here");
            }
            println!("protocol  v{}", s.protocol_version);
            println!("requests  {} served", s.requests_served);
            println!("api calls {}", s.api_calls);
            println!("polling   every {}s", s.polling_secs);
            println!("cache     {}", s.cache_age_summary());
            // Printed last so it is the line left on screen, and to stderr
            // so `boombox daemon --status | ...` still parses cleanly.
            if let Some(warning) = s.mismatch_warning() {
                eprintln!("\nboombox: {warning}");
            }
            Ok(())
        }
        Probe::Unresponsive => {
            // The pid is the one thing needed to deal with it by hand, and
            // the socket is the only place left to learn it.
            match listener_pid(&path).await {
                Ok(pid) => println!("stuck     pid {pid}"),
                Err(_) => println!("stuck"),
            }
            println!("socket    {}", path.display());
            eprintln!(
                "\nboombox: the daemon accepts connections but does not answer them. \
                 `boombox daemon --stop` ends it."
            );
            std::process::exit(1);
        }
        Probe::Absent => {
            println!("not running ({})", path.display());
            std::process::exit(1);
        }
    }
}

/// `boombox daemon --stop`
pub async fn stop(config: &Config) -> Result<()> {
    let path = config.socket_path()?;
    match probe(&path).await {
        Probe::Answering(_) => {
            IpcClient::oneshot(&path, Request::Shutdown).await?;
            println!("daemon stopping");
            Ok(())
        }
        // Asking is exactly what it will not answer, so it is ended instead.
        Probe::Unresponsive => {
            let pid = end_unresponsive(&path).await?;
            println!("daemon (pid {pid}) was not answering; ended it");
            Ok(())
        }
        Probe::Absent => {
            println!("not running ({})", path.display());
            std::process::exit(1);
        }
    }
}

/// What is on the other end of the socket.
#[derive(Debug)]
enum Probe {
    /// Nothing is listening: no file, or one left by a daemon that died
    /// without cleaning up.
    Absent,
    Answering(DaemonStatus),
    /// Something accepted the connection and did not answer a ping. Pings
    /// are answered from memory, so this is not a busy daemon but a stuck
    /// one -- stopped, deadlocked, or with every worker thread blocked.
    Unresponsive,
}

async fn probe(path: &Path) -> Probe {
    if IpcClient::connect(path).await.is_err() {
        return Probe::Absent;
    }
    match IpcClient::oneshot(path, Request::Ping).await {
        Ok(Response::Pong(status)) => Probe::Answering(status),
        // Debug, not warn: every caller says so in its own words, and a
        // warning here printed the same news twice with different advice.
        Ok(other) => {
            tracing::debug!("daemon answered a ping with {}", other.name());
            Probe::Unresponsive
        }
        // Held open and not answered: something is there, and stuck.
        Err(e @ Error::DaemonNotAnswering(_)) => {
            tracing::debug!("daemon did not answer a ping: {e}");
            Probe::Unresponsive
        }
        // Refused, reset or hung up rather than held open. That is what
        // happens when the listener goes away between the two connections:
        // a daemon exiting as it is probed, or a just-closed listener whose
        // descriptor a newly spawned process held for an instant. Look again
        // before calling it stuck, or boombox goes looking for a process to end.
        Err(e) => {
            if IpcClient::connect(path).await.is_err() {
                Probe::Absent
            } else {
                tracing::debug!("daemon hung up on a ping: {e}");
                Probe::Unresponsive
            }
        }
    }
}

/// How long a stuck daemon gets to act on SIGTERM, and then on SIGKILL.
///
/// Short: a wedged runtime never runs its signal handler, and a stopped
/// process cannot. The grace only covers the lucky case where shutdown still
/// works, which is worth a moment because it deregisters the Connect device.
const TERM_GRACE: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Ends a daemon that accepts connections but never answers them.
///
/// It cannot be asked to stop -- asking is the thing it will not answer --
/// so it is signalled. There is no pidfile; the pid comes from the socket,
/// where the kernel records which process is listening. It is checked to be
/// a boombox binary first, because `[daemon] socket` is configurable and a
/// signal sent to the wrong process cannot be taken back.
async fn end_unresponsive(path: &Path) -> Result<u32> {
    let pid = listener_pid(path).await?;
    if pid == std::process::id() {
        bail!("{} is held by this very process", path.display());
    }
    if !is_boombox_process(pid) {
        bail!("pid {pid} holds {} but is not boombox, so it was left alone", path.display());
    }
    tracing::info!(pid, "ending a daemon that is not answering");

    signal(pid, libc::SIGTERM)?;
    if !exits_within(pid, TERM_GRACE).await {
        signal(pid, libc::SIGKILL)?;
        if !exits_within(pid, KILL_GRACE).await {
            bail!("pid {pid} survived SIGKILL");
        }
    }
    // SIGKILL skips the daemon's own cleanup, and a socket file left behind
    // would make the next start believe a daemon is still there.
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).context("cannot remove the stuck daemon's socket")
        }
        _ => Ok(pid),
    }
}

/// The pid of whatever is listening on `path`, read from the socket itself.
///
/// The kernel attaches the listener's credentials to a connection when it is
/// made, whether or not the listener ever accepts it -- which is what makes
/// this work on a daemon that is stopped outright.
async fn listener_pid(path: &Path) -> Result<u32> {
    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path))
        .await
        .context("timed out connecting")?
        .with_context(|| format!("cannot connect to {}", path.display()))?;
    peer_pid(&stream)
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> Result<u32> {
    use std::os::fd::AsRawFd as _;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: the fd is a connected Unix socket kept open by `stream` for the
    // whole call, and `pid` and `len` are valid for writes of the sizes given.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read the listener's pid");
    }
    u32::try_from(pid).context("the kernel reported a negative pid")
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Result<u32> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: `ucred` is plain data, for which all-zero is a valid value.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: as for macOS: a live connected fd, and buffers of the sizes given.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read the listener's pid");
    }
    u32::try_from(cred.pid).context("the kernel reported a negative pid")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_pid(_stream: &UnixStream) -> Result<u32> {
    bail!("cannot identify the listening process on this platform")
}

/// Whether `pid` is running a binary called exactly `boombox`.
///
/// Through `ps` rather than a platform API: it reads the same on macOS and
/// Linux, and one process spawn is nothing on a path that only runs once
/// something has already gone wrong.
fn is_boombox_process(pid: u32) -> bool {
    let Ok(output) =
        std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "comm="]).output()
    else {
        return false;
    };
    is_boombox_name(&String::from_utf8_lossy(&output.stdout))
}

/// Whether a `ps -o comm=` line names a binary called exactly `boombox`: the
/// full path on macOS, the bare name on Linux.
fn is_boombox_name(comm: &str) -> bool {
    Path::new(comm.trim()).file_name().is_some_and(|n| n == "boombox")
}

fn signal(pid: u32, sig: libc::c_int) -> Result<()> {
    let raw = libc::pid_t::try_from(pid).context("pid out of range")?;
    // SAFETY: kill(2) has no memory-safety preconditions. The pid was checked
    // above to be a boombox process.
    if unsafe { libc::kill(raw, sig) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    // Already gone is the outcome being asked for.
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err).with_context(|| format!("cannot signal pid {pid}"))
}

/// Whether `pid` has exited within `wait`.
async fn exits_within(pid: u32, wait: Duration) -> bool {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return true;
    };
    let deadline = Instant::now() + wait;
    loop {
        // SAFETY: signal 0 only checks that the process exists.
        let exists = unsafe { libc::kill(raw, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !exists {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Connects to a daemon that is actually answering, or `None` to go straight
/// to the API.
///
/// Pinged rather than merely connected. The kernel completes a connection
/// for a daemon that is stopped or deadlocked, so a bare connect succeeded
/// and every command after it hung waiting for a reply.
pub async fn try_connect(config: &Config) -> Option<IpcClient> {
    let path = config.socket_path().ok()?;
    match probe(&path).await {
        Probe::Answering(_) => IpcClient::connect(&path).await.ok(),
        // Going direct still works, so a stuck daemon costs the three seconds
        // it took to notice rather than the whole command.
        Probe::Unresponsive => {
            eprintln!(
                "boombox: the daemon is not answering, so this went direct. \
                 `boombox daemon --stop` ends it."
            );
            None
        }
        Probe::Absent => None,
    }
}

/// How long to wait for a freshly spawned daemon to answer. The socket is
/// bound before librespot starts, deliberately, so this covers process
/// start and the token read rather than audio setup.
const START_TIMEOUT: Duration = Duration::from_secs(12);

/// How long to keep looking for our own device before giving up on
/// adopting it. Generous because nobody is waiting on this.
#[cfg(feature = "streaming")]
const ADOPT_WAIT: Duration = Duration::from_secs(30);
#[cfg(feature = "streaming")]
const ADOPT_POLL: Duration = Duration::from_secs(2);

/// What [`ensure_running`] had to do, so the caller can say so rather than
/// leaving the user to wonder why playback stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// A usable daemon was already there. The common case, and the quiet one.
    Joined,
    Started,
    /// The daemon spoke a protocol this build cannot talk to, so it was
    /// replaced. This is the only case that interrupts playback.
    Replaced,
    /// A daemon was accepting connections and not answering them, so it was
    /// ended and a fresh one started. Whatever it was playing is gone, but
    /// nothing could have controlled it anyway.
    Unstuck(u32),
    /// No daemon, and none could be started. The caller falls back to
    /// talking to the Web API directly, which still works.
    Unavailable(String),
}

/// Connects to a daemon, starting one if necessary.
///
/// A daemon from a *different build* is joined, not replaced. The daemon
/// holds the librespot session and is the Connect device, so restarting it
/// stops the music -- far too rude a thing to do because a binary was
/// rebuilt. Only a protocol it cannot speak justifies that, and then the
/// caller says so out loud.
pub async fn ensure_running(config: &Config) -> (Option<IpcClient>, Startup) {
    let Ok(path) = config.socket_path() else {
        return (None, Startup::Unavailable("no socket path configured".into()));
    };

    match probe(&path).await {
        Probe::Absent => {}
        // Speaks our dialect: use it, whatever build it is.
        Probe::Answering(status) if status.speaks_our_protocol() => {
            if let Ok(client) = IpcClient::connect(&path).await {
                return (Some(client), Startup::Joined);
            }
        }
        Probe::Answering(status) => {
            tracing::warn!(
                "replacing daemon (protocol v{} vs v{PROTOCOL_VERSION})",
                status.protocol_version
            );
            if let Err(e) = stop(config).await {
                let client = IpcClient::connect(&path).await.ok();
                return (client, Startup::Unavailable(format!("cannot stop it: {e}")));
            }
            return match start_and_wait(&path).await {
                Ok(client) => (Some(client), Startup::Replaced),
                Err(e) => (None, Startup::Unavailable(e.to_string())),
            };
        }
        // This used to be joined anyway, under a comment calling it too
        // broken to trust -- and then every request the TUI made hung.
        Probe::Unresponsive => {
            if !config.daemon.autostart {
                return (
                    None,
                    Startup::Unavailable(
                        "the daemon is not answering, and autostart is off, so it was left alone"
                            .into(),
                    ),
                );
            }
            return match end_unresponsive(&path).await {
                Ok(pid) => match start_and_wait(&path).await {
                    Ok(client) => (Some(client), Startup::Unstuck(pid)),
                    Err(e) => (None, Startup::Unavailable(e.to_string())),
                },
                Err(e) => (
                    None,
                    Startup::Unavailable(format!(
                        "the daemon is not answering and could not be ended: {e}"
                    )),
                ),
            };
        }
    }

    if !config.daemon.autostart {
        return (None, Startup::Unavailable("autostart is off".into()));
    }
    match start_and_wait(&path).await {
        Ok(client) => (Some(client), Startup::Started),
        Err(e) => (None, Startup::Unavailable(e.to_string())),
    }
}

async fn start_and_wait(path: &Path) -> Result<IpcClient> {
    spawn_detached()?;

    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(client) = IpcClient::connect(path).await {
            return Ok(client);
        }
    }
    bail!("daemon did not answer within {}s", START_TIMEOUT.as_secs())
}

/// Starts `boombox daemon` in its own session.
///
/// `setsid` rather than a plain spawn: a child in this terminal's session
/// is killed by the SIGHUP that arrives when the terminal closes, which
/// would make the daemon die with the TUI -- the opposite of the point.
/// Its output goes to the log file, because anything on stdout would be
/// painted over the TUI.
fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe().context("cannot find our own binary")?;
    let dir = boombox_core::config::state_dir()?;
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .context("cannot open the daemon log")?;

    let mut command = std::process::Command::new(&exe);
    command.arg("daemon").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Safe: setsid is async-signal-safe and this runs in the child
        // between fork and exec, where only such calls are permitted.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = command.spawn().with_context(|| format!("cannot start {}", exe.display()))?;
    tracing::info!(pid = child.id(), "started a daemon");
    // Deliberately not waited on: it is in its own session now and must
    // outlive us. It is not our child in any meaningful sense any more.
    Ok(())
}

/// The daemon's self-report, for a caller that wants to know which build it
/// just connected to. `None` when the daemon is too old to answer a ping in
/// a form we can read, which is itself worth knowing but not worth failing
/// over -- the connection still works for everything that has not changed.
pub async fn daemon_status(config: &Config) -> Option<DaemonStatus> {
    let path = config.socket_path().ok()?;
    match IpcClient::oneshot(&path, Request::Ping).await {
        Ok(Response::Pong(status)) => Some(status),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon with no Spotify behind it. Enough to exercise the request
    /// Both halves are required, and the missing one has to be named: this
    /// is the difference between "you never signed in" and "your audio is
    /// broken", which the daemon used to report identically.
    #[cfg(feature = "streaming")]
    #[test]
    fn streaming_needs_both_the_switch_and_the_sign_in() {
        assert_eq!(streaming_plan(true, true), StreamingPlan::Supervise);
        assert_eq!(streaming_plan(true, false), StreamingPlan::Idle(StreamingState::NotSignedIn));
        assert_eq!(streaming_plan(false, true), StreamingPlan::Idle(StreamingState::Disabled));
        assert_eq!(streaming_plan(false, false), StreamingPlan::Idle(StreamingState::Disabled));
    }

    /// On by default means most people meet this line rather than a device,
    /// so it has to carry the one command that fixes it.
    #[cfg(feature = "streaming")]
    #[test]
    fn a_daemon_without_the_streaming_sign_in_says_what_to_run() {
        let note = idle_streaming_note(&StreamingState::NotSignedIn);
        assert!(note.contains("auth login --streaming"), "{note}");
        assert!(note.contains("Everything else works"), "{note}");
    }

    const POLLED: &str = r#"{"is_playing":true,"progress_ms":60000,
        "device":{"id":"d","name":"Desk","type":"Computer","volume_percent":40},
        "item":{"type":"track","name":"x","uri":"spotify:track:x","duration_ms":200000,
                "artists":[],"album":{}}}"#;

    fn polled_state() -> PlaybackState {
        serde_json::from_str(POLLED).unwrap()
    }

    /// The whole point: the figure asked for is the one shown, until the
    /// player catches up. Without this the display answers with the value
    /// the keypress was meant to change.
    #[test]
    fn a_written_volume_is_shown_until_the_player_agrees() {
        let t0 = Instant::now();
        let intents =
            Intents { volume: Some(Pending::at(70, VOLUME_SETTLE, t0)), ..Intents::default() };
        let shown = overlay(&polled_state(), &intents, Duration::ZERO, t0 + Duration::from_secs(1));
        assert_eq!(shown.volume(), Some(70), "the player still says 40");
        assert!(shown.pending.volume, "and the reader is told it is not confirmed");

        let mut agreed = polled_state();
        agreed.device.as_mut().unwrap().volume_percent = Some(70);
        let shown = overlay(&agreed, &intents, Duration::ZERO, t0 + Duration::from_secs(1));
        assert_eq!(shown.volume(), Some(70));
        assert!(!shown.pending.volume, "the player agrees, so nothing is outstanding");
    }

    /// Pausing has to stop the clock as well as the glyph, or the timer
    /// runs on under a player that is not playing.
    #[test]
    fn a_pause_stops_the_clock_before_spotify_reports_it() {
        let t0 = Instant::now();
        let intents =
            Intents { playing: Some(Pending::at(false, PLAYING_SETTLE, t0)), ..Intents::default() };
        let shown =
            overlay(&polled_state(), &intents, Duration::from_secs(5), t0 + Duration::from_secs(5));
        assert!(!shown.is_playing);
        assert!(shown.pending.playing);
        assert_eq!(shown.progress_ms, Some(60_000), "not advanced by the five seconds");
    }

    /// A position does not stay where it was put: the seek happened after
    /// the last poll, so it advances from the keypress, not from the poll.
    #[test]
    fn a_seek_is_shown_from_where_it_was_asked_for() {
        let t0 = Instant::now();
        let intents = Intents {
            position: Some(Pending::at(65_000, POSITION_SETTLE, t0)),
            ..Intents::default()
        };
        let shown =
            overlay(&polled_state(), &intents, Duration::from_secs(9), t0 + Duration::from_secs(2));
        assert_eq!(shown.progress_ms, Some(67_000), "65s asked for, two seconds ago");
        assert!(shown.pending.position);
    }

    /// The backstop. A change that never lands must not be shown for ever.
    #[test]
    fn a_value_the_player_never_confirms_is_given_up() {
        let t0 = Instant::now();
        let intents =
            Intents { volume: Some(Pending::at(70, VOLUME_SETTLE, t0)), ..Intents::default() };
        let shown = overlay(&polled_state(), &intents, Duration::ZERO, t0 + VOLUME_SETTLE);
        assert_eq!(shown.volume(), Some(40), "back to what the player reports");
        assert!(!shown.pending.any());
    }

    #[test]
    fn a_poll_that_agrees_clears_what_was_outstanding() {
        let t0 = Instant::now();
        let mut intents = Intents {
            volume: Some(Pending::at(70, VOLUME_SETTLE, t0)),
            playing: Some(Pending::at(true, PLAYING_SETTLE, t0)),
            ..Intents::default()
        };
        let mut observed = polled_state();
        observed.device.as_mut().unwrap().volume_percent = Some(69);
        intents.settle_against(Some(&observed), t0 + Duration::from_secs(1));
        assert!(intents.volume.is_none(), "69 is within rounding of 70");
        assert!(intents.playing.is_none(), "the player is playing, as asked");
    }

    /// Reads change nothing, so they leave nothing behind.
    #[test]
    fn only_writes_are_remembered() {
        assert_eq!(wrote(&Request::SetVolume { percent: 70 }), Some(Wrote::Volume(70)));
        assert_eq!(wrote(&Request::Seek { position_ms: 1000 }), Some(Wrote::Position(1000)));
        assert_eq!(wrote(&Request::Pause), Some(Wrote::Playing(false)));
        assert_eq!(wrote(&Request::Next), Some(Wrote::Track));
        assert_eq!(wrote(&Request::PlaybackState), None);
        assert_eq!(wrote(&Request::Devices), None);
    }

    /// handling that does not touch the network.
    fn offline_daemon() -> Daemon {
        // Any client id will do; nothing here reaches the network.
        let config = Config { client_id: Some("test".into()), ..Config::default() };
        let auth = boombox_core::Auth::from_config(&config).expect("auth from a default config");
        Daemon {
            client: Arc::new(Client::new(Arc::new(auth))),
            cache: Arc::new(RwLock::new(Cache::default())),
            nudge: Arc::new(Notify::new()),
            stats: Arc::new(Stats::default()),
            #[cfg(feature = "streaming")]
            spectrum: Arc::new(RwLock::new(None)),
            streaming_state: Arc::new(RwLock::new(initial_streaming_state())),
            #[cfg(feature = "streaming")]
            envelope: Arc::new(RwLock::new(crate::spectrum::Envelope::new(ENVELOPE_BUCKETS))),
            active_interval: Duration::from_millis(1000),
            idle_interval: Duration::from_millis(15_000),
            recents: crate::recents::Store::ephemeral(),
            started: Instant::now(),
            #[cfg(feature = "streaming")]
            stall: Arc::new(RwLock::new(crate::stall::StallWatch::default())),
            #[cfg(feature = "streaming")]
            device_name: "boombox".into(),
            #[cfg(feature = "streaming")]
            device_id: Arc::new(RwLock::new(None)),
        }
    }

    /// The spectrum requests are answered on a runtime worker, and reading
    /// the tap by blocking there does not wait -- it panics, killing the
    /// connection so the caller gets no reply at all. From the front end
    /// that is indistinguishable from a daemon with no audio, which is how
    /// it went unnoticed: the TUI simply said nothing was being decoded.
    ///
    /// So this has to run *in* a runtime. The same test outside one would
    /// pass with the bug present.
    #[tokio::test]
    async fn spectrum_requests_are_answered_from_inside_the_runtime() {
        let daemon = offline_daemon();

        let (response, stop) = daemon.dispatch(Request::Spectrum { bands: 8 }).await;
        assert!(!stop);
        assert_eq!(response.name(), "spectrum", "should have answered");

        let (response, _) = daemon.dispatch(Request::Waveform { points: 8 }).await;
        assert_eq!(response.name(), "waveform");
    }

    /// With no Connect session the answer is an empty reading, not a
    /// failure: the front ends draw "nothing decoding here" from this.
    #[tokio::test]
    async fn a_daemon_with_no_session_reports_an_empty_spectrum() {
        let daemon = offline_daemon();
        match daemon.dispatch(Request::Spectrum { bands: 8 }).await.0 {
            Response::Spectrum(bands) => assert!(bands.is_empty()),
            other => panic!("got {}", other.name()),
        }
    }

    fn test_socket(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("boombox-daemon-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// The listener never accepts, exactly like a stopped daemon, and the
    /// kernel still says who it is. Everything about ending a stuck daemon
    /// rests on this.
    #[tokio::test]
    async fn the_listening_pid_is_readable_without_the_listener_accepting() {
        let path = test_socket("peer");
        let _listener = UnixListener::bind(&path).unwrap();
        assert_eq!(listener_pid(&path).await.unwrap(), std::process::id());
        let _ = std::fs::remove_file(&path);
    }

    /// Near enough is not enough before sending a signal. The test binary
    /// itself is `boombox-<hash>`, which is exactly the near miss to refuse.
    ///
    /// Tested on names rather than by running `ps`. Spawning a process from a
    /// test briefly duplicates every descriptor the test binary has open, and
    /// that kept other tests' just-closed listeners alive: the probe tests
    /// failed most runs of the full suite, and never with this one skipped.
    #[test]
    fn only_a_binary_called_exactly_boombox_is_taken_for_it() {
        assert!(is_boombox_name("/Users/someone/.cargo/bin/boombox\n"), "macOS reports the path");
        assert!(is_boombox_name("boombox"), "Linux reports the bare name");
        assert!(!is_boombox_name("/repo/target/debug/deps/boombox-690f2eeb326bf31c"));
        assert!(!is_boombox_name("boomboxd"));
        assert!(!is_boombox_name("not-boombox"));
        assert!(!is_boombox_name(""));
    }

    /// A socket file whose owner died must read as absent, not stuck --
    /// otherwise every start after a crash would go looking for a process
    /// to end.
    #[tokio::test]
    async fn a_socket_left_by_a_dead_daemon_is_absent() {
        let path = test_socket("dead");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "the file outlives its listener");
        let started = Instant::now();
        let seen = probe(&path).await;
        assert!(matches!(seen, Probe::Absent), "{seen:?} after {:?}", started.elapsed());
        let _ = std::fs::remove_file(&path);
    }

    /// The failure this is all for: connected, and silent.
    #[tokio::test(start_paused = true)]
    async fn a_listener_that_never_answers_is_unresponsive() {
        let path = test_socket("mute");
        let _listener = UnixListener::bind(&path).unwrap();
        let started = Instant::now();
        let seen = probe(&path).await;
        assert!(matches!(seen, Probe::Unresponsive), "{seen:?} after {:?}", started.elapsed());
        let _ = std::fs::remove_file(&path);
    }

    /// A daemon exiting as it is probed: it takes the ping, stops listening,
    /// and hangs up. Nothing is left to be stuck -- and calling it stuck would
    /// send boombox looking for a process to end.
    #[tokio::test]
    async fn a_daemon_exiting_as_it_is_probed_is_absent_not_stuck() {
        let path = test_socket("exiting");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (read, write) = stream.into_split();
                let mut line = String::new();
                // Only the connection carrying a request gets the exit. The
                // probe's opening connection closes without a word; acting on
                // content rather than counting connections keeps the test
                // independent of the order they are accepted in.
                if BufReader::new(read).read_line(&mut line).await.unwrap_or(0) > 0 {
                    drop(listener);
                    // Hang up only once nothing is listening.
                    drop(write);
                    return;
                }
            }
        });
        let started = Instant::now();
        let seen = probe(&path).await;
        assert!(matches!(seen, Probe::Absent), "{seen:?} after {:?}", started.elapsed());
        let _ = std::fs::remove_file(&path);
    }

    /// The other side of the line: one that hangs up but is still listening
    /// is really there, and really broken.
    #[tokio::test]
    async fn a_daemon_that_hangs_up_but_keeps_listening_is_unresponsive() {
        let path = test_socket("hangup");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let started = Instant::now();
        let seen = probe(&path).await;
        assert!(matches!(seen, Probe::Unresponsive), "{seen:?} after {:?}", started.elapsed());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlong_socket_paths_are_rejected_with_advice() {
        let long = PathBuf::from("/tmp").join("x".repeat(MAX_SOCKET_PATH)).join("boombox.sock");
        let err = check_socket_path(&long).unwrap_err().to_string();
        assert!(err.contains("socket ="), "should suggest the config key: {err}");
        assert!(err.contains(&MAX_SOCKET_PATH.to_string()), "{err}");
    }

    #[test]
    fn ordinary_socket_paths_pass() {
        assert!(check_socket_path(Path::new("/tmp/boombox.sock")).is_ok());
        assert!(
            check_socket_path(Path::new("/Users/someone/.local/state/boombox/boombox.sock"))
                .is_ok()
        );
    }
}
