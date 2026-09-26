//! What the user asked for, held in front of the truth until the truth
//! catches up.
//!
//! Spotify applies a write long before it reports one. A volume change
//! takes about eleven seconds to come back from the player endpoint, and a
//! pause or a seek takes a poll or two. Anything driven purely by polling
//! therefore shows the old value for a moment after the key, and then
//! jumps -- which reads as the app having ignored the keypress.
//!
//! The poll immediately after a write is the worst of them: a daemon that
//! refreshes as soon as it has written has gone and fetched the state that
//! contradicts what it just did.
//!
//! So a write records what it asked for, and reads are answered with that
//! until an observation agrees with it, or until the wait runs out --
//! because a change that failed, or one aimed at a device that will never
//! report it, must not leave a lie on screen for ever.

use std::time::{Duration, Instant};

/// A value asked for and not yet confirmed.
///
/// Agreement is the caller's to define: a device rounds volume, a position
/// keeps moving while it is being sought, and whether something is playing
/// is exactly true or false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending<T> {
    wanted: T,
    since: Instant,
    settle: Duration,
}

impl<T: Copy> Pending<T> {
    pub fn new(wanted: T, settle: Duration) -> Self {
        Self::at(wanted, settle, Instant::now())
    }

    /// With the clock supplied, so the waiting can be tested without it.
    pub fn at(wanted: T, settle: Duration, now: Instant) -> Self {
        Self { wanted, since: now, settle }
    }

    pub fn wanted(&self) -> T {
        self.wanted
    }

    /// Replaces the target and restarts the wait: a second keypress is a
    /// fresh request, not a continuation of the one before it.
    pub fn renew(&mut self, wanted: T, now: Instant) {
        self.wanted = wanted;
        self.since = now;
    }

    /// Whether the wait is over, whatever was observed.
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) >= self.settle
    }

    /// Whether this should still be shown in place of `observed`.
    ///
    /// No observation at all keeps it: that is a player reporting nothing
    /// mid-transfer, which is no evidence against what was asked for.
    pub fn holds(&self, observed: Option<T>, agrees: impl Fn(T, T) -> bool, now: Instant) -> bool {
        if self.expired(now) {
            return false;
        }
        match observed {
            Some(seen) => !agrees(self.wanted, seen),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: Duration = Duration::from_secs(10);
    const EXACT: fn(u32, u32) -> bool = |a, b| a == b;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn it_holds_while_the_player_still_reports_the_old_value() {
        let t0 = Instant::now();
        let pending = Pending::at(70, SETTLE, t0);
        assert!(pending.holds(Some(40), EXACT, t0 + secs(1)), "40 is the value we replaced");
        assert_eq!(pending.wanted(), 70);
    }

    /// The normal ending: the write landed and the player says so.
    #[test]
    fn it_ends_as_soon_as_the_player_agrees() {
        let t0 = Instant::now();
        let pending = Pending::at(70, SETTLE, t0);
        assert!(!pending.holds(Some(70), EXACT, t0 + secs(1)));
    }

    /// Devices round: ask for 55 and read back 54. An exact match would
    /// never arrive, and the figure would be defended until it expired.
    #[test]
    fn agreement_is_the_callers_to_define() {
        let t0 = Instant::now();
        let pending = Pending::at(55, SETTLE, t0);
        let within_two = |a: u32, b: u32| a.abs_diff(b) <= 2;
        assert!(!pending.holds(Some(54), within_two, t0 + secs(1)), "close enough");
        assert!(pending.holds(Some(40), within_two, t0 + secs(1)), "not close");
    }

    /// The backstop: a change that failed, or a device that will never
    /// report what was asked of it, must not leave a lie on screen.
    #[test]
    fn it_gives_up_when_the_wait_is_over() {
        let t0 = Instant::now();
        let pending = Pending::at(70, SETTLE, t0);
        assert!(pending.holds(Some(40), EXACT, t0 + secs(9)));
        assert!(!pending.holds(Some(40), EXACT, t0 + secs(10)));
    }

    /// Nothing reported is not disagreement. Spotify answers with no state
    /// at all for a moment during a transfer.
    #[test]
    fn nothing_reported_is_not_evidence_against_it() {
        let t0 = Instant::now();
        let pending = Pending::at(70, SETTLE, t0);
        assert!(pending.holds(None, EXACT, t0 + secs(1)));
        assert!(!pending.holds(None, EXACT, t0 + secs(11)), "but the wait still runs out");
    }

    /// Holding a key down must not let the wait expire underneath it.
    #[test]
    fn a_fresh_request_restarts_the_wait() {
        let t0 = Instant::now();
        let mut pending = Pending::at(70, SETTLE, t0);
        pending.renew(75, t0 + secs(9));
        assert_eq!(pending.wanted(), 75);
        assert!(pending.holds(Some(40), EXACT, t0 + secs(18)), "nine seconds into the new wait");
        assert!(!pending.holds(Some(40), EXACT, t0 + secs(19)));
    }
}
