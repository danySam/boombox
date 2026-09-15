//! Naming a playlist Spotify will not let us fetch.
//!
//! `GET /playlists/{id}` answers 404 for anything Spotify made -- Daily
//! Mixes, Discover Weekly, the editorial lists -- so the Web API cannot
//! name them. `open.spotify.com/oembed` can, needs no authentication, and
//! is a public endpoint published for embedding rather than anything
//! reverse-engineered.
//!
//! The two sources cover different things and neither covers everything: a
//! user's "Liked Songs" mirror resolves through the Web API and 404s here,
//! while the radios and mixes do the opposite. Ask the API first, fall
//! back to this.

use std::time::Duration;

use serde::Deserialize;

const TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Deserialize)]
struct OEmbed {
    title: String,
    #[serde(default)]
    thumbnail_url: Option<String>,
}

/// The display name and cover for a Spotify URI, or `None` if even this
/// cannot see it.
pub async fn describe(uri: &str) -> Option<(String, Option<String>)> {
    let mut parts = uri.split(':');
    let (kind, id) = match (parts.next(), parts.next(), parts.next()) {
        (Some("spotify"), Some(kind), Some(id)) if !id.is_empty() => (kind, id),
        _ => return None,
    };

    let client = reqwest::Client::builder().timeout(TIMEOUT).build().ok()?;
    let target = format!("https://open.spotify.com/{kind}/{id}");
    let response = client
        .get("https://open.spotify.com/oembed")
        .query(&[("url", target.as_str())])
        // Without a browser-ish agent this is refused.
        .header("user-agent", concat!("boombox/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        tracing::debug!("oembed for {uri}: {}", response.status());
        return None;
    }
    let body: OEmbed = response.json().await.ok()?;
    Some((body.title, body.thumbnail_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing here should reach the network for input that cannot be a
    /// Spotify URI in the first place.
    #[tokio::test]
    async fn a_malformed_uri_is_refused_without_a_request() {
        for bad in ["", "spotify:", "spotify:playlist", "spotify:playlist:", "nonsense"] {
            assert!(describe(bad).await.is_none(), "{bad:?}");
        }
    }
}
