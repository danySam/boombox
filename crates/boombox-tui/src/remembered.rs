//! What the TUI keeps between runs.
//!
//! Only the visualisation so far: someone who leaves the fire on expects to
//! find it on next time, not to press `v` four times to get back to it.
//! Kept in the state directory beside the daemon's own files, because it is
//! a record of what was last done rather than a setting anyone wrote.

use std::path::{Path, PathBuf};

use crate::app::VisualMode;

const FILE: &str = "tui.json";

fn path() -> Option<PathBuf> {
    boombox_core::config::state_dir().ok().map(|dir| dir.join(FILE))
}

/// The visualisation last left on, or `None` for the cover.
///
/// Anything missing or unreadable is the cover: a first run has nothing to
/// remember, and a damaged file is not worth an error over something this
/// small.
pub fn visual() -> Option<VisualMode> {
    path().and_then(|p| visual_at(&p))
}

/// Records the visualisation now showing. Failure is ignored for the same
/// reason a missing file is: losing this costs a keypress next time.
pub fn save_visual(mode: Option<VisualMode>) {
    if let Some(p) = path() {
        let _ = save_visual_at(&p, mode);
    }
}

fn visual_at(path: &Path) -> Option<VisualMode> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("visual")?.as_str().and_then(VisualMode::from_key)
}

fn save_visual_at(path: &Path, mode: Option<VisualMode>) -> std::io::Result<()> {
    let json = serde_json::json!({ "visual": mode.map(VisualMode::key).unwrap_or("off") });
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Beside the target and renamed over it, so a TUI killed mid-write
    // cannot leave half a file to be read as garbage next time.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.to_string())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("boombox-remembered-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE)
    }

    #[test]
    fn every_visualisation_comes_back_as_it_was_left() {
        let path = scratch("round-trip");
        for mode in [VisualMode::Bars, VisualMode::Spectrogram, VisualMode::Scope, VisualMode::Fire]
        {
            save_visual_at(&path, Some(mode)).unwrap();
            assert_eq!(visual_at(&path), Some(mode));
        }
    }

    /// Turning the visualiser off is a choice too, and has to stick.
    #[test]
    fn leaving_it_on_the_cover_is_remembered() {
        let path = scratch("off");
        save_visual_at(&path, Some(VisualMode::Fire)).unwrap();
        save_visual_at(&path, None).unwrap();
        assert_eq!(visual_at(&path), None);
    }

    #[test]
    fn a_first_run_or_a_damaged_file_starts_on_the_cover() {
        let path = scratch("damaged");
        assert_eq!(visual_at(&path), None, "nothing saved yet");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(visual_at(&path), None, "garbage");
        std::fs::write(&path, r#"{"visual":"lava-lamp"}"#).unwrap();
        assert_eq!(visual_at(&path), None, "a mode this build does not have");
    }
}
