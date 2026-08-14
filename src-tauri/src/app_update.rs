use serde::Serialize;

pub const REPO_OWNER: &str = "vpertj";
pub const REPO_NAME: &str = "deepseek-harness-desktop";

#[derive(Debug, Clone, Serialize)]
pub struct AppUpdateInfo {
    pub update_available: bool,
    pub current: String,
    pub latest: String,
    /// URL to the GitHub release page (user downloads and installs the dmg).
    pub url: String,
}

/// Compare the installed app version against the latest GitHub release.
/// Unadvertised limitation: silent auto-install needs a signed binary, so we
/// surface the update and open the release page instead.
pub async fn check_app_update() -> Result<AppUpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
    let url_for_api = url.clone();
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut resp = ureq::get(&url_for_api)
                .header("User-Agent", "deepseek-harness-desktop")
                .header("Accept", "application/vnd.github+json")
                .call()
                .map_err(|e| format!("无法访问 GitHub Releases: {e}"))?;
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("读取响应失败: {e}"))?;
            Ok(text)
        }),
    )
    .await
    .map_err(|_| "检查更新超时".to_string())?
    .map_err(|e| format!("检查更新任务失败: {e}"))??;

    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("解析响应失败: {e}"))?;
    let latest = json
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    if latest.is_empty() {
        return Err("未找到已发布版本".to_string());
    }
    let html_url = json
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or(&url)
        .to_string();
    let update_available = version_newer(&latest, &current);
    Ok(AppUpdateInfo {
        update_available,
        current,
        latest,
        url: html_url,
    })
}

/// True when `newer` is a greater semver than `older` (numeric compare).
pub fn version_newer(newer: &str, older: &str) -> bool {
    fn nums(v: &str) -> Vec<u64> {
        v.split('.')
            .filter_map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
            .collect()
    }
    let a = nums(newer);
    let b = nums(older);
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

#[tauri::command]
pub async fn app_update_check() -> Result<AppUpdateInfo, String> {
    check_app_update().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(version_newer("1.1.0", "1.0.0"));
        assert!(version_newer("1.0.1", "1.0.0"));
        assert!(version_newer("1.10.0", "1.9.9"));
        assert!(!version_newer("1.0.0", "1.0.0"));
        assert!(!version_newer("0.9.0", "1.0.0"));
        assert!(!version_newer("1.0.0", "1.0.1"));
    }
}
