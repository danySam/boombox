use std::time::Duration;

use boombox_core::api::library_models::{
    Page, PlaylistItem, SavedAlbum, SavedTrack, SearchResults, SearchType, SimplePlaylist,
};
use boombox_core::api::player::PlayOptions;
use boombox_core::api::{Device, PlaybackState, Queue, RepeatState};
use boombox_core::error::Error;
use boombox_core::recent::Recent;
use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes incompatibly. A daemon left running
/// across an upgrade would otherwise answer in a dialect the client cannot
/// parse, which is a confusing way to fail.
pub const PROTOCOL_VERSION: u32 = 6;

/// How long to wait on a request the daemon answers from memory.
///
/// None of these touch the network, so a reply slower than this is not a
/// busy daemon: it is one whose accept loop is not running at all --
/// stopped, deadlocked, or with every worker thread blocked. The kernel
/// still completes the connection on its behalf, which is why only the
/// reply can tell.
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait on a request the daemon forwards to Spotify.
///
/// Generous on purpose. It sits above the daemon's own HTTP timeouts --
/// 20s for the Web API, 30s for a token refresh that can precede it, and the
/// 6s oEmbed lookup a `Remember` adds -- so a slow request is never cut off
/// early. It exists so a wedged daemon costs a wait instead of forever.
pub const FORWARDED_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    PlaybackState,
    Devices,
    Play(PlayOptions),
    Pause,
    Next,
    Previous,
    Seek {
        position_ms: u64,
    },
    SetVolume {
        percent: u32,
    },
    SetShuffle {
        on: bool,
    },
    SetRepeat {
        state: RepeatState,
    },
    Transfer {
        device_id: String,
        play: bool,
    },
    Queue,
    /// The contexts the daemon has seen play. Answered from the daemon's
    /// own store; Spotify has no endpoint for this.
    Recents,
    /// Records a context the client knows about but the daemon has not
    /// seen play yet -- a pasted share link, most usefully, since that is
    /// the only way a Spotify-owned playlist enters the app at all.
    Remember {
        uri: String,
    },
    AddToQueue {
        uri: String,
    },

    Search {
        query: String,
        types: Vec<SearchType>,
        limit: u32,
        offset: u32,
    },
    MyPlaylists {
        limit: u32,
        offset: u32,
    },
    PlaylistItems {
        id: String,
        limit: u32,
        offset: u32,
    },
    SavedTracks {
        limit: u32,
        offset: u32,
    },
    SavedAlbums {
        limit: u32,
        offset: u32,
    },
    LibraryContains {
        uris: Vec<String>,
    },
    LibraryAdd {
        uris: Vec<String>,
    },
    LibraryRemove {
        uris: Vec<String>,
    },

    /// Live audio spectrum, when this daemon is streaming. Empty otherwise.
    Spectrum {
        bands: u16,
    },

    /// Recent waveform, resampled to `points` values in -1.0..=1.0.
    Waveform {
        points: u16,
    },

    /// Peak amplitude across the current track, as far as it has been played.
    Envelope {
        points: u16,
    },

    /// Liveness probe, also used to detect a stale socket file.
    Ping,
    /// Ask the daemon to shut down cleanly.
    Shutdown,
}

impl Request {
    /// The longest a healthy daemon could take to answer this.
    pub fn timeout(&self) -> Duration {
        match self {
            Self::Ping
            | Self::Shutdown
            | Self::PlaybackState
            | Self::Recents
            | Self::Spectrum { .. }
            | Self::Waveform { .. }
            | Self::Envelope { .. } => LOCAL_TIMEOUT,
            // Everything else reaches Spotify -- `Remember` included, since
            // it names the context before replying. New requests land here
            // by default, which is the safe side to err on: a local request
            // given too long only detects a wedge more slowly, while a
            // network request given too little would fail when healthy.
            _ => FORWARDED_TIMEOUT,
        }
    }
}

