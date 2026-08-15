use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::kernel::KernelManager;
use tauri::{AppHandle, Emitter};

/// Plugin management for the dsh web profile (`~/.dsh/profiles/web`).
/// Installs are delegated to the kernel's own CLI:
///   pnpm dsh plugin --profile web add <name>@<version> / remove <name>
/// The web profile is shared across shells, so a kernel restart is required
/// after any change for it to take effect.

fn profile_dir() -> PathBuf {
    let home = std::env::var("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".dsh"));
    home.join("profiles").join("web")
}

/// Installed plugin names + versions from the profile's package.json deps.
#[tauri::command]
pub fn plugin_list() -> Vec<String> {
    let pkg_path = profile_dir().join("package.json");
    let Ok(raw) = std::fs::read_to_string(&pkg_path) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return vec![];
    };
    let Some(deps) = v.get("dependencies").and_then(|d| d.as_object()) else {
        return vec![];
    };
    let mut out: Vec<String> = deps
        .iter()
        .map(|(name, ver)| format!("{name}@{ver}"))
        .collect();
    out.sort();
    out
}

/// Install a plugin by npm name (optionally pinned to a version).
#[tauri::command]
pub async fn plugin_install(
    manager: tauri::State<'_, KernelManager>,
    app: AppHandle,
    name: String,
    version: Option<String>,
) -> Result<(), String> {
    let kernel_dir = manager
        .kernel_dir()
        .ok_or("请先配置内核目录".to_string())?;
    if !crate::kernel::is_kernel_dir(&kernel_dir) {
        return Err("内核目录无效".into());
    }
    if name.trim().is_empty() {
        return Err("插件名不能为空".into());
    }
    let (pnpm, path_env) = crate::kernel::resolve_toolchain().await?;
    let spec = match version.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => format!("{name}@{v}"),
        None => name,
    };
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": format!("== 安装插件 {spec} ==") }),
    );
    crate::updater::run_streaming(
        &app,
        &kernel_dir,
        &path_env,
        &pnpm,
        &["dsh", "plugin", "--profile", "web", "add", &spec],
    )
    .await?;
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": "== 插件安装完成，重启内核后生效 ==" }),
    );
    Ok(())
}

/// Remove an installed plugin.
#[tauri::command]
pub async fn plugin_remove(
    manager: tauri::State<'_, KernelManager>,
    app: AppHandle,
    name: String,
) -> Result<(), String> {
    let kernel_dir = manager
        .kernel_dir()
        .ok_or("请先配置内核目录".to_string())?;
    if !crate::kernel::is_kernel_dir(&kernel_dir) {
        return Err("内核目录无效".into());
    }
    let (pnpm, path_env) = crate::kernel::resolve_toolchain().await?;
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": format!("== 卸载插件 {name} ==") }),
    );
    crate::updater::run_streaming(
        &app,
        &kernel_dir,
        &path_env,
        &pnpm,
        &["dsh", "plugin", "--profile", "web", "remove", &name],
    )
    .await?;
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": "== 插件已卸载，重启内核后生效 ==" }),
    );
    Ok(())
}

/// Sidecar plugins auto-installed on first kernel start. These ship with the
/// shell so the out-of-box experience matches the README screenshots:
/// dsh-better-sidebar gives the embedded UI a full workspace sidebar
/// (file tree / editor / terminal / git).
const SIDECAR_PLUGINS: &[&str] = &["dsh-better-sidebar"];

/// Ensure every sidecar plugin is installed in the web profile. Idempotent:
/// already-installed plugins are skipped. Safe to call before every kernel
/// start — it only does work the first time. Never fails the start: an
/// install error is logged and ignored so the kernel still comes up.
pub async fn ensure_sidecar_plugins(
    manager: &KernelManager,
    app: &AppHandle,
) -> Result<(), String> {
    let profile = profile_dir();
    if !profile.is_dir() {
        // The web profile is created by the kernel on first boot; nothing to
        // do until it exists. The next start after a kernel run will install.
        eprintln!("[plugins] web profile missing, skipping sidecar install");
        return Ok(());
    }
    let kernel_dir = manager.kernel_dir();
    let Some(kernel_dir) = kernel_dir else {
        eprintln!("[plugins] no kernel dir, skipping sidecar install");
        return Ok(());
    };
    if !crate::kernel::is_kernel_dir(&kernel_dir) {
        return Ok(());
    }

    // Which of the bundled plugins are missing from package.json deps?
    let pkg_path = profile.join("package.json");
    let pkg_raw = std::fs::read_to_string(&pkg_path).unwrap_or_default();
    let deps: std::collections::HashSet<String> = serde_json::from_str::<Value>(&pkg_raw)
        .ok()
        .and_then(|v| v.get("dependencies").and_then(|d| d.as_object()).cloned())
        .map(|d| d.keys().cloned().collect())
        .unwrap_or_default();
    let missing: Vec<&str> = SIDECAR_PLUGINS
        .iter()
        .copied()
        .filter(|p| !deps.contains(*p))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    // First-time setup for node-pty-style plugins: pnpm 11 blocks build
    // scripts and freshly-published versions unless opted in. Write both
    // grants directly (pnpm approve-builds is interactive, unusable here).
    let ws_path = profile.join("pnpm-workspace.yaml");
    let ws = std::fs::read_to_string(&ws_path).unwrap_or_default();
    let mut changed = false;
    if !ws.contains("allowBuilds:") {
        std::fs::write(
            &ws_path,
            format!("{ws}\nallowBuilds:\n  node-pty: true\n"),
        )
        .map_err(|e| format!("写入 pnpm-workspace.yaml 失败: {e}"))?;
        changed = true;
    }
    if !ws.contains("minimumReleaseAgeExclude") {
        let ws_now = std::fs::read_to_string(&ws_path).unwrap_or_default();
        std::fs::write(
            &ws_path,
            format!("{ws_now}\nminimumReleaseAgeExclude:\n  - dsh-better-sidebar\n"),
        )
        .map_err(|e| format!("写入 pnpm-workspace.yaml 失败: {e}"))?;
        changed = true;
    }
    if changed {
        eprintln!("[plugins] patched pnpm-workspace.yaml for sidecar plugins");
    }

    // Install missing sidecar plugins one by one.
    let (pnpm, path_env) = crate::kernel::resolve_toolchain().await?;
    for name in &missing {
        let _ = app.emit(
            "kernel-log",
            serde_json::json!({ "stream": "out", "line": format!("== 自动安装插件 {name} ==（首次启动，仅此一次）") }),
        );
        eprintln!("[plugins] installing sidecar {name}");
        let r = crate::updater::run_streaming(
            app,
            &kernel_dir,
            &path_env,
            &pnpm,
            &["dsh", "plugin", "--profile", "web", "add", name],
        )
        .await;
        if let Err(e) = r {
            eprintln!("[plugins] sidecar {name} install failed (ignored): {e}");
            let _ = app.emit(
                "kernel-log",
                serde_json::json!({ "stream": "err", "line": format!("插件 {name} 自动安装失败: {e}") }),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dir_points_at_web_profile() {
        let d = profile_dir();
        assert!(d.ends_with(".dsh/profiles/web") || d.ends_with("profiles/web"));
        let _ = Path::new(".");
    }
}
