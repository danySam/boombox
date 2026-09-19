use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// XDG-style paths on every platform, including macOS. `directories` would
/// hand us `~/Library/Application Support` there, which is wrong for a tool
/// people edit by hand from a shell.
fn base_dir(env_override: &str, xdg_var: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(env_override) {
        return Ok(PathBuf::from(dir));
    }
    let root = match std::env::var_os(xdg_var) {
        Some(v) => PathBuf::from(v),
        None => {
            let base = directories::BaseDirs::new()
                .ok_or_else(|| Error::Config("cannot determine home directory".into()))?;
            base.home_dir().join(fallback)
        }
    };
    Ok(root.join("boombox"))
}

pub fn config_dir() -> Result<PathBuf> {
    base_dir("BOOMBOX_CONFIG_DIR", "XDG_CONFIG_HOME", ".config")
}

pub fn state_dir() -> Result<PathBuf> {
    base_dir("BOOMBOX_STATE_DIR", "XDG_STATE_HOME", ".local/state")
}

pub fn cache_dir() -> Result<PathBuf> {
    base_dir("BOOMBOX_CACHE_DIR", "XDG_CACHE_HOME", ".cache")
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Your own Spotify app's client ID. `boombox setup` fills it in; the app's
    /// owner needs Spotify Premium.
    pub client_id: Option<String>,

    /// Port for the OAuth loopback redirect. 0 asks the OS for a free one,
    /// which Spotify permits for loopback literals only.
    pub redirect_port: u16,

    pub ui: UiConfig,
    pub daemon: DaemonConfig,
    pub auth: AuthConfig,
    pub streaming: StreamingConfig,
}

/// Only consulted in builds compiled with `--features streaming`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// Advertise this machine as a Spotify Connect device.
    ///
    /// On by default in a build that has streaming compiled in, because
    /// appearing in the device list costs the user nothing: playback moves
    /// here only when they pick it. It still needs the separate streaming
    /// sign-in, without which the daemon says so once and leaves it alone.
    pub enabled: bool,
    /// The name shown in Spotify's device picker.
    ///
    /// Unset means one built from this machine's own name, so two computers
    /// running boombox do not both appear as "boombox" -- which the person
    /// choosing cannot tell apart, and nor could boombox itself.
    pub device_name: Option<String>,
    /// 96, 160 or 320 kbps.
    pub bitrate: u16,
    /// 0-100. Spotify's wire format is 0-65535; the conversion is internal.
    pub initial_volume: u8,
    /// Audio backend name, e.g. "rodio". None picks librespot's default.
    pub backend: Option<String>,
    /// Even out loudness between tracks.
    pub normalisation: bool,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device_name: None,
            bitrate: 320,
            initial_volume: 50,
            backend: None,
            normalisation: false,
        }
    }
}

