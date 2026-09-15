//! Shared core for the `boombox` CLI, TUI and daemon.
//!
//! Everything that talks to Spotify lives here so the three front ends stay
//! thin and cannot drift apart.

pub mod api;
pub mod auth;
pub mod build_info;
pub mod config;
pub mod error;
pub mod oembed;
pub mod private;
pub mod recent;
pub mod uri;

pub use api::{Client, LibraryApi, PlayerApi};
pub use auth::Auth;
pub use config::Config;
pub use error::{Error, ExitCode, Result};
