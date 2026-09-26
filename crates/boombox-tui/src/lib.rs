//! The ratatui front end. Talks to Spotify only through [`PlayerApi`], so it
//! is identical whether it is driving the daemon or the Web API directly.

pub mod action;
pub mod app;
pub mod artwork;
pub mod graphics;
pub mod keymap;
pub mod palette;
pub mod sixel;
pub mod ui;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use boombox_core::api::library::{LibraryApi, RecentsApi};
use boombox_core::api::player::{PlayOptions, PlayerApi, SpectrumApi};
use boombox_core::api::{Device, Entry, PlaybackState, PlayingItem, SearchType};
use futures::StreamExt as _;
use ratatui::crossterm::event::{Event, EventStream, KeyEventKind};
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use crate::app::{App, Command};

/// Messages arriving from anywhere that is not the keyboard.
enum Message {
    Playback(Box<Option<PlaybackState>>),
    Devices(Vec<Device>),
    Queue(Vec<PlayingItem>),
    Entries {
        /// The list and query these were fetched for, so a fetch that
        /// finishes after the list has changed can be dropped.
        view: app::View,
        query: String,
        entries: Vec<Entry>,
        total: u32,
        append: bool,
    },
    Spectrum(Vec<f32>),
    /// A palette read from artwork, with the cover it came from so the
    /// result can be remembered. `None` means the cover had no usable colour.
    Artwork {
        url: String,
        artwork: crate::artwork::Artwork,
    },
    Waveform(Vec<f32>),
    Envelope(Vec<f32>),
    Done(&'static str),
    /// A long job reporting in. Unlike `Done` this does not mean the
    /// playback state has changed, so it must not trigger a poll -- a
    /// progress line every few tracks would otherwise cost an API call
    /// every few tracks.
    Progress(String),
    Failed(String),
    Redraw,
}

/// How many rows to pull per page. Search is capped far lower by Spotify.
const PAGE: u32 = 50;

/// How often to ask whether this computer has gone silent while playing.
/// A local ping, so cheap; nobody needs to hear about it within the second.
const SILENCE_INTERVAL: Duration = Duration::from_secs(5);

/// Spectrum refresh. A local socket round trip costs microseconds, so this is
/// bounded by what looks smooth rather than by what is affordable.
const SPECTRUM_INTERVAL: Duration = Duration::from_millis(33);
/// More bands than the bars strictly need, because the waterfall wants
/// frequency detail: harmonics only separate into visible lines if there are
/// enough bands to keep them apart. The bars resample down to the pane width
/// anyway.
const SPECTRUM_BANDS: u16 = 128;
/// More than the scope draws, so there is slack to search for a trigger point.
const WAVEFORM_POINTS: u16 = 512;
/// The seek bar changes with progress, not with the audio, so it needs far
/// less than the visualisers.
const ENVELOPE_INTERVAL: Duration = Duration::from_millis(500);
const ENVELOPE_POINTS: u16 = 240;

pub struct Options {
    /// How often to re-read playback state. Cheap against a daemon, expensive
    /// against the Web API, so the caller decides.
    pub poll: Duration,
    pub seek_step_secs: u32,
    pub connected_to_daemon: bool,
    /// Build string of the daemon on the other end, when there is one.
    pub daemon_version: Option<String>,
    /// The Connect device the daemon registered, so the device list can
    /// say which row is this machine.
    pub our_device_id: Option<String>,
    /// Raised at startup when the daemon speaks a different protocol, so the
    /// skew is visible before it breaks a request rather than after.
    pub daemon_warning: Option<String>,
    /// Something that happened on the way in and is worth mentioning but is
    /// not a problem -- a daemon started, a device adopted. Shown in the
    /// ordinary toast colour, not the alarming one.
    pub daemon_notice: Option<String>,
    /// How to draw album art: `auto`, `off`, or `kitty`.
    pub graphics: String,
}

pub async fn run<P>(api: Arc<P>, options: Options) -> Result<()>
where
    P: PlayerApi + LibraryApi + RecentsApi + SpectrumApi + 'static,
{
    let mut terminal = ratatui::init();
    // Without this, a panic leaves the user in raw mode on the alternate
    // screen with no prompt and no echo.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        previous_hook(info);
    }));

    let result = event_loop(&mut terminal, api, options).await;

    ratatui::restore();
    result
}

