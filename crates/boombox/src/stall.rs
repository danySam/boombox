//! Noticing when Spotify says this computer is playing and no sound comes out.
//!
//! The Connect session can stay registered while playback underneath it has
//! stopped working. Nothing else notices: the device is still listed, the
//! session still counts as connected, and Spotify's clock keeps advancing.

use std::time::{Duration, Instant};

use boombox_core::api::PlaybackState;

/// How long Spotify must have reported this device as playing, and how long
/// no audio must have reached it, before it counts. Long enough for a
/// transfer to start sounding and a slow buffer to fill; short enough to say
/// so while someone is still wondering why it is quiet.
pub const STALL_AFTER: Duration = Duration::from_secs(15);

/// Whether Spotify reports this device as the one playing.
pub fn playing_here(state: Option<&PlaybackState>, device_name: &str) -> bool {
    state.is_some_and(|s| s.is_playing && s.device.as_ref().is_some_and(|d| d.name == device_name))
}

#[derive(Debug, Default)]
pub struct StallWatch {
    /// Since when Spotify has reported playback here without a break.
    playing_since: Option<Instant>,
    stalled: bool,
}

/// A change worth saying out loud.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// Playing here for a while, and nothing reaching the sink for as long.
    Stalled,
    /// Audio is reaching the sink again.
    Recovered,
    /// Playback stopped or moved elsewhere while nothing was coming out.
    Cleared,
}

impl StallWatch {
    /// Takes one poll's worth of evidence.
    ///
    /// `since_audio` counts silence as audio -- a quiet passage still arrives
    /// as samples -- so only a sink receiving nothing at all looks silent.
    /// Both conditions have to have held for [`STALL_AFTER`]: moving playback
    /// here after an hour of idling starts an hour after the last audio, and
    /// that is not a stall.
    pub fn observe(
        &mut self,
        playing_here: bool,
        since_audio: Duration,
        now: Instant,
    ) -> Option<Change> {
        if !playing_here {
            self.playing_since = None;
            return std::mem::take(&mut self.stalled).then_some(Change::Cleared);
        }
        let playing_since = *self.playing_since.get_or_insert(now);
        let silent = since_audio >= STALL_AFTER;
        let settled = now.saturating_duration_since(playing_since) >= STALL_AFTER;
        match (self.stalled, silent) {
            (false, true) if settled => {
                self.stalled = true;
                Some(Change::Stalled)
            }
            (true, false) => {
                self.stalled = false;
                Some(Change::Recovered)
            }
            _ => None,
        }
    }

    pub fn is_stalled(&self) -> bool {
        self.stalled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn music_that_keeps_arriving_never_counts() {
        let (mut watch, t0) = (StallWatch::default(), Instant::now());
        for s in 0..120 {
            assert_eq!(watch.observe(true, Duration::from_millis(100), t0 + secs(s)), None);
        }
    }

    /// Moving playback here after an hour idle begins an hour after the last
    /// audio. The grace is what keeps that from reading as a stall.
    #[test]
    fn a_long_idle_before_playing_here_is_not_a_stall() {
        let (mut watch, t0) = (StallWatch::default(), Instant::now());
        assert_eq!(watch.observe(true, secs(3600), t0), None);
        assert_eq!(watch.observe(true, secs(3610), t0 + secs(10)), None, "still starting");
        assert_eq!(watch.observe(true, Duration::from_millis(100), t0 + secs(12)), None);
        assert!(!watch.is_stalled(), "and then it sounded");
    }

    #[test]
    fn playing_here_with_nothing_arriving_is_said_once() {
        let (mut watch, t0) = (StallWatch::default(), Instant::now());
        assert_eq!(watch.observe(true, secs(20), t0), None);
        assert_eq!(watch.observe(true, secs(35), t0 + secs(15)), Some(Change::Stalled));
        assert_eq!(watch.observe(true, secs(36), t0 + secs(16)), None, "not repeated");
        assert!(watch.is_stalled());
    }

    #[test]
    fn audio_arriving_again_ends_it() {
        let (mut watch, t0) = (StallWatch::default(), Instant::now());
        watch.observe(true, secs(20), t0);
        watch.observe(true, secs(35), t0 + secs(15));
        assert_eq!(
            watch.observe(true, Duration::from_millis(100), t0 + secs(30)),
            Some(Change::Recovered)
        );
        assert!(!watch.is_stalled());
    }

    #[test]
    fn playback_stopping_or_moving_away_clears_it() {
        let (mut watch, t0) = (StallWatch::default(), Instant::now());
        watch.observe(true, secs(20), t0);
        watch.observe(true, secs(35), t0 + secs(15));
        assert_eq!(watch.observe(false, secs(40), t0 + secs(20)), Some(Change::Cleared));
        assert_eq!(watch.observe(false, secs(41), t0 + secs(21)), None, "said once");
    }

    /// Resuming is another start, and gets the same grace as the first.
    #[test]
    fn a_pause_gives_the_next_start_its_own_grace() {
        let (mut watch, t0) = (StallWatch::default(), Instant::now());
        watch.observe(true, secs(20), t0);
        watch.observe(false, secs(30), t0 + secs(10));
        assert_eq!(watch.observe(true, secs(31), t0 + secs(11)), None);
        assert_eq!(watch.observe(true, secs(40), t0 + secs(20)), None, "only 9s since resuming");
        assert_eq!(watch.observe(true, secs(46), t0 + secs(26)), Some(Change::Stalled));
    }

    fn state(json: &str) -> PlaybackState {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn only_this_device_playing_counts_as_playing_here() {
        let here = state(r#"{"device":{"name":"boombox","type":"Computer"},"is_playing":true}"#);
        let paused = state(r#"{"device":{"name":"boombox","type":"Computer"},"is_playing":false}"#);
        let elsewhere =
            state(r#"{"device":{"name":"Kitchen","type":"Speaker"},"is_playing":true}"#);
        assert!(playing_here(Some(&here), "boombox"));
        assert!(!playing_here(Some(&paused), "boombox"));
        assert!(!playing_here(Some(&elsewhere), "boombox"));
        assert!(!playing_here(None, "boombox"));
    }
}
