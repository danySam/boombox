use serde::{Deserialize, Serialize};
use serde_json::json;

use super::player_models::{Device, Devices, PlaybackState, Queue, RepeatState};
use super::{Client, Request};
use crate::error::Result;

/// The swappable surface. Everything the front ends need to drive playback
/// goes through here, so a future librespot-backed implementation can stand in
/// without the CLI, TUI or daemon noticing.
///
/// The futures are declared `Send` rather than written as bare `async fn` so
/// the TUI can drive them from spawned tasks; a render loop that awaits the
/// network inline is a frozen render loop. Implementations still write
/// ordinary `async fn`.
pub trait PlayerApi: Send + Sync {
    /// `None` means Spotify answered 204: no device is active at all.
    fn playback_state(&self) -> impl Future<Output = Result<Option<PlaybackState>>> + Send;
    fn devices(&self) -> impl Future<Output = Result<Vec<Device>>> + Send;
    fn play(&self, opts: PlayOptions) -> impl Future<Output = Result<()>> + Send;
    fn pause(&self) -> impl Future<Output = Result<()>> + Send;
    fn next(&self) -> impl Future<Output = Result<()>> + Send;
    fn previous(&self) -> impl Future<Output = Result<()>> + Send;
    fn seek(&self, position_ms: u64) -> impl Future<Output = Result<()>> + Send;
    fn set_volume(&self, percent: u32) -> impl Future<Output = Result<()>> + Send;
    fn set_shuffle(&self, on: bool) -> impl Future<Output = Result<()>> + Send;
    fn set_repeat(&self, state: RepeatState) -> impl Future<Output = Result<()>> + Send;
    fn transfer(&self, device_id: &str, play: bool) -> impl Future<Output = Result<()>> + Send;
    fn queue(&self) -> impl Future<Output = Result<Queue>> + Send;
    fn add_to_queue(&self, uri: &str) -> impl Future<Output = Result<()>> + Send;
}

/// Live audio spectrum, available only when a streaming daemon is decoding.
///
/// Separate from [`PlayerApi`] because it is not a Spotify capability at all:
/// it needs local access to the samples, which only the daemon has. The HTTP
/// client implements it by returning nothing.
pub trait SpectrumApi: Send + Sync {
    /// `bands` magnitudes in 0.0..=1.0, low frequency first. An empty vector
    /// means no audio is being decoded here.
    fn spectrum(&self, bands: u16) -> impl Future<Output = Result<Vec<f32>>> + Send;

    /// The recent waveform, resampled to `points` values in -1.0..=1.0.
    fn waveform(&self, points: u16) -> impl Future<Output = Result<Vec<f32>>> + Send;

    /// Peak amplitude across the current track so far, for the seek bar.
    fn envelope(&self, points: u16) -> impl Future<Output = Result<Vec<f32>>> + Send;

    /// How long, in seconds, Spotify has reported this computer as playing
    /// while no audio reaches it -- when that is happening right now.
    ///
    /// Only a streaming daemon can know, so the default is nothing, which is
    /// the honest answer from anywhere else.
    fn audio_stalled_secs(&self) -> impl Future<Output = Result<Option<u64>>> + Send {
        async { Ok(None) }
    }
}

impl SpectrumApi for Client {
    // The Web API never carries audio; only the daemon can answer these.
    async fn spectrum(&self, _bands: u16) -> Result<Vec<f32>> {
        Ok(Vec::new())
    }

    async fn waveform(&self, _points: u16) -> Result<Vec<f32>> {
        Ok(Vec::new())
    }

