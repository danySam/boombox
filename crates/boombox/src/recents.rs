//! Keeping the list of recently played contexts on disk.
//!
//! The daemon is the only thing in a position to do this. It is the process
//! that sees playback continuously, it outlives any one TUI session, and it
//! already polls the state that carries the context URI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use boombox_core::error::Result;
use boombox_core::recent::{Recent, Recents};
use tokio::sync::RwLock;

/// Contexts not worth offering again: single tracks have no list to
/// resume, and the two collection pseudo-URIs are already permanent
/// entries in the sidebar.
fn worth_keeping(uri: &str) -> bool {
    matches!(uri.split(':').nth(1), Some("playlist" | "album" | "artist" | "show"))
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// The recents list plus its file.
#[derive(Clone)]
pub struct Store {
    inner: Arc<RwLock<Recents>>,
    path: Option<PathBuf>,
    /// How many times each context has failed to resolve, in memory only.
    ///
    /// A name is written once and never looked up again, so writing one
    /// after a single failure would make a dropped connection permanent.
    /// Counting first means only a context that is genuinely unnameable
    /// gets the fallback -- and a daemon restart tries again regardless.
    failures: Arc<RwLock<HashMap<String, u32>>>,
}

/// Attempts before accepting that nothing can name this context.
const GIVE_UP_AFTER: u32 = 3;

impl Store {
    /// Reads whatever is on disk. A missing or corrupt file is not an
    /// error worth failing a daemon start over -- it starts empty and
    /// fills up again by itself.
    pub fn load() -> Self {
        let path = boombox_core::config::state_dir().ok().map(|d| d.join("recents.json"));
        let inner = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| match serde_json::from_str(&text) {
                Ok(recents) => Some(recents),
                Err(e) => {
                    tracing::warn!("ignoring unreadable recents file: {e}");
                    None
                }
            })
            .unwrap_or_default();
        Self {
            inner: Arc::new(RwLock::new(inner)),
            path,
            failures: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub fn ephemeral() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Recents::default())),
            path: None,
            failures: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn list(&self) -> Vec<Recent> {
        self.inner.read().await.items.clone()
    }

    /// Notes a context as playing now, saving only if something changed --
    /// the poller calls this every second or two for hours on end.
    pub async fn touch(&self, uri: &str) {
        if !worth_keeping(uri) {
            return;
        }
        let changed = self.inner.write().await.touch(uri, now());
        if changed {
            self.save().await;
        }
    }

    /// Records a context and names it right away.
    ///
    /// The background namer would get to it within a couple of seconds,
    /// but this is the path where someone is watching: they paste a link
    /// and the row appears. Resolving it here means it appears with its
    /// real name rather than a raw id that corrects itself afterwards.
    pub async fn remember_now(&self, client: &boombox_core::api::Client, uri: &str) {
        self.touch(uri).await;
        if !worth_keeping(uri) {
            return;
        }
        self.resolve_one(client).await;
    }

    /// Fills an empty list from `/me/player/recently-played`, so a first
    /// run has something in it rather than nothing.
    ///
    /// Only when empty: once the daemon has been watching for itself, its
    /// own record is the better one, and this would cost an API call on
    /// every restart to reorder a list that was already right.
    pub async fn seed(&self, client: &boombox_core::api::Client) {
        if !self.inner.read().await.items.is_empty() {
            return;
        }
        let history = match client.recently_played().await {
            Ok(history) => history,
            Err(e) => {
                tracing::debug!("cannot seed recents: {e}");
                return;
            }
        };
        // The endpoint answers newest first, and `touch` pushes to the
        // front, so walk it backwards to end up in the same order.
        for entry in history.iter().rev() {
            if let Some(context) = entry.context.as_ref() {
                self.touch(&context.uri).await;
            }
        }
        tracing::info!("seeded {} recent contexts", self.inner.read().await.items.len());
    }

    /// Resolves one name, if any are outstanding. Called on a slow timer so
    /// a list seeded with forty contexts spreads its lookups out rather
    /// than firing forty requests at once.
    ///
    /// Returns whether there is more to do.
    pub async fn resolve_one(&self, client: &boombox_core::api::Client) -> bool {
        let Some(uri) = self.inner.read().await.unnamed() else {
            return false;
        };

        // Two sources, because neither covers everything. The Web API
        // knows a user's own playlists and 404s on Spotify's; oEmbed names
        // the radios and mixes the API hides. Ask the API first -- it is
        // authoritative for what it does know, and already authenticated.
        let described = match describe_via_api(client, &uri).await {
            Some(found) => Some(found),
            None => boombox_core::oembed::describe(&uri).await,
        };

        let (name, image) = match described {
            Some((name, image)) => {
                self.failures.write().await.remove(&uri);
                (Some(name), image)
            }
            None => {
                let attempts = {
                    let mut failures = self.failures.write().await;
                    let count = failures.entry(uri.clone()).or_insert(0);
                    *count += 1;
                    *count
                };
                if attempts < GIVE_UP_AFTER {
                    tracing::debug!("no name for {uri} yet (attempt {attempts})");
                    return true;
                }
                // Nothing can name it. Write the fallback, or this one URI
                // is retried forever and no other context is ever reached.
                (Some(fallback_name(&uri)), None)
            }
        };

        if self.inner.write().await.name(&uri, name, image) {
            self.save().await;
        }
        true
    }

    async fn save(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let snapshot = self.inner.read().await.clone();
        if let Err(e) = write(&path, &snapshot) {
            tracing::warn!("cannot save recents: {e}");
        }
    }
}

