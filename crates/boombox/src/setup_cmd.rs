//! `boombox setup`: from nothing to playing, without editing a config file.
//!
//! Spotify only lets a third-party player use an app the user registers
//! themselves, so a first run cannot simply work. Before this, a new user met
//! a numbered list, a config file to edit by hand and a second command; a
//! bare `boombox` was worse, sitting on a blank screen for twelve seconds and
//! then exiting with an error.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::sync::Arc;

use anyhow::{Result, bail};
use boombox_core::auth::{ClientCheck, TokenStore, check_client_id};
use boombox_core::{Client, Config};

const DASHBOARD: &str = "https://developer.spotify.com/dashboard";
const TICK: &str = "\u{2713}";

/// What is already done.
pub struct State {
    pub client_id: Option<String>,
    pub signed_in: bool,
}

impl State {
    pub fn read(config: &Config) -> Self {
        Self {
            client_id: config.resolve_client_id().ok(),
            signed_in: TokenStore::from_config(config).load().ok().flatten().is_some(),
        }
    }

    /// Enough to open the player. Playing audio here is optional, so setup
    /// offers it but nothing requires it.
    pub fn is_complete(&self) -> bool {
        self.client_id.is_some() && self.signed_in
    }

    /// The error for a command that cannot run until setup has, keeping the
    /// exit code scripts already rely on.
    pub fn missing(&self) -> boombox_core::Error {
        if self.client_id.is_none() {
            boombox_core::Error::NoClientId
        } else {
            boombox_core::Error::NotAuthenticated
        }
    }
}

/// Whether someone is at a terminal to answer questions.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// `boombox setup`.
pub async fn run() -> Result<()> {
    if !interactive() {
        bail!(
            "`boombox setup` asks questions, so it needs a terminal. In a script, use \
             `boombox auth login --client-id <id>`, which saves the ID for later commands"
        );
    }
    walk_through().await?;
    println!();
    println!("All set. Run `boombox` to open the player.");
    Ok(())
}

/// Setup on the way into the player, for the first run of a bare `boombox`.
pub async fn before_player() -> Result<()> {
    println!("boombox is not set up yet, so that comes first.");
    walk_through().await?;
    println!();
    println!("All set. Opening the player...");
    Ok(())
}

async fn walk_through() -> Result<()> {
    Config::ensure_exists()?;
    let config = Config::load()?;
    let steps = if cfg!(feature = "streaming") { 3 } else { 2 };

    heading(1, steps, "Your Spotify app");
    let configured = config.resolve_client_id().ok();
    let client_id = match configured.as_deref().map(boombox_core::config::normalise_client_id) {
        Some(Some(id)) => match check_client_id(&id).await {
            ClientCheck::Unknown => {
                println!(
                    "Spotify does not recognise the client ID in your config (ending {}), so",
                    last_four(&id)
                );
                println!("it needs replacing. Often it is the Client Secret, pasted in its place.");
                println!();
                ask_for_app(config.redirect_port).await?
            }
            check => {
                println!("{TICK} Using your app, client ID ending {}", last_four(&id));
                if check == ClientCheck::Unchecked {
                    println!(
                        "  Not the right one? `boombox auth login --client-id <id>` replaces it."
                    );
                }
                id
            }
        },
        // Filled in, but with nothing that can work. Better said now than
        // left for sign-in to fail with Spotify's less helpful version.
        Some(None) => {
            println!("The client ID in your config does not look like one, so it needs replacing.");
            println!();
            ask_for_app(config.redirect_port).await?
        }
        None => ask_for_app(config.redirect_port).await?,
    };
    let config = Config::load()?;

    heading(2, steps, "Signing in");
    sign_in_if_needed(&config, client_id).await?;

    #[cfg(feature = "streaming")]
    {
        heading(3, steps, "Playing audio on this computer");
        audio(&config).await?;
    }
    Ok(())
}