impl StreamingConfig {
    /// librespot takes volume as a u16 across the full range.
    pub fn initial_volume_u16(&self) -> u16 {
        (u32::from(self.initial_volume.min(100)) * u32::from(u16::MAX) / 100) as u16
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Store tokens in the OS keychain instead of an owner-only file.
    ///
    /// Off by default. On macOS the keychain recognises a program by its code
    /// signature, and a binary built from source -- by you, or by Homebrew --
    /// is a new program after every build or upgrade, so it asks for
    /// permission again each time. A dialog raised by the background daemon
    /// has nobody to answer it, and blocks every other boombox command while it
    /// waits. The file sits in a directory only you can read.
    ///
    /// False by default, deliberately -- the derived `Default` is the choice.
    pub keyring: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub theme: String,
    pub cover_art: bool,
    pub seek_step: u32,
    /// Percentage points per volume key. Presses accumulate, so a small
    /// step is not the obstacle it once was.
    pub volume_step: u32,
    /// How to draw album art: `auto`, `off`, or `kitty`.
    ///
    /// Named rather than probed. Asking a terminal what it supports means
    /// reading its reply, and a stray reply lands in the key handler --
    /// which is exactly how an earlier attempt at this broke every
    /// keypress. `auto` reads the environment and never writes a query.
    pub graphics: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub socket: Option<PathBuf>,
    pub poll_active_ms: u64,
    pub poll_idle_ms: u64,
    /// Whether the TUI may start a daemon when none is running. Off is for
    /// people who run one under launchd and do not want a stray second one.
    #[serde(default = "yes")]
    pub autostart: bool,
    /// Whether to make this machine the active device on startup when
    /// nothing else is playing. Never takes playback away from a device
    /// that is already going.
    ///
    /// Off by default. Registering a device is not a reason to move
    /// playback to it: boombox waits to be picked.
    #[serde(default = "no")]
    pub adopt_playback: bool,
}

fn yes() -> bool {
    true
}

fn no() -> bool {
    false
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: None,
            redirect_port: 8888,
            ui: UiConfig::default(),
            daemon: DaemonConfig::default(),
            auth: AuthConfig::default(),
            streaming: StreamingConfig::default(),
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "auto".into(),
            cover_art: true,
            seek_step: 5,
            volume_step: 5,
            graphics: "auto".into(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: None,
            poll_active_ms: 1000,
            poll_idle_ms: 15_000,
            autostart: true,
            adopt_playback: false,
        }
    }
}

/// A pasted Client ID, cleaned up, or `None` if it cannot be one.
///
/// Spotify's are 32 hexadecimal characters. People paste them with stray
/// spaces, or with the quotes from a config line, so those are forgiven.
/// Anything else is refused here rather than saved and found broken at
/// sign-in. A Client Secret has exactly the same shape, so whatever asks for
/// the ID has to name the field.
pub fn normalise_client_id(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    (trimmed.len() == 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| trimmed.to_ascii_lowercase())
}

/// Saves the client ID into the config file, creating the file from the
/// template if there is none. Returns the path written.
pub fn save_client_id(client_id: &str) -> Result<PathBuf> {
    let path = config_path()?;
    save_client_id_at(&path, client_id)?;
    Ok(path)
}

pub fn save_client_id_at(path: &Path, client_id: &str) -> Result<()> {
    edit_at(path, |doc| {
        doc["client_id"] = toml_edit::value(client_id);
        Ok(())
    })
}

/// Turns the Connect device on or off in the config file. Returns the path.
pub fn save_streaming_enabled(enabled: bool) -> Result<PathBuf> {
    let path = config_path()?;
    save_streaming_enabled_at(&path, enabled)?;
    Ok(path)
}

pub fn save_streaming_enabled_at(path: &Path, enabled: bool) -> Result<()> {
    edit_at(path, |doc| {
        match doc.get("streaming").map(toml_edit::Item::is_table_like) {
            None => doc["streaming"] = toml_edit::table(),
            Some(true) => {}
            Some(false) => return Err(Error::Config("`streaming` is not a table".into())),
        }
        doc["streaming"]["enabled"] = toml_edit::value(enabled);
        Ok(())
    })
}

/// Changes the config file in place, keeping its comments and layout. It is
/// a file people edit by hand, and rewriting it from the parsed values would
/// throw away whatever notes they left in it.
fn edit_at(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<()> {
    let located = |e: &dyn std::fmt::Display| Error::Config(format!("{}: {e}", path.display()));
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG_TOML.to_string(),
        Err(e) => return Err(located(&e)),
    };
    let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| located(&e))?;
    change(&mut doc)?;
    let updated = doc.to_string();
    // Never write a file that boombox itself would then refuse to load.
    toml::from_str::<Config>(&updated).map_err(|e| located(&e))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, updated)?;
    Ok(())
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# boombox configuration
#
# Spotify only lets a third-party player use an app you register yourself.
# In Development Mode an app works for up to five Spotify accounts that you
# add by hand, and its owner needs an active Premium subscription.
#
# `boombox setup` walks through it and fills in client_id. By hand:
#
#   1. https://developer.spotify.com/dashboard -> Create app
#   2. Redirect URI: http://127.0.0.1:8888/callback
#      (the loopback literal -- "localhost" is rejected)
#   3. If asked which APIs you will use: Web API
#   4. In the app's settings, User Management: add your Spotify account
#   5. Copy the Client ID from the app's settings here, then `boombox auth login`

