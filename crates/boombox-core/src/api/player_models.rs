use serde::{Deserialize, Serialize};

use super::models::Image;

/// Which parts of a state are what someone asked for rather than what the
/// player has reported yet.
///
/// Set by the daemon, which holds a write in front of the truth until the
/// truth catches up. Spotify never sends this, so it defaults to nothing
/// pending and a state read straight from the API is honest by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pendings {
    #[serde(default)]
    pub volume: bool,
    #[serde(default)]
    pub position: bool,
    #[serde(default)]
    pub playing: bool,
}

impl Pendings {
    /// Whether anything at all is waiting on the player to agree.
    pub fn any(&self) -> bool {
        self.volume || self.position || self.playing
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackState {
    pub device: Option<Device>,
    #[serde(default)]
    pub repeat_state: RepeatState,
    #[serde(default)]
    pub shuffle_state: bool,
    #[serde(default)]
    pub progress_ms: Option<u64>,
    #[serde(default)]
    pub is_playing: bool,
    pub item: Option<PlayingItem>,
    pub context: Option<Context>,
    /// Which of the values above are still waiting to be confirmed.
    #[serde(default)]
    pub pending: Pendings,
}

impl PlaybackState {
    pub fn progress(&self) -> u64 {
        self.progress_ms.unwrap_or(0)
    }

    pub fn duration(&self) -> u64 {
        self.item.as_ref().map(PlayingItem::duration_ms).unwrap_or(0)
    }

    /// 0.0..=1.0, saturating. Live streams report a zero duration.
    pub fn fraction(&self) -> f64 {
        match self.duration() {
            0 => 0.0,
            d => (self.progress() as f64 / d as f64).clamp(0.0, 1.0),
        }
    }

    pub fn volume(&self) -> Option<u32> {
        self.device.as_ref().and_then(|d| d.volume_percent)
    }

    /// A copy with the progress clock advanced by `elapsed`, clamped to the
    /// track. Paused states are returned unchanged. Both the daemon (between
    /// polls) and the TUI (between frames) need this, so it lives here.
    pub fn advanced_by(&self, elapsed: std::time::Duration) -> Self {
        let mut advanced = self.clone();
        if !self.is_playing {
            return advanced;
        }
        let projected = self.progress() + elapsed.as_millis() as u64;
        let duration = self.duration();
        advanced.progress_ms = Some(if duration > 0 { projected.min(duration) } else { projected });
        advanced
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub device_type: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_restricted: bool,
    #[serde(default)]
    pub volume_percent: Option<u32>,
    #[serde(default = "default_true")]
    pub supports_volume: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatState {
    #[default]
    Off,
    Track,
    Context,
}

impl RepeatState {
    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Context => "context",
        }
    }

    /// off -> context -> track -> off. Matches the cycle order in the official
    /// clients, which is what muscle memory expects.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Context,
            Self::Context => Self::Track,
            Self::Track => Self::Off,
        }
    }
}

impl std::str::FromStr for RepeatState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Self::Off),
            "track" | "one" | "song" => Ok(Self::Track),
            "context" | "all" | "playlist" | "album" => Ok(Self::Context),
            other => Err(format!("expected off, track or context, got `{other}`")),
        }
    }
}

impl std::fmt::Display for RepeatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_api_str())
    }
}

/// `type` discriminates these in the payload. Ads and unknown content arrive
/// with a null or unrecognised item, so the enum has to tolerate both.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PlayingItem {
    Track(Track),
    Episode(Episode),
    #[serde(other)]
    Unknown,
}

impl PlayingItem {
    pub fn name(&self) -> &str {
        match self {
            Self::Track(t) => &t.name,
            Self::Episode(e) => &e.name,
            Self::Unknown => "\u{2014}",
        }
    }

    /// Artists for a track, the show for an episode.
    pub fn byline(&self) -> String {
        match self {
            Self::Track(t) => t.artist_names(),
            Self::Episode(e) => e.show.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            Self::Unknown => String::new(),
        }
    }

    pub fn collection(&self) -> Option<&str> {
        match self {
            Self::Track(t) => Some(t.album.name.as_str()),
            Self::Episode(e) => e.show.as_ref().map(|s| s.name.as_str()),
            Self::Unknown => None,
        }
    }

