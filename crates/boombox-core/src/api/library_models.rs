use serde::{Deserialize, Serialize};

use super::models::Image;
use super::player::PlayOptions;
use super::player_models::{Album, Episode, PlayingItem, Track};

/// Spotify's offset-paged envelope. `next` is a full URL; we page by offset
/// instead, so it is kept only as a "there is more" flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub next: Option<String>,
}

impl<T> Page<T> {
    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self { items: Vec::new(), total: 0, limit: 0, offset: 0, next: None }
    }
}

/// `/me/tracks` still nests under `track`; only the playlist endpoints were
/// renamed to `item` in February 2026.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTrack {
    pub added_at: Option<String>,
    pub track: Track,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedAlbum {
    pub added_at: Option<String>,
    pub album: Album,
}

/// A row of `/playlists/{id}/items`. The nested field is `item`, not `track`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub added_at: Option<String>,
    #[serde(default)]
    pub is_local: bool,
    /// Null for items Spotify will not serve (removed or unavailable).
    pub item: Option<PlayingItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimplePlaylist {
    pub id: String,
    pub name: String,
    pub uri: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub collaborative: bool,
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default)]
    pub images: Vec<Image>,
    pub owner: Option<Owner>,
    /// Renamed from `tracks` in February 2026, and absent for playlists whose
    /// contents Spotify will not serve.
    #[serde(default, alias = "tracks")]
    pub items: Option<ItemsRef>,
}

impl SimplePlaylist {
    pub fn track_count(&self) -> Option<u32> {
        self.items.as_ref().map(|r| r.total)
    }

    pub fn owner_name(&self) -> &str {
        self.owner
            .as_ref()
            .and_then(|o| o.display_name.as_deref().or(Some(o.id.as_str())))
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemsRef {
    #[serde(default)]
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchType {
    Track,
    Album,
    Artist,
    Playlist,
    Show,
    Episode,
}

impl SearchType {
    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::Show => "show",
            Self::Episode => "episode",
        }
    }
}

impl std::str::FromStr for SearchType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "track" | "tracks" | "song" | "songs" => Ok(Self::Track),
            "album" | "albums" => Ok(Self::Album),
            "artist" | "artists" => Ok(Self::Artist),
            "playlist" | "playlists" => Ok(Self::Playlist),
            "show" | "shows" | "podcast" | "podcasts" => Ok(Self::Show),
            "episode" | "episodes" => Ok(Self::Episode),
            other => Err(format!("unknown search type `{other}`")),
        }
    }
}

impl std::fmt::Display for SearchType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_api_str())
    }
}

/// Spotify returns nulls inside search result arrays often enough that every
/// list here has to tolerate them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResults {
    #[serde(default)]
    pub tracks: Option<Page<Option<Track>>>,
    #[serde(default)]
    pub albums: Option<Page<Option<Album>>>,
    #[serde(default)]
    pub artists: Option<Page<Option<Artist>>>,
    #[serde(default)]
    pub playlists: Option<Page<Option<SimplePlaylist>>>,
    #[serde(default)]
    pub shows: Option<Page<Option<Show>>>,
    #[serde(default)]
    pub episodes: Option<Page<Option<Episode>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artist {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Show {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedShow {
    pub added_at: Option<String>,
    pub show: Show,
}

/// One row in any browsable list, flattened so the UI does not need a match
/// arm per endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub title: String,
    pub subtitle: String,
    /// Playable URI, if this row can be played at all.
    pub uri: Option<String>,
    pub duration_ms: Option<u64>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Track,
    Episode,
    Album,
    Artist,
    Playlist,
    Show,
    /// A context the daemon watched play, offered again.
    ///
    /// Distinct from [`Self::Playlist`] because these cannot be opened:
    /// the ones worth having here are Spotify's own, and fetching those
    /// answers 404. They are played, not browsed into.
    Recent,
}

impl EntryKind {
    /// Albums, artists, playlists and shows are played as a context; tracks
    /// and episodes are played as an explicit URI list.
    pub fn is_context(self) -> bool {
        matches!(self, Self::Album | Self::Artist | Self::Playlist | Self::Show | Self::Recent)
    }

    /// Plural, for a heading over a run of them.
    pub fn plural(self) -> &'static str {
        match self {
            Self::Track => "Tracks",
            Self::Episode => "Episodes",
            Self::Album => "Albums",
            Self::Artist => "Artists",
            Self::Playlist => "Playlists",
            Self::Show => "Shows",
            Self::Recent => "Recently played",
        }
    }
}