    async fn envelope(&self, _points: u16) -> Result<Vec<f32>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PlayOptions {
    /// An album, artist or playlist to play from.
    pub context_uri: Option<String>,
    /// Explicit tracks. Mutually exclusive with `context_uri`.
    pub uris: Vec<String>,
    /// Where in the context or list to start.
    pub offset: Option<Offset>,
    pub position_ms: Option<u64>,
    pub device_id: Option<String>,
}

/// How a chosen track should be played.
///
/// Three variants because Spotify offers three mechanisms, and which one
/// applies depends on where the track was chosen from:
///
/// - A playlist, an album or Liked Songs is a *context*: name it and
///   playback continues through the whole thing, however long it is and
///   however little of it we have loaded. Liked Songs is
///   `spotify:collection:tracks`, which the Web API accepts but does not
///   document.
/// - Search results are not a context and have no URI of their own, so the
///   tracks have to be sent one by one. Continuation therefore stops at
///   whatever has been paged in, which is a real limitation and not a bug.
/// - A single track is just itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Playback {
    /// This track, alone.
    Track(String),
    /// A context, optionally starting partway in.
    Context { uri: String, start: Option<String> },
    /// A list with no context of its own.
    Tracks { uris: Vec<String>, start: usize },
}

impl From<Playback> for PlayOptions {
    fn from(playback: Playback) -> Self {
        match playback {
            Playback::Track(uri) => Self::tracks(vec![uri]),
            Playback::Context { uri, start } => {
                Self { offset: start.map(Offset::Uri), ..Self::context(uri) }
            }
            Playback::Tracks { uris, start } => Self {
                // The whole list goes out with a starting point rather than
                // being sliced, so the tracks above the cursor stay part of
                // what is playing.
                offset: Some(Offset::Position(start as u32)),
                ..Self::tracks(uris)
            },
        }
    }
}

/// Where playback should begin within a context or list.
///
/// By URI wherever possible: a position is an index into a list the server
/// holds and we only see a page of, so the two can disagree. Naming the
/// track cannot drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Offset {
    Position(u32),
    Uri(String),
}

impl Offset {
    fn as_json(&self) -> serde_json::Value {
        match self {
            Self::Position(n) => json!({ "position": n }),
            Self::Uri(uri) => json!({ "uri": uri }),
        }
    }
}

impl PlayOptions {
    /// Resume whatever is loaded.
    pub fn resume() -> Self {
        Self::default()
    }

    pub fn context(uri: impl Into<String>) -> Self {
        Self { context_uri: Some(uri.into()), ..Self::default() }
    }

    pub fn tracks(uris: Vec<String>) -> Self {
        Self { uris, ..Self::default() }
    }

    pub fn on_device(mut self, device_id: Option<String>) -> Self {
        self.device_id = device_id;
        self
    }

    fn body(&self) -> Option<serde_json::Value> {
        let mut map = serde_json::Map::new();
        if let Some(uri) = &self.context_uri {
            map.insert("context_uri".into(), json!(uri));
        }
        if !self.uris.is_empty() {
            map.insert("uris".into(), json!(self.uris));
        }
        if let Some(offset) = &self.offset {
            map.insert("offset".into(), offset.as_json());
        }
        if let Some(pos) = self.position_ms {
            map.insert("position_ms".into(), json!(pos));
        }
        // An empty body means "resume", which is not the same as `{}`.
        (!map.is_empty()).then_some(serde_json::Value::Object(map))
    }
}

/// One entry from `/me/player/recently-played`.
///
/// Only the context matters here. The endpoint is the one place Spotify
/// admits which playlists you have been listening to, including its own,
/// which `/me/playlists` and search both leave out entirely.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PlayHistory {
    #[serde(default)]
    pub context: Option<crate::api::player_models::Context>,
    #[serde(default)]
    pub played_at: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct PlayHistoryPage {
    #[serde(default)]
    items: Vec<PlayHistory>,
}

impl Client {
    /// The last 50 plays, newest first. Used once at daemon start to give
    /// the recents list something in it before you have played anything.
    pub async fn recently_played(&self) -> Result<Vec<PlayHistory>> {
        let page: PlayHistoryPage =
            self.json(Request::get("/me/player/recently-played").query("limit", "50")).await?;
        Ok(page.items)
    }
}

impl PlayerApi for Client {
    async fn playback_state(&self) -> Result<Option<PlaybackState>> {
        // additional_types keeps podcast episodes from arriving as null items.
        self.json_opt(Request::get("/me/player").query("additional_types", "track,episode")).await
    }

