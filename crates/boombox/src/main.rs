mod auth_cmd;
mod config_cmd;
mod daemon;
mod fmt;
mod library_cmd;
mod player_cmd;
mod recents;
mod setup_cmd;
#[cfg(feature = "streaming")]
mod spectrum;
#[cfg(feature = "streaming")]
mod stall;
#[cfg(feature = "streaming")]
mod streaming;
mod tui_cmd;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use library_cmd::{LibraryCommand, PlaylistCommand, SearchArgs};
use player_cmd::{NowArgs, PlayerCommand, QueueCommand};

#[derive(Parser)]
#[command(
    name = "boombox",
    // The commit matters more than the version here: the workspace version
    // rarely moves, so `0.1.0` alone cannot answer "did my rebuild land?".
    version = boombox_core::build_info::long_static(),
    about = "A Spotify client for the terminal",
    long_about = "A Spotify client for the terminal: a TUI and a scriptable CLI \
                  over the same core.\n\n\
                  Spotify requires you to register your own app; `boombox setup` walks \
                  you through it."
)]
struct Cli {
    /// Bypass a running daemon and talk to the Spotify API directly
    #[arg(long, global = true)]
    direct: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Skip to the next track
    Next,
    /// Go back to the previous track
    #[command(alias = "prev")]
    Previous,
    /// Pause playback
    Pause,
    /// Resume, or start a Spotify URI
    Play {
        /// spotify:track:… , spotify:album:… , spotify:playlist:…
        uri: Option<String>,
    },
    /// Pause if playing, resume if paused
    Toggle,
    /// Show what is playing
    Now(NowArgs),
    /// Seek: 1:45, 90, +30, -10
    Seek { position: String },
    /// Get or set volume: 60, +10, -5
    #[command(alias = "volume")]
    Vol {
        /// Omit to print the current volume
        level: Option<String>,
    },
    /// Shuffle: on, off, toggle (default)
    Shuffle { mode: Option<String> },
    /// Repeat: off, track, context, cycle (default)
    Repeat { mode: Option<String> },
    /// List Spotify Connect devices
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Move playback to another device
    Connect {
        /// Device name, prefix, or id
        name: String,
    },
    /// Search Spotify
    Search(SearchArgs),
    /// Your playlists
    #[command(subcommand)]
    Playlist(PlaylistCommand),
    /// Your saved tracks
    Liked {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Your saved albums
    Albums {
        #[arg(long)]
        json: bool,
    },
    /// Save a track to your library (defaults to what is playing)
    Like { uri: Option<String> },
    /// Remove a track from your library (defaults to what is playing)
    Unlike { uri: Option<String> },

    /// Inspect or append to the queue
    #[command(subcommand)]
    Queue(QueueCommand),

    /// Open the terminal UI (also the default with no subcommand)
    Tui,

    /// Run the background daemon that caches player state
    Daemon {
        /// Report on a running daemon instead of starting one
        #[arg(long, conflicts_with = "stop")]
        status: bool,
        /// Ask a running daemon to shut down
        #[arg(long)]
        stop: bool,
    },

    /// Set boombox up: your Spotify app, signing in, and audio. Safe to run again
    Setup,
    /// Log in, check the session, or sign out
    #[command(subcommand)]
    Auth(auth_cmd::AuthCommand),
    /// Send a raw request to the Spotify Web API (debugging)
    Api {
        /// Path under /v1, e.g. /me/tracks?limit=1
        path: String,
        /// HTTP method
        #[arg(long, default_value = "GET")]
        method: String,
    },

    /// Inspect or create the config file
    #[command(subcommand)]
    Config(config_cmd::ConfigCommand),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(parse_failure_code(&err));
        }
    };

    // The TUI owns the screen, so it logs to a file and installs its own
    // subscriber. Everything else logs to stderr.
    if !matches!(cli.command, None | Some(Command::Tui)) {
        let builder = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_env("BOOMBOX_LOG")
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)),
            )
            .with_writer(std::io::stderr);
        if logs_over_time(&cli.command) {
            builder.init();
        } else {
            builder.without_time().init();
        }
    }

    // Every run tightens the state directory, so an install made before this
    // existed is fixed the first time a new binary runs, whatever the command.
    // Best effort: an unusual or read-only state directory should not stop
    // `boombox now` from working.
    match boombox_core::private::secure_state_dir() {
        Ok(restricted) => {
            for path in restricted {
                tracing::info!("restricted {} to its owner", path.display());
            }
        }
        Err(e) => tracing::warn!("could not restrict the state directory: {e}"),
    }

    let result = match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tui_cmd::run(cli.direct).await,
        Command::Auth(cmd) => cmd.run().await,
        Command::Setup => setup_cmd::run().await,
        Command::Config(cmd) => cmd.run(),
        Command::Daemon { status, stop } => run_daemon(status, stop).await,
        Command::Api { path, method } => raw_api(&method, &path).await,
        Command::Search(args) => library_cmd::run(LibraryCommand::Search(args), cli.direct).await,
        Command::Playlist(sub) => library_cmd::run(LibraryCommand::Playlist(sub), cli.direct).await,
        Command::Liked { limit, json } => {
            library_cmd::run(LibraryCommand::Liked { limit, json }, cli.direct).await
        }
        Command::Albums { json } => {
            library_cmd::run(LibraryCommand::Albums { json }, cli.direct).await
        }
        Command::Like { uri } => library_cmd::run(LibraryCommand::Like { uri }, cli.direct).await,
        Command::Unlike { uri } => {
            library_cmd::run(LibraryCommand::Unlike { uri }, cli.direct).await
        }
        other => player_cmd::run(into_player_command(other), cli.direct).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `:#` prints the whole context chain; a bare `{err}` hides the cause.
            eprintln!("boombox: {err:#}");
            // Preserve the documented exit codes for anything originating in
            // the core; anything else is a generic failure.
            let code =
                err.downcast_ref::<boombox_core::Error>().map(|e| e.exit_code() as u8).unwrap_or(1);
            ExitCode::from(code)
        }
    }
}

