use std::fmt;

/// Exit-code carrying error type. The discriminants are part of the CLI
/// contract: scripts branch on them, so don't renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Usage = 1,
    NotAuthenticated = 2,
    Api = 3,
    NoActiveDevice = 4,
    PremiumRequired = 5,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not signed in to Spotify. run `boombox setup`")]
    NotAuthenticated,

    #[error("not set up yet. run `boombox setup`")]
    NoClientId,

    /// Carries only the end of the ID: enough to recognise, not to copy.
    #[error(
        "Spotify does not recognise the client ID ending {0}. Copy the Client ID from your \
         app's settings at https://developer.spotify.com/dashboard -- not the Client Secret, \
         which looks just like it -- and run `boombox auth login --client-id <id>`"
    )]
    UnknownClientId(String),

    #[error("no active device. start playback somewhere, or `boombox connect <name>`")]
    NoActiveDevice,

    #[error("this requires Spotify Premium")]
    PremiumRequired,

    #[error("spotify api error {status}: {message}")]
    Api { status: u16, message: String },

    #[error("authorization failed: {0}")]
    Authorization(String),

    #[error(
        "authorization timed out after {0}s. If Spotify showed an error instead of asking \
         for approval, `client_id: Invalid` means the client ID is wrong (replace it with \
         `boombox auth login --client-id <id>`), and an invalid redirect URI means your app's \
         settings need http://127.0.0.1:<port>/callback exactly"
    )]
    AuthorizationTimeout(u64),

    #[error(
        "the OS keychain did not respond within {0}s -- a confirmation dialog may be waiting \
         for an answer. Answer it and retry, or set `[auth] keyring = false` in the config \
         (or BOOMBOX_NO_KEYRING=1) to use the token file instead."
    )]
    KeyringTimeout(u64),

    /// The daemon accepted the connection and never replied.
    ///
    /// Its own variant rather than an `Api` error, because callers treat it
    /// differently: it is transient for a poll that will simply try again,
    /// and fatal for the one-off check that decides whether to trust the
    /// daemon at all.
    #[error(
        "the boombox daemon did not answer within {0}s -- it may be stuck. \
         `boombox daemon --stop` ends it, or run with --direct to bypass it."
    )]
    DaemonNotAnswering(u64),

    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// A client ID Spotify has no app for.
    pub fn unknown_client_id(client_id: &str) -> Self {
        let tail: Vec<char> = client_id.chars().rev().take(4).collect();
        Self::UnknownClientId(tail.into_iter().rev().collect())
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Error::NotAuthenticated | Error::NoClientId | Error::UnknownClientId(_) => {
                ExitCode::NotAuthenticated
            }
            Error::NoActiveDevice => ExitCode::NoActiveDevice,
            Error::PremiumRequired => ExitCode::PremiumRequired,
            Error::Config(_) => ExitCode::Usage,
            _ => ExitCode::Api,
        }
    }
}

impl fmt::Display for ExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", *self as i32)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Deliberately not NotAuthenticated: that code means "run a login",
    /// and a login would block on the same wedged keychain.
    #[test]
    fn a_keychain_timeout_does_not_tell_scripts_to_re_login() {
        assert_eq!(Error::KeyringTimeout(3).exit_code(), ExitCode::Api);
        assert_ne!(Error::KeyringTimeout(3).exit_code(), ExitCode::NotAuthenticated);
    }

    /// The fix is a new login, so scripts get the code that says so.
    #[test]
    fn an_unknown_client_id_shows_only_its_end_and_points_at_the_fix() {
        let err = Error::unknown_client_id("0123456789abcdef0123456789abcdef");
        let message = err.to_string();
        assert!(message.contains("ending cdef."), "{message}");
        assert!(!message.contains("0123456789abcdef0123"), "{message}");
        assert!(message.contains("Client Secret") && message.contains("--client-id"), "{message}");
        assert_eq!(err.exit_code(), ExitCode::NotAuthenticated);
    }
}