    async fn devices(&self) -> Result<Vec<Device>> {
        let d: Devices = self.json(Request::get("/me/player/devices")).await?;
        Ok(d.devices)
    }

    async fn play(&self, opts: PlayOptions) -> Result<()> {
        let mut req =
            Request::put("/me/player/play").maybe_query("device_id", opts.device_id.as_ref());
        if let Some(body) = opts.body() {
            req = req.json(body);
        }
        self.empty(req).await
    }

    async fn pause(&self) -> Result<()> {
        self.empty(Request::put("/me/player/pause")).await
    }

    async fn next(&self) -> Result<()> {
        self.empty(Request::post("/me/player/next")).await
    }

    async fn previous(&self) -> Result<()> {
        self.empty(Request::post("/me/player/previous")).await
    }

    async fn seek(&self, position_ms: u64) -> Result<()> {
        self.empty(Request::put("/me/player/seek").query("position_ms", position_ms)).await
    }

    async fn set_volume(&self, percent: u32) -> Result<()> {
        self.empty(Request::put("/me/player/volume").query("volume_percent", percent.min(100)))
            .await
    }

    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.empty(Request::put("/me/player/shuffle").query("state", on)).await
    }

    async fn set_repeat(&self, state: RepeatState) -> Result<()> {
        self.empty(Request::put("/me/player/repeat").query("state", state.as_api_str())).await
    }

    async fn transfer(&self, device_id: &str, play: bool) -> Result<()> {
        self.empty(
            Request::put("/me/player").json(json!({ "device_ids": [device_id], "play": play })),
        )
        .await
    }

    async fn queue(&self) -> Result<Queue> {
        self.json(Request::get("/me/player/queue")).await
    }

    async fn add_to_queue(&self, uri: &str) -> Result<()> {
        self.empty(Request::post("/me/player/queue").query("uri", uri)).await
    }
}

