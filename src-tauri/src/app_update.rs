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

/// Compare the installed app version against the latest tagged GitHub release.
/// Uses `git ls-remote` (git protocol) instead of the GitHub REST API so the
/// check never hits the unauthenticated API rate limit. Silent auto-install
/// needs a signed binary, so we surface the update and open the release page.
pub async fn check_app_update() -> Result<AppUpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let url = format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest");

    let tags = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("git")
            .args([
                "ls-remote",
                "--tags",
                &format!("https://github.com/{REPO_OWNER}/{REPO_NAME}.git"),
                "v*",
            ])
            .output(),
    )
    .await
    .map_err(|_| "检查更新超时".to_string())?
    .map_err(|e| format!("无法访问 GitHub: {e}"))?;

    if !tags.status.success() {
        return Err("无法读取 GitHub 版本信息".to_string());
    }
    let text = String::from_utf8_lossy(&tags.stdout);
    // Lines look like: "<sha>\trefs/tags/v0.1.0" (peeled tags end with ^{}).
    let mut latest = String::new();
    for line in text.lines() {
        if line.contains("^{}") {
            continue;
        }
        let Some(tag) = line.split('\t').nth(1) else {
            continue;
        };
        let v = tag
            .trim_start_matches("refs/tags/")
            .trim_start_matches('v')
            .to_string();
        if !v.is_empty() && version_newer(&v, &latest) {
            latest = v;
        }
    }
    if latest.is_empty() {
        return Err("未找到已发布版本".to_string());
    }

    Ok(AppUpdateInfo {
        update_available: version_newer(&latest, &current),
        current,
        latest,
        url,
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