client_id = ""
redirect_port = 8888

[ui]
theme = "auto"
cover_art = true
seek_step = 5
volume_step = 5
# Album art. "auto" uses the kitty graphics protocol where the terminal
# reports itself as kitty, and falls back to block characters everywhere
# else. "off" forces the block characters; "kitty" forces the protocol.
graphics = "auto"

[daemon]
poll_active_ms = 1000
poll_idle_ms = 15000
# Start a daemon from the TUI when none is running. Turn this off if you run
# one under launchd and do not want a stray second one appearing.
autostart = true
# Make this machine the active Spotify device when the daemon registers one
# and nothing else is playing. Off by default: boombox appears in the device
# list, and playback moves here only when you pick it.
adopt_playback = false

[auth]
# Tokens go to an owner-only file in the state directory. Set this to true to
# keep them in the OS keychain instead. On macOS the keychain recognises boombox
# by its code signature, so it asks again after every build or upgrade.
keyring = false

# Only used by builds compiled with --features streaming, and needs one extra
# sign-in: `boombox auth login --streaming`, which `boombox setup` offers.
# Until that is done the daemon says so once at startup and carries on
# without a device.
#
# What being on gets you: the daemon appears in Spotify's device list as
# device_name, on every client signed in to your account, and can play the
# audio itself. Nothing moves to it until you pick it. Spotify allows one
# active stream per account, so when you do pick it, playback stops wherever
# else it was. The sound comes out of the machine running the daemon, which
# therefore has to be awake. Premium only.
[streaming]
enabled = true
# Shown in Spotify's device picker. Left out, it is "boombox on <this
# machine>", so two computers running boombox stay tellable apart.
# device_name = "boombox"
bitrate = 320
initial_volume = 50
normalisation = false
"#;

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                toml::from_str(&raw).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Config(format!("{}: {e}", path.display()))),
        }
    }

    /// Writes a commented starter config if none exists. Returns the path and
    /// whether it was newly created.
    pub fn ensure_exists() -> Result<(PathBuf, bool)> {
        let path = config_path()?;
        if path.exists() {
            return Ok((path, false));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, DEFAULT_CONFIG_TOML)?;
        Ok((path, true))
    }

    /// Config file first, then `SPOTIFY_CLIENT_ID` for people who would rather
    /// keep it in their shell profile.
    pub fn resolve_client_id(&self) -> Result<String> {
        self.client_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("SPOTIFY_CLIENT_ID").ok())
            .filter(|s| !s.trim().is_empty())
            .ok_or(Error::NoClientId)
    }

    pub fn socket_path(&self) -> Result<PathBuf> {
        match &self.daemon.socket {
            Some(p) => Ok(p.clone()),
            None => Ok(state_dir()?.join("boombox.sock")),
        }
    }
}

#[cfg(test)]
mod editing_tests {
    use super::*;