    pub fn duration_ms(&self) -> u64 {
        match self {
            Self::Track(t) => t.duration_ms,
            Self::Episode(e) => e.duration_ms,
            Self::Unknown => 0,
        }
    }

    pub fn uri(&self) -> Option<&str> {
        match self {
            Self::Track(t) => Some(&t.uri),
            Self::Episode(e) => Some(&e.uri),
            Self::Unknown => None,
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Track(t) => t.id.as_deref(),
            Self::Episode(e) => e.id.as_deref(),
            Self::Unknown => None,
        }
    }

    pub fn images(&self) -> &[Image] {
        match self {
            Self::Track(t) => &t.album.images,
            Self::Episode(e) => &e.images,
            Self::Unknown => &[],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    /// Null for local files.
    pub id: Option<String>,
    pub name: String,
    pub uri: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub is_local: bool,
    #[serde(default)]
    pub artists: Vec<SimpleArtist>,
    #[serde(default)]
    pub album: Album,
}

impl Track {
    pub fn artist_names(&self) -> String {
        self.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: Option<String>,
    pub name: String,
    pub uri: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub images: Vec<Image>,
    pub show: Option<Show>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Show {
    pub id: Option<String>,
    pub name: String,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Album {
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub artists: Vec<SimpleArtist>,
    #[serde(default)]
    pub total_tracks: Option<u32>,
}

impl Album {
    pub fn artist_names(&self) -> String {
        self.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleArtist {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub uri: String,
    #[serde(rename = "type")]
    pub context_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Devices {
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queue {
    pub currently_playing: Option<PlayingItem>,
    #[serde(default)]
    pub queue: Vec<PlayingItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYING_TRACK: &str = r#"{
        "device": {"id":"abc","is_active":true,"is_restricted":false,
                   "name":"MacBook Pro","type":"Computer","volume_percent":62,
                   "supports_volume":true},
        "repeat_state":"context","shuffle_state":true,
        "progress_ms":107000,"is_playing":true,
        "context":{"uri":"spotify:playlist:xyz","type":"playlist"},
        "item":{"type":"track","id":"t1","name":"Glass Roads",
                "uri":"spotify:track:t1","duration_ms":252000,"explicit":false,
                "artists":[{"id":"a1","name":"Lowtide","uri":"spotify:artist:a1"}],
                "album":{"id":"al1","name":"II","uri":"spotify:album:al1",
                         "images":[{"url":"https://i.scdn.co/x","height":640,"width":640}]}}
    }"#;

    #[test]
    fn parses_a_playing_track() {
        let s: PlaybackState = serde_json::from_str(PLAYING_TRACK).unwrap();
        assert!(s.is_playing);
        assert_eq!(s.repeat_state, RepeatState::Context);
        assert_eq!(s.volume(), Some(62));
        assert_eq!(s.duration(), 252000);

        let item = s.item.unwrap();
        assert_eq!(item.name(), "Glass Roads");
        assert_eq!(item.byline(), "Lowtide");
        assert_eq!(item.collection(), Some("II"));
        assert_eq!(item.images().len(), 1);
    }

    #[test]
    fn joins_multiple_artists() {
        let raw = PLAYING_TRACK.replace(
            r#"[{"id":"a1","name":"Lowtide","uri":"spotify:artist:a1"}]"#,
            r#"[{"name":"Meadow"},{"name":"Second Artist"}]"#,
        );
        let s: PlaybackState = serde_json::from_str(&raw).unwrap();
        assert_eq!(s.item.unwrap().byline(), "Meadow, Second Artist");
    }

    #[test]
    fn parses_an_episode() {
        let raw = r#"{"is_playing":true,"progress_ms":10,
            "item":{"type":"episode","id":"e1","name":"Ep 12",
                    "uri":"spotify:episode:e1","duration_ms":3600000,
                    "show":{"id":"s1","name":"Open Line","uri":"spotify:show:s1"}}}"#;
        let s: PlaybackState = serde_json::from_str(raw).unwrap();
        let item = s.item.unwrap();
        assert_eq!(item.name(), "Ep 12");
        assert_eq!(item.byline(), "Open Line");
    }

    #[test]
    fn tolerates_an_advert() {
        let raw = r#"{"is_playing":true,"currently_playing_type":"ad",
                      "item":{"type":"ad","name":"whatever"}}"#;
        let s: PlaybackState = serde_json::from_str(raw).unwrap();
        assert!(matches!(s.item, Some(PlayingItem::Unknown)));
        assert_eq!(s.duration(), 0);
    }

    #[test]
    fn tolerates_a_null_item_and_missing_device() {
        let s: PlaybackState = serde_json::from_str(r#"{"is_playing":false}"#).unwrap();
        assert!(s.item.is_none());
        assert!(s.device.is_none());
        assert_eq!(s.repeat_state, RepeatState::Off);
        assert_eq!(s.fraction(), 0.0);
    }

    #[test]
    fn parses_a_local_file_with_no_id() {
        let raw = r#"{"is_playing":true,"item":{"type":"track","id":null,
            "name":"demo.mp3","uri":"spotify:local:::demo:120","duration_ms":120000,
            "is_local":true,"artists":[],"album":{"name":"","uri":"","images":[]}}}"#;
        let s: PlaybackState = serde_json::from_str(raw).unwrap();
        let item = s.item.unwrap();
        assert!(item.id().is_none());
        assert_eq!(item.byline(), "");
    }

    #[test]
    fn fraction_is_clamped_when_progress_overshoots() {
        let raw = r#"{"is_playing":true,"progress_ms":999999,
            "item":{"type":"track","name":"x","uri":"u","duration_ms":1000,
                    "artists":[],"album":{}}}"#;
        let s: PlaybackState = serde_json::from_str(raw).unwrap();
        assert_eq!(s.fraction(), 1.0);
    }

    #[test]
    fn playing_state_advances_with_the_clock() {
        let s = state_at(10_000, 200_000, true);
        assert_eq!(s.advanced_by(std::time::Duration::from_millis(2500)).progress(), 12_500);
    }

    #[test]
    fn paused_state_does_not_advance() {
        let s = state_at(10_000, 200_000, false);
        assert_eq!(s.advanced_by(std::time::Duration::from_secs(30)).progress(), 10_000);
    }

    #[test]
    fn advance_stops_at_the_end_of_the_track() {
        let s = state_at(190_000, 200_000, true);
        assert_eq!(s.advanced_by(std::time::Duration::from_secs(60)).progress(), 200_000);
    }

    #[test]
    fn advance_is_unbounded_for_streams_with_no_duration() {
        let s = state_at(1_000, 0, true);
        assert_eq!(s.advanced_by(std::time::Duration::from_secs(10)).progress(), 11_000);
    }

    fn state_at(progress_ms: u64, duration_ms: u64, playing: bool) -> PlaybackState {
        serde_json::from_str(&format!(
            r#"{{"is_playing":{playing},"progress_ms":{progress_ms},
                "item":{{"type":"track","name":"x","uri":"spotify:track:x",
                        "duration_ms":{duration_ms},"artists":[],"album":{{}}}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn repeat_parses_aliases_and_cycles() {
        use std::str::FromStr as _;
        assert_eq!(RepeatState::from_str("ALL").unwrap(), RepeatState::Context);
        assert_eq!(RepeatState::from_str("one").unwrap(), RepeatState::Track);
        assert!(RepeatState::from_str("sideways").is_err());

        let mut r = RepeatState::Off;
        r = r.next();
        assert_eq!(r, RepeatState::Context);
        r = r.next();
        assert_eq!(r, RepeatState::Track);
        assert_eq!(r.next(), RepeatState::Off);
    }

    #[test]
    fn devices_without_volume_support_parse() {
        let raw = r#"{"devices":[{"id":"d1","name":"Kitchen","type":"Speaker",
            "is_active":false,"volume_percent":null,"supports_volume":false}]}"#;
        let d: Devices = serde_json::from_str(raw).unwrap();
        assert_eq!(d.devices[0].volume_percent, None);
        assert!(!d.devices[0].supports_volume);
    }
}
