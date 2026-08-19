use include_dir::{include_dir, Dir};
use std::path::PathBuf;
use std::process::Command;

/// Embedded dsh-memory-evolve plugin bundle (source — no node_modules).
const BUNDLED_PLUGIN: Dir =
    include_dir!("$CARGO_MANIFEST_DIR/resources/dsh-memory-evolve");

fn profile_dir() -> PathBuf {
    let home = std::env::var("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".dsh"));
    home.join("profiles").join("web")
}

/// The target directory where the plugin source lives before being installed
/// into the web profile's node_modules via `dsh plugin add`.
fn bundle_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::env::temp_dir())
        .join("deepseek-harness")
        .join("memory-evolve-bundle")
}

/// Marker file placed after successful install so we don't re-extract every
/// launch.
fn marker_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".dsh")
        .join("memory-evolve-installed")
}

/// Ensure the web profile's package.json has the two pnpm grants needed for
/// build-time native modules (same logic as sidecar install).
fn ensure_pnpm_workarounds(profile_dir: &std::path::Path) -> std::io::Result<()> {
    let ws_path = profile_dir.join("pnpm-workspace.yaml");
    let ws = std::fs::read_to_string(&ws_path).unwrap_or_default();
    if ws.contains("allowBuilds:") && ws.contains("minimumReleaseAgeExclude") {
        return Ok(());
    }
    let mut content = ws.to_string();
    if !content.contains("allowBuilds:") {
        content.push_str("\nallowBuilds:\n  node-pty: true\n");
    }
    if !content.contains("minimumReleaseAgeExclude") {
        content.push_str("minimumReleaseAgeExclude:\n  - dsh-memory-evolve\n");
    }
    std::fs::write(&ws_path, content)
}

/// Install the embedded dsh-memory-evolve plugin into the web profile on first
/// launch. Idempotent: skips if marker exists. Safe to call before every kernel
/// start. Returns Ok even on error — never blocks the app.
pub async fn ensure_installed() -> Result<(), String> {
    let marker = marker_file();
    if marker.exists() {
        return Ok(());
    }

    // Extract the bundled plugin to a temp dir.
    let bundle = bundle_dir();
    std::fs::create_dir_all(&bundle).map_err(|e| format!("创建缓存目录失败: {e}"))?;
    BUNDLED_PLUGIN
        .extract(&bundle)
        .map_err(|e| format!("解压内存插件失败: {e}"))?;

    let profile = profile_dir();
    ensure_pnpm_workarounds(&profile).map_err(|e| format!("写入 pnpm-workspace.yaml 失败: {e}"))?;

    // Resolve kernel dir and toolchain.
    let settings = crate::config::Settings::load();
    let kernel_dir = settings
        .effective_kernel_dir()
        .ok_or("内核目录未配置".to_string())?;
    if !crate::kernel::is_kernel_dir(&kernel_dir) {
        return Err("内核目录无效".into());
    }
    let (pnpm, path_env) = crate::kernel::resolve_toolchain().await?;

    // Run `dsh plugin --profile web add <bundle-path>`.
    let status = Command::new(pnpm)
        .args([
            "exec",
            "--",
            "dsh",
            "plugin",
            "--profile",
            "web",
            "add",
            bundle.to_string_lossy().as_ref(),
        ])
        .current_dir(&kernel_dir)
        .env("PATH", &path_env)
        .output()
        .map_err(|e| format!("执行 dsh plugin add 失败: {e}"))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(format!("内存插件安装失败: {stderr}"));
    }

    // Mark installed so we don't repeat on next launch.
    std::fs::create_dir_all(marker.parent().unwrap())
        .map_err(|e| format!("写入安装标记失败: {e}"))?;
    std::fs::write(&marker, "installed")
        .map_err(|e| format!("写入安装标记失败: {e}"))?;

    eprintln!("[memory-evolve] plugin installed successfully");
    Ok(())
}
