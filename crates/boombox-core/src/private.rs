//! Keeping boombox's secrets readable by the user who owns them, and nobody else.
//!
//! Two files in the state directory would let someone act as you on
//! Spotify: the Web API token, and librespot's reusable streaming login. The
//! token was always written owner-only. librespot's login was written with
//! the process's default permissions -- readable by anyone who could reach
//! the directory -- and the directory itself was created the same way. So
//! the keychain, when it was the default, only ever guarded half of it.

use std::path::{Path, PathBuf};

use crate::error::Result;

const TOKEN_FILE: &str = "tokens.json";
const LIBRESPOT_DIR: &str = "librespot";
const LIBRESPOT_CREDENTIALS: &str = "credentials.json";

/// The Web API token, when it is kept in a file.
pub fn token_file() -> Result<PathBuf> {
    Ok(crate::config::state_dir()?.join(TOKEN_FILE))
}

/// librespot's cache: its reusable login, and the volume.
pub fn librespot_dir() -> Result<PathBuf> {
    Ok(crate::config::state_dir()?.join(LIBRESPOT_DIR))
}

/// librespot's reusable login. Enough to open a streaming session as you.
pub fn librespot_credentials() -> Result<PathBuf> {
    Ok(librespot_dir()?.join(LIBRESPOT_CREDENTIALS))
}

/// Locks down the state directory and the secrets in it.
///
/// Run on every start, so an install made before this existed is fixed the
/// first time a new binary runs, whatever the command.
pub fn secure_state_dir() -> Result<Vec<PathBuf>> {
    secure_state_dir_at(&crate::config::state_dir()?)
}

/// [`secure_state_dir`] for a given directory. Returns what it restricted.
pub fn secure_state_dir_at(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut restricted = Vec::new();
    if dir.exists() {
        // Only a directory that is unmistakably boombox's. The default location
        // always ends in `boombox`; `BOOMBOX_STATE_DIR` is used exactly as given,
        // and could name a directory that other programs share -- locking
        // that would break them.
        if dir.file_name().is_some_and(|name| name == "boombox") && restrict(dir)? {
            restricted.push(dir.to_path_buf());
        }
    } else {
        create_private_dir(dir)?;
    }

    // These are boombox's own wherever the directory is.
    let librespot = dir.join(LIBRESPOT_DIR);
    let candidates =
        [librespot.clone(), dir.join(TOKEN_FILE), librespot.join(LIBRESPOT_CREDENTIALS)];
    for path in candidates {
        if restrict(&path)? {
            restricted.push(path);
        }
    }
    Ok(restricted)
}

/// Creates `dir` private, or restricts it if it already exists. For
/// directories that are boombox's own wherever they sit, like librespot's cache.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        restrict(dir)?;
        Ok(())
    } else {
        create_private_dir(dir)
    }
}

/// Removes group and other access from `path`, if it has any. Owner bits are
/// left as they are. Works on directories, files and sockets alike. A path
/// that does not exist is not an error.
///
/// Returns whether anything changed.
pub fn restrict(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let Ok(metadata) = std::fs::metadata(path) else {
            return Ok(false);
        };
        let current = metadata.permissions().mode() & 0o7777;
        let tightened = current & !0o077;
        if tightened == current {
            return Ok(false);
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(tightened))?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Writes a secret so that nobody else can read it, at any point.
///
/// Permissions are settled before the contents go in. The old token writer
/// wrote first and restricted after, which left each new token readable for
/// the moment in between.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        create_private_dir(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies to a file this call creates. One that already
        // existed keeps its old permissions, so they are fixed here -- after
        // the truncate, so the old contents are gone, and before the write.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(contents)?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, contents)?;
    Ok(())
}

/// Creates `dir` and any missing parents, readable only by their owner.
///
/// Parents too, because that is what the XDG spec asks of a base directory
/// that has to be created.
fn create_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("boombox-private-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_new_state_directory_is_created_private() {
        let root = scratch("new");
        let dir = root.join("boombox");
        secure_state_dir_at(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An install from before this existed, laid out exactly as found on a
    /// real machine: every directory and secret open to other users.
    #[test]
    fn an_existing_install_is_locked_down_and_a_second_pass_does_nothing() {
        let root = scratch("old");
        let dir = root.join("boombox");
        std::fs::create_dir_all(dir.join("librespot")).unwrap();
        std::fs::write(dir.join("tokens.json"), "{}").unwrap();
        std::fs::write(dir.join("librespot/credentials.json"), "{}").unwrap();
        set_mode(&dir, 0o755);
        set_mode(&dir.join("librespot"), 0o755);
        set_mode(&dir.join("tokens.json"), 0o644);
        set_mode(&dir.join("librespot/credentials.json"), 0o644);

        let restricted = secure_state_dir_at(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("librespot")), 0o700);
        assert_eq!(mode(&dir.join("tokens.json")), 0o600);
        assert_eq!(mode(&dir.join("librespot/credentials.json")), 0o600);
        assert_eq!(restricted.len(), 4, "{restricted:?}");

        assert!(secure_state_dir_at(&dir).unwrap().is_empty(), "already private");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `BOOMBOX_STATE_DIR` is used as given. Pointed at a shared directory,
    /// locking the directory would break everything else that uses it --
    /// but the secrets inside are still boombox's.
    #[test]
    fn a_shared_directory_is_left_alone_but_the_secrets_in_it_are_not() {
        let dir = scratch("shared");
        std::fs::create_dir_all(&dir).unwrap();
        set_mode(&dir, 0o755);
        std::fs::write(dir.join("tokens.json"), "{}").unwrap();
        set_mode(&dir.join("tokens.json"), 0o644);

        secure_state_dir_at(&dir).unwrap();
        assert_eq!(mode(&dir), 0o755, "not boombox's to change");
        assert_eq!(mode(&dir.join("tokens.json")), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The token is rewritten on every refresh, into a file that an older
    /// build may have left readable.
    #[test]
    fn writing_a_secret_over_a_readable_file_leaves_it_private() {
        let dir = scratch("overwrite");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tokens.json");
        std::fs::write(&file, "old").unwrap();
        set_mode(&file, 0o644);

        write_private(&file, b"new").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_secret_written_first_creates_a_private_directory_for_itself() {
        let root = scratch("fresh");
        let file = root.join("boombox").join("tokens.json");
        write_private(&file, b"x").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&root.join("boombox")), 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Sockets too: the daemon's can be configured to live in /tmp.
    #[test]
    fn restricting_keeps_the_owner_bits_and_skips_what_is_missing() {
        let dir = scratch("bits");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f");
        std::fs::write(&file, "").unwrap();
        set_mode(&file, 0o754);
        assert!(restrict(&file).unwrap());
        assert_eq!(mode(&file), 0o700, "owner rwx kept, the rest removed");
        assert!(!restrict(&dir.join("missing")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