async fn event_loop<P>(
    terminal: &mut ratatui::DefaultTerminal,
    api: Arc<P>,
    options: Options,
) -> Result<()>
where
    P: PlayerApi + LibraryApi + RecentsApi + SpectrumApi + 'static,
{
    let mut app = App::new(options.seek_step_secs, options.connected_to_daemon);
    app.graphics = crate::graphics::Protocol::detect(&options.graphics).usable();
    tracing::info!(protocol = ?app.graphics, "album art");
    app.daemon_version = options.daemon_version.clone();
    app.our_device_id = options.our_device_id.clone();
    // Surfaced immediately rather than left for the first request that
    // happens to hit a changed message, which is a baffling way to find out.
    // A real warning wins the one toast slot over a mere notice.
    if let Some(warning) = &options.daemon_warning {
        app.error(warning.clone());
    } else if let Some(notice) = &options.daemon_notice {
        app.info(notice.clone());
    }
    // Covers are per album, so a whole album is one fetch. Palettes are tiny
    // and the map is bounded by how many albums one sitting touches.
    let mut artwork_cache: ArtworkCache = std::collections::HashMap::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut keys = EventStream::new();

    spawn_poll(&api, &tx, options.poll);
    // Shared with the poll task so it can skip work while the view is hidden.
    let spectrum_wanted = Arc::new(AtomicBool::new(app.wants_spectrum()));
    let waveform_wanted = Arc::new(AtomicBool::new(app.wants_waveform()));
    spawn_spectrum_poll(&api, &tx, Arc::clone(&spectrum_wanted), Arc::clone(&waveform_wanted));
    spawn_envelope_poll(&api, &tx);
    spawn_silence_poll(&api, &tx);
    refresh_devices(&api, &tx);
    refresh_queue(&api, &tx);

    let mut painted = false;
    let mut last_queue_refresh = Instant::now() - QUEUE_REFRESH_INTERVAL;
    let mut graphics = crate::graphics::Graphics::new(app.graphics);
    loop {
        let mut cover_at = None;
        terminal.draw(|frame| cover_at = ui::draw(frame, &app))?;
        // After the frame is flushed, so the image lands on top of the
        // cells rather than being overwritten by them.
        if app.graphics != crate::graphics::Protocol::Cells {
            let result = graphics.sync(
                &mut std::io::stdout(),
                cover_at,
                app.cover_url.as_deref(),
                app.cover.as_deref(),
            );
            if let Err(e) = result {
                tracing::debug!("could not draw the cover: {e}");
            }
        }
        if !painted {
            painted = true;
            tracing::debug!("phase: first paint");
        }

        // Computed before the select so the branch borrows nothing.
        let deadline = app.next_deadline();

        tokio::select! {
            // Never fires unless something is outstanding: with nothing
            // pending this branch waits forever, so an idle TUI is not
            // woken on a timer it has no use for.
            () = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                for command in app.flush_due() {
                    dispatch(&api, &tx, command);
                }
            },
            Some(message) = rx.recv() => match message {
                Message::Playback(state) => {
                    let changed = app.set_playback(*state);
                    // A queue jump into a context is confirmed here rather
                    // than by an extra request: the poll is already
                    // running, and this is what it is for.
                    if let Some(correction) = app.check_jump() {
                        dispatch(&api, &tx, correction);
                    }
                    if changed {
                        // A new track means the queue we are showing moved
                        // on -- but not more often than this. Skipping
                        // through the queue changes the track several
                        // times a second, and a queue read is a real API
                        // call each time; that burst is what was tripping
                        // the rate limiter on a long jump.
                        if last_queue_refresh.elapsed() >= QUEUE_REFRESH_INTERVAL {
                            last_queue_refresh = Instant::now();
                            refresh_queue(&api, &tx);
                        }
                        // ...and that the colours should follow the music.
                        // set_playback has already applied the URI-derived
                        // palette, so a cached cover has to be re-applied here
                        // or the second track of an album loses it.
                        if let Some(url) = app.artwork_url() {
                            match apply_artwork(&tx, url.clone(), &artwork_cache) {
                                Some(known) => {
                                    if let Some(palette) = known.palette {
                                        app.set_palette(palette);
                                    }
                                    app.set_cover(
                                        Some(url),
                                        known.cover.map(std::sync::Arc::new),
                                    );
                                }
                                // Fetch under way: drop the old cover so the
                                // previous album is not shown against this one.
                                None => app.set_cover(None, None),
                            }
                        }
                    }
                }
                Message::Devices(devices) => app.set_devices(devices),
                Message::Queue(queue) => app.set_queue(queue),
                Message::Entries { view, query, entries, total, append } => {
                    app.accept_entries(&view, &query, entries, total, append)
                }
                Message::Spectrum(bands) => app.set_spectrum(bands),
                Message::Artwork { url, artwork } => {
                    // Only apply it if it is still the cover we are on: a
                    // slow fetch for the previous track must not repaint
                    // the one that replaced it.
                    if app.artwork_url().as_deref() == Some(url.as_str()) {
                        if let Some(palette) = artwork.palette {
                            app.set_palette(palette);
                        }
                        app.set_cover(
                            Some(url.clone()),
                            artwork.cover.clone().map(std::sync::Arc::new),
                        );
                    }
                    remember_artwork(&mut artwork_cache, url, artwork);
                }
                Message::Waveform(points) => app.set_waveform(points),
                Message::Envelope(points) => app.set_envelope(points),
                Message::Progress(text) => app.info(text),
                Message::Done(what) => {
                    app.info(what);
                    // The state we are showing is now stale by definition.
                    poll_once(&api, &tx);
                    if what == "transferred" {
                        // Which device is active has changed under the list.
                        refresh_devices(&api, &tx);
                    }
                }
                Message::Failed(e) => app.error(e),
                Message::Redraw => {}
            },
            Some(Ok(event)) = keys.next() => {
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        // Any key wakes the stage, and still does its own
                        // job. Waking that swallowed the keystroke would
                        // make every wake cost two presses.
                        app.note_input();
                        // The search box swallows printable keys, so which
                        // table applies depends on where focus is.
                        let mapped = if app.is_typing() {
                            keymap::map_typing(key)
                        } else {
                            keymap::map(key)
                        };
                        if let Some(action) = mapped {
                            if let Some(command) = app.update(action) {
                                dispatch(&api, &tx, command);
                            }
                            // Paging happens on cursor movement, not on a timer.
                            if let Some(more) = app.maybe_load_more() {
                                dispatch(&api, &tx, more);
                            }
                        }
                    }
                    // The terminal repaints everything on a resize, which
                    // takes the image with it.
                    Event::Resize(_, _) => graphics.invalidate(),
                    _ => {}
                }
            },
        }

        spectrum_wanted.store(app.wants_spectrum(), Ordering::Relaxed);
        waveform_wanted.store(app.wants_waveform(), Ordering::Relaxed);

        if app.should_quit {
            // Leaving an image behind would paint it over the shell.
            let _ = graphics.clear(&mut std::io::stdout());
            return Ok(());
        }
    }
}

