use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::kernel::{is_kernel_dir, resolve_toolchain, KernelManager};

/// Repo the kernel is cloned from.
const KERNEL_REPO: &str = "https://github.com/deepseek-ai/deepseek-harness.git";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current: Option<String>,
    pub latest: Option<String>,
    /// Commits the local branch is behind origin.
    pub behind: u32,
    pub update_available: bool,
    pub dirty: bool,
    pub error: Option<String>,
}

/// Default location for a self-managed kernel checkout.
pub fn default_kernel_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("com.deepseekharness.desktop").join("kernel")
}

/// Run a command, streaming stdout+stderr lines as `kernel-log` events.
async fn run_streaming(
    app: &AppHandle,
    cwd: &Path,
    path_env: &str,
    cmd: &str,
    args: &[&str],
) -> Result<(), String> {
    let mut child = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .env("PATH", path_env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("执行失败 {cmd}: {e}"))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let app_out = app.clone();
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = app_out.emit(
                "kernel-log",
                serde_json::json!({ "stream": "out", "line": line }),
            );
        }
    });
    let app_err = app.clone();
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = app_err.emit(
                "kernel-log",
                serde_json::json!({ "stream": "err", "line": line }),
            );
        }
    });

    let status = child.wait().await.map_err(|e| format!("等待进程失败: {e}"))?;
    let _ = tokio::join!(out_task, err_task);
    if status.success() {
        Ok(())
    } else {
        Err(format!("命令 `{cmd} {}` 退出码: {:?}", args.join(" "), status.code()))
    }
}

/// Run git inside the kernel dir, returning trimmed stdout on success.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("git 执行失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} 失败: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Fetch origin and compare local HEAD with the remote branch.
pub async fn check_update(manager: &KernelManager, app: &AppHandle) -> Result<UpdateInfo, String> {
    let info = manager.status().await;
    let Some(dir) = info.kernel_dir.clone() else {
        return Err("尚未设置内核目录".to_string());
    };
    if !is_kernel_dir(&dir) {
        return Err(format!("内核目录无效: {}", dir.display()));
    }

    let current = crate::kernel::git_revision(&dir).0;

    // Fetch with visible logs.
    let (_pnpm, path_env) = resolve_toolchain().await?;
    if let Err(e) = run_streaming(app, &dir, &path_env, "git", &["fetch", "origin"]).await {
        return Ok(UpdateInfo {
            current,
            latest: None,
            behind: 0,
            update_available: false,
            dirty: info.dirty,
            error: Some(format!("git fetch 失败（可能是网络问题）: {e}")),
        });
    }

    // Determine current branch and remote ref.
    let branch = git(&dir, &["symbolic-ref", "--short", "HEAD"])
        .await
        .unwrap_or_else(|_| "master".into());
    let remote_ref = format!("origin/{branch}");
    let latest = git(&dir, &["rev-parse", "--short", &remote_ref])
        .await
        .unwrap_or_else(|_| "?".into());
    // Commits on origin/<branch> that the local branch does not have yet.
    let behind: u32 = git(&dir, &["rev-list", "--count", &format!("HEAD..{remote_ref}")])
        .await
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let update_available = behind > 0;
    let phase = if update_available && latest != "?" {
        "update_available"
    } else {
        "up_to_date"
    };
    let _ = app.emit(
        "update-status",
        serde_json::json!({
            "phase": phase,
            "current": current,
            "latest": latest,
            "behind": behind,
        }),
    );

    Ok(UpdateInfo {
        current,
        latest: Some(latest),
        behind,
        update_available,
        dirty: info.dirty,
        error: None,
    })
}

/// Pull the kernel to origin, reinstall deps and rebuild. Restarts the web
/// server if it was running before the update.
pub async fn apply_update(
    manager: &KernelManager,
    app: &AppHandle,
) -> Result<(), String> {
    let info = manager.status().await;
    let Some(dir) = info.kernel_dir.clone() else {
        return Err("尚未设置内核目录".to_string());
    };
    if !is_kernel_dir(&dir) {
        return Err(format!("内核目录无效: {}", dir.display()));
    }
    if info.dirty {
        return Err("内核目录有本地改动（dirty），请先在终端处理（git stash 或 commit）后再更新".to_string());
    }
    if !matches!(info.status, crate::kernel::KernelStatus::Stopped) {
        return Err("更新前请先停止内核（点顶栏「停止」按钮）".to_string());
    }

    let (pnpm, path_env) = resolve_toolchain().await?;
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 开始更新内核 ==" }));

    // 1. Pull (ff-only is safe: dirty check above guarantees a clean tree).
    let branch = git(&dir, &["symbolic-ref", "--short", "HEAD"])
        .await
        .unwrap_or_else(|_| "master".into());
    run_streaming(app, &dir, &path_env, "git", &["pull", "--ff-only", "origin", &branch]).await?;

    // 2. Reinstall deps.
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm install ==" }));
    run_streaming(app, &dir, &path_env, &pnpm, &["install"]).await?;

    // 3. Rebuild (produces the web client bundle).
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm run build ==" }));
    run_streaming(app, &dir, &path_env, &pnpm, &["run", "build"]).await?;

    let rev = crate::kernel::git_revision(&dir).0.unwrap_or_else(|| "?".into());
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": format!("== 更新完成，当前版本 {rev} ==") }),
    );
    let _ = app.emit(
        "update-status",
        serde_json::json!({ "phase": "done", "revision": rev }),
    );
    Ok(())
}

/// Fresh clone + install + build of the kernel into `target_dir`, then set it
/// as the active kernel directory.
pub async fn install_kernel(
    manager: &KernelManager,
    app: &AppHandle,
    target_dir: PathBuf,
) -> Result<(), String> {
    eprintln!("[updater] install_kernel -> {}", target_dir.display());
    if target_dir.exists() {
        if is_kernel_dir(&target_dir) {
            // Already a kernel checkout: adopt it.
            eprintln!("[updater] adopting existing checkout");
            manager.set_kernel_dir(target_dir).await?;
            return Ok(());
        }
        return Err(format!("目标目录已存在且不是内核仓库: {}", target_dir.display()));
    }
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    let (pnpm, path_env) = resolve_toolchain().await?;
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 克隆 deepseek-harness ==" }));
    run_streaming(app, target_dir.parent().unwrap_or(Path::new(".")), &path_env, "git", &["clone", KERNEL_REPO, target_dir.file_name().unwrap().to_str().unwrap()]).await?;

    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm install ==" }));
    run_streaming(app, &target_dir, &path_env, &pnpm, &["install"]).await?;
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm run build ==" }));
    run_streaming(app, &target_dir, &path_env, &pnpm, &["run", "build"]).await?;

    manager.set_kernel_dir(target_dir).await?;
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": "== 内核安装完成 ==" }),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn update_check(
    manager: tauri::State<'_, KernelManager>,
    app: AppHandle,
) -> Result<UpdateInfo, String> {
    check_update(&manager, &app).await
}

#[tauri::command]
pub async fn update_apply(
    manager: tauri::State<'_, KernelManager>,
    app: AppHandle,
) -> Result<(), String> {
    apply_update(&manager, &app).await
}

#[tauri::command]
pub async fn kernel_install(
    manager: tauri::State<'_, KernelManager>,
    app: AppHandle,
) -> Result<(), String> {
    install_kernel(&manager, &app, default_kernel_dir()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kernel_dir_is_under_data_dir() {
        let dir = default_kernel_dir();
        assert!(dir.ends_with("com.deepseekharness.desktop/kernel"));
    }
}