/// How a device name typed on the command line resolved.
#[derive(Debug, PartialEq)]
pub enum DeviceMatch<'a> {
    One(&'a Device),
    None,
    Ambiguous(Vec<&'a str>),
}

/// Exact id, then exact name, then case-insensitive name, then unique
/// case-insensitive prefix or substring. Anything matching more than one
/// device is reported rather than guessed at.
pub fn resolve_device<'a>(devices: &'a [Device], needle: &str) -> DeviceMatch<'a> {
    if let Some(d) = devices.iter().find(|d| d.id.as_deref() == Some(needle)) {
        return DeviceMatch::One(d);
    }
    if let Some(d) = devices.iter().find(|d| d.name == needle) {
        return DeviceMatch::One(d);
    }

    let lower = needle.to_lowercase();
    for candidates in [
        devices.iter().filter(|d| d.name.to_lowercase() == lower).collect::<Vec<_>>(),
        devices.iter().filter(|d| d.name.to_lowercase().starts_with(&lower)).collect(),
        devices.iter().filter(|d| d.name.to_lowercase().contains(&lower)).collect(),
    ] {
        match candidates.len() {
            0 => continue,
            1 => return DeviceMatch::One(candidates[0]),
            _ => {
                return DeviceMatch::Ambiguous(
                    candidates.iter().map(|d| d.name.as_str()).collect(),
                );
            }
        }
    }
    DeviceMatch::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, id: &str) -> Device {
        Device {
            id: Some(id.into()),
            name: name.into(),
            device_type: "Computer".into(),
            is_active: false,
            is_restricted: false,
            volume_percent: Some(50),
            supports_volume: true,
        }
    }

    fn fixture() -> Vec<Device> {
        vec![device("MacBook Pro", "d1"), device("Kitchen", "d2"), device("Kitchen Speaker", "d3")]
    }

    #[test]
    fn resume_sends_no_body() {
        assert!(PlayOptions::resume().body().is_none());
    }

    #[test]
    fn a_context_starts_where_it_is_told() {
        let opts = PlayOptions::from(Playback::Context {
            uri: "spotify:playlist:p".into(),
            start: Some("spotify:track:t".into()),
        });
        let body = opts.body().unwrap();
        assert_eq!(body["context_uri"], "spotify:playlist:p");
        // By URI, not position: we only ever see a page of the list.
        assert_eq!(body["offset"]["uri"], "spotify:track:t");
        assert!(body.get("uris").is_none());
    }

    #[test]
    fn a_context_without_a_start_plays_from_the_top() {
        let opts =
            PlayOptions::from(Playback::Context { uri: "spotify:album:a".into(), start: None });
        let body = opts.body().unwrap();
        assert_eq!(body["context_uri"], "spotify:album:a");
        assert!(body.get("offset").is_none());
    }

    /// A list with no context of its own goes out whole, with a position,
    /// so the tracks above the chosen one stay part of what is playing.
    #[test]
    fn a_track_list_keeps_what_comes_before_the_starting_point() {
        let opts = PlayOptions::from(Playback::Tracks {
            uris: vec!["spotify:track:a".into(), "spotify:track:b".into()],
            start: 1,
        });
        let body = opts.body().unwrap();
        assert_eq!(body["uris"].as_array().unwrap().len(), 2);
        assert_eq!(body["offset"]["position"], 1);
        assert!(body.get("context_uri").is_none());
    }

    #[test]
    fn a_single_track_carries_no_offset_at_all() {
        let opts = PlayOptions::from(Playback::Track("spotify:track:t".into()));
        let body = opts.body().unwrap();
        assert_eq!(body["uris"], json!(["spotify:track:t"]));
        assert!(body.get("offset").is_none());
        assert!(body.get("context_uri").is_none());
    }

    #[test]
    fn context_and_offset_are_shaped_the_way_spotify_wants() {
        let opts = PlayOptions {
            offset: Some(Offset::Position(3)),
            ..PlayOptions::context("spotify:album:x")
        };
        let body = opts.body().unwrap();
        assert_eq!(body["context_uri"], "spotify:album:x");
        assert_eq!(body["offset"]["position"], 3);
        assert!(body.get("uris").is_none());
    }

    #[test]
    fn explicit_tracks_are_sent_as_uris() {
        let body = PlayOptions::tracks(vec!["spotify:track:a".into()]).body().unwrap();
        assert_eq!(body["uris"][0], "spotify:track:a");
    }

    #[test]
    fn device_resolves_by_id_and_exact_name() {
        let d = fixture();
        assert_eq!(resolve_device(&d, "d2"), DeviceMatch::One(&d[1]));
        assert_eq!(resolve_device(&d, "MacBook Pro"), DeviceMatch::One(&d[0]));
    }

    #[test]
    fn device_resolves_case_insensitively() {
        let d = fixture();
        assert_eq!(resolve_device(&d, "macbook pro"), DeviceMatch::One(&d[0]));
    }

    #[test]
    fn exact_name_wins_over_a_longer_prefix_match() {
        let d = fixture();
        // "Kitchen" is also a prefix of "Kitchen Speaker"; the exact hit wins.
        assert_eq!(resolve_device(&d, "Kitchen"), DeviceMatch::One(&d[1]));
    }

    #[test]
    fn unique_prefix_and_substring_both_resolve() {
        let d = fixture();
        assert_eq!(resolve_device(&d, "Kitchen Sp"), DeviceMatch::One(&d[2]));
        assert_eq!(resolve_device(&d, "book"), DeviceMatch::One(&d[0]));
    }

    #[test]
    fn ambiguity_is_reported_not_guessed() {
        let d = vec![device("Kitchen One", "a"), device("Kitchen Two", "b")];
        match resolve_device(&d, "kitchen") {
            DeviceMatch::Ambiguous(names) => assert_eq!(names, vec!["Kitchen One", "Kitchen Two"]),
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn unknown_device_is_none() {
        assert_eq!(resolve_device(&fixture(), "hifi"), DeviceMatch::None);
    }
}
