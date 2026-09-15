use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use boombox_core::auth::{Auth, ClientCheck, SCOPES, TokenStore, check_client_id};
use boombox_core::{Client, Config};
use clap::Subcommand;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Subcommand)]
pub enum AuthCommand {
    /// Authorize boombox against your Spotify account
    Login {
        /// Use this client ID, and save it in place of any in the config file
        #[arg(long, env = "SPOTIFY_CLIENT_ID")]
        client_id: Option<String>,

        /// Loopback port for the OAuth redirect (must match your dashboard entry)
        #[arg(long)]
        port: Option<u16>,

        /// Print the URL instead of opening a browser
        #[arg(long)]
        no_browser: bool,

        /// Re-authorize even if a valid session already exists
        #[arg(long)]
        force: bool,

        /// Authorize the streaming session instead of the Web API one.
        /// Separate because the Connect protocol will not accept a
        /// Development Mode client ID. Needs --features streaming.
        #[arg(long)]
        streaming: bool,
    },

    /// Show whether the stored session works
    Status {
        #[arg(long)]
        json: bool,

        /// Exchange the refresh token first, to prove that path works
        #[arg(long)]
        refresh: bool,
    },

    /// Delete the stored tokens
    Logout,
}

impl AuthCommand {
    pub async fn run(self) -> Result<()> {
        match self {
            Self::Login { client_id, port, no_browser, force, streaming } => {
                if streaming {
                    return streaming_login().await;
                }
                login(client_id, port, no_browser, force).await
            }
            Self::Status { json, refresh } => status(json, refresh).await,
            Self::Logout => logout(),
        }
    }
}

#[cfg(feature = "streaming")]
async fn streaming_login() -> Result<()> {
    crate::streaming::login(&Config::load()?).await
}

#[cfg(not(feature = "streaming"))]
async fn streaming_login() -> Result<()> {
    anyhow::bail!(
        "this build has no streaming support. Rebuild with:\n  \
         cargo build --release --features streaming"
    )
}

async fn login(
    client_id_override: Option<String>,
    port_override: Option<u16>,
    no_browser: bool,
    force: bool,
) -> Result<()> {
    // Checked before anything is written, so a typo does not leave a config
    // file behind on its way to an error.
    let client_id_override = client_id_override
        .map(|given| {
            boombox_core::config::normalise_client_id(&given).ok_or_else(|| {
                anyhow::anyhow!(
                    "that is not a Client ID: those are 32 letters and digits, shown in your \
                     app's settings"
                )
            })
        })
        .transpose()?;
    // Asked before anything is written too, so an ID Spotify has no app for
    // is never saved.
    if let Some(id) = &client_id_override {
        refuse_unknown_client(id).await?;
    }
    let mut checked = client_id_override.is_some();

    let (config_path, created) = Config::ensure_exists()?;
    if created {
        println!("created {}", config_path.display());
    }
    let config = Config::load()?;
    let port = port_override.unwrap_or(config.redirect_port);

    let configured = config.resolve_client_id().ok();
    let replaced = replaces_configured(client_id_override.as_deref(), configured.as_deref());
    let client_id = match (client_id_override, configured) {
        // Given, and not what later commands would read: save it. With none
        // saved, every later command would fail for want of what this one had;
        // with a different one saved, they would all use the old app -- and a
        // wrong ID entered at setup had no other way out.
        (Some(id), configured) => {
            if replaced || configured.is_none() {
                let path = boombox_core::config::save_client_id(&id)?;
                println!("saved the client ID to {}", path.display());
            }
            id
        }
        (None, Some(id)) => id,
        // Exactly where someone who skipped `boombox setup` arrives, so ask
        // rather than fail -- when there is anyone at the terminal to ask.
        (None, None) if crate::setup_cmd::interactive() => {
            checked = true;
            crate::setup_cmd::ask_for_app(port).await?
        }
        (None, None) => return Err(boombox_core::Error::NoClientId.into()),
    };

    // Tokens belong to the app that issued them; the new one cannot refresh
    // them, so a changed ID signs in again whatever is stored.
    if !force
        && !replaced
        && let Some(existing) = TokenStore::from_config(&config).load()?
        && !existing.is_expired()
    {
        println!("already authorized. use --force to authorize again.");
        return Ok(());
    }

    if !checked {
        refuse_unknown_client(&client_id).await?;
    }
    let signed = sign_in(&config, client_id, Some(port), no_browser).await?;
    println!();
    println!("Authorized as {} ({})", signed.label, signed.id);
    println!("Tokens stored in {}", signed.backend);
    if !signed.missing_scopes.is_empty() {
        println!();
        println!(
            "Note: Spotify withheld {} scope(s): {}",
            signed.missing_scopes.len(),
            signed.missing_scopes.join(", ")
        );
    }
    Ok(())
}

/// What a successful sign-in established.
pub struct SignedIn {
    pub label: String,
    pub id: String,
    pub backend: String,
    pub missing_scopes: Vec<&'static str>,
}

