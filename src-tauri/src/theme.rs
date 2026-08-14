use std::path::PathBuf;

use serde::Serialize;

/// Read the theme preference dsh persists in its host user-settings file
/// (`$DSH_HOME/settings.yaml`, default `~/.dsh/settings.yaml`), so the shell
/// chrome follows the embedded web UI's appearance setting.
const THEME_NS: &str = "ui-theme";
const THEME_FIELD: &str = "preference";

#[derive(Debug, Clone, Serialize)]
pub struct ThemeDto {
    /// "light" | "dark" | "system" — None when dsh hasn't written a preference.
    pub preference: Option<String>,
    /// Absolute path of the settings file we read (for debugging).
    pub source: String,
}

fn dsh_settings_path() -> PathBuf {
    if let Ok(home) = std::env::var("DSH_HOME") {
        return PathBuf::from(home).join("settings.yaml");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dsh")
        .join("settings.yaml")
}

/// Minimal parser for the small YAML subset dsh writes: top-level `ui-theme:`
/// block containing an indented `preference: <value>` line. Robust to comment
/// lines and CRLF; anything else yields None.
fn parse_preference(raw: &str) -> Option<String> {
    let mut in_ns = false;
    for line in raw.lines() {
        let line = line.trim_end();
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !in_ns {
            if let Some(key) = line.strip_suffix(':') {
                if key.trim() == THEME_NS {
                    in_ns = true;
                }
            }
            continue;
        }
        // Inside ui-theme: block.
        if !line.starts_with(char::is_whitespace) {
            break; // next top-level key
        }
        if let Some((key, value)) = line.trim().split_once(':') {
            if key.trim() == THEME_FIELD {
                let v = value.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

#[tauri::command]
pub fn get_theme() -> ThemeDto {
    let path = dsh_settings_path();
    let preference = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| parse_preference(&raw));
    ThemeDto {
        preference,
        source: path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dark() {
        let raw = "ui-onboarding:\n  welcomeNoticeVersion: 2026-08-13.1\nui-theme:\n  preference: dark\npermission:\n  defaultPreset: danger-full-access\n";
        assert_eq!(parse_preference(raw).as_deref(), Some("dark"));
    }

    #[test]
    fn parses_light_with_quotes_and_crlf() {
        let raw = "ui-theme:\r\n  preference: \"light\"\r\n";
        assert_eq!(parse_preference(raw).as_deref(), Some("light"));
    }

    #[test]
    fn returns_none_when_missing() {
        assert_eq!(parse_preference("ui-onboarding:\n  x: 1\n"), None);
        assert_eq!(parse_preference(""), None);
        assert_eq!(parse_preference("ui-theme:\n  other: 1\n"), None);
    }
}
