use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Shell app settings, persisted as JSON in the app config dir.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Absolute path to the deepseek-harness kernel checkout.
    pub kernel_dir: Option<PathBuf>,
    /// Start the kernel automatically when the app launches.
    pub auto_start: bool,
    /// Persist kernel logs to a file.
    pub persist_logs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            kernel_dir: None,
            auto_start: true,
            persist_logs: true,
        }
    }
}

impl Settings {
    pub fn config_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("com.deepseekharness.desktop")
            .join("settings.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::config_path())
    }

    fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
        }
        let raw =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化配置失败: {e}"))?;
        std::fs::write(path, raw).map_err(|e| format!("写入配置失败: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-desktop-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings.json")
    }

    #[test]
    fn settings_roundtrip() {
        let path = temp_settings_path("roundtrip");
        let original = Settings {
            kernel_dir: Some(PathBuf::from("/tmp/fake-kernel")),
            auto_start: false,
            persist_logs: false,
        };
        original.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, original);
    }

    #[test]
    fn settings_default_when_missing() {
        let path = temp_settings_path("missing");
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, Settings::default());
        assert!(loaded.auto_start, "auto_start 默认开启");
    }

    #[test]
    fn settings_survives_bad_json() {
        let path = temp_settings_path("badjson");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json {{{").unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, Settings::default());
    }
}