/// Runs the browser sign-in, stores the tokens, and proves they work.
pub async fn sign_in(
    config: &Config,
    client_id: String,
    port: Option<u16>,
    no_browser: bool,
) -> Result<SignedIn> {
    let store = TokenStore::from_config(config);
    let auth = Arc::new(Auth::with_store(client_id, store.load()?, store));
    let port = port.unwrap_or(config.redirect_port);

    let tokens = auth
        .login(port, LOGIN_TIMEOUT, |url| {
            println!("Opening your browser to authorize boombox.");
            println!();
            if no_browser || open::that_detached(url).is_err() {
                println!("Open this URL:");
                println!();
                println!("  {url}");
                println!();
            }
            println!("Waiting for the callback on http://127.0.0.1:{port}/callback ...");
        })
        .await?;

    let backend = store.save(&tokens)?;

    // Prove the token actually works rather than just claiming success.
    let client = Client::new(Arc::clone(&auth));
    let user = match client.current_user().await {
        Ok(user) => user,
        Err(boombox_core::Error::Api { status: 403, .. }) => {
            return Err(crate::setup_cmd::refused());
        }
        Err(e) => return Err(e.into()),
    };
    Ok(SignedIn {
        label: user.label().to_string(),
        id: user.id.clone(),
        backend: backend.to_string(),
        missing_scopes: tokens.missing_scopes(SCOPES),
    })
}

async fn status(json: bool, force_refresh: bool) -> Result<()> {
    let config = Config::load()?;
    let store = TokenStore::from_config(&config);
    let Some(tokens) = store.load()? else {
        if json {
            println!("{}", serde_json::json!({"authenticated": false}));
        } else {
            println!("not signed in. run `boombox setup`");
        }
        std::process::exit(boombox_core::ExitCode::NotAuthenticated as i32);
    };

    let auth = Arc::new(Auth::with_store(config.resolve_client_id()?, Some(tokens.clone()), store));
    if force_refresh {
        let before = tokens.access_token.clone();
        let after = auth.refresh().await?;
        if !json {
            let rotated = if after.access_token == before { "unchanged" } else { "rotated" };
            println!("refresh ok, access token {rotated}");
        }
    }
    let client = Client::new(Arc::clone(&auth));
    let user = client.current_user().await?;

    // access_token() may have refreshed above; report the live values.
    let live = auth.tokens().await.unwrap_or(tokens);
    let expires_in = live.expires_in().as_secs();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "authenticated": true,
                "user": { "id": user.id, "display_name": user.display_name, "uri": user.uri },
                "expires_in_secs": expires_in,
                "scopes": live.granted_scopes().collect::<Vec<_>>(),
            })
        );
    } else {
        println!("authenticated as {} ({})", user.label(), user.id);
        println!("token valid for {}", human_duration(expires_in));
        println!("scopes granted: {}", live.granted_scopes().count());
        println!(
            "token storage: {}",
            if store.uses_keyring() { "system keychain" } else { "file (0600)" }
        );
    }
    Ok(())
}

fn logout() -> Result<()> {
    TokenStore::from_config(&Config::load()?).clear()?;
    println!("signed out. stored tokens removed.");
    Ok(())
}

/// Stops before the browser opens when Spotify has no app with this ID.
///
/// Otherwise Spotify's own error page is the first sign, and this end waits
/// out the whole login timeout. Anything short of a definite no carries on.
async fn refuse_unknown_client(client_id: &str) -> Result<()> {
    match check_client_id(client_id).await {
        ClientCheck::Unknown => Err(boombox_core::Error::unknown_client_id(client_id).into()),
        ClientCheck::Known | ClientCheck::Unchecked => Ok(()),
    }
}

/// Whether a client ID given to `auth login` differs from the one configured.
///
/// `given` is already normalised; `configured` may be as typed in the file.
fn replaces_configured(given: Option<&str>, configured: Option<&str>) -> bool {
    match (given, configured) {
        (Some(given), Some(configured)) => {
            boombox_core::config::normalise_client_id(configured).as_deref() != Some(given)
        }
        _ => false,
    }
}

fn human_duration(secs: u64) -> String {
    match secs {
        0 => "less than a minute".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "0123456789abcdef0123456789abcdef";
    const NEW: &str = "fedcba9876543210fedcba9876543210";

    /// The way out of a wrong ID entered at setup.
    #[test]
    fn a_different_id_replaces_the_configured_one() {
        assert!(replaces_configured(Some(NEW), Some(OLD)));
    }

    /// Same app, so the stored sign-in is still good and nothing is rewritten.
    #[test]
    fn the_same_id_is_not_a_replacement_however_it_was_typed() {
        assert!(!replaces_configured(Some(OLD), Some(OLD)));
        assert!(!replaces_configured(Some(OLD), Some("  \"0123456789ABCDEF0123456789ABCDEF\" ")));
    }

    #[test]
    fn nothing_given_or_nothing_configured_is_not_a_replacement() {
        assert!(!replaces_configured(None, Some(OLD)));
        assert!(!replaces_configured(Some(NEW), None));
    }

    /// A configured value that could never work is replaced too.
    #[test]
    fn a_malformed_configured_id_is_replaced() {
        assert!(replaces_configured(Some(NEW), Some("not-a-client-id")));
    }
}
