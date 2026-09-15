use super::library_models::{
    Page, PlaylistItem, SavedAlbum, SavedShow, SavedTrack, SearchResults, SearchType,
    SimplePlaylist,
};
use super::{Client, Request};
use crate::error::Result;

/// Spotify caps search at ten results per type as of February 2026 and
/// rejects anything larger with a 400 rather than clamping.
pub const SEARCH_MAX_LIMIT: u32 = 10;
/// Everything else still allows fifty.
pub const PAGE_MAX_LIMIT: u32 = 50;

/// Browsing and search. Separate from [`super::PlayerApi`] because a
/// librespot-backed player would implement one and not the other.
pub trait LibraryApi: Send + Sync {
    fn search(
        &self,
        query: &str,
        types: &[SearchType],
        limit: u32,
        offset: u32,
    ) -> impl Future<Output = Result<SearchResults>> + Send;

    fn my_playlists(
        &self,
        limit: u32,
        offset: u32,
    ) -> impl Future<Output = Result<Page<SimplePlaylist>>> + Send;

    fn playlist(&self, id: &str) -> impl Future<Output = Result<SimplePlaylist>> + Send;

    fn playlist_items(
        &self,
        id: &str,
        limit: u32,
        offset: u32,
    ) -> impl Future<Output = Result<Page<PlaylistItem>>> + Send;

    fn saved_tracks(
        &self,
        limit: u32,
        offset: u32,
    ) -> impl Future<Output = Result<Page<SavedTrack>>> + Send;

    fn saved_albums(
        &self,
        limit: u32,
        offset: u32,
    ) -> impl Future<Output = Result<Page<SavedAlbum>>> + Send;

    fn saved_shows(
        &self,
        limit: u32,
        offset: u32,
    ) -> impl Future<Output = Result<Page<SavedShow>>> + Send;

    /// One bool per URI, in the order given.
    fn library_contains(&self, uris: &[String]) -> impl Future<Output = Result<Vec<bool>>> + Send;
    fn library_add(&self, uris: &[String]) -> impl Future<Output = Result<()>> + Send;
    fn library_remove(&self, uris: &[String]) -> impl Future<Output = Result<()>> + Send;
}

impl LibraryApi for Client {
    async fn search(
        &self,
        query: &str,
        types: &[SearchType],
        limit: u32,
        offset: u32,
    ) -> Result<SearchResults> {
        let types = types.iter().map(|t| t.as_api_str()).collect::<Vec<_>>().join(",");
        self.json(
            Request::get("/search")
                .query("q", query)
                .query("type", types)
                .query("limit", limit.clamp(1, SEARCH_MAX_LIMIT))
                .query("offset", offset),
        )
        .await
    }

    async fn my_playlists(&self, limit: u32, offset: u32) -> Result<Page<SimplePlaylist>> {
        self.json(
            Request::get("/me/playlists")
                .query("limit", limit.clamp(1, PAGE_MAX_LIMIT))
                .query("offset", offset),
        )
        .await
    }

    async fn playlist(&self, id: &str) -> Result<SimplePlaylist> {
        self.json(Request::get(format!("/playlists/{id}"))).await
    }

    async fn playlist_items(
        &self,
        id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Page<PlaylistItem>> {
        // `/tracks` became `/items` in February 2026.
        self.json(
            Request::get(format!("/playlists/{id}/items"))
                .query("limit", limit.clamp(1, PAGE_MAX_LIMIT))
                .query("offset", offset)
                .query("additional_types", "track,episode"),
        )
        .await
    }

    async fn saved_tracks(&self, limit: u32, offset: u32) -> Result<Page<SavedTrack>> {
        self.json(
            Request::get("/me/tracks")
                .query("limit", limit.clamp(1, PAGE_MAX_LIMIT))
                .query("offset", offset),
        )
        .await
    }

    async fn saved_albums(&self, limit: u32, offset: u32) -> Result<Page<SavedAlbum>> {
        self.json(
            Request::get("/me/albums")
                .query("limit", limit.clamp(1, PAGE_MAX_LIMIT))
                .query("offset", offset),
        )
        .await
    }

    async fn saved_shows(&self, limit: u32, offset: u32) -> Result<Page<SavedShow>> {
        self.json(
            Request::get("/me/shows")
                .query("limit", limit.clamp(1, PAGE_MAX_LIMIT))
                .query("offset", offset),
        )
        .await
    }

    async fn library_contains(&self, uris: &[String]) -> Result<Vec<bool>> {
        // The per-type /me/{tracks,albums}/contains endpoints were removed in
        // February 2026; this one takes URIs of any type at once.
        self.json(Request::get("/me/library/contains").query("uris", uris.join(","))).await
    }

    // `uris` goes in the query string, not a JSON body -- a body of
    // {"uris": [...]} is rejected with 400 "Missing required field: uris",
    // which the docs do not mention. Verified against the live API.
    async fn library_add(&self, uris: &[String]) -> Result<()> {
        self.empty(Request::put("/me/library").query("uris", uris.join(","))).await
    }

    async fn library_remove(&self, uris: &[String]) -> Result<()> {
        self.empty(
            Request::new(reqwest::Method::DELETE, "/me/library").query("uris", uris.join(",")),
        )
        .await
    }
}

/// The daemon's memory of what you have listened to.
///
/// Separate from [`LibraryApi`] because Spotify has no equivalent: nothing
/// in the Web API answers "which playlists have I been playing", and the
/// ones it hides hardest -- Daily Mix, Discover Weekly -- are exactly the
/// ones worth remembering. Only a long-lived process watching playback can
/// build this, so the daemon does, and going direct simply has no answer.
pub trait RecentsApi: Send + Sync {
    fn recents(&self) -> impl Future<Output = Result<Vec<crate::recent::Recent>>> + Send {
        async { Ok(Vec::new()) }
    }

    /// Notes a context the user reached for explicitly, whether or not the
    /// daemon has watched it play.
    fn remember(&self, _uri: &str) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

/// Going direct, there is nobody keeping the list -- the defaults are the
/// honest answer, not a stub.
impl RecentsApi for Client {}
