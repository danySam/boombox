use std::time::{Duration, Instant};

use boombox_core::api::{Device, Entry, Playback, PlaybackState, PlayingItem, RepeatState};

use crate::action::Action;
use crate::palette::Palette;

/// How long a toast stays on screen.
const TOAST_TTL: Duration = Duration::from_secs(4);

/// How fast peak markers fall, in full-scale units per second. A peak at the
/// top of the pane takes about 2.2s to reach the bottom: slow enough to read
/// comfortably, still fast enough to track a changing mix.
const PEAK_DECAY_PER_SEC: f32 = 0.45;

/// How far each band bleeds into its neighbours, per step of distance.
///
/// A raw FFT draws as a row of independent spikes; spreading each band's
/// energy outward makes the display read as one connected surface, which is
/// most of what separates a polished analyser from a plotted transform.
/// cava calls this "monstercat smoothing".
const SPREAD: f32 = 1.6;
/// Bands further than this contribute nothing worth computing.
const SPREAD_REACH: usize = 4;

/// How fast bands sink, in full-scale units per second. Rising is immediate:
/// an analyser that lags the attack feels broken, but one that drops
/// instantly flickers.
const BAND_FALL_PER_SEC: f32 = 1.6;

/// Points the scope draws. The daemon is asked for more than this so there is
/// slack to search for a trigger point.
pub const SCOPE_SPAN: usize = 256;

/// Traces kept behind the current one, drawn dimmer.
///
/// A phosphor scope does not clear between sweeps, and that afterglow is most
/// of why one looks alive: the space around the trace carries where the signal
/// has just been rather than sitting empty.
pub const SCOPE_TRAILS: usize = 5;

/// How fast the scope's automatic gain follows the signal, per second.
///
/// Asymmetric on purpose. Turning the gain down when the music gets loud has
/// to be quick or the trace clips; turning it back up when the music goes
/// quiet has to be slow, or every gap gets amplified to full height and the
/// trace pumps.
const GAIN_FALL_PER_SEC: f32 = 6.0;
const GAIN_RISE_PER_SEC: f32 = 0.8;

/// Frames of spectrum history kept for the waterfall. Wider than any sensible
/// terminal, so the display never runs out of past to draw.
const HISTORY: usize = 256;

/// Index of the first rising zero crossing within `limit`, or 0.
///
/// A rising edge rather than any crossing, so the trace always starts the same
/// way up and successive frames line up with each other.
fn trigger_offset(samples: &[f32], limit: usize) -> usize {
    let limit = limit.min(samples.len().saturating_sub(1));
    (0..limit).find(|i| samples[*i] <= 0.0 && samples[i + 1] > 0.0).unwrap_or(0)
}

/// Bleeds each band into its neighbours, keeping the loudest contribution.
///
/// Taking the max rather than summing matters: summing lifts the whole display
/// as bands get busier, so quiet passages and dense ones stop being
/// distinguishable.
fn spread_bands(bands: &[f32]) -> Vec<f32> {
    if bands.len() < 3 {
        return bands.to_vec();
    }
    let mut out = bands.to_vec();
    for (i, value) in bands.iter().enumerate() {
        if *value <= 0.0 {
            continue;
        }
        for distance in 1..=SPREAD_REACH {
            let shed = value / SPREAD.powi(distance as i32);
            if shed <= 0.0 {
                break;
            }
            if let Some(left) = i.checked_sub(distance) {
                out[left] = out[left].max(shed);
            }
            if let Some(right) = out.get_mut(i + distance) {
                *right = right.max(shed);
            }
        }
    }
    out
}

/// Which visualisation the Spectrum view is showing.
/// How long without a keypress before the stage goes full-bleed. "A couple
/// of seconds": long enough not to trip while you are still reaching for a
/// key, short enough that leaving it alone visibly does something.
/// Smallest cover worth downloading. Spotify's 64-pixel thumbnail is fine
/// for sampling colours and hopeless to look at, and one download has to
/// serve both.
const MIN_COVER_WIDTH: u32 = 200;

/// How long to let a queue jump settle before deciding it missed.
///
/// The API reports the previous track for a moment after a change, so
/// judging straight away would call every jump a miss and correct a jump
/// that was fine.
const JUMP_SETTLE: Duration = Duration::from_millis(2500);

const IDLE_AFTER: Duration = Duration::from_secs(3);

/// How long to wait for more volume keys before sending one change.
///
/// Every press used to be its own API call computed from the last polled
/// reading -- so a run of taps all read the same stale value, all asked
/// for the same target, and all but one were wasted. Eight presses moved
/// the volume by one step. Gathering them locally fixes that and stops the
/// rate limiter being provoked at the same time.
const VOLUME_COALESCE: Duration = Duration::from_millis(220);

/// Backstop for how long the requested volume is shown in place of the
/// reported one.
///
/// Normally the local figure is dropped as soon as the API agrees with it.
/// This only covers the case where it never does -- a change that failed,
/// or a device that reports something we will never match. Measured
/// against the real thing: a change takes about eleven seconds to appear
/// in the API, so anything shorter makes the display snap back to a stale
/// number and then jump forward again.
const VOLUME_SETTLE: Duration = Duration::from_secs(20);

/// How long a seek target stays the base for the next seek key.
///
/// Long enough to chain a run of presses: without it each one reads the
/// same polled position and asks for the same place, so a run of taps
/// makes a single jump -- the bug volume had. Short enough that a target
/// from a minute ago never bases a fresh seek, by which time the track
/// has played on past it.
const SEEK_CHAIN: Duration = Duration::from_millis(1200);

/// How long to wait for more seek keys before sending one seek.
///
/// A run of presses is one intent: the bar follows every press at once,
/// and the API hears the place the user stopped on. Ten taps used to be
/// ten writes, each one lurching the bar as its confirmation arrived.
const SEEK_COALESCE: Duration = Duration::from_millis(300);

/// How long to wait before deciding two taps of space cancelled out.
///
/// Short enough that a single press still feels immediate -- the glyph
/// has already flipped locally by then -- and long enough that a double
/// tap is recognised as the nothing it is.
const PLAYING_COALESCE: Duration = Duration::from_millis(200);

/// Backstops, for a change the player never comes to report.
const SEEK_SETTLE: Duration = Duration::from_secs(10);
const PLAYING_SETTLE: Duration = Duration::from_secs(8);

/// How far out a reported position may be and still count as agreement:
/// the track plays on between the write landing and the poll seeing it.
const POSITION_TOLERANCE_MS: u64 = 2_500;

/// How far out the reported volume may be and still count as agreement.
/// The device rounds -- ask for 55 and it reads back 54 -- so an exact
/// match would never arrive.
const VOLUME_TOLERANCE: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualMode {
    Bars,
    Spectrogram,
    Scope,
}

impl VisualMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bars => "Bars",
            Self::Spectrogram => "Spectrogram",
            Self::Scope => "Oscilloscope",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Bars => Self::Spectrogram,
            Self::Spectrogram => Self::Scope,
            Self::Scope => Self::Bars,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    NowPlaying,
    Search,
    Liked,
    Albums,
    Playlists,
    Spectrum,
    /// Drilled into one playlist. Not reachable from the sidebar.
    PlaylistItems {
        id: String,
        name: String,
    },
    Queue,
    Devices,
}

impl View {
    /// The sidebar entries, in order.
    /// What the palette can be filtered to. Now Playing and the
    /// visualisation are the stage, not destinations, so they are not here.
    pub const BROWSE: [View; 6] =
        [View::Search, View::Liked, View::Albums, View::Playlists, View::Queue, View::Devices];

    pub fn label(&self) -> String {
        match self {
            Self::NowPlaying => "Now Playing".into(),
            Self::Search => "Search".into(),
            Self::Liked => "Liked Songs".into(),
            Self::Albums => "Albums".into(),
            Self::Playlists => "Playlists".into(),
            Self::Spectrum => "Spectrum".into(),
            Self::PlaylistItems { name, .. } => name.clone(),
            Self::Queue => "Queue".into(),
            Self::Devices => "Devices".into(),
        }
    }

    /// The Spotify context this list plays as, if it has one.
    ///
    /// Having one is what lets playback carry on past the page we have
    /// loaded: name the playlist and Spotify plays all of it. Search
    /// results have no such URI, so they fall back to sending the tracks.
    ///
    /// `spotify:collection:tracks` is Liked Songs. The Web API accepts it
    /// and does not document it -- verified against a live account, where
    /// it starts the saved-tracks list and honours an offset by URI.
    pub fn context_uri(&self) -> Option<String> {
        match self {
            Self::Liked => Some("spotify:collection:tracks".into()),
            Self::PlaylistItems { id, .. } => Some(format!("spotify:playlist:{id}")),
            _ => None,
        }
    }

