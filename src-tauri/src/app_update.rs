use serde::Serialize;
use tauri::{AppHandle, Emitter};

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

/// Download the latest release dmg, mount it, and hand a replace-and-relaunch
/// script to a detached shell (it survives this app exiting). The app quits
/// right after, the script swaps the .app and reopens the new version —
/// full in-app auto-update without a code-signing certificate.
#[tauri::command]
pub async fn app_download_update(app: AppHandle) -> Result<(), String> {
    let info = check_app_update().await?;
    if !info.update_available {
        return Err("当前已是最新版本".to_string());
    }

    let app_name = "deepseek-harness-desktop";
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    // tauri dmg naming: <productName>_<version>_<arch>.dmg (no "v" prefix).
    let dmg_url = format!(
        "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/v{}/{}_{}_{}.dmg",
        info.latest, app_name, info.latest, arch
    );
    let dmg_path = std::env::temp_dir().join(format!("dsh-update-{}.dmg", info.latest));

    // 1. Download with progress events.
    let _ = app.emit("app-update-progress", serde_json::json!({ "phase": "downloading", "percent": 0 }));
    download_with_progress(&app, &dmg_url, &dmg_path, &info.latest).await?;

    // 2. Mount the dmg and locate the .app inside.
    let _ = app.emit("app-update-progress", serde_json::json!({ "phase": "mounting", "percent": 100 }));
    let mount = mount_dmg(&dmg_path, &info.latest)?;
    let mounted_app = std::path::Path::new(&mount).join("deepseek-harness-desktop.app");
    if !mounted_app.is_dir() {
        let _ = hdiutil_detach(&mount);
        return Err(format!("dmg 中未找到应用: {}", mounted_app.display()));
    }

    // 3. Write the replace+relaunch script and detach it.
    let script = install_script(&mounted_app, &mount, &info.latest);
    let script_path = std::env::temp_dir().join("dsh-update-install.sh");
    std::fs::write(&script_path, script).map_err(|e| format!("写入更新脚本失败: {e}"))?;

    std::process::Command::new("/bin/sh")
        .arg(&script_path)
        .spawn()
        .map_err(|e| format!("启动更新脚本失败: {e}"))?;

    let _ = app.emit("app-update-progress", serde_json::json!({ "phase": "ready", "percent": 100 }));

    // Give the frontend a moment to show "ready", then exit the app for real.
    // The detached install script waits for us to quit, swaps the .app and
    // relaunches. Exiting from Rust (not the frontend) avoids depending on
    // window-close IPC permissions.
    let app_exit = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        app_exit.exit(0);
    });
    Ok(())
}

/// Download the dmg via system curl (follows GitHub CDN redirects, no rate
/// limit on release assets). Emits a phase event when finished.
async fn download_with_progress(
    app: &AppHandle,
    url: &str,
    dest: &std::path::Path,
    version: &str,
) -> Result<(), String> {
    let url = url.to_string();
    let dest = dest.to_path_buf();
    let dest_for_dl = dest.clone();
    let app = app.clone();
    let version = version.to_string();
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let out = std::process::Command::new("curl")
                .args(["-L", "-sS", "--fail", "-o"])
                .arg(&dest_for_dl)
                .arg(&url)
                .output()
                .map_err(|e| format!("启动下载失败: {e}"))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(format!("下载失败: {}", err.trim()));
            }
            Ok(())
        }),
    )
    .await
    .map_err(|_| "下载超时".to_string())?
    .map_err(|e| e.to_string())?;
    // Guard against an empty/broken download (e.g. release asset not yet
    // published): fail with a clear message instead of a confusing mount error.
    let size = std::fs::metadata(&dest)
        .map(|m| m.len())
        .unwrap_or(0);
    if size < 1024 * 1024 {
        let _ = std::fs::remove_file(&dest);
        return Err("下载的文件无效或为空（可能新版本尚未发布完成），请稍后再试".to_string());
    }
    let _ = app.emit(
        "app-update-progress",
        serde_json::json!({ "phase": "downloaded", "percent": 100, "version": version }),
    );
    Ok(())
}

/// Attach the dmg at a fixed, predictable mount point (`-mountpoint` avoids
/// parsing hdiutil's space-bearing volume names) and return it.
fn mount_dmg(dmg: &std::path::Path, version: &str) -> Result<String, String> {
    let mount = format!("/Volumes/dsh-update-{version}");
    let out = std::process::Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(dmg)
        .output()
        .map_err(|e| format!("挂载 dmg 失败: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("挂载 dmg 失败: {}", err.trim()));
    }
    if !std::path::Path::new(&mount).is_dir() {
        return Err(format!("挂载点未生成: {mount}"));
    }
    Ok(mount)
}

fn hdiutil_detach(mount: &str) -> Result<(), String> {
    std::process::Command::new("hdiutil")
        .args(["detach", mount])
        .output()
        .map(|_| ())
        .map_err(|e| format!("卸载 dmg 失败: {e}"))
}

/// Build the install script: wait for the old app to exit, swap in the new
/// .app, strip quarantine, unmount the dmg, then relaunch.
fn install_script(mounted_app: &std::path::Path, mount: &str, version: &str) -> String {
    let app_path = "/Applications/deepseek-harness-desktop.app";
    let mounted = mounted_app.display().to_string();
    format!(
        r#"#!/bin/sh
# auto-update to v{version}
while pgrep -f "Contents/MacOS/deepseek-harness-desktop" > /dev/null 2>&1; do sleep 1; done
rm -rf "{app_path}"
ditto "{mounted}" "{app_path}"
xattr -dr com.apple.quarantine "{app_path}" 2>/dev/null || true
hdiutil detach "{mount}" > /dev/null 2>&1 || true
open "{app_path}"
"#
    )
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
