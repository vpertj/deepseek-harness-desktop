use include_dir::{include_dir, Dir};
use std::fs;
use std::path::PathBuf;

/// Bundled superpowers plugin directory (embedded at compile time).
const BUNDLED_PLUGIN: Dir = include_dir!("$CARGO_MANIFEST_DIR/resources/superpowers");

/// Target directory where ZCode discovers the plugin.
fn target_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".zcode")
        .join("plugins")
        .join("user")
        .join("superpowers")
}

/// Marker file indicating the plugin has been extracted to the target dir.
fn marker_file() -> PathBuf {
    target_dir().join(".bundled_from_dsh")
}

/// Extract the bundled superpowers plugin to `~/.zcode/plugins/user/superpowers/`
/// if not already present (idempotent via a marker file). Returns Ok(()) when
/// the target is in sync with the bundle; Err(_) if extraction fails
/// (non-fatal — the app continues without the plugin).
pub fn ensure_installed() -> Result<(), std::io::Error> {
    let target = target_dir();
    let marker = marker_file();

    // Already installed — skip.
    if marker.exists() {
        return Ok(());
    }

    // Remove any stale target (e.g. from a previous manual install or partial extract).
    if target.is_dir() {
        fs::remove_dir_all(&target)?;
    }
    fs::create_dir_all(target.parent().unwrap())?;

    // Recursively extract all bundled files to disk.
    BUNDLED_PLUGIN.extract(&target)?;

    // Write marker so future launches skip this work.
    fs::write(&marker, "bundled")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_plugin_is_non_empty() {
        assert!(BUNDLED_PLUGIN.entries().len() > 0,
            "resources/superpowers/ must contain plugin files");
    }

    #[test]
    fn marker_path_looks_correct() {
        let m = marker_file();
        assert!(m.to_string_lossy().contains(".zcode/plugins/user/superpowers"));
    }
}