fn spawn_poll<P>(api: &Arc<P>, tx: &mpsc::UnboundedSender<Message>, every: Duration)
where
    P: PlayerApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        // A late tick should not cause a burst of catch-up polls.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // A stuck daemon is said once per outage. Silence left the player
        // frozen on stale state with nothing to explain it, and a toast on
        // every tick would be worse than silence.
        let mut reported_stuck = false;
        loop {
            ticker.tick().await;
            let message = match api.playback_state().await {
                Ok(state) => {
                    reported_stuck = false;
                    Message::Playback(Box::new(state))
                }
                Err(e @ boombox_core::Error::DaemonNotAnswering(_)) if !reported_stuck => {
                    reported_stuck = true;
                    Message::Failed(e.to_string())
                }
                Err(e) => {
                    tracing::debug!("poll failed: {e}");
                    // Keep the last known state on screen rather than blanking
                    // the UI over one dropped request.
                    Message::Redraw
                }
            };
            if tx.send(message).is_err() {
                return;
            }
        }
    });
}

/// Polls the spectrum only while its view is on screen. The daemon computes
/// the transform, so an idle TUI costs nothing.
fn spawn_spectrum_poll<P>(
    api: &Arc<P>,
    tx: &mpsc::UnboundedSender<Message>,
    wanted: Arc<AtomicBool>,
    waveform_wanted: Arc<AtomicBool>,
) where
    P: PlayerApi + LibraryApi + RecentsApi + SpectrumApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SPECTRUM_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            // The scope needs samples rather than bands, so ask for whichever
            // the visible mode actually draws -- never both.
            if waveform_wanted.load(Ordering::Relaxed) {
                match api.waveform(WAVEFORM_POINTS).await {
                    Ok(points) => {
                        if tx.send(Message::Waveform(points)).is_err() {
                            return;
                        }
                    }
                    // Slow is not the same as unsupported: try again next tick.
                    Err(boombox_core::Error::DaemonNotAnswering(_)) => {}
                    Err(e) => {
                        tracing::debug!("waveform unavailable, no longer polling: {e}");
                        return;
                    }
                }
                continue;
            }
            if !wanted.load(Ordering::Relaxed) {
                continue;
            }
            match api.spectrum(SPECTRUM_BANDS).await {
                Ok(bands) => {
                    if tx.send(Message::Spectrum(bands)).is_err() {
                        return;
                    }
                }
                // Slow is not the same as unsupported: try again next tick.
                Err(boombox_core::Error::DaemonNotAnswering(_)) => {}
                Err(e) => {
                    // A daemon with no audio tap will not grow one; stop asking
                    // rather than log 30 times a second.
                    tracing::debug!("spectrum unavailable, no longer polling: {e}");
                    return;
                }
            }
        }
    });
}