/// Walks through registering the app, and saves the Client ID it produces.
pub async fn ask_for_app(redirect_port: u16) -> Result<String> {
    println!("Spotify only lets a third-party player use an app you register yourself.");
    println!("It takes a couple of minutes, and the app's owner needs Spotify Premium.");
    println!();
    println!("  1. Open the dashboard and create an app:");
    println!("       {DASHBOARD}");
    println!("  2. Give it any name and description.");
    println!("  3. Add this Redirect URI, exactly as written:");
    println!("       {}", redirect_uri(redirect_port));
    println!("  4. If it asks which APIs you will use, tick Web API.");
    println!("  5. Accept the terms, and create the app.");
    println!("  6. In the app's settings, open User Management and add the");
    println!("     Spotify account you will use with boombox.");
    println!("  7. Copy the Client ID from the app's settings.");
    println!();
    if confirm("Open the dashboard in your browser now?", true)?
        && open::that_detached(DASHBOARD).is_err()
    {
        println!("Could not open a browser. The address is in step 1.");
    }
    println!();
    // The last ID Spotify turned down, so the same one pasted again can be
    // kept anyway: the check should never be the only thing in the way.
    let mut rejected: Option<String> = None;
    let client_id = loop {
        let answer = ask("Paste the Client ID (not the Client Secret): ")?;
        if answer.is_empty() {
            continue;
        }
        let Some(id) = boombox_core::config::normalise_client_id(&answer) else {
            println!(
                "That is not a Client ID: those are 32 letters and digits, shown in the \
                 app's settings. Try again, or press Ctrl-C to stop."
            );
            continue;
        };
        match check_client_id(&id).await {
            ClientCheck::Known => break id,
            ClientCheck::Unchecked => {
                println!("Could not reach Spotify to check it, so carrying on with it.");
                break id;
            }
            ClientCheck::Unknown if rejected.as_deref() == Some(id.as_str()) => {
                if confirm("Spotify still does not recognise it. Use it anyway?", false)? {
                    break id;
                }
            }
            ClientCheck::Unknown => {
                println!(
                    "Spotify does not recognise that Client ID. Check it is the Client ID \
                     from the app's settings, not the Client Secret, which looks just like it."
                );
                rejected = Some(id);
            }
        }
    };
    let path = boombox_core::config::save_client_id(&client_id)?;
    println!("{TICK} Saved to {}", path.display());
    Ok(client_id)
}

async fn sign_in_if_needed(config: &Config, client_id: String) -> Result<()> {
    let store = TokenStore::from_config(config);
    if let Some(tokens) = store.load()? {
        let auth = Arc::new(boombox_core::Auth::with_store(client_id.clone(), Some(tokens), store));
        match Client::new(auth).current_user().await {
            Ok(user) => {
                println!("{TICK} Signed in as {}", user.label());
                return Ok(());
            }
            Err(boombox_core::Error::Api { status: 403, .. }) => return Err(refused()),
            Err(e) => {
                println!("The saved sign-in no longer works ({e}), so signing in again.");
                println!();
            }
        }
    }
    println!("Your browser will open at Spotify. Approve boombox there, and this carries");
    println!("on by itself. If Spotify says the redirect URI is invalid, the one in your");
    println!("app's settings must be exactly {}", redirect_uri(config.redirect_port));
    println!("If it says the client_id is invalid, the ID is wrong -- often the Client");
    println!("Secret pasted by mistake. Press Ctrl-C, then run");
    println!("`boombox auth login --client-id <id>` with the Client ID from the app's settings.");
    println!();
    let signed = crate::auth_cmd::sign_in(config, client_id, None, false).await?;
    println!("{TICK} Signed in as {}", signed.label);
    Ok(())
}

