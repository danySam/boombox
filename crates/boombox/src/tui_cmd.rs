use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use boombox_core::{Client, Config};

/// A daemon read is a local socket round trip, so the TUI can poll often
/// enough to feel live. Going direct, the same rate would exhaust the
/// Development Mode rate limit within a minute.
const POLL_WITH_DAEMON: Duration = Duration::from_millis(250);
const POLL_DIRECT: Duration = Duration::from_millis(1000);

pub async fn run(direct: bool) -> Result<()> {
    let t_entry = std::time::Instant::now();
    let mut config = Config::load()?;
    init_file_logging(&config)?;

    // Before anything starts a daemon. A first run used to launch one that
    // could not work without a client ID, wait twelve seconds on a blank
    // screen for it to answer, and then exit with an error.
    let setup = crate::setup_cmd::State::read(&config);
    if !setup.is_complete() {
        if !crate::setup_cmd::interactive() {
            return Err(setup.missing().into());
        }
        crate::setup_cmd::before_player().await?;
        config = Config::load()?;
    }

    let seek_step_secs = config.ui.seek_step;
    let graphics = config.ui.graphics.clone();

    tracing::info!(version = boombox_core::build_info::long(), "tui starting");
    tracing::debug!(ms = t_entry.elapsed().as_millis(), "phase: config+logging");

    if !direct {
        let t0 = std::time::Instant::now();
        let (client, startup) = crate::daemon::ensure_running(&config).await;
        tracing::debug!(ms = t0.elapsed().as_millis(), "phase: ensure_running");
        if let Some(ipc) = client {
            let t1 = std::time::Instant::now();
            let status = crate::daemon::daemon_status(&config).await;
            tracing::debug!(ms = t1.elapsed().as_millis(), "phase: daemon_status");
            let mismatch = status.as_ref().and_then(|s| s.mismatch_warning());
            // Taken before the version moves out of `status` below.
            let our_device_id = status.as_ref().and_then(|s| s.device_id.clone());

            // Adopting is worth reporting: it changes where sound comes
            // from, which the user did not explicitly ask for.
            // Kept at debug: this is the number to look at if the TUI ever
            // feels slow to appear again.
            tracing::debug!(ms = t0.elapsed().as_millis(), "phase: ready to paint");
            let notice = startup_notice(&startup);

            tracing::info!(
                daemon = status.as_ref().map(|s| s.version.as_str()).unwrap_or("unknown"),
                startup = ?startup,
                "tui: using the daemon"
            );
            return boombox_tui::run(
                Arc::new(ipc),
                boombox_tui::Options {
                    poll: POLL_WITH_DAEMON,
                    seek_step_secs,
                    connected_to_daemon: true,
                    daemon_version: status.map(|s| s.version).filter(|v| !v.is_empty()),
                    our_device_id,
                    daemon_warning: mismatch,
                    daemon_notice: notice,
                    graphics: graphics.clone(),
                },
            )
            .await;
        }
        if let crate::daemon::Startup::Unavailable(why) = &startup {
            tracing::warn!("no daemon: {why}");
        }
    }

    tracing::info!("tui: talking to the Web API directly");
    let auth = Arc::new(boombox_core::Auth::from_config(&config)?);
    let client = Client::new(auth);
    boombox_tui::run(
        Arc::new(client),
        boombox_tui::Options {
            poll: POLL_DIRECT,
            seek_step_secs,
            connected_to_daemon: false,
            daemon_version: None,
            // Without a daemon there is no device of ours to point at.
            our_device_id: None,
            daemon_warning: None,
            daemon_notice: None,
            graphics,
        },
    )
    .await
}

/// What to tell the user about how the daemon got there. `None` for the
/// ordinary case of joining one that was already running -- that needs no
/// announcement.
fn startup_notice(startup: &crate::daemon::Startup) -> Option<String> {
    use crate::daemon::Startup;
    match startup {
        Startup::Joined => None,
        Startup::Started => Some("started the daemon".to_string()),
        Startup::Replaced => {
            Some("replaced a daemon that spoke an older protocol; playback stopped".to_string())
        }
        Startup::Unstuck(pid) => {
            Some(format!("the daemon (pid {pid}) had stopped answering, so it was replaced"))
        }
        Startup::Unavailable(_) => None,
    }
}

/// Anything written to stderr would land on top of the rendered UI, so the
/// TUI logs to a file instead. `BOOMBOX_LOG` still selects the level.
fn init_file_logging(_config: &Config) -> Result<()> {
    let dir = boombox_core::config::state_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("boombox.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot open log file {}", path.display()))?;

    let filter = tracing_subscriber::EnvFilter::try_from_env("BOOMBOX_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    // The CLI already installed a stderr subscriber; replacing it is not
    // possible, so only set one if we got here first.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Startup;

    /// Joining an existing daemon is the common case and says nothing --
    /// a toast on every launch would be noise.
    #[test]
    fn joining_a_running_daemon_is_silent() {
        assert_eq!(startup_notice(&Startup::Joined), None);
    }

    #[test]
    fn starting_one_is_worth_a_word() {
        let notice = startup_notice(&Startup::Started).unwrap();
        assert!(notice.contains("started the daemon"), "{notice}");
    }

    /// The one case that costs the user something. It must say so, because
    /// silence would leave them wondering why the music stopped.
    #[test]
    fn replacing_one_admits_that_playback_stopped() {
        let notice = startup_notice(&Startup::Replaced).unwrap();
        assert!(notice.contains("playback stopped"), "{notice}");
        assert!(notice.contains("older protocol"), "{notice}");
    }

    /// Ending a process is the most drastic thing startup does, so the
    /// notice names the process and why.
    #[test]
    fn replacing_a_stuck_one_names_it_and_the_reason() {
        let notice = startup_notice(&Startup::Unstuck(4242)).unwrap();
        assert!(notice.contains("4242"), "{notice}");
        assert!(notice.contains("stopped answering"), "{notice}");
    }

    /// Adopting changes where sound comes from without being asked, so it
    /// is reported even when nothing else happened.
    /// Falling back to the Web API is logged, not toasted: the TUI still
    /// works, and the status dot already shows there is no daemon.
    #[test]
    fn an_unavailable_daemon_does_not_raise_a_toast() {
        assert_eq!(startup_notice(&Startup::Unavailable("nope".into())), None);
    }
}
