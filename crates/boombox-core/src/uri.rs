//! Turning whatever the user pasted into a Spotify URI.
//!
//! The share button in every Spotify client copies an `open.spotify.com`
//! link, not a `spotify:` URI, so a link is what people actually have to
//! hand. It is also the only route to a playlist the API will not let a
//! third-party client list -- a Daily Mix can be played by URI but never
//! found by search, so pasting its link is the way in.

/// The things a link can point at that we know what to do with.
const KINDS: [&str; 6] = ["track", "album", "artist", "playlist", "episode", "show"];

/// Converts an `open.spotify.com` link to a `spotify:` URI, passing an
/// already-correct URI through unchanged.
///
/// Returns `None` for anything that is neither, so callers can give their
/// own message rather than guessing at one here.
pub fn normalise(input: &str) -> Option<String> {
    let input = input.trim();
    if let Some(rest) = input.strip_prefix("spotify:") {
        let (kind, id) = rest.split_once(':')?;
        return valid(kind, id).then(|| format!("spotify:{kind}:{id}"));
    }

    // Everything before the host varies: with or without a scheme, and
    // "www." sometimes. What matters is the path after the host.
    let path = input
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .strip_prefix("open.spotify.com/")?;

    // A share link carries `?si=` tracking, and a localised one has a
    // segment in front: open.spotify.com/intl-de/track/...
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let first = parts.next()?;
    let (kind, id) = if first.starts_with("intl-") {
        (parts.next()?, parts.next()?)
    } else {
        (first, parts.next()?)
    };
    valid(kind, id).then(|| format!("spotify:{kind}:{id}"))
}

/// Spotify ids are base62. Checking the shape catches a truncated paste,
/// which would otherwise become a confident 404 much later.
fn valid(kind: &str, id: &str) -> bool {
    KINDS.contains(&kind) && !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uri_passes_straight_through() {
        assert_eq!(
            normalise("spotify:playlist:37i9dQZF1EExampleMix02").as_deref(),
            Some("spotify:playlist:37i9dQZF1EExampleMix02")
        );
        assert_eq!(
            normalise("spotify:track:ExampleTrack0000000001").as_deref(),
            Some("spotify:track:ExampleTrack0000000001")
        );
    }

    /// What the share button actually copies.
    #[test]
    fn a_share_link_becomes_a_uri() {
        assert_eq!(
            normalise("https://open.spotify.com/playlist/37i9dQZF1EExampleMix02").as_deref(),
            Some("spotify:playlist:37i9dQZF1EExampleMix02")
        );
    }

    /// The share button appends tracking, and it must not end up in the id.
    #[test]
    fn the_tracking_parameter_is_dropped() {
        assert_eq!(
            normalise("https://open.spotify.com/album/ExampleAlbum0000000001?si=aBcD1234")
                .as_deref(),
            Some("spotify:album:ExampleAlbum0000000001")
        );
    }

    /// A link copied from a localised web player carries a locale segment.
    #[test]
    fn a_localised_link_still_resolves() {
        assert_eq!(
            normalise("https://open.spotify.com/intl-de/track/ExampleTrack0000000001").as_deref(),
            Some("spotify:track:ExampleTrack0000000001")
        );
    }

    #[test]
    fn the_scheme_and_www_are_optional() {
        for form in [
            "http://open.spotify.com/artist/ExampleArtist000000001",
            "open.spotify.com/artist/ExampleArtist000000001",
            "https://www.open.spotify.com/artist/ExampleArtist000000001",
            "  https://open.spotify.com/artist/ExampleArtist000000001  ",
        ] {
            assert_eq!(
                normalise(form).as_deref(),
                Some("spotify:artist:ExampleArtist000000001"),
                "{form}"
            );
        }
    }

    #[test]
    fn every_kind_we_can_play_is_accepted() {
        for kind in KINDS {
            let link = format!("https://open.spotify.com/{kind}/abc123");
            assert_eq!(normalise(&link).as_deref(), Some(&*format!("spotify:{kind}:abc123")));
        }
    }

    /// A truncated paste would otherwise become a 404 a long way from here.
    #[test]
    fn nonsense_is_refused_rather_than_guessed_at() {
        for bad in [
            "",
            "hello",
            "https://example.com/playlist/abc",
            "https://open.spotify.com/",
            "https://open.spotify.com/playlist",
            "https://open.spotify.com/playlist/",
            "spotify:playlist",
            "spotify:playlist:",
            "spotify:nonsense:abc123",
            "https://open.spotify.com/nonsense/abc123",
            // A user link is a real Spotify URL and not something to play.
            "https://open.spotify.com/user/someone",
        ] {
            assert_eq!(normalise(bad), None, "{bad:?} should be refused");
        }
    }
}