impl Entry {
    /// A remembered context, shown as something to start again.
    pub fn from_recent(recent: &crate::recent::Recent) -> Self {
        Self {
            title: recent.label(),
            subtitle: recent.describe(),
            uri: Some(recent.uri.clone()),
            duration_ms: None,
            kind: EntryKind::Recent,
        }
    }

    pub fn from_track(track: &Track) -> Self {
        Self {
            title: track.name.clone(),
            subtitle: track.artist_names(),
            uri: Some(track.uri.clone()),
            duration_ms: Some(track.duration_ms),
            kind: EntryKind::Track,
        }
    }

    pub fn from_playing_item(item: &PlayingItem) -> Option<Self> {
        Some(Self {
            title: item.name().to_string(),
            subtitle: item.byline(),
            uri: item.uri().map(str::to_owned),
            duration_ms: Some(item.duration_ms()),
            kind: match item {
                PlayingItem::Track(_) => EntryKind::Track,
                PlayingItem::Episode(_) => EntryKind::Episode,
                PlayingItem::Unknown => return None,
            },
        })
    }

    pub fn from_album(album: &Album) -> Self {
        Self {
            title: album.name.clone(),
            subtitle: album.artist_names(),
            uri: Some(album.uri.clone()),
            duration_ms: None,
            kind: EntryKind::Album,
        }
    }

    /// The subtitle is all but guaranteed to be empty, and there is no
    /// fixing it from here.
    ///
    /// Search returns a stripped artist object -- id, name, uri, images
    /// and nothing else. Fetching the full one does not help: under
    /// Development Mode `GET /artists/{id}` answers 200 with the same
    /// fields and no genres, followers or popularity, and the batch
    /// `GET /artists?ids=` is refused outright with 403. All checked
    /// against a live account.
    ///
    /// The genres are still read, because the field costs nothing and the
    /// restriction may not be permanent. Meanwhile a row with no subtitle
    /// is given the full width by the list, rather than being held to a
    /// column that will stay blank.
    pub fn from_artist(artist: &Artist) -> Self {
        Self {
            title: artist.name.clone(),
            subtitle: artist.genres.join(", "),
            uri: Some(artist.uri.clone()),
            duration_ms: None,
            kind: EntryKind::Artist,
        }
    }

    pub fn from_playlist(playlist: &SimplePlaylist) -> Self {
        let count = playlist
            .track_count()
            .map(|n| format!("{n} tracks"))
            .unwrap_or_else(|| "contents unavailable".into());
        Self {
            title: playlist.name.clone(),
            subtitle: format!("{}  \u{b7}  {count}", playlist.owner_name()),
            uri: Some(playlist.uri.clone()),
            duration_ms: None,
            kind: EntryKind::Playlist,
        }
    }

    /// How to start this row: a context for albums, artists, playlists and
    /// shows; an explicit URI for tracks and episodes.
    pub fn play_options(&self) -> Option<PlayOptions> {
        let uri = self.uri.as_ref()?;
        Some(if self.kind.is_context() {
            PlayOptions::context(uri.clone())
        } else {
            PlayOptions::tracks(vec![uri.clone()])
        })
    }

    pub fn from_show(show: &Show) -> Self {
        Self {
            title: show.name.clone(),
            subtitle: show.publisher.clone().unwrap_or_default(),
            uri: Some(show.uri.clone()),
            duration_ms: None,
            kind: EntryKind::Show,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_items_use_the_renamed_item_field() {
        // Shape confirmed against the live API in August 2026.
        let raw = r#"{"href":"x","limit":2,"offset":0,"total":4,"next":null,
            "items":[{"added_at":"2026-04-04T16:40:24Z","is_local":false,
                      "added_by":{"id":"u"},"primary_color":null,
                      "item":{"type":"track","id":"t","name":"Open Road",
                              "uri":"spotify:track:t","duration_ms":1000,
                              "artists":[{"name":"A"}],"album":{"name":"B"}}}]}"#;
        let page: Page<PlaylistItem> = serde_json::from_str(raw).unwrap();
        assert_eq!(page.total, 4);
        let entry = Entry::from_playing_item(page.items[0].item.as_ref().unwrap()).unwrap();
        assert_eq!(entry.title, "Open Road");
        assert_eq!(entry.kind, EntryKind::Track);
    }

    #[test]
    fn a_null_playlist_row_does_not_break_the_page() {
        let raw = r#"{"items":[{"added_at":null,"item":null}],"total":1}"#;
        let page: Page<PlaylistItem> = serde_json::from_str(raw).unwrap();
        assert!(page.items[0].item.is_none());
    }

    #[test]
    fn saved_tracks_still_nest_under_track() {
        let raw = r#"{"total":898,"limit":1,"offset":0,
            "next":"https://api.spotify.com/v1/me/tracks?offset=1&limit=1",
            "items":[{"added_at":"2026-08-26T10:07:17Z",
                      "track":{"type":"track","id":"t","name":"First Light",
                               "uri":"spotify:track:t","duration_ms":152988,
                               "artists":[{"name":"Noëlle"}],
                               "album":{"name":"Horizons"}}}]}"#;
        let page: Page<SavedTrack> = serde_json::from_str(raw).unwrap();
        assert_eq!(page.total, 898);
        assert!(page.has_more());
        assert_eq!(Entry::from_track(&page.items[0].track).subtitle, "No\u{eb}lle");
    }