/// Adjacently tagged: variants wrap sequences and options, which the
/// internally tagged representation cannot encode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", content = "data", rename_all = "snake_case")]
pub enum Response {
    PlaybackState(Option<PlaybackState>),
    Devices(Vec<Device>),
    Queue(Queue),
    Search(Box<SearchResults>),
    Playlists(Page<SimplePlaylist>),
    Recents(Vec<Recent>),
    PlaylistItems(Page<PlaylistItem>),
    SavedTracks(Page<SavedTrack>),
    SavedAlbums(Page<SavedAlbum>),
    Contains(Vec<bool>),
    Spectrum(Vec<f32>),
    Waveform(Vec<f32>),
    Envelope(Vec<f32>),
    Unit,
    Pong(DaemonStatus),
    Error(WireError),
}

impl Response {
    pub fn name(&self) -> &'static str {
        match self {
            Self::PlaybackState(_) => "playback_state",
            Self::Devices(_) => "devices",
            Self::Queue(_) => "queue",
            Self::Search(_) => "search",
            Self::Playlists(_) => "playlists",
            Self::Recents(_) => "recents",
            Self::PlaylistItems(_) => "playlist_items",
            Self::SavedTracks(_) => "saved_tracks",
            Self::SavedAlbums(_) => "saved_albums",
            Self::Contains(_) => "contains",
            Self::Spectrum(_) => "spectrum",
            Self::Waveform(_) => "waveform",
            Self::Envelope(_) => "envelope",
            Self::Unit => "unit",
            Self::Pong(_) => "pong",
            Self::Error(_) => "error",
        }
    }
}

/// Whether the daemon has a Connect device, and if not, why not.
///
/// A bare flag could not tell "switched off" from "signed in and broken", so
/// `daemon --status` could only report the one thing the user could already
/// see: that no device had appeared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingState {
    /// Registered with Spotify and ready to be picked.
    Live,
    /// Turned off in the config.
    Disabled,
    /// On, but the separate streaming sign-in has not been done here.
    NotSignedIn,
    /// On and signed in, but no device: no audio output, or Spotify would
    /// not have the session. Carries the reason.
    Unavailable(String),
    /// This build has no streaming support compiled in.
    NotCompiled,
}

/// The one message that must stay readable across every version, in both
/// directions: it is how a version mismatch gets reported, so it cannot be
/// allowed to fail to parse on a mismatch. Only ever add fields, and only
/// with `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub protocol_version: u32,
    /// Build string of the running daemon, e.g. `0.1.0 (df71b69) 2026-08-31`.
    /// Defaulted because a daemon predating this field must still answer a
    /// ping in a way a newer client can read.
    #[serde(default)]
    pub version: String,
    /// Whether a Connect device is registered right now.
    ///
    /// Separate from the daemon being up, because the two really do come
    /// apart: a session dropped by the server leaves the daemon answering
    /// every request while the device has gone. Defaulted so an older
    /// daemon still answers a ping.
    #[serde(default)]
    pub streaming: bool,
    /// Seconds without audio while Spotify reports playback on this device,
    /// when that is happening right now. Nothing otherwise -- including from
    /// a daemon too old to watch for it, which is why it is defaulted.
    #[serde(default)]
    pub audio_stalled_secs: Option<u64>,
    /// Why there is or is not a device. Defaulted, so a daemon predating it
    /// still answers a ping; `streaming` alone is the fallback.
    #[serde(default)]
    pub streaming_state: Option<StreamingState>,
    /// The Connect device the daemon registered, when it has one.
    ///
    /// How a front end tells which row in the device list is this machine.
    /// The name cannot answer that: any device can be renamed to anything,
    /// including the name of another.
    #[serde(default)]
    pub device_id: Option<String>,
    pub pid: u32,
    pub uptime_secs: u64,
    pub requests_served: u64,
    pub api_calls: u64,
    /// Seconds since the cached playback state was last refreshed, and
    /// `u64::MAX` when nothing has been fetched yet. Read it through
    /// [`Self::cache_age_summary`] rather than printing it raw.
    pub cache_age_secs: u64,
    pub polling_secs: u64,
}