    /// A fresh config file must not change behaviour the moment it is
    /// written, so the template and the defaults have to agree.
    #[test]
    fn the_template_matches_the_defaults_it_documents() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("the template parses");
        let default = Config::default();
        assert_eq!(parsed.streaming.enabled, default.streaming.enabled);
        assert_eq!(parsed.daemon.adopt_playback, default.daemon.adopt_playback);
        assert_eq!(parsed.daemon.autostart, default.daemon.autostart);
        assert_eq!(parsed.streaming.device_name, default.streaming.device_name);
    }

    /// Appearing in the device list is free; taking playback is not.
    #[test]
    fn streaming_is_on_by_default_and_never_takes_playback_by_itself() {
        let config = Config::default();
        assert!(config.streaming.enabled, "the device should be offered");
        assert!(!config.daemon.adopt_playback, "but playback stays where it is");
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("boombox-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn a_pasted_client_id_is_forgiven_its_quotes_spaces_and_case() {
        assert_eq!(normalise_client_id(ID).as_deref(), Some(ID));
        assert_eq!(normalise_client_id(&format!("  \"{ID}\"\n")).as_deref(), Some(ID));
        assert_eq!(normalise_client_id(&ID.to_uppercase()).as_deref(), Some(ID));
    }

    #[test]
    fn anything_that_cannot_be_a_client_id_is_refused() {
        let too_long = format!("{ID}0");
        let not_hex = "0123456789abcdef0123456789abcdeg";
        for bad in
            ["", &ID[..31], too_long.as_str(), not_hex, "https://developer.spotify.com/dashboard"]
        {
            assert_eq!(normalise_client_id(bad), None, "{bad:?}");
        }
    }

    /// The template's guidance must survive the ID being filled in beneath it.
    #[test]
    fn saving_into_a_new_config_keeps_the_template_and_its_comments() {
        let path = scratch("new");
        save_client_id_at(&path, ID).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("`boombox setup` walks through it"), "{written}");
        assert!(written.contains(&format!("client_id = \"{ID}\"")), "{written}");
        assert_eq!(Config::load_from(&path).unwrap().client_id.as_deref(), Some(ID));
    }

    /// A file someone has edited by hand keeps their settings and notes.
    #[test]
    fn saving_into_an_edited_config_keeps_what_was_there() {
        let path = scratch("edited");
        std::fs::write(
            &path,
            "# my notes\n# client_id = \"\"\nredirect_port = 9999\n\n[ui]\n# darker\ntheme = \"dark\"\n",
        )
        .unwrap();
        save_client_id_at(&path, ID).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# my notes") && written.contains("# darker"), "{written}");
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.client_id.as_deref(), Some(ID));
        assert_eq!(config.redirect_port, 9999);
        assert_eq!(config.ui.theme, "dark");
    }

    #[test]
    fn saving_again_replaces_the_client_id_rather_than_adding_another() {
        let path = scratch("replace");
        save_client_id_at(&path, "ffffffffffffffffffffffffffffffff").unwrap();
        save_client_id_at(&path, ID).unwrap();
        assert_eq!(Config::load_from(&path).unwrap().client_id.as_deref(), Some(ID));
        assert_eq!(std::fs::read_to_string(&path).unwrap().matches("client_id =").count(), 1);
    }

    #[test]
    fn streaming_is_switched_in_place_or_given_a_table() {
        let from_template = scratch("streaming");
        // Off first: with streaming on by default, writing `false` is the
        // path setup takes when someone declines the device.
        save_streaming_enabled_at(&from_template, false).unwrap();
        let written = std::fs::read_to_string(&from_template).unwrap();
        assert!(
            written.contains("Nothing moves to it until you pick it."),
            "comment kept: {written}"
        );
        assert!(!Config::load_from(&from_template).unwrap().streaming.enabled);

        save_streaming_enabled_at(&from_template, true).unwrap();
        assert!(Config::load_from(&from_template).unwrap().streaming.enabled);

        let without = scratch("no-streaming");
        std::fs::write(&without, "redirect_port = 8888\n").unwrap();
        save_streaming_enabled_at(&without, true).unwrap();
        assert!(Config::load_from(&without).unwrap().streaming.enabled);
    }

    /// Better to refuse than to overwrite a file boombox cannot read.
    #[test]
    fn a_config_that_does_not_parse_is_left_untouched() {
        let path = scratch("broken");
        std::fs::write(&path, "this is = = not toml").unwrap();
        assert!(save_client_id_at(&path, ID).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is = = not toml");
    }
}
