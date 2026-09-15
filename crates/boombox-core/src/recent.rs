//! Playlists and albums you have listened to, remembered.
//!
//! Spotify will not tell a third-party client what its own playlists are:
//! Daily Mixes and Discover Weekly are absent from `/me/playlists`, absent
//! from search, and 404 when fetched by id. The ids are not guessable
//! either -- 183 single-character variants of a known Daily Mix id resolve
//! to nothing.
//!
//! What does work is remembering the ones that go past. A context arrives
//! either because you played it, or because you pasted its link, and once
//! it is written down it can be named, drawn and replayed for good.

use serde::{Deserialize, Serialize};

/// One context worth offering again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    /// `spotify:playlist:...`, `spotify:album:...`.
    pub uri: String,
    /// Resolved once and kept, because the lookup is the expensive part
    /// and the answer does not change.
    #[serde(default)]
    pub name: Option<String>,
    /// Cover, from whichever source could name it.
    #[serde(default)]
    pub image: Option<String>,
    /// Unix seconds, for ordering.
    pub last_played: u64,
    /// How many separate times it has come round.
    #[serde(default = "one")]
    pub plays: u32,
}

fn one() -> u32 {
    1
}

impl Recent {
    /// What to show before a name has been resolved -- or if it never is.
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            let id = self.uri.rsplit(':').next().unwrap_or("");
            let kind = self.uri.split(':').nth(1).unwrap_or("playlist");
            format!("{kind} {}", &id[..id.len().min(8)])
        })
    }

    /// The line under the name: what kind of thing it is, and when it was
    /// last on.
    pub fn describe(&self) -> String {
        let kind = match self.uri.split(':').nth(1) {
            Some("album") => "Album",
            Some("artist") => "Artist",
            Some("show") => "Show",
            _ => "Playlist",
        };
        format!("{kind}  \u{b7}  {}", ago(self.last_played, crate::recent::clock()))
    }
}

/// "3 days ago", give or take. Precision past this is noise on a list
/// whose only job is ordering.
fn ago(then: u64, now: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..=90 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 3600 * 36 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

fn clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The remembered list, newest first.
///
/// Bounded: this is a shortcut list, not an archive, and a long one would
/// be worse at the job.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recents {
    #[serde(default)]
    pub items: Vec<Recent>,
}

/// Past this the list stops being a set of shortcuts and starts being
/// history nobody reads.
pub const LIMIT: usize = 40;

impl Recents {
    /// Notes that a context was playing, moving it to the front.
    ///
    /// Returns whether anything changed, so a caller can avoid writing the
    /// file on every poll -- the same context playing for an hour should
    /// cost one write, not thousands.
    pub fn touch(&mut self, uri: &str, at: u64) -> bool {
        if let Some(i) = self.items.iter().position(|r| r.uri == uri) {
            let mut existing = self.items.remove(i);
            // A context that is simply still playing is not a new visit.
            let returning = at.saturating_sub(existing.last_played) > REVISIT_AFTER;
            if returning {
                existing.plays += 1;
            }
            existing.last_played = at;
            let moved = i != 0;
            self.items.insert(0, existing);
            return moved || returning;
        }
        self.items.insert(
            0,
            Recent { uri: uri.to_string(), name: None, image: None, last_played: at, plays: 1 },
        );
        self.items.truncate(LIMIT);
        true
    }

    /// Records a name and cover against a context already known.
    pub fn name(&mut self, uri: &str, name: Option<String>, image: Option<String>) -> bool {
        let Some(item) = self.items.iter_mut().find(|r| r.uri == uri) else {
            return false;
        };
        if item.name == name && item.image == image {
            return false;
        }
        item.name = name;
        item.image = image;
        true
    }

    /// The first context still missing a name, if any.
    pub fn unnamed(&self) -> Option<String> {
        self.items.iter().find(|r| r.name.is_none()).map(|r| r.uri.clone())
    }
}