/// Says once per outage when Spotify reports this computer playing and no
/// audio is coming out -- a failure that otherwise looks, from here, exactly
/// like everything working.
fn spawn_silence_poll<P>(api: &Arc<P>, tx: &mpsc::UnboundedSender<Message>)
where
    P: SpectrumApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SILENCE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut reported = false;
        loop {
            ticker.tick().await;
            match api.audio_stalled_secs().await {
                Ok(Some(secs)) if !reported => {
                    reported = true;
                    let message = format!(
                        "Spotify says this computer is playing, but no audio has come out for \
                         {secs}s. `boombox daemon --stop`, then `boombox`, starts a fresh session"
                    );
                    if tx.send(Message::Failed(message)).is_err() {
                        return;
                    }
                }
                Ok(None) => reported = false,
                _ => {}
            }
        }
    });
}

/// The seek bar is always visible, so this one is not gated on a view.
fn spawn_envelope_poll<P>(api: &Arc<P>, tx: &mpsc::UnboundedSender<Message>)
where
    P: PlayerApi + LibraryApi + RecentsApi + SpectrumApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(ENVELOPE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match api.envelope(ENVELOPE_POINTS).await {
                Ok(points) => {
                    if tx.send(Message::Envelope(points)).is_err() {
                        return;
                    }
                }
                // Slow is not the same as unsupported: try again next tick.
                // Stopping here meant one stuck moment blanked the seek bar's
                // waveform for the rest of the session.
                Err(boombox_core::Error::DaemonNotAnswering(_)) => {}
                Err(e) => {
                    tracing::debug!("envelope unavailable, no longer polling: {e}");
                    return;
                }
            }
        }
    });
}

/// Applies the palette for a cover, fetching it the first time.
///
/// Misses are remembered too: a greyscale or busy sleeve will not become
/// usable on a second look, and re-fetching it for every track on the album
/// would be pure waste.
/// Covers are a few hundred kilobytes each at full resolution, so unlike
/// the palettes they cannot simply accumulate. One sitting rarely revisits
/// more albums than this, and dropping the lot is cheaper to reason about
/// than evicting the least recently used one.
const ARTWORK_CACHE_LIMIT: usize = 12;

