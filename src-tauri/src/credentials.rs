use std::process::Command;

/// DeepSeek API key stored in the macOS Keychain (never on disk in plaintext).
const KEYCHAIN_SERVICE: &str = "deepseek-harness-desktop";
const KEYCHAIN_ACCOUNT: &str = "deepseek-api-key";

fn keychain() -> Command {
    let mut cmd = Command::new("security");
    cmd.args(["add-generic-password", "-U", "-a", KEYCHAIN_SERVICE, "-s", KEYCHAIN_ACCOUNT]);
    cmd
}

/// Save the API key to the Keychain. Returns Ok(()) on success.
pub fn set_api_key(key: &str) -> Result<(), String> {
    let out = keychain()
        .args(["-w", key])
        .output()
        .map_err(|e| format!("无法调用 security CLI: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "保存到钥匙串失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Read the API key from the Keychain. Returns None when not configured.
pub fn get_api_key() -> Option<String> {
    let out = Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            KEYCHAIN_SERVICE,
            "-s",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// Remove the API key from the Keychain (no-op when absent).
pub fn clear_api_key() -> Result<(), String> {
    let out = Command::new("security")
        .args(["delete-generic-password", "-a", KEYCHAIN_SERVICE, "-s", KEYCHAIN_ACCOUNT])
        .output()
        .map_err(|e| format!("无法调用 security CLI: {e}"))?;
    if out.status.success() || String::from_utf8_lossy(&out.stderr).contains("could not be found") {
        Ok(())
    } else {
        Err(format!(
            "删除钥匙串条目失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// True when an API key is stored in the Keychain.
pub fn has_api_key() -> bool {
    get_api_key().is_some()
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn api_key_status() -> Result<bool, String> {
    Ok(has_api_key())
}

#[tauri::command]
pub fn api_key_set(key: String) -> Result<(), String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("API Key 不能为空".to_string());
    }
    set_api_key(trimmed)
}

#[tauri::command]
pub fn api_key_clear() -> Result<(), String> {
    clear_api_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_roundtrip() {
        // Only meaningful on macOS with a working Keychain; skip failures
        // gracefully so CI on other platforms doesn't break.
        let probe = Command::new("security")
            .args(["help"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !probe {
            eprintln!("跳过: security CLI 不可用");
            return;
        }
        let _ = clear_api_key();
        assert!(!has_api_key());
        set_api_key("sk-test-roundtrip").unwrap();
        assert_eq!(get_api_key().as_deref(), Some("sk-test-roundtrip"));
        clear_api_key().unwrap();
        assert!(!has_api_key());
    }
}