/// Long enough that leaving one playlist on all afternoon counts once, and
/// short enough that coming back to it tomorrow counts again.
const REVISIT_AFTER: u64 = 60 * 60 * 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_context_goes_to_the_front() {
        let mut r = Recents::default();
        assert!(r.touch("spotify:playlist:a", 1000));
        assert!(r.touch("spotify:playlist:b", 1001));
        assert_eq!(r.items[0].uri, "spotify:playlist:b");
        assert_eq!(r.items.len(), 2);
    }

    /// The same playlist left on for an hour must not be written down a
    /// thousand times, nor counted as a thousand visits.
    #[test]
    fn a_context_still_playing_changes_nothing() {
        let mut r = Recents::default();
        r.touch("spotify:playlist:a", 1000);
        assert!(!r.touch("spotify:playlist:a", 2000), "no change worth saving");
        assert_eq!(r.items[0].plays, 1);
        assert_eq!(r.items[0].last_played, 2000);
    }

    #[test]
    fn coming_back_the_next_day_counts_again() {
        let mut r = Recents::default();
        r.touch("spotify:playlist:a", 1000);
        assert!(r.touch("spotify:playlist:a", 1000 + REVISIT_AFTER + 1));
        assert_eq!(r.items[0].plays, 2);
    }

    #[test]
    fn returning_to_an_older_context_brings_it_forward() {
        let mut r = Recents::default();
        r.touch("spotify:playlist:a", 1000);
        r.touch("spotify:playlist:b", 1001);
        assert!(r.touch("spotify:playlist:a", 1002));
        assert_eq!(r.items[0].uri, "spotify:playlist:a");
        assert_eq!(r.items.len(), 2, "moved, not duplicated");
    }

    #[test]
    fn the_list_stays_bounded() {
        let mut r = Recents::default();
        for i in 0..LIMIT + 10 {
            r.touch(&format!("spotify:playlist:{i}"), 1000 + i as u64);
        }
        assert_eq!(r.items.len(), LIMIT);
        assert_eq!(r.items[0].uri, format!("spotify:playlist:{}", LIMIT + 9), "newest kept");
    }

    /// The lookup is the expensive part, so a name is written once and the
    /// caller can tell whether it needs saving.
    #[test]
    fn naming_reports_whether_it_changed_anything() {
        let mut r = Recents::default();
        r.touch("spotify:playlist:a", 1000);
        assert!(r.name("spotify:playlist:a", Some("Daily Mix 2".into()), None));
        assert!(!r.name("spotify:playlist:a", Some("Daily Mix 2".into()), None), "already known");
        assert!(!r.name("spotify:playlist:missing", Some("x".into()), None));
    }

    #[test]
    fn the_next_unnamed_context_is_offered_until_it_is_named() {
        let mut r = Recents::default();
        r.touch("spotify:playlist:a", 1000);
        assert_eq!(r.unnamed().as_deref(), Some("spotify:playlist:a"));
        r.name("spotify:playlist:a", Some("Daily Mix 2".into()), None);
        assert_eq!(r.unnamed(), None);
    }

    /// A name that never resolves still has to draw as something.
    #[test]
    fn the_second_line_says_what_it_is_and_when() {
        assert_eq!(ago(1000, 1000), "just now");
        assert_eq!(ago(1000, 1000 + 600), "10 min ago");
        assert_eq!(ago(1000, 1000 + 7200), "2h ago");
        assert_eq!(ago(1000, 1000 + 86_400 * 3), "3d ago");
    }

    #[test]
    fn an_unnamed_context_falls_back_to_its_id() {
        let r = Recent {
            uri: "spotify:playlist:37i9dQZF1EExampleMix01".into(),
            name: None,
            image: None,
            last_played: 0,
            plays: 1,
        };
        assert_eq!(r.label(), "playlist 37i9dQZF");
    }
}