    #[test]
    fn playlist_accepts_both_items_and_the_old_tracks_alias() {
        let new: SimplePlaylist = serde_json::from_str(
            r#"{"id":"p","name":"Mixtape","uri":"spotify:playlist:p",
                "owner":{"id":"u","display_name":"Alex"},"items":{"total":4}}"#,
        )
        .unwrap();
        assert_eq!(new.track_count(), Some(4));
        assert_eq!(new.owner_name(), "Alex");

        let old: SimplePlaylist =
            serde_json::from_str(r#"{"id":"p","name":"x","uri":"u","tracks":{"total":9}}"#)
                .unwrap();
        assert_eq!(old.track_count(), Some(9));
    }

    #[test]
    fn a_playlist_with_no_readable_contents_says_so() {
        let p: SimplePlaylist =
            serde_json::from_str(r#"{"id":"p","name":"Editorial","uri":"u"}"#).unwrap();
        assert_eq!(p.track_count(), None);
        assert!(Entry::from_playlist(&p).subtitle.contains("unavailable"));
    }

    #[test]
    fn search_results_tolerate_null_entries() {
        let raw = r#"{"tracks":{"items":[null,{"type":"track","name":"K","uri":"u",
                      "duration_ms":1,"artists":[],"album":{}}],
                      "total":18,"limit":10,"offset":0,"next":"..."}}"#;
        let r: SearchResults = serde_json::from_str(raw).unwrap();
        let tracks = r.tracks.unwrap();
        assert_eq!(tracks.total, 18);
        assert!(tracks.has_more());
        assert!(tracks.items[0].is_none());
        assert_eq!(tracks.items[1].as_ref().unwrap().name, "K");
    }

    #[test]
    fn missing_result_sections_are_none_not_an_error() {
        let r: SearchResults =
            serde_json::from_str(r#"{"artists":{"items":[],"total":0}}"#).unwrap();
        assert!(r.tracks.is_none());
        assert!(r.artists.is_some());
    }

    #[test]
    fn search_types_parse_forgivingly() {
        use std::str::FromStr as _;
        assert_eq!(SearchType::from_str("Tracks").unwrap(), SearchType::Track);
        assert_eq!(SearchType::from_str("podcast").unwrap(), SearchType::Show);
        assert!(SearchType::from_str("vibes").is_err());
    }

    #[test]
    fn context_kinds_are_distinguished_from_playable_uris() {
        assert!(EntryKind::Album.is_context());
        assert!(EntryKind::Playlist.is_context());
        assert!(!EntryKind::Track.is_context());
        assert!(!EntryKind::Episode.is_context());
    }

    #[test]
    fn play_options_match_the_entry_kind() {
        let album = Entry {
            title: "II".into(),
            subtitle: "Lowtide".into(),
            uri: Some("spotify:album:a".into()),
            duration_ms: None,
            kind: EntryKind::Album,
        };
        let opts = album.play_options().unwrap();
        assert_eq!(opts.context_uri.as_deref(), Some("spotify:album:a"));
        assert!(opts.uris.is_empty());

        let track = Entry {
            title: "x".into(),
            subtitle: String::new(),
            uri: Some("spotify:track:t".into()),
            duration_ms: Some(1),
            kind: EntryKind::Track,
        };
        let opts = track.play_options().unwrap();
        assert!(opts.context_uri.is_none());
        assert_eq!(opts.uris, vec!["spotify:track:t"]);
    }

    #[test]
    fn an_entry_with_no_uri_cannot_be_played() {
        let e = Entry {
            title: "x".into(),
            subtitle: String::new(),
            uri: None,
            duration_ms: None,
            kind: EntryKind::Track,
        };
        assert!(e.play_options().is_none());
    }
}