#[cfg(feature = "streaming")]
async fn audio(config: &Config) -> Result<()> {
    let authorized = crate::streaming::authorized();
    let name = &config.streaming.device_name;
    if authorized && config.streaming.enabled {
        println!("{TICK} boombox plays audio here, as the device \"{name}\"");
        return Ok(());
    }
    println!("boombox can play music itself. The daemon registers with Spotify as a");
    println!("device called \"{name}\", which appears in the picker on your phone, in the");
    println!("desktop app and on the web player. What that means:");
    println!();
    println!("  \u{b7} Nothing moves to it until you pick it.");
    println!("  \u{b7} Spotify allows one stream per account, so when you do pick it,");
    println!("    playback stops wherever else it was.");
    println!("  \u{b7} The sound comes out of this machine, so it has to be awake.");
    println!("  \u{b7} It needs one more sign-in, under Spotify's own login rather than");
    println!("    your app. No Client ID to copy: your browser opens once more.");
    println!();
    if !confirm("Register this machine as a Spotify device?", true)? {
        let path = boombox_core::config::save_streaming_enabled(false)?;
        println!("Skipped, and turned off in {}.", path.display());
        println!("`boombox auth login --streaming` turns it on whenever you want it.");
        return Ok(());
    }
    if !authorized {
        println!();
        println!("Spotify only streams through its own login, not through your app.");
        crate::streaming::authorize(config).await?;
    }
    let path = boombox_core::config::save_streaming_enabled(true)?;
    println!("{TICK} Turned on in {}", path.display());
    if crate::daemon::daemon_status(config).await.is_some() {
        println!("A daemon is already running with the old settings. `boombox daemon --stop`,");
        println!("then `boombox`, starts one that plays here.");
    }
    Ok(())
}

/// The error for an account Spotify signs in and then refuses.
///
/// In Development Mode Spotify will often complete the sign-in for an account
/// the app does not allow, then answer every request with 403 -- which, left
/// unexplained, reads as boombox being broken.
pub fn refused() -> anyhow::Error {
    anyhow::anyhow!(
        "Spotify signed you in, then refused the account (403).\n\n\
         An app in Development Mode only works for accounts added under User Management \
         in its settings, and the app's owner needs Spotify Premium. Add the account you \
         signed in with, then run `boombox setup` again."
    )
}

fn heading(step: usize, of: usize, title: &str) {
    println!();
    println!("Step {step} of {of} \u{b7} {title}");
    println!();
}

/// The redirect URI to register. Port 0 asks the OS for one at sign-in, and
/// Spotify accepts a loopback address registered without a port for exactly
/// that case.
fn redirect_uri(port: u16) -> String {
    if port == 0 {
        "http://127.0.0.1/callback".to_string()
    } else {
        format!("http://127.0.0.1:{port}/callback")
    }
}

/// Enough of a client ID to recognise it, from one already normalised.
fn last_four(id: &str) -> &str {
    &id[id.len().saturating_sub(4)..]
}

fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!(
            "setup stopped before it finished. Run `boombox setup` to carry on where it left off"
        );
    }
    Ok(line.trim().to_string())
}

fn confirm(question: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        match parse_answer(&ask(&format!("{question} {hint} "))?, default) {
            Some(answer) => return Ok(answer),
            None => println!("Please answer y or n."),
        }
    }
}

fn parse_answer(answer: &str, default: bool) -> Option<bool> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_takes_the_default_and_anything_unclear_is_asked_again() {
        assert_eq!(parse_answer("", true), Some(true));
        assert_eq!(parse_answer("", false), Some(false));
        assert_eq!(parse_answer(" Yes ", false), Some(true));
        assert_eq!(parse_answer("N", true), Some(false));
        assert_eq!(parse_answer("maybe", true), None);
    }

    /// Spotify matches the redirect URI exactly, so the one setup tells people
    /// to register has to be exact too.
    #[test]
    fn the_redirect_uri_to_register_follows_the_port() {
        assert_eq!(redirect_uri(8888), "http://127.0.0.1:8888/callback");
        assert_eq!(redirect_uri(0), "http://127.0.0.1/callback", "port 0 registers without one");
    }

    #[test]
    fn what_is_missing_keeps_its_exit_code() {
        let no_app = State { client_id: None, signed_in: false };
        assert!(matches!(no_app.missing(), boombox_core::Error::NoClientId));
        let no_sign_in = State { client_id: Some("x".into()), signed_in: false };
        assert!(matches!(no_sign_in.missing(), boombox_core::Error::NotAuthenticated));
        assert!(!no_sign_in.is_complete());
        assert!(State { client_id: Some("x".into()), signed_in: true }.is_complete());
    }
}
