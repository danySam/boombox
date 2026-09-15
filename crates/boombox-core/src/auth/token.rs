use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const KEYRING_SERVICE: &str = "boombox";
const KEYRING_USER: &str = "spotify-oauth";

/// Refresh this far ahead of real expiry so an in-flight request never races
/// the boundary.
const REFRESH_SKEW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub scope: String,
}

impl Tokens {
    pub fn from_response(resp: TokenResponse, previous_refresh: Option<&str>) -> Result<Self> {
        // Spotify omits refresh_token on some refresh responses; keep the old one.
        let refresh_token =
            resp.refresh_token.or_else(|| previous_refresh.map(str::to_owned)).ok_or_else(
                || Error::Authorization("token response carried no refresh_token".into()),
            )?;

        Ok(Self {
            access_token: resp.access_token,
            refresh_token,
            expires_at: now_secs() + resp.expires_in,
            scope: resp.scope.unwrap_or_default(),
        })
    }

    pub fn is_expired(&self) -> bool {
        now_secs() + REFRESH_SKEW.as_secs() >= self.expires_at
    }

    pub fn expires_in(&self) -> Duration {
        Duration::from_secs(self.expires_at.saturating_sub(now_secs()))
    }

    pub fn granted_scopes(&self) -> impl Iterator<Item = &str> {
        self.scope.split_whitespace()
    }

    pub fn missing_scopes<'a>(&self, required: &[&'a str]) -> Vec<&'a str> {
        let granted: Vec<&str> = self.granted_scopes().collect();
        required.iter().copied().filter(|s| !granted.contains(s)).collect()
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// An owner-only file by default, or the OS keychain when asked for -- with
/// the file still the fallback where no keychain service is running, as on a
/// headless box.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenStore {
    /// False by default, deliberately: the keychain is opt-in.
    use_keyring: bool,
}

/// How long to wait for the OS keychain before giving up on it.
///
/// The underlying call takes no timeout of its own: on macOS a pending
/// authorisation dialog blocks `SecKeychainFindGenericPassword` inside a
/// synchronous Mach call to securityd, and it simply never returns until
/// someone answers. Not waiting forever has to be our decision.
///
/// Two values because the question is really "is there a human here to
/// answer the dialog?". If there is, they need time to find and click it.
/// If there is not -- a daemon under launchd, a CI run -- waiting at all
/// only delays an outcome that is already decided.
const KEYRING_WAIT_INTERACTIVE: Duration = Duration::from_secs(30);
const KEYRING_WAIT_DETACHED: Duration = Duration::from_secs(3);

fn keyring_wait() -> Duration {
    use std::io::IsTerminal as _;
    // stderr rather than stdin: the CLI is routinely piped to, but a
    // terminal on stderr still means someone is watching.
    if std::io::stderr().is_terminal() { KEYRING_WAIT_INTERACTIVE } else { KEYRING_WAIT_DETACHED }
}

/// Runs a blocking keychain call on a thread we can walk away from.
///
/// The thread is deliberately abandoned on timeout rather than joined. It
/// is parked inside a Mach call that cannot be cancelled, and there is no
/// safe way to kill a thread holding Security framework locks -- so it
/// costs one blocked thread until the process exits, which is the cheapest
/// correct option available.
fn with_deadline<T: Send + 'static>(
    wait: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Fails only if we already gave up and dropped the receiver.
        let _ = tx.send(f());
    });
    rx.recv_timeout(wait).ok()
}

/// What a keychain read produced. `TimedOut` is distinct from `Empty`
/// because they call for opposite advice: one means "log in", the other
/// means "your keychain is not answering", and telling someone to log in
/// when the keychain is wedged sends them into the same hang again.
enum KeyringRead {
    Found(String),
    Empty,
    TimedOut,
}

/// `BOOMBOX_NO_KEYRING=1` forces the file store regardless of config, for one-off
/// runs and CI. Config is the place to set it permanently.
fn env_disables_keyring() -> bool {
    std::env::var_os("BOOMBOX_NO_KEYRING").is_some_and(|v| v != "0" && !v.is_empty())
}