/// Written via a temporary file: a daemon killed mid-write would otherwise
/// leave truncated JSON that the next start has to throw away.
fn write(path: &PathBuf, recents: &Recents) -> Result<()> {
    let text = serde_json::to_string_pretty(recents)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// The Web API's answer, for the playlists and albums it will admit to.
async fn describe_via_api(
    client: &boombox_core::api::Client,
    uri: &str,
) -> Option<(String, Option<String>)> {
    use boombox_core::api::library::LibraryApi as _;

    let mut parts = uri.split(':');
    let (kind, id) = (parts.nth(1)?, parts.next()?);
    if kind != "playlist" {
        // Albums and artists are reachable through oEmbed just as well,
        // and going through the API for them would spend rate limit for
        // nothing.
        return None;
    }
    let playlist = client.playlist(id).await.ok()?;
    let image = playlist.images.first().map(|i| i.url.clone());
    Some((playlist.name, image))
}

fn fallback_name(uri: &str) -> String {
    Recent { uri: uri.to_string(), name: None, image: None, last_played: 0, plays: 0 }.label()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single track is not a list, so resuming it is what the queue is
    /// for -- keeping it here would push out the contexts that are useful.
    #[test]
    fn only_contexts_you_can_resume_are_kept() {
        assert!(worth_keeping("spotify:playlist:37i9dQZF1EExampleMix01"));
        assert!(worth_keeping("spotify:album:x"));
        assert!(!worth_keeping("spotify:track:x"));
        assert!(!worth_keeping("spotify:collection:tracks"));
        assert!(!worth_keeping("nonsense"));
    }

    #[tokio::test]
    async fn a_track_context_never_reaches_the_list() {
        let store = Store::ephemeral();
        store.touch("spotify:track:abc").await;
        assert!(store.list().await.is_empty());
    }

    #[tokio::test]
    async fn a_playlist_context_is_remembered() {
        let store = Store::ephemeral();
        store.touch("spotify:playlist:abc").await;
        assert_eq!(store.list().await.len(), 1);
    }

    /// Otherwise one unnameable context blocks every other lookup forever.
    #[test]
    fn an_unnameable_context_still_gets_a_name() {
        assert_eq!(fallback_name("spotify:playlist:37i9dQZF1E38"), "playlist 37i9dQZF");
    }
}