impl DaemonStatus {
    /// Whether this daemon speaks the dialect the calling binary was built
    /// for. Front ends stay usable when it does not -- most requests predate
    /// any given change -- so this warns rather than refusing to connect.
    pub fn speaks_our_protocol(&self) -> bool {
        self.protocol_version == PROTOCOL_VERSION
    }

    /// The cache age, or what to say when there is nothing cached yet.
    pub fn cache_age_summary(&self) -> String {
        if self.cache_age_secs == u64::MAX {
            "never fetched".into()
        } else {
            format!("{}s old", self.cache_age_secs)
        }
    }

    /// One line for `daemon --status`, naming the fix where there is one.
    pub fn streaming_summary(&self) -> String {
        match &self.streaming_state {
            Some(StreamingState::Live) => "connected".into(),
            Some(StreamingState::Disabled) => {
                "off -- set `[streaming] enabled = true` in the config".into()
            }
            Some(StreamingState::NotSignedIn) => {
                "on, but not signed in -- run `boombox auth login --streaming`".into()
            }
            Some(StreamingState::Unavailable(why)) => format!("on, but no device: {why}"),
            Some(StreamingState::NotCompiled) => {
                "not in this build -- rebuild with --features streaming".into()
            }
            // A daemon too old to say; the flag is all there is.
            None if self.streaming => "connected".into(),
            None => "no Connect device".into(),
        }
    }