impl TokenStore {
    pub fn new(use_keyring: bool) -> Self {
        Self { use_keyring: use_keyring && !env_disables_keyring() }
    }

    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::new(config.auth.keyring)
    }

    pub fn uses_keyring(&self) -> bool {
        self.use_keyring
    }

    pub fn load(&self) -> Result<Option<Tokens>> {
        let read = self.keyring_get();
        if let KeyringRead::Found(raw) = &read {
            return Ok(Some(serde_json::from_str(raw)?));
        }
        let path = crate::private::token_file()?;
        match std::fs::read_to_string(&path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No tokens anywhere. If the keychain never answered then
                // that is the real reason, and reporting "not authenticated"
                // would send the user to a login that hangs identically.
                if matches!(read, KeyringRead::TimedOut) {
                    return Err(Error::KeyringTimeout(keyring_wait().as_secs()));
                }
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, tokens: &Tokens) -> Result<StorageBackend> {
        let raw = serde_json::to_string(tokens)?;
        if !self.use_keyring {
            let path = crate::private::token_file()?;
            crate::private::write_private(&path, raw.as_bytes())?;
            return Ok(StorageBackend::File(path.display().to_string()));
        }
        let written = with_deadline(keyring_wait(), {
            let raw = raw.clone();
            move || {
                keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                    .and_then(|entry| entry.set_password(&raw))
            }
        });
        let Some(written) = written else {
            tracing::warn!("keychain did not respond in time, storing the token in a file");
            let path = crate::private::token_file()?;
            crate::private::write_private(&path, raw.as_bytes())?;
            return Ok(StorageBackend::File(path.display().to_string()));
        };
        match written {
            Ok(()) => {
                // Don't leave a stale copy on disk shadowing the keychain.
                let _ = std::fs::remove_file(crate::private::token_file()?);
                Ok(StorageBackend::Keyring)
            }
            Err(e) => {
                tracing::debug!("keyring unavailable, falling back to file: {e}");
                let path = crate::private::token_file()?;
                crate::private::write_private(&path, raw.as_bytes())?;
                Ok(StorageBackend::File(path.display().to_string()))
            }
        }
    }

    pub fn clear(&self) -> Result<()> {
        let deleted = with_deadline(keyring_wait(), || {
            keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                .and_then(|entry| entry.delete_credential())
        });
        match deleted {
            Some(Ok(())) | Some(Err(keyring::Error::NoEntry)) => {}
            Some(Err(e)) => tracing::debug!("keyring delete failed: {e}"),
            // The file copy below is still removed, so a logout with a
            // wedged keychain does not silently leave the token readable.
            None => tracing::warn!("keychain did not respond in time; entry may remain"),
        }
        match std::fs::remove_file(crate::private::token_file()?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn keyring_get(&self) -> KeyringRead {
        if !self.use_keyring {
            return KeyringRead::Empty;
        }
        let read = with_deadline(keyring_wait(), || {
            keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                .and_then(|entry| entry.get_password())
        });
        match read {
            Some(Ok(raw)) => KeyringRead::Found(raw),
            Some(Err(keyring::Error::NoEntry)) => KeyringRead::Empty,
            Some(Err(e)) => {
                tracing::debug!("keyring read failed: {e}");
                KeyringRead::Empty
            }
            None => {
                tracing::warn!(
                    "keychain did not respond within {}s; a confirmation dialog may be waiting",
                    keyring_wait().as_secs()
                );
                KeyringRead::TimedOut
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum StorageBackend {
    Keyring,
    File(String),
}

impl std::fmt::Display for StorageBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyring => write!(f, "system keychain"),
            Self::File(p) => write!(f, "{p} (0600)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: a call that never returns must not take the process
    /// with it. Modelled on the real failure, where the keychain read parks
    /// in a Mach call that cannot be cancelled.
    #[test]
    fn a_call_that_never_returns_is_abandoned() {
        let start = std::time::Instant::now();
        let result = with_deadline(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(60));
            "never gets here"
        });
        assert!(result.is_none(), "should have given up");
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
    }

    #[test]
    fn a_call_that_answers_in_time_is_returned() {
        assert_eq!(with_deadline(Duration::from_secs(30), || 42), Some(42));
    }

    /// Errors still have to come back as errors -- the deadline must not
    /// flatten a real keychain failure into a timeout.
    #[test]
    fn an_error_is_passed_through_rather_than_swallowed() {
        let result = with_deadline(Duration::from_secs(30), || Err::<(), _>("no entry"));
        assert_eq!(result, Some(Err("no entry")));
    }

    /// A daemon has nobody to answer a dialog, so waiting only delays an
    /// outcome that is already settled. Tests run without a terminal, which
    /// is the same situation.
    #[test]
    fn a_detached_process_does_not_wait_long() {
        assert_eq!(keyring_wait(), KEYRING_WAIT_DETACHED);
        assert!(KEYRING_WAIT_DETACHED < KEYRING_WAIT_INTERACTIVE);
    }

    /// "not authenticated" would send someone to a login that hangs the
    /// same way, so the timeout has to say something different.
    #[test]
    fn the_timeout_error_names_the_dialog_and_the_escape_hatch() {
        let message = Error::KeyringTimeout(3).to_string();
        assert!(message.contains("dialog"), "{message}");
        assert!(message.contains("keyring = false"), "{message}");
        assert!(message.contains("BOOMBOX_NO_KEYRING"), "{message}");
        assert!(!message.contains("boombox auth login"), "must not advise a login: {message}");
    }

    fn tokens_expiring_in(secs: u64) -> Tokens {
        Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now_secs() + secs,
            scope: "user-read-playback-state user-modify-playback-state".into(),
        }
    }

    /// The keychain asks again after every build or upgrade on macOS, so it
    /// is something to opt into, not something to meet on first run.
    #[test]
    fn tokens_go_to_the_file_unless_the_keychain_is_asked_for() {
        assert!(!TokenStore::default().uses_keyring());
        assert!(!crate::config::AuthConfig::default().keyring);
    }

    #[test]
    fn config_selects_the_backend_and_the_env_var_can_only_disable() {
        // SAFETY: single-threaded test process, no other reader of this var.
        unsafe {
            std::env::remove_var("BOOMBOX_NO_KEYRING");
            assert!(TokenStore::new(true).uses_keyring());
            assert!(!TokenStore::new(false).uses_keyring(), "config off wins");

            std::env::set_var("BOOMBOX_NO_KEYRING", "1");
            assert!(!TokenStore::new(true).uses_keyring(), "env overrides config on");

            std::env::set_var("BOOMBOX_NO_KEYRING", "0");
            assert!(TokenStore::new(true).uses_keyring(), "0 must not disable it");
            std::env::set_var("BOOMBOX_NO_KEYRING", "");
            assert!(TokenStore::new(true).uses_keyring(), "empty must not disable it");
            std::env::remove_var("BOOMBOX_NO_KEYRING");
        }
    }

    #[test]
    fn expiry_accounts_for_skew() {
        assert!(!tokens_expiring_in(3600).is_expired());
        assert!(tokens_expiring_in(30).is_expired(), "inside the 60s skew");
        assert!(tokens_expiring_in(0).is_expired());
    }

    #[test]
    fn refresh_token_is_carried_over_when_omitted() {
        let resp = TokenResponse {
            access_token: "new".into(),
            expires_in: 3600,
            refresh_token: None,
            scope: None,
        };
        let t = Tokens::from_response(resp, Some("old-refresh")).unwrap();
        assert_eq!(t.refresh_token, "old-refresh");
        assert_eq!(t.access_token, "new");
    }

    #[test]
    fn missing_refresh_token_with_no_previous_is_an_error() {
        let resp = TokenResponse {
            access_token: "new".into(),
            expires_in: 3600,
            refresh_token: None,
            scope: None,
        };
        assert!(Tokens::from_response(resp, None).is_err());
    }

    #[test]
    fn missing_scopes_are_reported() {
        let t = tokens_expiring_in(3600);
        let missing = t.missing_scopes(&[
            "user-read-playback-state",
            "user-library-read",
            "playlist-read-private",
        ]);
        assert_eq!(missing, vec!["user-library-read", "playlist-read-private"]);
    }
}
