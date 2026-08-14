use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use crate::config::Settings;

const MAX_BYTES: u64 = 5 * 1024 * 1024; // rotate at 5 MB

fn log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Logs/deepseek-harness-desktop/kernel.log")
}

/// Append one kernel log line to ~/Library/Logs/deepseek-harness-desktop/
/// kernel.log when persist_logs is enabled. Rotates to kernel.log.1 at 5 MB.
/// Cheap enough to call per line (settings file read is tiny).
pub fn persist(stream: &str, line: &str) {
    if !Settings::load().persist_logs {
        return;
    }
    let path = log_path();
    let parent = path.parent().expect("log path has parent");
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let rotated = parent.join("kernel.log.1");
        let _ = std::fs::rename(&path, rotated);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{stream}] {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_is_under_library_logs() {
        let p = log_path();
        assert!(p.ends_with("Library/Logs/deepseek-harness-desktop/kernel.log"));
    }
}
