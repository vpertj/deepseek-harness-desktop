use serde::Serialize;
use std::path::PathBuf;

use crate::config::Settings;

#[derive(Debug, Clone, Serialize)]
pub struct SettingsDto {
    pub kernel_dir: Option<PathBuf>,
    pub auto_start: bool,
    pub persist_logs: bool,
}

impl From<Settings> for SettingsDto {
    fn from(s: Settings) -> Self {
        SettingsDto {
            kernel_dir: s.kernel_dir,
            auto_start: s.auto_start,
            persist_logs: s.persist_logs,
        }
    }
}

#[tauri::command]
pub fn get_settings() -> SettingsDto {
    Settings::load().into()
}

#[tauri::command]
pub fn set_auto_start(auto_start: bool) -> Result<SettingsDto, String> {
    let mut s = Settings::load();
    s.auto_start = auto_start;
    s.save()?;
    Ok(s.into())
}

#[tauri::command]
pub fn set_persist_logs(persist_logs: bool) -> Result<SettingsDto, String> {
    let mut s = Settings::load();
    s.persist_logs = persist_logs;
    s.save()?;
    Ok(s.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_from_settings() {
        let dto: SettingsDto = Settings::default().into();
        assert!(dto.auto_start);
        assert!(dto.persist_logs);
        assert_eq!(dto.kernel_dir, None);
    }
}
