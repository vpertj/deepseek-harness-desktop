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