    /// What to tell the user, or `None` when the daemon matches. Names the
    /// fix, because restarting the daemon is never obvious from the symptom:
    /// a stale daemon fails one unlucky request, not startup.
    pub fn mismatch_warning(&self) -> Option<String> {
        if self.speaks_our_protocol() {
            return None;
        }
        let daemon = if self.version.is_empty() {
            "an older build".to_string()
        } else {
            format!("build {}", self.version)
        };
        Some(format!(
            "daemon speaks protocol v{} but this is v{PROTOCOL_VERSION} ({daemon});              some commands will fail until you run `boombox daemon --stop` and start it again",
            self.protocol_version,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorKind {
    NotAuthenticated,
    NoActiveDevice,
    PremiumRequired,
    Api,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub kind: WireErrorKind,
    pub message: String,
    #[serde(default)]
    pub status: u16,
}

/// Errors have to survive the socket without losing the distinctions the
/// documented exit codes depend on.
impl From<&Error> for WireError {
    fn from(err: &Error) -> Self {
        // An Api error carries the bare message, not its Display form.
        // `Error::Api` prefixes "spotify api error {status}:" when shown,
        // so sending the formatted string would have the client rebuild an
        // Api error around text that already had the prefix, and print it
        // twice.
        let (kind, status, message) = match err {
            Error::NotAuthenticated | Error::NoClientId => {
                (WireErrorKind::NotAuthenticated, 0, err.to_string())
            }
            Error::NoActiveDevice => (WireErrorKind::NoActiveDevice, 0, err.to_string()),
            Error::PremiumRequired => (WireErrorKind::PremiumRequired, 0, err.to_string()),
            Error::Api { status, message } => (WireErrorKind::Api, *status, message.clone()),
            _ => (WireErrorKind::Other, 0, err.to_string()),
        };
        Self { kind, message, status }
    }
}

impl From<WireError> for Error {
    fn from(wire: WireError) -> Self {
        match wire.kind {
            WireErrorKind::NotAuthenticated => Error::NotAuthenticated,
            WireErrorKind::NoActiveDevice => Error::NoActiveDevice,
            WireErrorKind::PremiumRequired => Error::PremiumRequired,
            WireErrorKind::Api => Error::Api { status: wire.status, message: wire.message },
            WireErrorKind::Other => Error::Api { status: 0, message: wire.message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(req: &Request) -> Request {
        serde_json::from_str(&serde_json::to_string(req).unwrap()).unwrap()
    }

    #[test]
    fn requests_survive_the_wire() {
        assert!(matches!(roundtrip(&Request::Next), Request::Next));
        assert!(matches!(
            roundtrip(&Request::Seek { position_ms: 1500 }),
            Request::Seek { position_ms: 1500 }
        ));
        assert!(matches!(
            roundtrip(&Request::SetRepeat { state: RepeatState::Track }),
            Request::SetRepeat { state: RepeatState::Track }
        ));
    }

    #[test]
    fn play_options_survive_the_wire() {
        let req = Request::Play(PlayOptions::context("spotify:album:x"));
        match roundtrip(&req) {
            Request::Play(o) => assert_eq!(o.context_uri.as_deref(), Some("spotify:album:x")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn responses_survive_every_variant_shape() {
        for response in [
            Response::Unit,
            Response::Devices(vec![]),
            Response::PlaybackState(None),
            Response::Error(WireError {
                kind: WireErrorKind::NoActiveDevice,
                message: "no active device".into(),
                status: 0,
            }),
        ] {
            let encoded = serde_json::to_string(&response).unwrap();
            assert!(!encoded.contains('\n'), "{encoded}");
            let decoded: Response = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.name(), response.name(), "{encoded}");
        }
    }

    #[test]
    fn library_requests_survive_the_wire() {
        let req = Request::Search {
            query: "meadow".into(),
            types: vec![SearchType::Track, SearchType::Album],
            limit: 10,
            offset: 0,
        };
        match roundtrip(&req) {
            Request::Search { query, types, limit, .. } => {
                assert_eq!(query, "meadow");
                assert_eq!(types, vec![SearchType::Track, SearchType::Album]);
                assert_eq!(limit, 10);
            }
            other => panic!("got {other:?}"),
        }

        match roundtrip(&Request::LibraryAdd { uris: vec!["spotify:track:t".into()] }) {
            Request::LibraryAdd { uris } => assert_eq!(uris, vec!["spotify:track:t"]),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn library_responses_survive_the_wire() {
        for response in [
            Response::Contains(vec![true, false]),
            Response::Playlists(Page::default()),
            Response::SavedTracks(Page::default()),
            Response::Search(Box::default()),
            Response::Spectrum(vec![0.0, 0.5, 1.0]),
            Response::Waveform(vec![-1.0, 0.0, 1.0]),
        ] {
            let encoded = serde_json::to_string(&response).unwrap();
            assert!(!encoded.contains('\n'), "{encoded}");
            let decoded: Response = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.name(), response.name());
        }
    }

    fn status(protocol_version: u32) -> DaemonStatus {
        DaemonStatus {
            protocol_version,
            version: "0.1.0 (abc123def)".into(),
            streaming: true,
            streaming_state: Some(StreamingState::Live),
            device_id: None,
            audio_stalled_secs: None,
            pid: 1,
            uptime_secs: 0,
            requests_served: 0,
            api_calls: 0,
            cache_age_secs: 0,
            polling_secs: 1,
        }
    }

    /// Every reason for having no device names its own fix, because "no
    /// Connect device" on its own is what sent people looking in the wrong
    /// place.
    #[test]
    fn the_status_line_says_why_there_is_no_device() {
        let with = |state| DaemonStatus { streaming_state: state, ..status(PROTOCOL_VERSION) };

        assert_eq!(with(Some(StreamingState::Live)).streaming_summary(), "connected");
        assert!(
            with(Some(StreamingState::Disabled)).streaming_summary().contains("enabled = true")
        );
        assert!(
            with(Some(StreamingState::NotSignedIn))
                .streaming_summary()
                .contains("auth login --streaming")
        );
        assert!(
            with(Some(StreamingState::NotCompiled))
                .streaming_summary()
                .contains("--features streaming")
        );
        let broken = with(Some(StreamingState::Unavailable("no audio device".into())));
        assert!(broken.streaming_summary().contains("no audio device"));
    }

    /// A daemon that has answered before it first polled printed its
    /// sentinel: "cache 18446744073709551615s old".
    #[test]
    fn a_cache_that_has_never_been_filled_says_so() {
        let fresh = DaemonStatus { cache_age_secs: u64::MAX, ..status(PROTOCOL_VERSION) };
        assert_eq!(fresh.cache_age_summary(), "never fetched");
        let warm = DaemonStatus { cache_age_secs: 9, ..status(PROTOCOL_VERSION) };
        assert_eq!(warm.cache_age_summary(), "9s old");
    }

    /// A daemon too old to carry the reason still produces a sensible line.
    #[test]
    fn an_older_daemon_falls_back_to_the_plain_flag() {
        let old = DaemonStatus { streaming_state: None, ..status(PROTOCOL_VERSION) };
        assert_eq!(old.streaming_summary(), "connected");
        let off = DaemonStatus { streaming: false, ..old };
        assert_eq!(off.streaming_summary(), "no Connect device");
    }

    /// Pings decide whether a daemon is trusted at all, so they must fail
    /// fast; anything Spotify answers must be allowed its full time.
    #[test]
    fn requests_answered_from_memory_time_out_quickly() {
        assert_eq!(Request::Ping.timeout(), LOCAL_TIMEOUT);
        assert_eq!(Request::PlaybackState.timeout(), LOCAL_TIMEOUT);
        assert_eq!(Request::Envelope { points: 8 }.timeout(), LOCAL_TIMEOUT);
        assert_eq!(Request::Pause.timeout(), FORWARDED_TIMEOUT);
    }

    /// The one that looks local and is not: it resolves a name over the
    /// network before replying.
    #[test]
    fn remembering_a_context_is_given_network_time() {
        let request = Request::Remember { uri: "spotify:playlist:x".into() };
        assert_eq!(request.timeout(), FORWARDED_TIMEOUT);
    }

    #[test]
    fn a_matching_daemon_raises_nothing() {
        assert!(status(PROTOCOL_VERSION).speaks_our_protocol());
        assert!(status(PROTOCOL_VERSION).mismatch_warning().is_none());
    }

    #[test]
    fn a_stale_daemon_is_named_along_with_the_fix() {
        let warning = status(PROTOCOL_VERSION - 1).mismatch_warning().unwrap();
        assert!(warning.contains(&format!("v{}", PROTOCOL_VERSION - 1)), "{warning}");
        assert!(warning.contains("0.1.0 (abc123def)"), "{warning}");
        assert!(warning.contains("boombox daemon --stop"), "{warning}");
    }

    /// The mismatch is reported by reading a ping reply, so a daemon too old
    /// to know about the `version` field must still produce a readable one.
    /// If this breaks, every future mismatch fails as a parse error instead.
    #[test]
    fn a_ping_reply_predating_the_version_field_still_parses() {
        let old = r#"{"reply":"pong","data":{"protocol_version":3,"pid":42,
            "uptime_secs":9,"requests_served":1,"api_calls":2,
            "cache_age_secs":0,"polling_secs":5}}"#;
        match serde_json::from_str::<Response>(old).unwrap() {
            Response::Pong(s) => {
                assert_eq!(s.protocol_version, 3);
                assert_eq!(s.pid, 42);
                assert!(s.version.is_empty());
                let warning = s.mismatch_warning().unwrap();
                assert!(warning.contains("an older build"), "{warning}");
            }
            other => panic!("got {}", other.name()),
        }
    }

    /// A daemon that predates the field is not claiming to be streaming;
    /// it simply cannot say. False is the safer reading.
    #[test]
    fn an_older_daemon_reports_no_streaming_rather_than_failing() {
        let old = r#"{"reply":"pong","data":{"protocol_version":5,"pid":42,
            "uptime_secs":9,"requests_served":1,"api_calls":2,
            "cache_age_secs":0,"polling_secs":5}}"#;
        match serde_json::from_str::<Response>(old).unwrap() {
            Response::Pong(s) => assert!(!s.streaming),
            other => panic!("got {}", other.name()),
        }
    }

    /// A daemon too old to watch for silence has nothing to report, which
    /// must read as nothing wrong rather than as a parse failure.
    #[test]
    fn an_older_daemon_reports_no_silence() {
        let old = r#"{"reply":"pong","data":{"protocol_version":6,"pid":42,
            "uptime_secs":9,"requests_served":1,"api_calls":2,
            "cache_age_secs":0,"polling_secs":5,"streaming":true}}"#;
        match serde_json::from_str::<Response>(old).unwrap() {
            Response::Pong(s) => assert_eq!(s.audio_stalled_secs, None),
            other => panic!("got {}", other.name()),
        }
    }

    /// The other direction: a daemon newer than the client. Unknown fields
    /// must be ignored rather than rejected, or the warning never arrives.
    #[test]
    fn a_ping_reply_from_a_newer_daemon_still_parses() {
        let future = r#"{"reply":"pong","data":{"protocol_version":99,"version":"9.9.9",
            "pid":42,"uptime_secs":9,"requests_served":1,"api_calls":2,
            "cache_age_secs":0,"polling_secs":5,"something_new":[1,2,3]}}"#;
        match serde_json::from_str::<Response>(future).unwrap() {
            Response::Pong(s) => {
                assert!(!s.speaks_our_protocol());
                assert!(s.mismatch_warning().unwrap().contains("9.9.9"));
            }
            other => panic!("got {}", other.name()),
        }
    }

    #[test]
    fn requests_are_one_json_line() {
        let encoded = serde_json::to_string(&Request::Pause).unwrap();
        assert!(!encoded.contains('\n'));
        assert_eq!(encoded, r#"{"cmd":"pause"}"#);
    }

    #[test]
    fn error_kinds_round_trip_so_exit_codes_survive() {
        for original in [
            Error::NotAuthenticated,
            Error::NoActiveDevice,
            Error::PremiumRequired,
            Error::Api { status: 429, message: "rate limited".into() },
        ] {
            let expected = original.exit_code();
            let wire = WireError::from(&original);
            let encoded = serde_json::to_string(&wire).unwrap();
            let decoded: WireError = serde_json::from_str(&encoded).unwrap();
            let restored = Error::from(decoded);
            assert_eq!(restored.exit_code(), expected, "for {original}");
        }
    }

    /// The daemon adds no prefix the client will add again. Seen live as
    /// "spotify api error 403: spotify api error 403: Restriction violated".
    #[test]
    fn an_api_error_is_not_wrapped_twice_on_the_way_through() {
        let original = Error::Api { status: 403, message: "Player command failed".into() };
        let restored = Error::from(WireError::from(&original));
        let shown = restored.to_string();
        assert_eq!(shown.matches("spotify api error").count(), 1, "{shown}");
        assert_eq!(shown, original.to_string());
    }

    #[test]
    fn api_status_survives_so_rate_limits_stay_legible() {
        let wire = WireError::from(&Error::Api { status: 429, message: "rate limited".into() });
        let restored = Error::from(wire);
        assert!(restored.to_string().contains("429"), "{restored}");
    }

    #[test]
    fn a_playback_state_response_round_trips() {
        let state: PlaybackState = serde_json::from_str(
            r#"{"is_playing":true,"progress_ms":1000,"shuffle_state":false,
                "item":{"type":"track","name":"x","uri":"spotify:track:x",
                        "duration_ms":2000,"artists":[{"name":"a"}],"album":{"name":"b"}}}"#,
        )
        .unwrap();

        let encoded = serde_json::to_string(&Response::PlaybackState(Some(state))).unwrap();
        let decoded: Response = serde_json::from_str(&encoded).unwrap();
        match decoded {
            Response::PlaybackState(Some(s)) => {
                assert_eq!(s.progress(), 1000);
                assert_eq!(s.item.unwrap().name(), "x");
            }
            other => panic!("got {}", other.name()),
        }
    }

    #[test]
    fn an_advert_survives_the_wire_as_unknown() {
        let state: PlaybackState =
            serde_json::from_str(r#"{"is_playing":true,"item":{"type":"ad","name":"x"}}"#).unwrap();
        let encoded = serde_json::to_string(&Response::PlaybackState(Some(state))).unwrap();
        let decoded: Response = serde_json::from_str(&encoded).unwrap();
        match decoded {
            Response::PlaybackState(Some(s)) => assert_eq!(s.duration(), 0),
            other => panic!("got {}", other.name()),
        }
    }
}
