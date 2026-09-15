//! What this binary is, for the places that have to answer "which build?".
//!
//! Every front end reports the same string, so a daemon and a TUI that
//! disagree can be spotted by eye without digging through logs.

/// Workspace version, e.g. `0.1.0`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Short commit hash, or empty for a build made outside a git checkout.
pub const COMMIT: &str = env!("BOOMBOX_COMMIT");
/// How many commits lead to this one, or empty when it cannot be known.
///
/// The one part of the stamp that can be *compared*: `#41` is plainly
/// later than `#38`, where two hashes say nothing about order and the
/// date is too coarse to separate a day's work.
///
/// Counted along the current branch, so a feature branch numbers its own
/// commits and two branches can reach the same number. Read it with the
/// hash, which is unambiguous, rather than on its own.
pub const BUILD: &str = env!("BOOMBOX_BUILD");
/// Commit date as `YYYY-MM-DD`, or empty. The commit date rather than the
/// build date, so two machines building the same commit agree.
pub const COMMIT_DATE: &str = env!("BOOMBOX_COMMIT_DATE");
/// `+` when the working tree had uncommitted changes, else empty.
pub const DIRTY: &str = env!("BOOMBOX_DIRTY");

/// `0.1.0 #41 (df71b69+)` -- what identifies a build in one glance. Kept
/// short enough for a status bar; [`long`] is for `--version`.
pub fn short() -> String {
    if COMMIT.is_empty() {
        return VERSION.to_string();
    }
    let count = if BUILD.is_empty() { String::new() } else { format!(" #{BUILD}") };
    format!("{VERSION}{count} ({COMMIT}{DIRTY})")
}

/// Adds the commit date, for `boombox --version` and the daemon's startup log.
pub fn long() -> String {
    match (COMMIT.is_empty(), COMMIT_DATE.is_empty()) {
        (true, _) => VERSION.to_string(),
        (false, true) => short(),
        (false, false) => format!("{} {COMMIT_DATE}", short()),
    }
}

/// [`long`] as a `&'static str`, for the callers that need one -- clap's
/// `version` among them.
pub fn long_static() -> &'static str {
    static LONG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LONG.get_or_init(long)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_always_reported() {
        assert!(!short().is_empty());
        assert!(short().starts_with(VERSION));
        assert!(long().starts_with(VERSION));
    }

    /// The stamp is what tells two builds apart, so losing it silently
    /// would defeat the point. Only enforced in a checkout.
    #[test]
    fn the_static_form_matches_the_owned_one() {
        assert_eq!(long_static(), long());
    }

    /// The number exists to be compared, which means it has to be a
    /// number -- an empty or non-numeric one would sort as nonsense.
    #[test]
    fn the_build_number_is_a_count_or_absent() {
        if BUILD.is_empty() {
            return; // Tarball or shallow clone: nothing to check.
        }
        assert!(BUILD.chars().all(|c| c.is_ascii_digit()), "{BUILD}");
        assert!(BUILD.parse::<u64>().unwrap() > 0);
        assert!(short().contains(&format!("#{BUILD}")), "{}", short());
    }

    #[test]
    fn a_checkout_build_carries_its_commit() {
        if COMMIT.is_empty() {
            return; // Built from a tarball: nothing to assert.
        }
        assert!(short().contains(COMMIT), "{}", short());
        assert!(COMMIT.chars().all(|c| c.is_ascii_hexdigit()), "{COMMIT}");
    }
}