/// Least time between queue reads prompted by a track change.
///
/// A queue read is a real API call. Ordinary listening changes track every
/// few minutes and this never bites; skipping through the queue changes it
/// several times a second, and one read per change was most of the burst
/// that tripped the rate limiter.
const QUEUE_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

type ArtworkCache = std::collections::HashMap<String, crate::artwork::Artwork>;

fn remember_artwork(cache: &mut ArtworkCache, url: String, artwork: crate::artwork::Artwork) {
    if cache.len() >= ARTWORK_CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(url, artwork);
}

/// Returns what we already know about this cover, fetching it if we do not
/// know yet. One request yields both the colours and the picture.
fn apply_artwork(
    tx: &mpsc::UnboundedSender<Message>,
    url: String,
    cache: &ArtworkCache,
) -> Option<crate::artwork::Artwork> {
    match cache.get(&url) {
        // Seen before: either we have it or we know there is none to be had.
        Some(known) => Some(known.clone()),
        None => {
            let tx = tx.clone();
            tokio::spawn(async move {
                let artwork = crate::artwork::fetch(&url).await;
                let _ = tx.send(Message::Artwork { url, artwork });
            });
            None
        }
    }
}

fn poll_once<P>(api: &Arc<P>, tx: &mpsc::UnboundedSender<Message>)
where
    P: PlayerApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        // Spotify needs a beat before a write is reflected in player state.
        tokio::time::sleep(Duration::from_millis(400)).await;
        if let Ok(state) = api.playback_state().await {
            let _ = tx.send(Message::Playback(Box::new(state)));
        }
    });
}

fn refresh_devices<P>(api: &Arc<P>, tx: &mpsc::UnboundedSender<Message>)
where
    P: PlayerApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(match api.devices().await {
            Ok(devices) => Message::Devices(devices),
            Err(e) => Message::Failed(e.to_string()),
        });
    });
}

fn refresh_queue<P>(api: &Arc<P>, tx: &mpsc::UnboundedSender<Message>)
where
    P: PlayerApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        // A missing queue is normal with no active device; don't shout about it.
        match api.queue().await {
            Ok(queue) => {
                let _ = tx.send(Message::Queue(queue.queue));
            }
            Err(e) => tracing::debug!("queue refresh failed: {e}"),
        }
    });
}

/// Loads one page of whatever `view` shows and turns it into flat entries.
fn load_entries<P>(
    api: &Arc<P>,
    tx: &mpsc::UnboundedSender<Message>,
    view: app::View,
    offset: u32,
    append: bool,
    query: String,
) where
    P: PlayerApi + LibraryApi + RecentsApi + SpectrumApi + 'static,
{
    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = fetch_entries(&*api, &view, offset, &query).await;
        let _ = tx.send(match result {
            Ok((entries, total)) => Message::Entries { view, query, entries, total, append },
            Err(e) => Message::Failed(e.to_string()),
        });
    });
}

/// Playing from Liked Songs in the native app reports a hidden mirror
/// playlist as the context, so it turns up here as an ordinary playlist --
/// a second route to a list the sidebar already offers permanently.
///
/// Filtered by name, because nothing structural sets it apart: the API
/// describes it as a normal user-owned playlist. On an account in another
/// language this quietly stops matching, which costs one duplicate row and
/// nothing else.
fn is_liked_mirror(recent: &boombox_core::recent::Recent) -> bool {
    recent.name.as_deref() == Some(&app::View::Liked.label())
}