async fn raw_api(method: &str, path: &str) -> anyhow::Result<()> {
    let config = boombox_core::Config::load()?;
    let auth = std::sync::Arc::new(boombox_core::Auth::from_config(&config)?);
    let (status, body) = boombox_core::Client::new(auth).raw(method, path).await?;
    eprintln!("HTTP {status}");
    println!("{body}");
    Ok(())
}

async fn run_daemon(status: bool, stop: bool) -> anyhow::Result<()> {
    let config = boombox_core::Config::load()?;
    if status {
        daemon::status(&config).await
    } else if stop {
        daemon::stop(&config).await
    } else {
        daemon::run(&config).await
    }
}

fn into_player_command(command: Command) -> PlayerCommand {
    match command {
        Command::Next => PlayerCommand::Next,
        Command::Previous => PlayerCommand::Previous,
        Command::Pause => PlayerCommand::Pause,
        Command::Play { uri } => PlayerCommand::Play { uri },
        Command::Toggle => PlayerCommand::Toggle,
        Command::Now(args) => PlayerCommand::Now(args),
        Command::Seek { position } => PlayerCommand::Seek { position },
        Command::Vol { level } => PlayerCommand::Volume { level },
        Command::Shuffle { mode } => PlayerCommand::Shuffle { mode },
        Command::Repeat { mode } => PlayerCommand::Repeat { mode },
        Command::Devices { json } => PlayerCommand::Devices { json },
        Command::Connect { name } => PlayerCommand::Connect { name },
        Command::Queue(q) => PlayerCommand::Queue(q),
        Command::Auth(_)
        | Command::Setup
        | Command::Config(_)
        | Command::Daemon { .. }
        | Command::Api { .. }
        | Command::Search(_)
        | Command::Playlist(_)
        | Command::Liked { .. }
        | Command::Albums { .. }
        | Command::Like { .. }
        | Command::Unlike { .. }
        | Command::Tui => {
            unreachable!("handled before dispatch")
        }
    }
}