    /// Views backed by the paged entry list rather than their own state.
    pub fn is_entry_list(&self) -> bool {
        matches!(
            self,
            Self::Search
                | Self::Liked
                | Self::Albums
                | Self::Playlists
                | Self::PlaylistItems { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: Instant,
}

/// Work the update loop wants performed against Spotify. Returned rather than
/// executed so `App::update` stays synchronous and testable.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Resume whatever is already loaded.
    Resume,
    Pause,
    Next,
    Previous,
    Seek(u64),
    SetVolume(u32),
    SetShuffle(bool),
    SetRepeat(RepeatState),
    Transfer(String),
    RefreshAll,
    RefreshDevices,
    RefreshQueue,
    /// Load the first page of whatever the current view shows. The string
    /// is the search query, empty for views that do not need one.
    LoadView(View, String),
    /// Append the next page to the current list.
    LoadMore(View, u32, String),
    /// Start playing, however the chosen row implies.
    Play(Playback),
    /// Add a pasted link to the list, and play it.
    ///
    /// Distinct from [`Self::Play`] because it also asks the daemon to
    /// write the context down first. That call resolves the name too, so
    /// the row is complete when the list reloads rather than appearing as
    /// a raw id and correcting itself a moment later.
    AddAndPlay(String),
    ToggleSave(Entry),
    /// Append one entry to the play queue.
    Enqueue(Entry),
    /// Append several, one API call at a time.
    EnqueueAll(Vec<String>),
}

/// A jump waiting to be confirmed by the next poll.
#[derive(Debug, Clone)]
struct Expecting {
    /// The track the jump was aimed at.
    uri: String,
    /// The queue rows to play instead if it missed. Exact, but only as
    /// long as the queue we can see.
    fallback: Vec<String>,
    asked_at: Instant,
}

/// Spotify caps search at ten results per type per request.
pub const SEARCH_PAGE: u32 = 10;

/// The list a playlist was opened from, kept whole.
///
/// Leaving a playlist used to reload this from Spotify, with the cursor back
/// at the top, so reaching the next playlist meant scrolling down to it
/// again. Keeping it is also instant, and keeps whatever had been paged in.
struct Parent {
    view: View,
    entries: Vec<Entry>,
    total: u32,
    index: usize,
    unpaged: usize,
}

pub struct App {
    pub view: View,
    pub focus: Focus,
    pub sidebar_index: usize,

    playback: Option<PlaybackState>,
    playback_at: Instant,

    pub devices: Vec<Device>,
    pub device_index: usize,
    pub queue: Vec<PlayingItem>,
    pub queue_index: usize,

    pub entries: Vec<Entry>,
    pub entry_index: usize,
    pub entry_total: u32,
    /// How many entries at the top of the list did not come from the paged
    /// source, and so must not be counted into the next page's offset.
    unpaged: usize,
    /// The query whose results are on screen -- not the search box, which may
    /// already hold the next one. Paging and late results are checked
    /// against this.
    searched: String,
    /// How many pages of search results are on screen, so the next request
    /// can ask each type for its own next ten.
    search_pages: u32,
    /// A page of search results came back empty: there is nothing further.
    search_done: bool,
    /// Set when going back put a saved list on screen, so `update` does not
    /// treat the change of view as an arrival and fetch that list again.
    restored: bool,
    pub loading: bool,
    pub query: String,
    pub typing: bool,
    /// The link prompt's buffer while it is open.
    ///
    /// Separate from `query` so opening it never disturbs a search you
    /// had already run and might still want to go back to.
    pub link: Option<String>,
    /// Latest band magnitudes, 0.0..=1.0. Empty when nothing is decoding here.
    pub spectrum: Vec<f32>,
    /// Falling peak markers, one per band.
    pub peaks: Vec<f32>,
    /// What actually gets drawn: spread across neighbours, with a gravity
    /// release. `spectrum` stays the raw reading.
    pub smoothed: Vec<f32>,
    /// Recent frames, oldest first, for the waterfall.
    pub history: std::collections::VecDeque<Vec<f32>>,
    pub visual: Option<VisualMode>,
    /// Derived from the current track, so every visualisation agrees and the
    /// display changes when the music does.
    pub palette: Palette,
    /// The current album cover, once it has been fetched. Shared rather
    /// than copied because the same cover is held in the cache.
    pub cover: Option<std::sync::Arc<crate::artwork::Cover>>,
    /// URL of the cover currently held, so the graphics layer can tell
    /// when the picture has actually changed.
    pub cover_url: Option<String>,
    /// How covers get drawn. When the terminal draws real pixels the cell
    /// grid leaves the area blank and the graphics layer fills it.
    pub graphics: crate::graphics::Protocol,
    /// Recent waveform, -1.0..=1.0, triggered and gain-corrected, ready to
    /// draw.
    pub waveform: Vec<f32>,
    /// Previous traces, oldest first, for the afterglow.
    pub scope_trails: std::collections::VecDeque<Vec<f32>>,
    scope_gain: f32,
    scope_at: Instant,
    /// Peak amplitude across the current track, for the seek bar.
    pub envelope: Vec<f32>,
    spectrum_at: Instant,
    /// Where Back returns to from a drill-down.
    parent: Option<Parent>,
    /// A queue jump that has been asked for but not yet confirmed.
    ///
    /// Jumping into a context is one call and keeps the list playing past
    /// the twenty rows the queue shows -- but the API accepts an offset
    /// naming a track the context does not contain, answers 204, and
    /// plays something else. Rather than guess whether a row came from the
    /// context or was queued by hand, this asks, then checks the next
    /// playback poll and puts it right if it went astray.
    expecting: Option<Expecting>,

    /// Whether the browse palette is over the stage. The stage keeps
    /// playing behind it; this only decides what gets drawn on top and
    /// where the cursor keys go.
    pub browse_open: bool,
    /// When a key last arrived. The stage goes full-bleed once this gets
    /// old enough, which is the whole point of a player you leave running.
    pub last_input: Instant,

    pub toast: Option<Toast>,
    pub show_help: bool,
    pub should_quit: bool,
    pub connected_to_daemon: bool,
    /// Build string of the daemon, when one is answering. Shown in help so
    /// "which build is actually running?" can be answered without leaving
    /// the TUI -- the daemon outlives the binary that started it.
    pub daemon_version: Option<String>,
    /// The Connect device this machine's daemon registered, when there is
    /// one. Matched by id: a device can be renamed to anything at all.
    pub our_device_id: Option<String>,

    seek_step_ms: u64,
    volume_step: u32,
    /// What the user has asked for, ahead of the API confirming it. Shown
    /// in place of the polled figure so the display answers the keypress
    /// rather than the network.
    pending_volume: Option<u32>,
    /// When the last volume key arrived.
    volume_touched: Instant,
    /// Whether `pending_volume` has been sent yet.
    volume_sent: bool,
    /// Where the last seek key asked to go, so the next one counts from
    /// there rather than from a position the API has not caught up to.
    pending_seek: Option<u64>,
    /// When that target was set.
    seek_touched: Instant,
    /// Whether `pending_seek` has been sent yet.
    seek_sent: bool,
    /// Whether the user has asked to be playing or paused, ahead of the
    /// API agreeing. Two taps of space inside the window cancel out and
    /// nothing is sent at all.
    pending_playing: Option<bool>,
    /// When the last play/pause key arrived.
    playing_touched: Instant,
    /// Whether `pending_playing` has been sent yet.
    playing_sent: bool,
}

impl App {
    pub fn new(seek_step_secs: u32, connected_to_daemon: bool) -> Self {
        Self {
            view: View::NowPlaying,
            focus: Focus::Main,
            sidebar_index: 0,
            playback: None,
            playback_at: Instant::now(),
            devices: Vec::new(),
            device_index: 0,
            queue: Vec::new(),
            queue_index: 0,
            entries: Vec::new(),
            unpaged: 0,
            searched: String::new(),
            search_pages: 0,
            search_done: false,
            restored: false,
            entry_index: 0,
            entry_total: 0,
            loading: false,
            query: String::new(),
            typing: false,
            link: None,
            spectrum: Vec::new(),
            peaks: Vec::new(),
            smoothed: Vec::new(),
            history: std::collections::VecDeque::new(),
            visual: None,
            palette: Palette::default(),
            cover: None,
            cover_url: None,
            graphics: crate::graphics::Protocol::Cells,
            waveform: Vec::new(),
            scope_trails: std::collections::VecDeque::new(),
            scope_gain: 1.0,
            scope_at: Instant::now(),
            envelope: Vec::new(),
            spectrum_at: Instant::now(),
            parent: None,
            expecting: None,
            browse_open: false,
            last_input: Instant::now(),
            toast: None,
            show_help: false,
            should_quit: false,
            connected_to_daemon,
            daemon_version: None,
            our_device_id: None,
            seek_step_ms: u64::from(seek_step_secs) * 1000,
            volume_step: 5,
            pending_volume: None,
            volume_touched: Instant::now(),
            volume_sent: false,
            pending_seek: None,
            seek_touched: Instant::now(),
            seek_sent: false,
            pending_playing: None,
            playing_touched: Instant::now(),
            playing_sent: false,
        }
    }

    /// Returns true when the playing item changed, which is the caller's cue
    /// that the queue it is showing is now wrong.
    pub fn set_playback(&mut self, state: Option<PlaybackState>) -> bool {
        let previous = self.current_uri();
        self.playback = state;
        self.playback_at = Instant::now();
        self.reconcile_volume();
        self.reconcile_seek();
        self.reconcile_playing();
        let changed = previous != self.current_uri();
        if changed {
            // A target measured against the track that just ended would
            // otherwise base the next seek in the one that replaced it.
            self.pending_seek = None;
            self.seek_sent = false;
            self.palette = match self.current_uri() {
                Some(uri) => Palette::for_uri(&uri),
                None => Palette::default(),
            };
        }
        changed
    }

    fn current_uri(&self) -> Option<String> {
        self.playback.as_ref()?.item.as_ref().and_then(PlayingItem::uri).map(str::to_owned)
    }

    /// What the player last reported, with the clock advanced to now and
    /// nothing of ours laid over it.
    ///
    /// Arithmetic uses this, never [`Self::playback`]: basing a seek on a
    /// position we are already previewing would add a step to a step and
    /// walk away from the music.
    fn reported(&self) -> Option<PlaybackState> {
        self.playback.as_ref().map(|s| s.advanced_by(self.playback_at.elapsed()))
    }

    /// Playback as the user has asked for it, with the progress clock
    /// advanced to now so the timer moves between polls instead of
    /// stepping.
    ///
    /// Anything outstanding is shown in place of the reported value. This
    /// is what makes a keypress land on screen at once rather than a poll
    /// or two later, and it is why the seek bar can be scrubbed.
    pub fn playback(&self) -> Option<PlaybackState> {
        let mut state = self.playback.clone()?;
        if let Some(playing) = self.pending_playing {
            state.is_playing = playing;
            state.pending.playing = true;
        }
        match self.pending_seek {
            // Held still at the target: the bar answers the keys, not the
            // clock, until the seek is sent and confirmed.
            Some(target) => {
                let duration = state.duration();
                state.progress_ms = Some(if duration > 0 { target.min(duration) } else { target });
                state.pending.position = true;
            }
            None => state = state.advanced_by(self.playback_at.elapsed()),
        }
        if let Some(volume) = self.pending_volume
            && let Some(device) = state.device.as_mut()
        {
            device.volume_percent = Some(volume);
            state.pending.volume = true;
        }
        Some(state)
    }

    pub fn set_devices(&mut self, devices: Vec<Device>) {
        self.device_index = self.device_index.min(devices.len().saturating_sub(1));
        self.devices = devices;
    }

    pub fn set_queue(&mut self, queue: Vec<PlayingItem>) {
        self.queue_index = self.queue_index.min(queue.len().saturating_sub(1));
        self.queue = queue;
    }

    /// Replaces the list (first page) or appends to it (subsequent pages).
    pub fn set_entries(&mut self, entries: Vec<Entry>, total: u32, append: bool) {
        self.loading = false;
        if self.view == View::Search {
            if append {
                self.search_pages += 1;
                self.search_done = entries.is_empty();
            } else {
                self.search_pages = 1;
                self.search_done = false;
            }
        }
        if append && self.view == View::Search {
            self.merge_search_page(entries);
        } else if append {
            self.entries.extend(entries);
        } else {
            self.entries = entries;
            self.entry_index = 0;
        }
        self.entry_total = total;
        // Recents sit above the paged list without being part of it, so
        // they cannot count toward the next page's offset -- doing so
        // would skip exactly that many real playlists.
        self.unpaged = self
            .entries
            .iter()
            .take_while(|e| e.kind == boombox_core::api::EntryKind::Recent)
            .count();
        self.entry_index = self.entry_index.min(self.entries.len().saturating_sub(1));
    }

    pub fn set_spectrum(&mut self, bands: Vec<f32>) {
        // Decay against wall-clock rather than frame count: a stalled or
        // resized terminal must not change how fast the peaks fall.
        let elapsed = self.spectrum_at.elapsed().as_secs_f32();
        self.spectrum_at = Instant::now();
        let fall = PEAK_DECAY_PER_SEC * elapsed;

        if self.peaks.len() != bands.len() {
            self.peaks = bands.clone();
        } else {
            for (peak, value) in self.peaks.iter_mut().zip(&bands) {
                *peak = (*peak - fall).max(*value).max(0.0);
            }
        }

        // Spatial first, then temporal: spreading an already-decayed frame
        // would smear the decay outward too.
        let spread = spread_bands(&bands);
        if self.smoothed.len() != spread.len() {
            self.smoothed = spread.clone();
        } else {
            let fall = BAND_FALL_PER_SEC * elapsed;
            for (shown, target) in self.smoothed.iter_mut().zip(&spread) {
                *shown = if *target >= *shown { *target } else { (*shown - fall).max(*target) };
            }
        }

        // Store the raw frame. Smoothing is right for the bars and wrong here:
        // spreading blurs harmonics into each other and the gravity release
        // smears energy across time, so the waterfall turns into cloud. What
        // makes a spectrogram readable is exactly the structure smoothing
        // removes.
        if !bands.is_empty() {
            self.history.push_back(bands.clone());
            while self.history.len() > HISTORY {
                self.history.pop_front();
            }
        }
        self.spectrum = bands;
    }

    pub fn set_waveform(&mut self, points: Vec<f32>) {
        if points.is_empty() {
            self.waveform.clear();
            self.scope_trails.clear();
            return;
        }

        // Start the trace at a rising zero crossing. Without it the window
        // begins wherever the buffer happens to sit, so a steady tone slides
        // across the pane every frame instead of standing still.
        let start = trigger_offset(&points, points.len().saturating_sub(SCOPE_SPAN));
        let end = (start + SCOPE_SPAN).min(points.len());
        let window = &points[start..end];

        let peak = window.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        let target = if peak > 0.01 { (0.92 / peak).min(40.0) } else { self.scope_gain };

        let elapsed = self.scope_at.elapsed().as_secs_f32();
        self.scope_at = Instant::now();
        let rate = if target < self.scope_gain { GAIN_FALL_PER_SEC } else { GAIN_RISE_PER_SEC };
        let step = rate * elapsed * self.scope_gain.max(1.0);
        self.scope_gain = if target < self.scope_gain {
            (self.scope_gain - step).max(target)
        } else {
            (self.scope_gain + step).min(target)
        };

        self.waveform = window.iter().map(|s| (s * self.scope_gain).clamp(-1.0, 1.0)).collect();
    }

    /// The scope needs raw samples; the other modes work from the bands.
    pub fn wants_waveform(&self) -> bool {
        self.visual == Some(VisualMode::Scope)
    }

    pub fn set_envelope(&mut self, points: Vec<f32>) {
        self.envelope = points;
    }

    /// Replaces the palette with one read from artwork.
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// The smallest cover for the current track, which is all the colour
    /// extraction needs and the cheapest thing to fetch.
    pub fn set_cover(
        &mut self,
        url: Option<String>,
        cover: Option<std::sync::Arc<crate::artwork::Cover>>,
    ) {
        self.cover_url = url;
        self.cover = cover;
    }

    /// Whether the cell grid should draw the cover itself.
    pub fn draws_cover_in_cells(&self) -> bool {
        self.graphics == crate::graphics::Protocol::Cells
    }

    /// The cover to fetch.
    ///
    /// Spotify offers roughly 640, 300 and 64 pixels. The smallest is
    /// plenty for reading colours off but far too coarse to draw, so this
    /// takes the smallest that is still big enough to look at -- one
    /// download then serves both purposes.
    pub fn artwork_url(&self) -> Option<String> {
        let images = self.playback.as_ref()?.item.as_ref()?.images();
        tracing::debug!(
            "cover candidates: {:?}",
            images.iter().map(|i| (i.width, i.height)).collect::<Vec<_>>()
        );
        images
            .iter()
            .filter(|i| i.width.is_some_and(|w| w >= MIN_COVER_WIDTH))
            .min_by_key(|i| i.width.unwrap_or(u32::MAX))
            .or_else(|| images.iter().max_by_key(|i| i.width.unwrap_or(0)))
            .or_else(|| images.last())
            .map(|i| i.url.clone())
    }

    /// Off, then each visualisation, then off again. Folding "off" into the
    /// cycle means the stage is chosen with one key rather than a key and a
    /// separate toggle.
    pub fn cycle_visual(&mut self) {
        self.visual = match self.visual {
            None => Some(VisualMode::Bars),
            Some(VisualMode::Scope) => None,
            Some(mode) => Some(mode.next()),
        };
    }

    /// Whether the spectrum needs polling this frame. Driven by the stage,
    /// not by which list happens to be open: the visualisation keeps running
    /// underneath the palette.
    pub fn wants_spectrum(&self) -> bool {
        matches!(self.visual, Some(VisualMode::Bars | VisualMode::Spectrogram))
    }

    /// Records that the user is still here. Anything that redraws on its own
    /// -- polling, the audio tap -- deliberately does not call this.
    pub fn note_input(&mut self) {
        self.last_input = Instant::now();
    }

    /// Whether to hand the whole screen to the visualisation.
    ///
    /// Deliberately narrow. Idling into a black rectangle while paused, or
    /// while the user is mid-search, would be worse than not idling at all.
    pub fn is_idle(&self) -> bool {
        self.visual.is_some()
            && !self.browse_open
            && !self.typing
            && !self.show_help
            && self.playback.as_ref().is_some_and(|s| s.is_playing)
            && self.last_input.elapsed() >= IDLE_AFTER
    }

    /// Pretends the volume keys stopped long enough ago to flush, so a
    /// test does not have to sleep through the coalescing window.
    #[cfg(test)]
    pub(crate) fn expire_volume_coalesce(&mut self) {
        self.volume_touched = Instant::now() - VOLUME_COALESCE;
    }

    /// Pretends the seek keys stopped long enough ago that the next one
    /// starts from the live position again.
    #[cfg(test)]
    pub(crate) fn expire_seek_chain(&mut self) {
        self.seek_touched = Instant::now() - SEEK_CHAIN;
    }

    /// Whether a volume change is still in flight -- the user has asked
    /// for something the API has not yet confirmed.
    pub fn volume_pending(&self) -> bool {
        self.pending_volume.is_some()
    }

    /// What to show: the figure the user asked for while one is
    /// outstanding, otherwise whatever the API last reported.
    pub fn volume(&self) -> Option<u32> {
        self.pending_volume.or_else(|| self.playback.as_ref()?.volume())
    }

    /// When [`flush_volume`] next needs calling, or `None` when there is
    /// nothing outstanding -- so an idle TUI is not woken for this.
    ///
    /// [`flush_volume`]: Self::flush_volume
    pub fn volume_deadline(&self) -> Option<Instant> {
        self.pending_volume?;
        let wait = if self.volume_sent { VOLUME_SETTLE } else { VOLUME_COALESCE };
        Some(self.volume_touched + wait)
    }

    /// Hands control of the displayed volume back to the API once it
    /// reports something close enough to what was asked for.
    ///
    /// Waiting on a timer alone would be wrong in both directions: too
    /// short and the display snaps back to a stale reading before the
    /// change lands, too long and a failed change lingers as a lie.
    fn reconcile_volume(&mut self) {
        let (Some(wanted), true) = (self.pending_volume, self.volume_sent) else {
            return;
        };
        if let Some(reported) = self.playback.as_ref().and_then(|s| s.volume())
            && reported.abs_diff(wanted) <= VOLUME_TOLERANCE
        {
            self.pending_volume = None;
        }
    }

    /// Everything with a deadline that has come due, in one call: the
    /// loop wakes once for the earliest of them and asks what to do.
    pub fn flush_due(&mut self) -> Vec<Command> {
        [self.flush_volume(), self.flush_seek(), self.flush_playing()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// When the loop next needs waking, or `None` when nothing is
    /// outstanding -- so an idle TUI sleeps instead of ticking.
    pub fn next_deadline(&self) -> Option<Instant> {
        [self.volume_deadline(), self.seek_deadline(), self.playing_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    fn seek_deadline(&self) -> Option<Instant> {
        self.pending_seek?;
        let wait = if self.seek_sent { SEEK_SETTLE } else { SEEK_COALESCE };
        Some(self.seek_touched + wait)
    }

    fn playing_deadline(&self) -> Option<Instant> {
        self.pending_playing?;
        let wait = if self.playing_sent { PLAYING_SETTLE } else { PLAYING_COALESCE };
        Some(self.playing_touched + wait)
    }

    /// Sends the place the keys stopped on, once they have stopped.
    fn flush_seek(&mut self) -> Option<Command> {
        let target = self.pending_seek?;
        if !self.seek_sent {
            if self.seek_touched.elapsed() < SEEK_COALESCE {
                return None;
            }
            self.seek_sent = true;
            return Some(Command::Seek(target));
        }
        if self.seek_touched.elapsed() >= SEEK_SETTLE {
            self.pending_seek = None;
            self.seek_sent = false;
        }
        None
    }

    /// Sends a play or a pause, unless the taps cancelled each other out.
    fn flush_playing(&mut self) -> Option<Command> {
        let wanted = self.pending_playing?;
        if !self.playing_sent {
            if self.playing_touched.elapsed() < PLAYING_COALESCE {
                return None;
            }
            // Two taps inside the window leave the player where it
            // already was, so there is nothing to ask for.
            if self.playback.as_ref().is_some_and(|s| s.is_playing == wanted) {
                self.pending_playing = None;
                return None;
            }
            self.playing_sent = true;
            return Some(if wanted { Command::Resume } else { Command::Pause });
        }
        if self.playing_touched.elapsed() >= PLAYING_SETTLE {
            self.pending_playing = None;
            self.playing_sent = false;
        }
        None
    }

    /// Hands the position back to the API once it reports somewhere near
    /// where the seek asked to be.
    fn reconcile_seek(&mut self) {
        let (Some(target), true) = (self.pending_seek, self.seek_sent) else {
            return;
        };
        if let Some(reported) = self.playback.as_ref().and_then(|s| s.progress_ms)
            && reported.abs_diff(target) <= POSITION_TOLERANCE_MS
        {
            self.pending_seek = None;
            self.seek_sent = false;
            // The clock starts again from the poll that agreed.
            self.playback_at = Instant::now();
        }
    }

    fn reconcile_playing(&mut self) {
        let (Some(wanted), true) = (self.pending_playing, self.playing_sent) else {
            return;
        };
        if self.playback.as_ref().is_some_and(|s| s.is_playing == wanted) {
            self.pending_playing = None;
            self.playing_sent = false;
        }
    }

    /// Pretends the seek keys stopped long enough ago to flush.
    #[cfg(test)]
    pub(crate) fn expire_seek_coalesce(&mut self) {
        self.seek_touched = Instant::now() - SEEK_COALESCE;
    }

    /// Pretends the play/pause keys stopped long enough ago to flush.
    #[cfg(test)]
    pub(crate) fn expire_playing_coalesce(&mut self) {
        self.playing_touched = Instant::now() - PLAYING_COALESCE;
    }

    /// Sends the accumulated volume once the keys have stopped, then drops
    /// the local figure if the API never catches up.
    pub fn flush_volume(&mut self) -> Option<Command> {
        let wanted = self.pending_volume?;
        if !self.volume_sent {
            if self.volume_touched.elapsed() < VOLUME_COALESCE {
                return None;
            }
            self.volume_sent = true;
            return Some(Command::SetVolume(wanted));
        }
        if self.volume_touched.elapsed() >= VOLUME_SETTLE {
            self.pending_volume = None;
        }
        None
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.entry_index)
    }

    pub fn is_typing(&self) -> bool {
        self.typing || self.link.is_some()
    }

    /// The link prompt's contents, if it is open.
    pub fn link_prompt(&self) -> Option<&str> {
        self.link.as_deref()
    }

    /// Entries fetched for `view`, applied only if that is still the list on
    /// screen.
    ///
    /// Fetches finish in their own time. Without this, a playlist still
    /// loading when you left it would land on top of the list you went back
    /// to, and results for an earlier search would replace a later one's.
    pub fn accept_entries(
        &mut self,
        view: &View,
        query: &str,
        entries: Vec<Entry>,
        total: u32,
        append: bool,
    ) {
        let current = *view == self.view && (*view != View::Search || query == self.searched);
        if current {
            self.set_entries(entries, total, append);
        }
    }

    /// Adds a further page of search results under the headings they belong
    /// to, rather than after the last group -- which put a second "Tracks"
    /// heading below the playlists. The cursor stays on the row it was on.
    fn merge_search_page(&mut self, page: Vec<Entry>) {
        let selected = self.entry_index;
        let mut rows: Vec<(usize, Entry)> =
            std::mem::take(&mut self.entries).into_iter().chain(page).enumerate().collect();
        // Kinds keep the order they first appeared in, which is Spotify's.
        let mut kinds = Vec::new();
        for (_, entry) in &rows {
            if !kinds.contains(&entry.kind) {
                kinds.push(entry.kind);
            }
        }
        rows.sort_by_key(|(_, entry)| kinds.iter().position(|kind| *kind == entry.kind));
        self.entry_index = rows.iter().position(|(original, _)| *original == selected).unwrap_or(0);
        self.entries = rows.into_iter().map(|(_, entry)| entry).collect();
    }

    /// The list's name, with where it was opened from when there is a way back.
    pub fn list_title(&self) -> String {
        match &self.parent {
            Some(parent) => format!("{} \u{203a} {}", parent.view.label(), self.view.label()),
            None => self.view.label(),
        }
    }

    /// Whether Backspace leads back to a list rather than out of the browser.
    pub fn can_go_back(&self) -> bool {
        self.parent.is_some()
    }

    pub fn has_more(&self) -> bool {
        // Spotify's search totals move between pages, so an empty page is the
        // only reliable sign that there is nothing further.
        if self.view == View::Search && self.search_done {
            return false;
        }
        (self.entries.len() as u32) < self.entry_total
    }

    /// A pasted link, turned into something to play.
    ///
    /// Rejections say what was wrong rather than failing silently, because
    /// the two ways to get this wrong -- a link to something that is not a
    /// Spotify object, and a half-copied one -- look identical on screen.
    fn submit_link(&mut self, buffer: &str) -> Option<Command> {
        if buffer.trim().is_empty() {
            return None;
        }
        let Some(uri) = boombox_core::uri::normalise(buffer) else {
            self.error("That does not look like a Spotify link");
            return None;
        };
        self.info("Added, and playing it");
        Some(Command::AddAndPlay(uri))
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), kind: ToastKind::Info, at: Instant::now() });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), kind: ToastKind::Error, at: Instant::now() });
    }

    pub fn visible_toast(&self) -> Option<&Toast> {
        self.toast.as_ref().filter(|t| t.at.elapsed() < TOAST_TTL)
    }

    fn list_len(&self) -> usize {
        match &self.view {
            View::Queue => self.queue.len() + usize::from(self.playing_row()),
            View::Devices => self.devices.len(),
            View::NowPlaying => 0,
            v if v.is_entry_list() => self.entries.len(),
            _ => 0,
        }
    }

    /// Whether the queue list carries the current track as its first row.
    ///
    /// It sits at the top so the list reads as "here, then next", and so
    /// that a jump has somewhere to leave the cursor: the track it just
    /// started is row zero.
    pub fn playing_row(&self) -> bool {
        self.playback.as_ref().is_some_and(|s| s.item.is_some())
    }

    /// The upcoming track a queue row refers to, or `None` for the row
    /// showing what is already playing.
    pub fn queue_target(&self, row: usize) -> Option<&PlayingItem> {
        let offset = usize::from(self.playing_row());
        self.queue.get(row.checked_sub(offset)?)
    }

    fn cursor(&mut self) -> &mut usize {
        if self.focus == Focus::Sidebar {
            return &mut self.sidebar_index;
        }
        match &self.view {
            View::Queue => &mut self.queue_index,
            View::Devices => &mut self.device_index,
            v if v.is_entry_list() => &mut self.entry_index,
            _ => &mut self.sidebar_index,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let cursor = self.cursor();
        let next = (*cursor as isize + delta).clamp(0, len as isize - 1);
        *cursor = next as usize;
    }

    /// Applies an action, returning any Spotify work it implies.
    pub fn update(&mut self, action: Action) -> Option<Command> {
        let view_before = self.view.clone();
        let command = self.apply(action);
        // Taken every time, so it can only ever cover the action that set it.
        let restored = std::mem::take(&mut self.restored);
        // Arriving at a view is the cue to load what it shows -- by digit key,
        // by sidebar navigation, or by Enter. Demand-driven beats polling
        // device and queue endpoints the user is not looking at. Except when
        // the list was put back rather than arrived at: fetching it would clear
        // the very list, and the cursor position, that going back restored.
        if command.is_none() && self.view != view_before && !restored {
            return self.refresh_for_view();
        }
        command
    }

    fn refresh_for_view(&mut self) -> Option<Command> {
        match &self.view {
            View::Devices => Some(Command::RefreshDevices),
            View::Queue => Some(Command::RefreshQueue),
            View::NowPlaying => None,
            // Search waits for a query rather than fetching on arrival.
            View::Search if self.query.trim().is_empty() => None,
            v if v.is_entry_list() => {
                self.loading = true;
                self.entries.clear();
                self.entry_index = 0;
                Some(Command::LoadView(v.clone(), self.query.clone()))
            }
            _ => None,
        }
    }

    fn apply(&mut self, action: Action) -> Option<Command> {
        // While help is up, every key just closes it.
        if self.show_help {
            self.show_help = matches!(action, Action::ToggleHelp) && !self.show_help;
            if matches!(action, Action::Quit) {
                self.should_quit = true;
            }
            return None;
        }

        match action {
            Action::Quit => self.should_quit = true,
            Action::ToggleHelp => self.show_help = true,
            // One key backs out of whatever is on top, innermost first.
            Action::Dismiss => {
                if self.link.is_some() {
                    self.link = None;
                } else if self.toast.is_some() {
                    self.toast = None;
                } else if self.typing {
                    self.typing = false;
                } else {
                    self.close_browse();
                }
            }

            Action::FocusNext | Action::FocusPrevious => self.focus = Focus::Main,
            Action::Up => self.move_cursor(-1),
            Action::Down => self.move_cursor(1),
            Action::PageUp => self.move_cursor(-10),
            Action::PageDown => self.move_cursor(10),
            Action::Top => self.move_cursor(isize::MIN / 2),
            Action::Bottom => self.move_cursor(isize::MAX / 2),

            Action::OpenQueue => return self.open(View::Queue),
            Action::OpenDevices => return self.open(View::Devices),
            Action::OpenLiked => return self.open(View::Liked),
            Action::OpenAlbums => return self.open(View::Albums),
            Action::OpenPlaylists => return self.open(View::Playlists),
            Action::CycleVisual => self.cycle_visual(),
            Action::OpenSearch => {
                let command = self.open(View::Search);
                // Search is the one list you arrive at with nothing to show,
                // so it opens ready to type.
                self.typing = true;
                return command;
            }
            Action::AddPlaylist => {
                // Only from a list, because that is where the pasted
                // playlist will appear and where the legend offers it.
                if self.view.is_entry_list() {
                    self.link = Some(String::new());
                }
            }
            Action::ToggleBrowse => {
                if self.browse_open {
                    self.close_browse();
                } else {
                    return self.open(self.view.clone());
                }
            }
            Action::CloseBrowse => self.close_browse(),

            Action::Char(c) => match &mut self.link {
                Some(buffer) => buffer.push(c),
                None => self.query.push(c),
            },
            Action::Backspace => match &mut self.link {
                Some(buffer) => {
                    buffer.pop();
                }
                None => {
                    self.query.pop();
                }
            },
            Action::Submit => {
                if let Some(buffer) = self.link.take() {
                    return self.submit_link(&buffer);
                }
                self.typing = false;
                if self.query.trim().is_empty() {
                    return None;
                }

                // A pasted share link is not a search term, and someone
                // who pastes one into the search box means the same thing
                // as someone who uses the link prompt.
                if boombox_core::uri::normalise(&self.query).is_some() {
                    let buffer = std::mem::take(&mut self.query);
                    return self.submit_link(&buffer);
                }

                self.loading = true;
                self.entries.clear();
                self.entry_index = 0;
                self.searched = self.query.clone();
                return Some(Command::LoadView(View::Search, self.searched.clone()));
            }
            Action::Back => return self.back(),
            Action::Save => {
                let entry = self.selected_entry()?.clone();
                return Some(Command::ToggleSave(entry));
            }
            Action::Enqueue => {
                let entry = self.selected_entry()?.clone();
                return Some(Command::Enqueue(entry));
            }
            Action::PlayTrackOnly => {
                let uri = self.selected_entry()?.uri.clone()?;
                return Some(Command::Play(Playback::Track(uri)));
            }
            Action::EnqueueAll => {
                let uris = self.track_uris();
                if uris.is_empty() {
                    self.error("nothing here to queue");
                    return None;
                }
                return Some(Command::EnqueueAll(uris));
            }

            Action::Select => return self.select(),
            Action::Refresh => return Some(Command::RefreshAll),

            Action::PlayPause => {
                let playing = self.playback.as_ref().is_some_and(|s| s.is_playing);
                // Recorded rather than sent: the glyph flips now, and if
                // a second tap arrives inside the window the two cancel
                // and nothing goes out at all.
                let wanted = !self.pending_playing.unwrap_or(playing);
                self.pending_playing = Some(wanted);
                self.playing_touched = Instant::now();
                self.playing_sent = false;
                return None;
            }
            Action::NextTrack => return Some(Command::Next),
            Action::PreviousTrack => return Some(Command::Previous),

            Action::SeekForward | Action::SeekBackward => {
                // Advanced to now, not the figure the last poll carried:
                // that one is a fraction of a second behind the music.
                let live = self.reported()?;
                // Counted from the last target while the keys are still
                // coming, for the reason volume is: presses inside one
                // poll interval all read the same position, so they would
                // all ask for the same place and collapse into one jump.
                let current = match self.pending_seek {
                    Some(target) if self.seek_touched.elapsed() < SEEK_CHAIN => target,
                    _ => live.progress(),
                };
                let target = if matches!(action, Action::SeekForward) {
                    current.saturating_add(self.seek_step_ms).min(live.duration().max(current))
                } else {
                    current.saturating_sub(self.seek_step_ms)
                };
                // The bar moves now; the seek goes out when the keys stop.
                self.pending_seek = Some(target);
                self.seek_touched = Instant::now();
                self.seek_sent = false;
                return None;
            }

            Action::VolumeUp | Action::VolumeDown => {
                let state = self.playback.as_ref()?;
                if let Some(device) = &state.device
                    && !device.supports_volume
                {
                    self.error(format!("{} does not support volume control", device.name));
                    return None;
                }
                // Counted from what we last asked for, not from the last
                // poll: the polled figure lags a change by seconds, so a
                // run of taps would all compute the same target from the
                // same stale reading and cancel each other out.
                let current = self.pending_volume.or_else(|| state.volume())? as i32;
                let delta = if matches!(action, Action::VolumeUp) {
                    self.volume_step as i32
                } else {
                    -(self.volume_step as i32)
                };
                self.pending_volume = Some((current + delta).clamp(0, 100) as u32);
                self.volume_touched = Instant::now();
                self.volume_sent = false;
                // Nothing goes out yet; flush_volume sends one call for the
                // whole run once the keys stop.
                return None;
            }

            Action::ToggleShuffle => {
                let state = self.playback.as_ref()?;
                return Some(Command::SetShuffle(!state.shuffle_state));
            }
            Action::CycleRepeat => {
                let state = self.playback.as_ref()?;
                return Some(Command::SetRepeat(state.repeat_state.next()));
            }
        }
        None
    }

    /// Opens the palette on `view`, loading it if it is a list. Re-opening
    /// the list already shown keeps its contents and cursor, so `1` twice
    /// is not a refresh.
    fn open(&mut self, view: View) -> Option<Command> {
        let changed = self.view != view || !self.browse_open;
        if let Some(i) = View::BROWSE.iter().position(|v| *v == view) {
            self.sidebar_index = i;
        }
        let same_list = self.view == view && !self.entries.is_empty();
        self.view = view;
        self.browse_open = true;
        self.focus = Focus::Main;
        self.typing = false;
        self.parent = None;
        if !changed || same_list {
            return None;
        }
        self.load_current()
    }

    fn close_browse(&mut self) {
        self.browse_open = false;
        self.typing = false;
    }

    /// What the palette needs fetched to show `self.view`.
    fn load_current(&mut self) -> Option<Command> {
        match self.view {
            View::Queue => Some(Command::RefreshQueue),
            View::Devices => Some(Command::RefreshDevices),
            View::Search if self.query.trim().is_empty() => None,
            _ if self.view.is_entry_list() => {
                self.loading = true;
                self.entries.clear();
                self.entry_index = 0;
                if self.view == View::Search {
                    self.searched = self.query.clone();
                }
                Some(Command::LoadView(self.view.clone(), self.query.clone()))
            }
            _ => None,
        }
    }

    /// Leaves a drill-down, or falls back to the sidebar.
    fn back(&mut self) -> Option<Command> {
        match self.parent.take() {
            // Put the list back exactly as it was, cursor and all, rather than
            // asking Spotify for it again.
            Some(parent) => {
                self.view = parent.view;
                self.entries = parent.entries;
                self.entry_total = parent.total;
                self.entry_index = parent.index.min(self.entries.len().saturating_sub(1));
                self.unpaged = parent.unpaged;
                self.loading = false;
                self.restored = true;
                None
            }
            // Nothing to go back to: Backspace leaves the palette entirely.
            None => {
                self.close_browse();
                None
            }
        }
    }

    /// Called when the cursor reaches the end of a partially loaded list.
    pub fn maybe_load_more(&mut self) -> Option<Command> {
        if self.loading || !self.has_more() || !self.view.is_entry_list() {
            return None;
        }
        // Only when the cursor is actually near the bottom.
        if self.entry_index + 5 < self.entries.len() {
            return None;
        }
        self.loading = true;
        if self.view == View::Search {
            // Each type is paged on its own: its next ten, not an offset of
            // however many rows of every kind are on screen. That skipped
            // results 10 to 34 of every type on the way to page two.
            let offset = self.search_pages * SEARCH_PAGE;
            return Some(Command::LoadMore(View::Search, offset, self.searched.clone()));
        }
        let offset = self.entries.len().saturating_sub(self.unpaged) as u32;
        Some(Command::LoadMore(self.view.clone(), offset, self.query.clone()))
    }

    /// Checks whether a queue jump landed, and returns the correction if
    /// it did not.
    ///
    /// Called after each playback poll. The delay before judging is for
    /// the API, which reports the previous track for a moment after a
    /// change; judging immediately would call every jump a miss.
    pub fn check_jump(&mut self) -> Option<Command> {
        let pending = self.expecting.as_ref()?;
        if pending.asked_at.elapsed() < JUMP_SETTLE {
            return None;
        }
        let playing = self.playback.as_ref()?.item.as_ref()?.uri()?;
        let pending = self.expecting.take()?;
        if playing == pending.uri {
            return None;
        }
        // The row was not part of the context after all -- queued by hand,
        // or something Spotify's autoplay put there. Play the rows.
        tracing::debug!("queue jump missed; playing the queue rows instead");
        Some(Command::Play(Playback::Tracks { uris: pending.fallback, start: 0 }))
    }

    /// How playing this row should behave, given where it was chosen from.
    ///
    /// A row that is itself a context -- an album, an artist -- plays as
    /// that context. A track plays as part of the list it was chosen from,
    /// which is what the Spotify app does and what makes a list keep
    /// going. Only a list with no context of its own falls back to sending
    /// the tracks, and only then is continuation limited to what is loaded.
    fn playback_for(&self, entry: &Entry) -> Playback {
        let uri = entry.uri.clone().unwrap_or_default();
        if entry.kind.is_context() {
            return Playback::Context { uri, start: None };
        }
        if let Some(context) = self.view.context_uri() {
            return Playback::Context { uri: context, start: Some(uri) };
        }
        // No context: send the tracks. The whole loaded list goes, with a
        // starting point, so what is above the cursor stays part of it.
        let uris = self.track_uris();
        let start = uris.iter().position(|u| *u == uri).unwrap_or(0);
        if uris.is_empty() { Playback::Track(uri) } else { Playback::Tracks { uris, start } }
    }

    /// Every playable track in the loaded list, in order.
    ///
    /// Albums and artists are skipped rather than played as contexts: a
    /// mixed search result cannot be handed to the player as one list, and
    /// dropping them keeps the tracks in the order they were shown.
    pub fn track_uris(&self) -> Vec<String> {
        self.entries.iter().filter(|e| !e.kind.is_context()).filter_map(|e| e.uri.clone()).collect()
    }

    fn select(&mut self) -> Option<Command> {
        // Drilling into a playlist replaces the list and remembers the way back.
        if self.view.is_entry_list() {
            let entry = self.selected_entry()?.clone();
            if entry.kind == boombox_core::api::EntryKind::Playlist
                && let Some(id) = entry.uri.as_ref().and_then(|u| u.rsplit(':').next())
            {
                self.parent = Some(Parent {
                    view: self.view.clone(),
                    entries: std::mem::take(&mut self.entries),
                    total: self.entry_total,
                    index: self.entry_index,
                    unpaged: self.unpaged,
                });
                self.view = View::PlaylistItems { id: id.to_string(), name: entry.title.clone() };
                self.entry_index = 0;
                self.entry_total = 0;
                self.unpaged = 0;
                self.loading = true;
                return Some(Command::LoadView(self.view.clone(), self.query.clone()));
            }
            return Some(Command::Play(self.playback_for(&entry)));
        }

        match (self.focus, &self.view) {
            (Focus::Main, View::Devices) => {
                let device = self.devices.get(self.device_index)?;
                match &device.id {
                    Some(id) => Some(Command::Transfer(id.clone())),
                    None => {
                        self.error(format!("{} reports no device id", device.name));
                        None
                    }
                }
            }
            (Focus::Main, View::Queue) => {
                // Row zero is what is already playing, when there is
                // anything: selecting it would restart the track, which is
                // not what pressing enter on "now playing" should mean.
                self.queue_target(self.queue_index)?;
                let skip = self.queue_index - usize::from(self.playing_row());
                let rows: Vec<String> = self
                    .queue
                    .iter()
                    .skip(skip)
                    .filter_map(|i| i.uri().map(str::to_string))
                    .collect();
                let Some(target) = rows.first().cloned() else {
                    // Local files carry no URI, so there is nothing to name.
                    self.error("nothing playable from here");
                    return None;
                };

                // Prefer the context. Playing the queue's own rows is
                // exact but stops at the twenty the API will show, which
                // turns a nine-hundred-track list into a handful. The
                // context keeps going, and the poll below puts it right if
                // this row turns out not to belong to it.
                // The chosen track becomes the current one, and the
                // current one is row zero, so that is where the cursor
                // belongs -- leaving it where it was would point at a
                // track that has just moved up the list.
                self.queue_index = 0;

                match self.playback.as_ref().and_then(|s| s.context.as_ref()) {
                    Some(context) => {
                        self.expecting = Some(Expecting {
                            uri: target.clone(),
                            fallback: rows,
                            asked_at: Instant::now(),
                        });
                        Some(Command::Play(Playback::Context {
                            uri: context.uri.clone(),
                            start: Some(target),
                        }))
                    }
                    // Nothing to continue: the rows are all there is.
                    None => Some(Command::Play(Playback::Tracks { uris: rows, start: 0 })),
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn playing_state(volume: u32, shuffle: bool, playing: bool) -> PlaybackState {
        serde_json::from_str(&format!(
            r#"{{"is_playing":{playing},"progress_ms":60000,"shuffle_state":{shuffle},
                "repeat_state":"off",
                "device":{{"id":"d1","name":"MacBook","type":"Computer",
                          "volume_percent":{volume},"supports_volume":true}},
                "item":{{"type":"track","name":"x","uri":"spotify:track:x",
                        "duration_ms":200000,"artists":[],"album":{{}}}}}}"#
        ))
        .unwrap()
    }

    fn app_with_state(state: PlaybackState) -> App {
        let mut app = App::new(5, false);
        app.set_playback(Some(state));
        app
    }

    /// Pressing it shows the change at once and sends nothing yet; the
    /// command follows when the keys have stopped.
    #[test]
    fn play_pause_depends_on_current_state() {
        let mut app = app_with_state(playing_state(50, false, true));
        assert_eq!(app.update(Action::PlayPause), None, "nothing goes out per press");
        assert!(!app.playback().unwrap().is_playing, "but the screen says paused");
        app.expire_playing_coalesce();
        assert_eq!(app.flush_due(), vec![Command::Pause]);

        let mut app = app_with_state(playing_state(50, false, false));
        app.update(Action::PlayPause);
        app.expire_playing_coalesce();
        assert_eq!(app.flush_due(), vec![Command::Resume]);
    }

    #[test]
    fn transport_actions_are_safe_with_no_playback() {
        let mut app = App::new(5, false);
        // No state yet: these must not panic, and must not invent a command.
        assert_eq!(app.update(Action::SeekForward), None);
        assert_eq!(app.update(Action::VolumeUp), None);
        assert_eq!(app.update(Action::ToggleShuffle), None);
        assert_eq!(app.update(Action::CycleRepeat), None);
        // Next/previous are unconditional; the API reports its own errors.
        assert_eq!(app.update(Action::NextTrack), Some(Command::Next));
    }

    /// The base is advanced to the moment of the keypress, so the exact
    /// millisecond depends on how long the test itself took. A step is
    /// five seconds; a hundred milliseconds of slack cannot hide an error
    /// that matters.
    /// Presses the key, then lets the window lapse, which is what the
    /// event loop does when the keys stop arriving.
    #[track_caller]
    fn seeks_to(app: &mut App, action: Action, expected: u64) {
        assert_eq!(app.update(action), None, "nothing goes out per press");
        app.expire_seek_coalesce();
        let commands = app.flush_due();
        let [Command::Seek(ms)] = commands.as_slice() else {
            panic!("expected one seek, got {commands:?}");
        };
        let ms = *ms;
        assert!(
            (expected..expected + 100).contains(&ms),
            "expected about {expected}ms, got {ms}ms"
        );
    }

    /// The same bug volume had, in the other axis: every press read the
    /// position the last poll carried, so a run of them all asked to go to
    /// the same place and arrived as one jump.
    #[test]
    fn a_run_of_seeks_accumulates_instead_of_collapsing() {
        let mut app = app_with_state(playing_state(50, false, true));
        seeks_to(&mut app, Action::SeekForward, 65_000);
        seeks_to(&mut app, Action::SeekForward, 70_000);
        seeks_to(&mut app, Action::SeekForward, 75_000);
    }

    /// Going back undoes going forward, rather than landing a step behind
    /// where it started.
    #[test]
    fn a_seek_back_undoes_a_seek_forward() {
        let mut app = app_with_state(playing_state(50, false, true));
        seeks_to(&mut app, Action::SeekForward, 65_000);
        seeks_to(&mut app, Action::SeekBackward, 60_000);
    }

    /// Chaining is for a run of presses. A target from a minute ago is not
    /// where the track is any more, so the next seek starts from the music.
    #[test]
    fn a_seek_after_a_gap_starts_from_the_live_position() {
        let mut app = app_with_state(playing_state(50, false, true));
        seeks_to(&mut app, Action::SeekForward, 65_000);
        app.expire_seek_chain();
        seeks_to(&mut app, Action::SeekForward, 65_000);
    }

    /// The point of the whole exercise: two taps of space are a decision
    /// not to change anything, and must cost no API call at all.
    #[test]
    fn two_quick_taps_of_space_cancel_each_other_out() {
        let mut app = app_with_state(playing_state(50, false, true));
        app.update(Action::PlayPause);
        app.update(Action::PlayPause);
        assert!(app.playback().unwrap().is_playing, "back where it started");
        app.expire_playing_coalesce();
        assert_eq!(app.flush_due(), vec![], "and nothing was asked of Spotify");
    }

    /// Three taps is one decision, and it is the odd one out.
    #[test]
    fn an_odd_number_of_taps_still_changes_it() {
        let mut app = app_with_state(playing_state(50, false, true));
        for _ in 0..3 {
            app.update(Action::PlayPause);
        }
        app.expire_playing_coalesce();
        assert_eq!(app.flush_due(), vec![Command::Pause]);
    }

    /// The bar answers the key straight away, before anything is sent.
    #[test]
    fn the_bar_shows_the_seek_before_it_is_sent() {
        let mut app = app_with_state(playing_state(50, false, true));
        app.update(Action::SeekForward);
        assert_eq!(app.playback().unwrap().progress_ms, Some(65_000));
        assert!(app.playback().unwrap().pending.position, "and says it is not confirmed");
    }

    /// A run of keys is one write, whatever its length.
    #[test]
    fn a_run_of_seeks_sends_exactly_one_command() {
        let mut app = app_with_state(playing_state(50, false, true));
        for _ in 0..10 {
            assert_eq!(app.update(Action::SeekForward), None);
        }
        app.expire_seek_coalesce();
        assert_eq!(app.flush_due(), vec![Command::Seek(110_000)], "60s + 10 x 5s");
    }

    /// A target belongs to the track it was measured in.
    #[test]
    fn a_new_track_drops_the_chained_target() {
        let mut app = app_with_state(playing_state(50, false, true));
        seeks_to(&mut app, Action::SeekForward, 65_000);
        app.set_playback(Some(
            serde_json::from_str(
                r#"{"is_playing":true,"progress_ms":10000,
                "item":{"type":"track","name":"next","uri":"spotify:track:next",
                        "duration_ms":200000,"artists":[],"album":{}}}"#,
            )
            .unwrap(),
        ));
        seeks_to(&mut app, Action::SeekForward, 15_000);
    }

    #[test]
    fn seek_uses_the_configured_step_and_clamps() {
        let mut app = app_with_state(playing_state(50, false, true));
        seeks_to(&mut app, Action::SeekForward, 65_000);

        let mut app = App::new(5, false);
        app.set_playback(Some(
            serde_json::from_str(
                r#"{"is_playing":true,"progress_ms":1000,
                "item":{"type":"track","name":"x","uri":"u","duration_ms":200000,
                        "artists":[],"album":{}}}"#,
            )
            .unwrap(),
        ));
        seeks_to(&mut app, Action::SeekBackward, 0);
    }

    /// The end of the track is the end of the seek, however many presses
    /// are chained past it.
    #[test]
    fn a_run_of_seeks_stops_at_the_end_of_the_track() {
        let mut app = app_with_state(playing_state(50, false, true));
        for _ in 0..60 {
            app.update(Action::SeekForward);
        }
        seeks_to(&mut app, Action::SeekForward, 200_000);
        assert_eq!(app.flush_due(), vec![], "one seek for the whole run, not sixty");
    }

    /// The reported bug: eight rapid presses moved the volume by one step,
    /// because each read the same stale polled figure and asked for the
    /// same target. They must accumulate.
    #[test]
    fn a_run_of_presses_accumulates_instead_of_cancelling_out() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        for _ in 0..8 {
            assert_eq!(app.update(Action::VolumeUp), None, "nothing goes out per press");
        }
        assert_eq!(app.volume(), Some(90), "50 + 8 x 5");
    }

    /// One API call for the whole run, not eight. This is what stops the
    /// rate limiter being provoked.
    #[test]
    fn a_run_of_presses_sends_exactly_one_call() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        for _ in 0..8 {
            app.update(Action::VolumeUp);
        }
        // Pretend the keys stopped long enough ago.
        app.volume_touched = Instant::now() - VOLUME_COALESCE;
        assert_eq!(app.flush_volume(), Some(Command::SetVolume(90)));
        assert_eq!(app.flush_volume(), None, "and not again");
    }

    /// The display has to answer the keypress, not the network -- the
    /// polled figure lags a change by seconds.
    #[test]
    fn the_display_shows_the_asked_for_volume_before_the_api_agrees() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        app.update(Action::VolumeUp);
        assert_eq!(app.volume(), Some(55));
        // A poll arrives still reporting the old figure.
        app.set_playback(Some(playing_state(50, false, true)));
        assert_eq!(app.volume(), Some(55), "the stale poll must not undo it");
    }

    /// Control returns to the API the moment it agrees, not on a timer.
    /// The device rounds -- ask for 55, read back 54 -- so agreement has a
    /// tolerance or it would never arrive.
    #[test]
    fn the_api_takes_over_once_it_reports_the_new_figure() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        app.update(Action::VolumeUp);
        app.volume_touched = Instant::now() - VOLUME_COALESCE;
        assert_eq!(app.flush_volume(), Some(Command::SetVolume(55)));

        // Measured against the real thing: about eleven seconds of polls
        // still reporting the old figure.
        for _ in 0..11 {
            app.set_playback(Some(playing_state(50, false, true)));
            assert_eq!(app.volume(), Some(55), "must not snap back mid-flight");
        }
        // Then the device reports 54, having rounded what we asked for.
        app.set_playback(Some(playing_state(54, false, true)));
        assert_eq!(app.volume(), Some(54), "the API has caught up and takes over");
    }

    /// A change that never lands must not leave a wrong number on screen
    /// forever, so the timer is still there as a backstop.
    #[test]
    fn a_change_that_never_lands_is_given_up_on() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        app.update(Action::VolumeUp);
        app.volume_touched = Instant::now() - VOLUME_COALESCE;
        app.flush_volume();
        assert_eq!(app.volume(), Some(55));

        app.volume_touched = Instant::now() - VOLUME_SETTLE;
        app.flush_volume();
        assert_eq!(app.volume(), Some(50), "back to what the API actually reports");
    }

    /// An idle TUI must not be woken on a timer it has no use for.
    #[test]
    fn there_is_no_deadline_with_nothing_pending() {
        let mut app = App::new(5, false);
        assert_eq!(app.volume_deadline(), None);
        app.set_playback(Some(playing_state(50, false, true)));
        assert_eq!(app.volume_deadline(), None);
        app.update(Action::VolumeUp);
        assert!(app.volume_deadline().is_some());
    }

    #[test]
    fn presses_still_clamp_at_both_ends() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(95, false, true)));
        for _ in 0..5 {
            app.update(Action::VolumeUp);
        }
        assert_eq!(app.volume(), Some(100));

        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(5, false, true)));
        for _ in 0..5 {
            app.update(Action::VolumeDown);
        }
        assert_eq!(app.volume(), Some(0));
    }

    #[test]
    fn a_single_press_still_reaches_the_api_clamped() {
        let mut app = app_with_state(playing_state(97, false, true));
        app.update(Action::VolumeUp);
        app.volume_touched = Instant::now() - VOLUME_COALESCE;
        assert_eq!(app.flush_volume(), Some(Command::SetVolume(100)));

        let mut app = app_with_state(playing_state(2, false, true));
        app.update(Action::VolumeDown);
        app.volume_touched = Instant::now() - VOLUME_COALESCE;
        assert_eq!(app.flush_volume(), Some(Command::SetVolume(0)));
    }

    #[test]
    fn volume_on_a_fixed_device_explains_itself_instead_of_failing() {
        let state: PlaybackState = serde_json::from_str(
            r#"{"is_playing":true,"progress_ms":0,
                "device":{"id":"d","name":"Kitchen","type":"Speaker",
                          "volume_percent":null,"supports_volume":false},
                "item":{"type":"track","name":"x","uri":"u","duration_ms":1000,
                        "artists":[],"album":{}}}"#,
        )
        .unwrap();
        let mut app = app_with_state(state);
        assert_eq!(app.update(Action::VolumeUp), None);
        assert!(app.visible_toast().unwrap().text.contains("Kitchen"));
    }

    #[test]
    fn shuffle_and_repeat_invert_the_current_state() {
        let mut app = app_with_state(playing_state(50, true, true));
        assert_eq!(app.update(Action::ToggleShuffle), Some(Command::SetShuffle(false)));
        assert_eq!(app.update(Action::CycleRepeat), Some(Command::SetRepeat(RepeatState::Context)));
    }

    #[test]
    fn opening_a_list_shows_the_browser_and_asks_for_its_data() {
        let mut app = App::new(5, false);
        assert!(!app.browse_open, "the browser starts closed");
        assert_eq!(app.update(Action::OpenDevices), Some(Command::RefreshDevices));
        assert!(app.browse_open);
        assert_eq!(app.view, View::Devices);
        assert_eq!(app.update(Action::OpenQueue), Some(Command::RefreshQueue));
        assert_eq!(
            app.update(Action::OpenPlaylists),
            Some(Command::LoadView(View::Playlists, String::new()))
        );
    }

    /// Re-opening the list already shown must not throw away its contents,
    /// or pressing the same digit twice would silently reload.
    #[test]
    fn reopening_the_same_list_keeps_what_it_already_has() {
        let mut app = App::new(5, false);
        app.update(Action::OpenPlaylists);
        app.set_entries(vec![entry("Mixtape")], 1, false);
        assert_eq!(app.update(Action::OpenPlaylists), None, "no refetch");
        assert_eq!(app.entries.len(), 1, "and nothing thrown away");
    }

    #[test]
    fn the_browser_closes_and_reopens_on_one_key() {
        let mut app = App::new(5, false);
        app.update(Action::OpenQueue);
        assert!(app.browse_open);
        app.update(Action::ToggleBrowse);
        assert!(!app.browse_open, "Tab closes it");
        app.update(Action::ToggleBrowse);
        assert!(app.browse_open, "and reopens it");
    }

    /// Esc peels one layer at a time rather than dumping you out at once.
    #[test]
    fn escape_backs_out_innermost_first() {
        let mut app = App::new(5, false);
        app.update(Action::OpenSearch);
        app.error("something went wrong");
        assert!(app.browse_open && app.is_typing() && app.toast.is_some());

        app.update(Action::Dismiss);
        assert!(app.toast.is_none(), "the toast goes first");
        assert!(app.is_typing(), "and typing survives it");

        app.update(Action::Dismiss);
        assert!(!app.is_typing(), "then the search box");
        assert!(app.browse_open, "but the list is still up");

        app.update(Action::Dismiss);
        assert!(!app.browse_open, "and finally the browser");
    }

    #[test]
    fn search_waits_for_a_query_rather_than_fetching_on_arrival() {
        let mut app = App::new(5, false);
        assert_eq!(app.update(Action::OpenSearch), None, "an empty query fetches nothing");
        assert!(app.is_typing(), "the search box takes focus");

        for c in "meadow".chars() {
            app.update(Action::Char(c));
        }
        assert_eq!(app.query, "meadow");
        app.update(Action::Backspace);
        assert_eq!(app.query, "meado");

        assert_eq!(
            app.update(Action::Submit),
            Some(Command::LoadView(View::Search, "meado".into()))
        );
        assert!(!app.is_typing(), "submitting leaves the box");
    }

    /// The only route to a Spotify-made playlist: it cannot be searched
    /// for, cannot be listed, and 404s when fetched by id. Pasting the link
    /// has to work, or those playlists are unreachable from the app.
    #[test]
    fn a_pasted_share_link_plays_instead_of_searching() {
        let mut app = App::new(5, false);
        app.update(Action::OpenSearch);
        for c in "https://open.spotify.com/playlist/37i9dQZF1EExampleMix01?si=0ee5b31".chars() {
            app.update(Action::Char(c));
        }
        assert_eq!(
            app.update(Action::Submit),
            Some(Command::AddAndPlay("spotify:playlist:37i9dQZF1EExampleMix01".into())),
            "the tracking parameter must not stop it being recognised"
        );
        assert!(app.query.is_empty(), "the box is cleared, not left holding a URL");
    }

    #[test]
    fn a_pasted_track_link_plays_just_that_track() {
        let mut app = App::new(5, false);
        app.update(Action::OpenSearch);
        for c in "spotify:track:ExampleTrack0000000002".chars() {
            app.update(Action::Char(c));
        }
        assert_eq!(
            app.update(Action::Submit),
            Some(Command::AddAndPlay("spotify:track:ExampleTrack0000000002".into()))
        );
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.update(Action::Char(c));
        }
    }

    #[test]
    fn the_link_prompt_takes_a_pasted_url_and_plays_it() {
        let mut app = App::new(5, false);
        app.update(Action::OpenPlaylists);
        app.update(Action::AddPlaylist);
        assert!(app.link_prompt().is_some(), "the prompt is open");
        assert!(app.is_typing(), "and takes characters rather than key bindings");

        typed(&mut app, "https://open.spotify.com/playlist/37i9dQZF1EExampleMix01?si=0ee");
        assert_eq!(
            app.update(Action::Submit),
            Some(Command::AddAndPlay("spotify:playlist:37i9dQZF1EExampleMix01".into()))
        );
        assert!(app.link_prompt().is_none(), "the prompt closes behind it");
    }

    /// Opening the prompt must not disturb a search you had already run,
    /// which is why the two do not share a buffer.
    #[test]
    fn the_link_prompt_leaves_the_search_query_alone() {
        let mut app = App::new(5, false);
        app.update(Action::OpenSearch);
        typed(&mut app, "meadow");
        app.update(Action::Submit);

        app.update(Action::AddPlaylist);
        typed(&mut app, "spotify:album:x");
        app.update(Action::Dismiss);
        assert_eq!(app.query, "meadow", "the search box is untouched");
    }

    /// Half a copied link, or a link to something else entirely, look the
    /// same on screen -- so it has to say something.
    #[test]
    fn a_link_that_is_not_a_spotify_link_is_refused_out_loud() {
        let mut app = App::new(5, false);
        app.update(Action::OpenPlaylists);
        app.update(Action::AddPlaylist);
        typed(&mut app, "https://example.com/nope");
        assert_eq!(app.update(Action::Submit), None);
        assert!(app.toast.is_some(), "it must say why");
    }

    #[test]
    fn escape_closes_the_link_prompt_before_anything_else() {
        let mut app = App::new(5, false);
        app.update(Action::OpenPlaylists);
        app.update(Action::AddPlaylist);
        app.update(Action::Dismiss);
        assert!(app.link_prompt().is_none());
        assert!(app.browse_open, "the list underneath stays open");
    }

    /// Nothing to paste into on the Now Playing stage.
    #[test]
    fn the_link_prompt_only_opens_over_a_list() {
        let mut app = App::new(5, false);
        app.update(Action::AddPlaylist);
        assert!(app.link_prompt().is_none());
    }

    /// An ordinary search must not be mistaken for a link.
    #[test]
    fn a_normal_query_still_searches() {
        let mut app = App::new(5, false);
        app.update(Action::OpenSearch);
        for c in "meadow".chars() {
            app.update(Action::Char(c));
        }
        assert_eq!(
            app.update(Action::Submit),
            Some(Command::LoadView(View::Search, "meadow".into()))
        );
    }

    #[test]
    fn submitting_an_empty_query_does_nothing() {
        let mut app = App::new(5, false);
        app.update(Action::OpenSearch);
        app.update(Action::Char(' '));
        assert_eq!(app.update(Action::Submit), None);
    }

    #[test]
    fn selecting_a_playlist_drills_in_and_back_returns() {
        let mut app = App::new(5, false);
        app.update(Action::OpenPlaylists);
        app.set_entries(vec![playlist_row("Mixtape")], 1, false);

        let command = app.update(Action::Select).unwrap();
        assert!(matches!(command, Command::LoadView(View::PlaylistItems { .. }, _)));
        assert_eq!(app.view.label(), "Mixtape");

        assert_eq!(app.update(Action::Back), None, "the list is put back, not fetched again");
        assert_eq!(app.view, View::Playlists);
        assert_eq!(app.entries[0].title, "Mixtape");
        assert!(!app.loading);
    }

    fn playlist_row(title: &str) -> Entry {
        Entry {
            title: title.into(),
            subtitle: "Alex".into(),
            uri: Some(format!("spotify:playlist:{title}")),
            duration_ms: None,
            kind: boombox_core::api::EntryKind::Playlist,
        }
    }

    fn row_of_kind(title: &str, kind: boombox_core::api::EntryKind) -> Entry {
        Entry { kind, ..entry(title) }
    }

    /// Coming back from a playlist used to put the cursor at the top, so
    /// reaching the next one meant scrolling down to it again.
    #[test]
    fn leaving_a_playlist_puts_the_cursor_back_on_it() {
        let mut app = App::new(5, false);
        app.update(Action::OpenPlaylists);
        let rows: Vec<Entry> = (0..60).map(|i| playlist_row(&format!("p{i}"))).collect();
        app.set_entries(rows, 60, false);
        for _ in 0..42 {
            app.update(Action::Down);
        }
        app.update(Action::Select);
        app.set_entries(vec![entry("Aurora")], 1, false);

        app.update(Action::Back);
        assert_eq!(app.entry_index, 42);
        assert_eq!(app.entries.len(), 60, "everything paged in is still there");
        assert_eq!(app.entry_total, 60);
    }

    /// A playlist still loading when you leave it must not land on top of
    /// the list you went back to.
    #[test]
    fn a_fetch_for_a_list_no_longer_on_screen_is_ignored() {
        let mut app = App::new(5, false);
        app.update(Action::OpenPlaylists);
        app.set_entries(vec![playlist_row("Mixtape"), playlist_row("Sundays")], 2, false);
        app.update(Action::Select);
        let opened = app.view.clone();
        app.update(Action::Back);

        app.accept_entries(&opened, "", vec![entry("Aurora")], 1, false);
        assert_eq!(app.entries.len(), 2, "the late playlist fetch was dropped");
        assert_eq!(app.entries[0].title, "Mixtape");
    }

    #[test]
    fn results_for_an_earlier_search_do_not_replace_a_later_one() {
        let mut app = App::new(5, false);
        app.view = View::Search;
        app.searched = "meadow".into();
        app.accept_entries(&View::Search, "mead", vec![entry("stale")], 1, false);
        assert!(app.entries.is_empty());
        app.accept_entries(&View::Search, "meadow", vec![entry("Aurora")], 1, false);
        assert_eq!(app.entries[0].title, "Aurora");
    }

    /// Proven against Spotify: page two asked every type for offset 35 --
    /// the number of rows on screen -- so results 10 to 34 of each type never
    /// appeared. Each type now pages by its own ten.
    #[test]
    fn further_search_results_ask_each_type_for_its_next_ten() {
        use boombox_core::api::EntryKind::{Album, Playlist, Track};
        let mut app = App::new(5, false);
        app.view = View::Search;
        app.searched = "meadow".into();
        let mut page: Vec<Entry> = (0..10).map(|i| row_of_kind(&format!("t{i}"), Track)).collect();
        page.extend((0..10).map(|i| row_of_kind(&format!("a{i}"), Album)));
        page.extend((0..5).map(|i| row_of_kind(&format!("p{i}"), Playlist)));
        app.accept_entries(&View::Search, "meadow", page, 400, false);

        app.update(Action::Bottom);
        assert_eq!(
            app.maybe_load_more(),
            Some(Command::LoadMore(View::Search, 10, "meadow".into()))
        );
        app.accept_entries(&View::Search, "meadow", vec![row_of_kind("t10", Track)], 400, true);
        app.update(Action::Bottom);
        assert_eq!(
            app.maybe_load_more(),
            Some(Command::LoadMore(View::Search, 20, "meadow".into()))
        );
    }

    /// A further page used to go after the playlists, putting a second
    /// "Tracks" heading at the bottom of the list.
    #[test]
    fn a_further_page_of_results_joins_its_own_heading_and_the_cursor_stays_put() {
        use boombox_core::api::EntryKind::{Album, Playlist, Track};
        let mut app = App::new(5, false);
        app.view = View::Search;
        app.searched = "q".into();
        let first =
            vec![row_of_kind("t1", Track), row_of_kind("a1", Album), row_of_kind("p1", Playlist)];
        app.accept_entries(&View::Search, "q", first, 100, false);
        app.update(Action::Bottom);
        let second =
            vec![row_of_kind("t2", Track), row_of_kind("a2", Album), row_of_kind("p2", Playlist)];
        app.accept_entries(&View::Search, "q", second, 100, true);

        let titles: Vec<&str> = app.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["t1", "t2", "a1", "a2", "p1", "p2"]);
        assert_eq!(app.selected_entry().unwrap().title, "p1", "the cursor kept its row");
    }

    #[test]
    fn an_empty_page_of_search_results_ends_the_search() {
        let mut app = App::new(5, false);
        app.view = View::Search;
        app.searched = "q".into();
        app.accept_entries(&View::Search, "q", vec![entry("t1")], 900, false);
        assert!(app.has_more(), "Spotify claims 900");
        app.accept_entries(&View::Search, "q", Vec::new(), 900, true);
        assert!(!app.has_more(), "but an empty page is the truth");
    }

    fn track(name: &str, uri: &str) -> Entry {
        Entry {
            title: name.into(),
            subtitle: String::new(),
            uri: Some(uri.into()),
            duration_ms: Some(1000),
            kind: boombox_core::api::EntryKind::Track,
        }
    }

    /// Liked Songs plays as a context, so it carries on through all of
    /// them rather than stopping at the page we happen to have loaded.
    #[test]
    fn a_track_in_liked_songs_plays_the_whole_collection() {
        let mut app = App::new(5, false);
        app.view = View::Liked;
        app.set_entries(
            vec![track("First Light", "spotify:track:a"), track("Low Tide", "spotify:track:b")],
            898,
            false,
        );
        app.entry_index = 1;
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Context {
                uri: "spotify:collection:tracks".into(),
                start: Some("spotify:track:b".into()),
            }))
        );
    }

    #[test]
    fn a_track_in_a_playlist_plays_that_playlist() {
        let mut app = App::new(5, false);
        app.view = View::PlaylistItems { id: "p1".into(), name: "Weeknights".into() };
        app.set_entries(vec![track("x", "spotify:track:a")], 1, false);
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Context {
                uri: "spotify:playlist:p1".into(),
                start: Some("spotify:track:a".into()),
            }))
        );
    }

    /// Search results are not a context, so the tracks go out themselves.
    /// The whole loaded list is sent with a starting point rather than
    /// being sliced, so what is above the cursor stays part of it.
    #[test]
    fn search_results_are_sent_as_a_list_starting_at_the_cursor() {
        let mut app = App::new(5, false);
        app.view = View::Search;
        app.set_entries(
            vec![
                track("a", "spotify:track:a"),
                track("b", "spotify:track:b"),
                track("c", "spotify:track:c"),
            ],
            3,
            false,
        );
        app.entry_index = 2;
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Tracks {
                uris: vec![
                    "spotify:track:a".into(),
                    "spotify:track:b".into(),
                    "spotify:track:c".into()
                ],
                start: 2,
            }))
        );
    }

    /// A row that is itself a context plays as that context, wherever it
    /// was found -- an album in a search result is still an album.
    #[test]
    fn a_context_row_plays_as_itself() {
        let mut app = App::new(5, false);
        app.view = View::Search;
        let album = Entry {
            title: "II".into(),
            subtitle: String::new(),
            uri: Some("spotify:album:x".into()),
            duration_ms: None,
            kind: boombox_core::api::EntryKind::Album,
        };
        app.set_entries(vec![album], 1, false);
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Context { uri: "spotify:album:x".into(), start: None }))
        );
    }

    /// The escape hatch: one track, no list, wherever you are.
    #[test]
    fn t_plays_the_track_alone_even_inside_a_context() {
        let mut app = App::new(5, false);
        app.view = View::Liked;
        app.set_entries(vec![track("First Light", "spotify:track:a")], 898, false);
        assert_eq!(
            app.update(Action::PlayTrackOnly),
            Some(Command::Play(Playback::Track("spotify:track:a".into())))
        );
    }

    #[test]
    fn enqueue_all_takes_the_tracks_and_skips_the_rest() {
        let mut app = App::new(5, false);
        app.view = View::Search;
        let album = Entry {
            title: "II".into(),
            subtitle: String::new(),
            uri: Some("spotify:album:x".into()),
            duration_ms: None,
            kind: boombox_core::api::EntryKind::Album,
        };
        app.set_entries(vec![track("a", "spotify:track:a"), album], 2, false);
        assert_eq!(
            app.update(Action::EnqueueAll),
            Some(Command::EnqueueAll(vec!["spotify:track:a".into()]))
        );
    }

    #[test]
    fn enqueue_all_says_so_when_there_is_nothing_to_queue() {
        let mut app = App::new(5, false);
        app.view = View::Search;
        assert_eq!(app.update(Action::EnqueueAll), None);
        assert!(app.toast.is_some(), "should have explained itself");
    }

    #[test]
    fn paging_appends_and_only_fires_near_the_bottom() {
        let mut app = App::new(5, false);
        app.view = View::Liked;
        app.focus = Focus::Main;
        let rows: Vec<Entry> = (0..50).map(|i| entry(&format!("t{i}"))).collect();
        app.set_entries(rows, 898, false);

        assert!(app.has_more());
        assert_eq!(app.maybe_load_more(), None, "cursor is at the top");

        app.update(Action::Bottom);
        let more = app.maybe_load_more().unwrap();
        assert_eq!(more, Command::LoadMore(View::Liked, 50, String::new()));

        // A second call while the first is in flight must not double-fetch.
        assert_eq!(app.maybe_load_more(), None);

        app.set_entries((0..50).map(|i| entry(&format!("u{i}"))).collect(), 898, true);
        assert_eq!(app.entries.len(), 100, "the page was appended, not replaced");
    }

    fn recent(uri: &str) -> Entry {
        Entry {
            title: "Daily Mix 1".into(),
            subtitle: "Playlist  \u{b7}  2h ago".into(),
            uri: Some(uri.into()),
            duration_ms: None,
            kind: boombox_core::api::EntryKind::Recent,
        }
    }

    /// The recents sit above the playlists without being part of the paged
    /// list, so counting them into the offset would skip exactly that many
    /// real playlists on the second page -- silently, and only for people
    /// with enough playlists to page at all.
    #[test]
    fn recents_at_the_top_do_not_shift_the_paging_offset() {
        let mut app = App::new(5, false);
        app.view = View::Playlists;
        app.focus = Focus::Main;
        let mut rows = vec![recent("spotify:playlist:mix1"), recent("spotify:playlist:mix2")];
        rows.extend((0..50).map(|i| entry(&format!("p{i}"))));
        app.set_entries(rows, 900, false);

        app.update(Action::Bottom);
        let more = app.maybe_load_more().unwrap();
        assert_eq!(
            more,
            Command::LoadMore(View::Playlists, 50, String::new()),
            "52 rows on screen, but only 50 of them came from the API"
        );
    }

    /// Spotify's own playlists answer 404 when fetched, so opening one is
    /// the one thing a recent must never do.
    #[test]
    fn selecting_a_recent_plays_it_rather_than_opening_it() {
        let mut app = App::new(5, false);
        app.view = View::Playlists;
        app.focus = Focus::Main;
        app.set_entries(vec![recent("spotify:playlist:37i9dQZF1EExampleMix01")], 1, false);

        let command = app.update(Action::Select).unwrap();
        assert_eq!(
            command,
            Command::Play(boombox_core::api::Playback::Context {
                uri: "spotify:playlist:37i9dQZF1EExampleMix01".into(),
                start: None,
            })
        );
        assert_eq!(app.view, View::Playlists, "stayed put");
    }

    #[test]
    fn paging_stops_once_everything_is_loaded() {
        let mut app = App::new(5, false);
        app.view = View::Liked;
        app.focus = Focus::Main;
        app.set_entries(vec![entry("only")], 1, false);
        app.update(Action::Bottom);
        assert!(!app.has_more());
        assert_eq!(app.maybe_load_more(), None);
    }

    pub(crate) fn entry(title: &str) -> Entry {
        Entry {
            title: title.into(),
            subtitle: String::new(),
            uri: Some(format!("spotify:track:{title}")),
            duration_ms: Some(1000),
            kind: boombox_core::api::EntryKind::Track,
        }
    }

    #[test]
    fn a_track_change_is_reported_so_the_queue_can_be_reloaded() {
        let mut app = App::new(5, false);
        assert!(app.set_playback(Some(playing_state(50, false, true))), "first state is a change");
        assert!(!app.set_playback(Some(playing_state(50, false, true))), "same track");

        let other: PlaybackState = serde_json::from_str(
            r#"{"is_playing":true,"progress_ms":0,
                "item":{"type":"track","name":"y","uri":"spotify:track:y",
                        "duration_ms":1000,"artists":[],"album":{}}}"#,
        )
        .unwrap();
        assert!(app.set_playback(Some(other)), "different track");
    }

    #[test]
    fn cursor_does_not_run_past_a_shrinking_list() {
        let mut app = App::new(5, false);
        app.view = View::Devices;
        app.focus = Focus::Main;
        app.set_devices(vec![device("a"), device("b"), device("c")]);
        app.update(Action::Bottom);
        assert_eq!(app.device_index, 2);
        // A device disappears between polls.
        app.set_devices(vec![device("a")]);
        assert_eq!(app.device_index, 0);
    }

    fn queued(name: &str, uri: &str) -> PlayingItem {
        serde_json::from_str(&format!(
            r#"{{"type":"track","name":"{name}","uri":"{uri}","duration_ms":1000,
                 "artists":[{{"name":"someone"}}],"album":{{"name":"an album"}}}}"#
        ))
        .expect("a track")
    }

    /// Naming the tracks is exact, where pressing next once per row
    /// drifted, and restarting a context silently played the wrong thing
    /// for anything that context did not contain.
    fn playing_with_context(context: &str) -> PlaybackState {
        serde_json::from_str(&format!(
            r#"{{"is_playing":true,"progress_ms":0,"shuffle_state":false,
                 "context":{{"uri":"{context}","type":"playlist"}},
                 "item":{{"type":"track","name":"current","uri":"spotify:track:cur",
                          "duration_ms":1000,"artists":[{{"name":"a"}}],
                          "album":{{"name":"b"}}}}}}"#
        ))
        .expect("a state")
    }

    fn queue_app(context: Option<&str>) -> App {
        let mut app = App::new(5, false);
        app.view = View::Queue;
        app.focus = Focus::Main;
        if let Some(c) = context {
            app.set_playback(Some(playing_with_context(c)));
        }
        app.set_queue(vec![
            queued("first", "spotify:track:a"),
            queued("second", "spotify:track:b"),
            queued("third", "spotify:track:c"),
        ]);
        app
    }

    /// The context keeps playing past the twenty rows the queue shows.
    /// Playing the rows alone turned a nine-hundred-track list into three.
    #[test]
    fn a_queue_jump_continues_the_context_it_came_from() {
        let mut app = queue_app(Some("spotify:playlist:p"));
        // Row 0 is what is playing; row 2 is the second upcoming track.
        app.queue_index = 2;
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Context {
                uri: "spotify:playlist:p".into(),
                start: Some("spotify:track:b".into()),
            }))
        );
    }

    /// Nothing to continue, so the rows are all there is.
    #[test]
    fn a_queue_jump_without_a_context_plays_the_rows() {
        // Nothing playing, so there is no "now playing" row and the
        // upcoming tracks start at zero.
        let mut app = queue_app(None);
        app.queue_index = 1;
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Tracks {
                uris: vec!["spotify:track:b".into(), "spotify:track:c".into()],
                start: 0,
            }))
        );
    }

    /// The API answers 204 and plays something else when the offset names
    /// a track outside the context, so a jump has to be checked rather
    /// than trusted.
    #[test]
    fn a_jump_that_missed_is_corrected_with_the_queue_rows() {
        let mut app = queue_app(Some("spotify:playlist:p"));
        app.queue_index = 2;
        app.update(Action::Select);

        // Too soon to judge: the API still reports the old track.
        assert_eq!(app.check_jump(), None, "must not judge before it settles");

        app.expecting.as_mut().unwrap().asked_at = Instant::now() - JUMP_SETTLE;
        // Landed somewhere else entirely.
        app.set_playback(Some(playing_with_context("spotify:playlist:p")));
        assert_eq!(
            app.check_jump(),
            Some(Command::Play(Playback::Tracks {
                uris: vec!["spotify:track:b".into(), "spotify:track:c".into()],
                start: 0,
            }))
        );
        assert_eq!(app.check_jump(), None, "corrected once, not in a loop");
    }

    #[test]
    fn a_jump_that_landed_needs_no_correction() {
        let mut app = queue_app(Some("spotify:playlist:p"));
        app.queue_index = 2;
        app.update(Action::Select);
        app.expecting.as_mut().unwrap().asked_at = Instant::now() - JUMP_SETTLE;

        let mut landed = playing_with_context("spotify:playlist:p");
        landed.item = serde_json::from_str(
            r#"{"type":"track","name":"second","uri":"spotify:track:b","duration_ms":1000,
                "artists":[{"name":"a"}],"album":{"name":"b"}}"#,
        )
        .ok();
        app.set_playback(Some(landed));
        assert_eq!(app.check_jump(), None, "it went where it was aimed");
    }

    #[test]
    fn selecting_a_queue_row_plays_the_queue_from_there() {
        let mut app = App::new(5, false);
        app.view = View::Queue;
        app.focus = Focus::Main;
        app.set_queue(vec![
            queued("first", "spotify:track:a"),
            queued("second", "spotify:track:b"),
            queued("third", "spotify:track:c"),
        ]);

        app.queue_index = 1;
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Tracks {
                // From the chosen row on, so what was above it is gone and
                // what was below it still follows.
                uris: vec!["spotify:track:b".into(), "spotify:track:c".into()],
                start: 0,
            }))
        );
    }

    #[test]
    fn selecting_the_last_queue_row_plays_just_that_track() {
        let mut app = App::new(5, false);
        app.view = View::Queue;
        app.focus = Focus::Main;
        app.set_queue(vec![queued("only", "spotify:track:z")]);
        assert_eq!(
            app.update(Action::Select),
            Some(Command::Play(Playback::Tracks {
                uris: vec!["spotify:track:z".into()],
                start: 0,
            }))
        );
    }

    /// The track playing sits at the top of the list, so pressing enter
    /// on it means "I am already here" rather than restarting it.
    #[test]
    fn the_now_playing_row_is_not_somewhere_to_jump_to() {
        let mut app = queue_app(Some("spotify:playlist:p"));
        app.queue_index = 0;
        assert_eq!(app.update(Action::Select), None);
    }

    /// After a jump the chosen track is the one playing, and the one
    /// playing is row zero -- leaving the cursor where it was would point
    /// at a track that has moved up the list.
    #[test]
    fn a_jump_leaves_the_cursor_on_what_is_now_playing() {
        let mut app = queue_app(Some("spotify:playlist:p"));
        app.queue_index = 3;
        app.update(Action::Select);
        assert_eq!(app.queue_index, 0);
    }

    /// With nothing playing there is no first row, so the upcoming tracks
    /// are the whole list.
    #[test]
    fn the_rows_shift_by_one_only_when_something_is_playing() {
        let playing = queue_app(Some("spotify:playlist:p"));
        assert!(playing.playing_row());
        assert!(playing.queue_target(0).is_none(), "row 0 is the current track");
        assert_eq!(playing.queue_target(1).map(|i| i.name()), Some("first"));

        let idle = queue_app(None);
        assert!(!idle.playing_row());
        assert_eq!(idle.queue_target(0).map(|i| i.name()), Some("first"));
    }

    #[test]
    fn selecting_an_empty_queue_does_nothing() {
        let mut app = App::new(5, false);
        app.view = View::Queue;
        app.focus = Focus::Main;
        assert_eq!(app.update(Action::Select), None);
    }

    #[test]
    fn help_swallows_the_next_key_but_still_allows_quitting() {
        let mut app = App::new(5, false);
        app.update(Action::ToggleHelp);
        assert!(app.show_help);
        app.update(Action::Down);
        assert!(!app.show_help, "any key closes help");

        let mut app = App::new(5, false);
        app.update(Action::ToggleHelp);
        app.update(Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn spreading_bleeds_energy_into_neighbours() {
        let mut raw = vec![0.0; 9];
        raw[4] = 1.0;
        let out = spread_bands(&raw);
        assert_eq!(out[4], 1.0, "the source band is untouched");
        assert!(out[3] > 0.0 && out[5] > 0.0, "neighbours pick it up: {out:?}");
        assert!(out[3] > out[2], "and it falls off with distance");
        assert!(out[2] > out[1]);
    }

    #[test]
    fn spreading_is_symmetric_and_bounded() {
        // Wide enough that the ends sit beyond SPREAD_REACH of the source.
        let mut raw = vec![0.0; 15];
        raw[7] = 1.0;
        let out = spread_bands(&raw);
        assert!((out[6] - out[8]).abs() < 1e-6, "same both sides");
        assert!(out.iter().all(|v| *v <= 1.0), "never exceeds the source: {out:?}");
        assert_eq!(out[7 - SPREAD_REACH - 1], 0.0, "beyond the reach it stays silent");
        assert_eq!(out[7 + SPREAD_REACH + 1], 0.0);
        assert!(out[7 - SPREAD_REACH] > 0.0, "but the edge of the reach is lit");
    }

    #[test]
    fn spreading_takes_the_max_rather_than_summing() {
        // Two loud neighbours must not add up into something louder than
        // either, or busy passages would wash the whole display out.
        let raw = vec![0.0, 1.0, 1.0, 0.0];
        let out = spread_bands(&raw);
        assert!(out.iter().all(|v| *v <= 1.0), "{out:?}");
    }

    #[test]
    fn spreading_leaves_tiny_band_counts_alone() {
        assert_eq!(spread_bands(&[0.5, 0.2]), vec![0.5, 0.2]);
        assert!(spread_bands(&[]).is_empty());
    }

    #[test]
    fn bands_rise_instantly_and_sink_gradually() {
        let mut app = App::new(5, false);
        app.set_spectrum(vec![0.0; 8]);
        app.set_spectrum(vec![0.9; 8]);
        assert!(app.smoothed[4] > 0.85, "attack is immediate: {:?}", app.smoothed[4]);

        std::thread::sleep(Duration::from_millis(60));
        app.set_spectrum(vec![0.0; 8]);
        let shown = app.smoothed[4];
        assert!(shown > 0.0, "release is gradual, not a cut to silence");
        assert!(shown < 0.9, "but it is falling: {shown}");
    }

    #[test]
    fn the_raw_reading_is_kept_alongside_the_smoothed_one() {
        let mut app = App::new(5, false);
        let mut raw = vec![0.0; 8];
        raw[0] = 1.0;
        app.set_spectrum(raw.clone());
        assert_eq!(app.spectrum, raw, "history and analysis want the truth");
        assert!(app.smoothed[1] > 0.0, "only the display is smoothed");
    }

    #[test]
    fn the_trigger_finds_a_rising_zero_crossing() {
        //            0     1     2    3     4
        let wave = [-0.5, -0.2, 0.3, 0.6, -0.1];
        assert_eq!(trigger_offset(&wave, 5), 1, "the sample before the rise");
    }

    #[test]
    fn the_trigger_ignores_falling_crossings() {
        let wave = [0.5, 0.2, -0.3, -0.1, 0.4];
        // Index 1->2 falls through zero and must not trigger; 3->4 rises.
        assert_eq!(trigger_offset(&wave, 5), 3);
    }

    #[test]
    fn the_trigger_falls_back_to_the_start_when_nothing_rises() {
        assert_eq!(trigger_offset(&[0.5, 0.4, 0.3], 3), 0, "no crossing at all");
        assert_eq!(trigger_offset(&[], 0), 0, "and no samples");
    }

    #[test]
    fn the_trigger_only_searches_the_slack_it_is_given() {
        // The rise is at index 3, beyond the search limit of 2.
        let wave = [-0.5, -0.4, -0.3, 0.6, 0.7];
        assert_eq!(trigger_offset(&wave, 2), 0, "must not consume the display window");
    }

    #[test]
    fn the_scope_window_is_trimmed_to_the_span_it_draws() {
        let mut app = App::new(5, false);
        let long: Vec<f32> = (0..512).map(|i| ((i as f32) / 20.0).sin() * 0.5).collect();
        app.set_waveform(long);
        assert_eq!(app.waveform.len(), SCOPE_SPAN);
        assert!(app.waveform.iter().all(|s| (-1.0..=1.0).contains(s)));
    }

    #[test]
    fn scope_gain_rises_slowly_and_falls_quickly() {
        let mut app = App::new(5, false);
        // Above the silence floor, or the gain is deliberately held.
        let quiet: Vec<f32> = (0..512).map(|i| ((i as f32) / 20.0).sin() * 0.05).collect();
        let loud: Vec<f32> = (0..512).map(|i| ((i as f32) / 20.0).sin() * 0.9).collect();

        // A very quiet signal wants a large gain, but must not get it at once.
        app.set_waveform(quiet.clone());
        std::thread::sleep(Duration::from_millis(40));
        app.set_waveform(quiet);
        let after_quiet = app.scope_gain;
        assert!(after_quiet > 1.0, "gain should be climbing: {after_quiet}");
        assert!(after_quiet < 18.0, "but nowhere near the target of ~18: {after_quiet}");

        // Going loud pulls it back down much faster than it went up.
        std::thread::sleep(Duration::from_millis(40));
        app.set_waveform(loud);
        assert!(app.scope_gain < after_quiet, "loud audio pulls the gain down");
    }

    #[test]
    fn an_empty_waveform_clears_the_trace() {
        let mut app = App::new(5, false);
        app.set_waveform(vec![0.5; 512]);
        app.set_waveform(Vec::new());
        assert!(app.waveform.is_empty());
    }

    #[test]
    fn peaks_start_at_the_first_reading_then_fall_toward_it() {
        let mut app = App::new(5, false);
        app.set_spectrum(vec![0.9, 0.2]);
        assert_eq!(app.peaks, vec![0.9, 0.2], "first frame seeds the peaks");

        std::thread::sleep(Duration::from_millis(60));
        app.set_spectrum(vec![0.0, 0.0]);
        assert!(app.peaks[0] < 0.9, "peak must fall");
        assert!(app.peaks[0] > 0.0, "but not instantly");
    }

    #[test]
    fn a_peak_never_sits_below_the_current_value() {
        let mut app = App::new(5, false);
        app.set_spectrum(vec![0.1]);
        app.set_spectrum(vec![0.8]);
        assert_eq!(app.peaks[0], 0.8, "a rising band pushes its own peak up");
    }

    #[test]
    fn peaks_never_go_negative_or_desync_from_the_band_count() {
        let mut app = App::new(5, false);
        app.set_spectrum(vec![0.5; 4]);
        std::thread::sleep(Duration::from_millis(80));
        app.set_spectrum(vec![0.0; 4]);
        assert!(app.peaks.iter().all(|p| *p >= 0.0), "{:?}", app.peaks);

        // The band count can change if the pane is reconfigured.
        app.set_spectrum(vec![0.3; 16]);
        assert_eq!(app.peaks.len(), 16);
    }

    #[test]
    fn history_accumulates_newest_last_and_is_capped() {
        let mut app = App::new(5, false);
        for i in 0..(HISTORY + 40) {
            app.set_spectrum(vec![i as f32 / 1000.0]);
        }
        assert_eq!(app.history.len(), HISTORY, "history must not grow without bound");
        let newest = app.history.back().unwrap()[0];
        let oldest = app.history.front().unwrap()[0];
        assert!(newest > oldest, "newest frame belongs at the back");
    }

    #[test]
    fn empty_spectrum_frames_are_not_recorded() {
        let mut app = App::new(5, false);
        app.set_spectrum(Vec::new());
        assert!(app.history.is_empty(), "silence with no bands is not a frame");
    }

    #[test]
    /// "Off" is part of the cycle, so one key covers the whole choice of
    /// stage rather than a key plus a separate toggle.
    fn v_cycles_off_through_every_visualisation_and_back_to_off() {
        let mut app = App::new(5, false);
        assert_eq!(app.visual, None, "the stage starts on the track itself");
        for expected in [VisualMode::Bars, VisualMode::Spectrogram, VisualMode::Scope] {
            app.update(Action::CycleVisual);
            assert_eq!(app.visual, Some(expected));
        }
        app.update(Action::CycleVisual);
        assert_eq!(app.visual, None, "and back off");
    }

    #[test]
    fn raw_samples_are_only_requested_by_the_scope() {
        let mut app = App::new(5, false);
        assert!(!app.wants_spectrum(), "nothing to poll with the stage off");

        app.cycle_visual(); // bars
        assert!(app.wants_spectrum());
        assert!(!app.wants_waveform(), "bars work from the band stream");
        app.cycle_visual(); // spectrogram
        assert!(!app.wants_waveform(), "so does the spectrogram");
        app.cycle_visual(); // scope
        assert!(app.wants_waveform(), "the scope needs samples");
        assert!(!app.wants_spectrum(), "and not the bands");

        app.cycle_visual(); // off
        assert!(!app.wants_waveform());
        assert!(!app.wants_spectrum());
    }

    /// The stage keeps running behind the browser -- that is the point of
    /// making browsing a layer rather than a separate view.
    /// Idle hands the screen to the stage. Each of these conditions was
    /// chosen to stop it firing at a moment that would be actively annoying.
    #[test]
    fn idle_needs_a_visualisation_playing_and_nothing_in_the_way() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        app.cycle_visual();
        app.last_input = Instant::now() - Duration::from_secs(60);
        assert!(app.is_idle(), "playing, visualising, and left alone");

        app.note_input();
        assert!(!app.is_idle(), "a keypress wakes it");
    }

    #[test]
    fn idle_stays_away_while_something_is_in_front_of_the_stage() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        app.cycle_visual();
        app.last_input = Instant::now() - Duration::from_secs(60);

        app.browse_open = true;
        assert!(!app.is_idle(), "not with the browser open");
        app.browse_open = false;

        app.typing = true;
        assert!(!app.is_idle(), "and not mid-search");
        app.typing = false;

        app.show_help = true;
        assert!(!app.is_idle(), "nor over the help");
        app.show_help = false;
        assert!(app.is_idle());
    }

    /// A full screen of silence is worse than not idling at all.
    #[test]
    fn idle_does_not_fire_when_the_music_is_not_playing() {
        let mut app = App::new(5, false);
        app.cycle_visual();
        app.last_input = Instant::now() - Duration::from_secs(60);
        assert!(!app.is_idle(), "nothing is playing at all");

        let mut paused = playing_state(50, false, true);
        paused.is_playing = false;
        app.set_playback(Some(paused));
        assert!(!app.is_idle(), "and a paused track is not a screensaver");
    }

    #[test]
    fn idle_does_not_fire_with_the_stage_showing_the_track() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        app.last_input = Instant::now() - Duration::from_secs(60);
        assert_eq!(app.visual, None);
        assert!(!app.is_idle(), "there is nothing to go full screen with");
    }

    #[test]
    fn the_visualisation_keeps_polling_while_the_browser_is_open() {
        let mut app = App::new(5, false);
        app.cycle_visual();
        assert!(app.wants_spectrum());
        app.update(Action::OpenQueue);
        assert!(app.browse_open);
        assert!(app.wants_spectrum(), "the stage did not stop for the browser");
    }

    #[test]
    fn playback_progress_advances_between_polls() {
        let mut app = App::new(5, false);
        app.set_playback(Some(playing_state(50, false, true)));
        let first = app.playback().unwrap().progress();
        std::thread::sleep(Duration::from_millis(30));
        assert!(app.playback().unwrap().progress() > first);
    }

    pub(crate) fn device(name: &str) -> Device {
        Device {
            id: Some(format!("id-{name}")),
            name: name.into(),
            device_type: "Computer".into(),
            is_active: false,
            is_restricted: false,
            volume_percent: Some(50),
            supports_volume: true,
        }
    }
}