async fn fetch_entries<A>(
    api: &A,
    view: &app::View,
    offset: u32,
    query: &str,
) -> boombox_core::Result<(Vec<Entry>, u32)>
where
    A: LibraryApi + RecentsApi,
{
    use app::View;
    Ok(match view {
        View::Liked => {
            let page = api.saved_tracks(PAGE, offset).await?;
            (page.items.iter().map(|s| Entry::from_track(&s.track)).collect(), page.total)
        }
        View::Albums => {
            let page = api.saved_albums(PAGE, offset).await?;
            (page.items.iter().map(|s| Entry::from_album(&s.album)).collect(), page.total)
        }
        View::Playlists => {
            let page = api.my_playlists(PAGE, offset).await?;
            let mut entries: Vec<Entry> = page.items.iter().map(Entry::from_playlist).collect();
            let total = page.total;

            // Only on the first page: the recents belong at the top once,
            // not repeated every time more playlists are appended.
            if offset == 0 {
                let recents: Vec<Entry> = api
                    .recents()
                    .await
                    .unwrap_or_default()
                    .iter()
                    // What is already below under "Your playlists" does not
                    // need saying twice; the point of this section is the
                    // ones the library cannot show you.
                    .filter(|r| !entries.iter().any(|e| e.uri.as_deref() == Some(&r.uri)))
                    .filter(|r| !is_liked_mirror(r))
                    .map(Entry::from_recent)
                    .collect();
                let count = recents.len();
                entries.splice(0..0, recents);
                return Ok((entries, total.saturating_add(count as u32)));
            }
            (entries, total)
        }
        View::PlaylistItems { id, .. } => {
            let page = api.playlist_items(id, PAGE, offset).await?;
            let entries = page
                .items
                .iter()
                .filter_map(|row| row.item.as_ref())
                .filter_map(Entry::from_playing_item)
                .collect();
            (entries, page.total)
        }
        View::Search => {
            if query.trim().is_empty() {
                return Ok((Vec::new(), 0));
            }
            // Spotify caps this at ten per type, so ask for several types at
            // once rather than paging one type deeply.
            let types =
                [SearchType::Track, SearchType::Album, SearchType::Artist, SearchType::Playlist];
            let r = api.search(query, &types, app::SEARCH_PAGE, offset).await?;
            let mut entries = Vec::new();
            let mut total = 0;
            if let Some(p) = &r.tracks {
                total += p.total;
                entries.extend(p.items.iter().flatten().map(Entry::from_track));
            }
            if let Some(p) = &r.albums {
                total += p.total;
                entries.extend(p.items.iter().flatten().map(Entry::from_album));
            }
            if let Some(p) = &r.artists {
                total += p.total;
                entries.extend(p.items.iter().flatten().map(Entry::from_artist));
            }
            if let Some(p) = &r.playlists {
                total += p.total;
                entries.extend(p.items.iter().flatten().map(Entry::from_playlist));
            }
            (entries, total)
        }
        _ => (Vec::new(), 0),
    })
}