/// Warnings from everything, our own information, and nothing from the
/// audio decoders.
///
/// One track that librespot could not decrypt had symphonia write 1,752
/// lines about junk bytes and bad frame headers in ten minutes -- every one
/// of them about audio nothing could be done with, and none of them
/// actionable. Their errors still come through, and BOOMBOX_LOG overrides
/// all of it.
///
/// Our own crates sit at info because a daemon's log is read afterwards,
/// to answer what happened: at plain `warn` it recorded a streaming
/// connection dropping and then never said whether it came back. There are
/// a handful of these lines and almost all are one-shot, so the log stays
/// readable. The directive matches by prefix, so it covers `boombox_core`
/// and the rest alongside `boombox` itself.
const DEFAULT_LOG_FILTER: &str = "warn,boombox=info,\
     symphonia=error,symphonia_bundle_flac=error,symphonia_bundle_mp3=error,\
     symphonia_codec_vorbis=error,symphonia_core=error,symphonia_format_ogg=error,\
     symphonia_metadata=error,symphonia_utils_xiph=error";

/// Whether this command's log will be read later rather than watched.
///
/// Only the daemon's is: it writes to a file for hours, and the question
/// asked of that file afterwards is always when something happened. A
/// timestamp on every line of `boombox now` would be noise in front of
/// output the user is already watching arrive.
fn logs_over_time(command: &Option<Command>) -> bool {
    matches!(command, Some(Command::Daemon { status: false, stop: false }))
}

/// The exit code for arguments that did not parse.
///
/// clap exits with 2 by default, which is the code documented for "not signed
/// in": a script could not tell a mistyped flag from a missing login. Help and
/// version are not failures at all.
fn parse_failure_code(err: &clap::Error) -> u8 {
    if err.use_stderr() { boombox_core::ExitCode::Usage as u8 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filter that does not parse is not a compile error: it would fall
    /// back to a bare "warn" at runtime and the spam would quietly return.
    #[test]
    fn the_default_log_filter_parses_and_quiets_the_decoders() {
        let filter: tracing_subscriber::EnvFilter =
            DEFAULT_LOG_FILTER.parse().expect("the default filter must parse");
        let rendered = filter.to_string();
        assert!(rendered.contains("symphonia_bundle_mp3=error"), "{rendered}");
        assert!(rendered.contains("symphonia_core=error"), "{rendered}");
        // What a reconnect reports is only in the log if this survives.
        assert!(rendered.contains("boombox=info"), "{rendered}");
    }

    /// Diagnosing a daemon means placing events in time; a crash report of
    /// "it stopped at 11:20" cannot be matched to a log that says only what
    /// happened, not when.
    #[test]
    fn only_a_running_daemon_stamps_its_log_with_the_time() {
        assert!(logs_over_time(&Some(Command::Daemon { status: false, stop: false })));
        assert!(!logs_over_time(&Some(Command::Daemon { status: true, stop: false })));
        assert!(!logs_over_time(&Some(Command::Daemon { status: false, stop: true })));
        assert!(!logs_over_time(&Some(Command::Setup)));
        assert!(!logs_over_time(&None), "the TUI installs its own subscriber");
    }

    /// Documented as 1, and distinct from 2, which means "not signed in".
    #[test]
    fn a_usage_error_exits_with_the_documented_code() {
        for args in [["boombox", "--no-such-flag"], ["boombox", "nxt"]] {
            let err = Cli::try_parse_from(args).err().expect("should not parse");
            assert_eq!(parse_failure_code(&err), 1, "{args:?}");
        }
    }

    #[test]
    fn help_and_version_are_not_failures() {
        for flag in ["--help", "--version"] {
            let err =
                Cli::try_parse_from(["boombox", flag]).err().expect("clap reports these as errors");
            assert_eq!(parse_failure_code(&err), 0, "{flag}");
        }
    }
}