/// Every command runs on its own task; the render loop never awaits Spotify.
fn dispatch<P>(api: &Arc<P>, tx: &mpsc::UnboundedSender<Message>, command: Command)
where
    P: PlayerApi + LibraryApi + RecentsApi + SpectrumApi + 'static,
{
    match command {
        Command::RefreshAll => {
            refresh_devices(api, tx);
            refresh_queue(api, tx);
            poll_once(api, tx);
            return;
        }
        Command::RefreshDevices => return refresh_devices(api, tx),
        Command::RefreshQueue => return refresh_queue(api, tx),
        Command::LoadView(ref view, ref query) => {
            return load_entries(api, tx, view.clone(), 0, false, query.clone());
        }
        Command::LoadMore(ref view, offset, ref query) => {
            return load_entries(api, tx, view.clone(), offset, true, query.clone());
        }
        _ => {}
    }

    let api = Arc::clone(api);
    let tx = tx.clone();
    tokio::spawn(async move {
        let (result, label) = match command {
            Command::Resume => (api.play(PlayOptions::resume()).await, "playing"),
            Command::Pause => (api.pause().await, "paused"),
            Command::Next => (api.next().await, "next"),
            Command::Previous => (api.previous().await, "previous"),
            Command::Seek(ms) => (api.seek(ms).await, "seeked"),
            Command::SetVolume(v) => (api.set_volume(v).await, "volume set"),
            Command::SetShuffle(on) => {
                (api.set_shuffle(on).await, if on { "shuffle on" } else { "shuffle off" })
            }
            Command::SetRepeat(state) => (api.set_repeat(state).await, "repeat set"),
            Command::Transfer(id) => (api.transfer(&id, true).await, "transferred"),
            Command::Play(playback) => (api.play(PlayOptions::from(playback)).await, "playing"),
            Command::AddAndPlay(uri) => {
                // Remembered before playing, and deliberately not fatal if
                // it fails: a daemonless session has nowhere to keep the
                // list, and that is no reason not to play the link.
                if let Err(e) = api.remember(&uri).await {
                    tracing::debug!("not remembering {uri}: {e}");
                }
                let playback = if uri.contains(":track:") {
                    boombox_core::api::Playback::Track(uri)
                } else {
                    boombox_core::api::Playback::Context { uri, start: None }
                };
                let result = api.play(PlayOptions::from(playback)).await;
                if result.is_ok() {
                    // So the new row appears where it was just added,
                    // rather than on whenever the view is next opened.
                    load_entries(&api, &tx, app::View::Playlists, 0, false, String::new());
                }
                (result, "added, and playing it")
            }
            Command::EnqueueAll(uris) => {
                // One API call per track: the endpoint takes a single URI
                // and rejects every way of batching. Measured at about two
                // and a half a second, so a page of fifty is twenty
                // seconds of work -- which is why this reports as it goes
                // rather than returning when it is done.
                let total = uris.len();
                let mut added = 0usize;
                for uri in uris {
                    match api.add_to_queue(&uri).await {
                        Ok(()) => added += 1,
                        Err(e) => {
                            let _ =
                                tx.send(Message::Failed(format!("queued {added} of {total}: {e}")));
                            return;
                        }
                    }
                    if added.is_multiple_of(10) && added != total {
                        let _ = tx.send(Message::Progress(format!("queueing {added}/{total}")));
                    }
                }
                let _ = tx.send(Message::Progress(format!("queued {added} tracks")));
                return;
            }
            Command::ToggleSave(entry) => match entry.uri.clone() {
                Some(uri) => {
                    let uris = vec![uri];
                    // Ask first: the API has no toggle, only add and remove.
                    match api.library_contains(&uris).await {
                        Ok(flags) if flags.first().copied().unwrap_or(false) => {
                            (api.library_remove(&uris).await, "removed from library")
                        }
                        Ok(_) => (api.library_add(&uris).await, "saved to library"),
                        Err(e) => (Err(e), "save failed"),
                    }
                }
                None => (Ok(()), "nothing to save"),
            },
            Command::Enqueue(entry) => match entry.uri.clone() {
                // Only a track can be queued; an album or artist URI is
                // rejected by the API with a message nobody can act on.
                Some(uri) if entry.kind == boombox_core::api::EntryKind::Track => {
                    (api.add_to_queue(&uri).await, "added to queue")
                }
                Some(_) => (Ok(()), "only tracks can be queued"),
                None => (Ok(()), "nothing to queue"),
            },
            Command::RefreshAll
            | Command::RefreshDevices
            | Command::RefreshQueue
            | Command::LoadView(..)
            | Command::LoadMore(..) => unreachable!("handled above"),
        };

        let _ = tx.send(match result {
            Ok(()) => Message::Done(label),
            Err(e) => Message::Failed(e.to_string()),
        });
    });
}

#[cfg(test)]
mod recents_tests {
    use boombox_core::recent::Recent;

    fn named(name: &str) -> Recent {
        Recent {
            uri: "spotify:playlist:37i9dQZF1F5ExampleLkd1".into(),
            name: Some(name.into()),
            image: None,
            last_played: 0,
            plays: 1,
        }
    }

    /// The mirror is Liked Songs over again, which is already one keystroke
    /// away.
    #[test]
    fn the_liked_songs_mirror_is_not_offered_as_a_recent() {
        assert!(super::is_liked_mirror(&named("Liked Songs")));
        assert!(!super::is_liked_mirror(&named("Daily Mix 1")));
        // Not yet resolved: nothing to compare, and it must not be dropped
        // on the strength of a name it does not have yet.
        assert!(!super::is_liked_mirror(&Recent { name: None, ..named("x") }));
    }
}
