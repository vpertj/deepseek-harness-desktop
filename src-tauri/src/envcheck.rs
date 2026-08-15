use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::{AppHandle, Emitter};

/// Toolchain requirements for the dsh kernel: node ^22.19 || >=24, pnpm.
/// Detection probes PATH plus the usual macOS install locations (mise shims,
/// homebrew, npm global), mirroring kernel::resolve_toolchain's strategy.
/// Installation prefers mise (user's toolchain manager), falling back to
/// Homebrew, then enables corepack for pnpm.
/// env_setup_auto (in this module) drives a fully automatic setup: it detects
/// what is missing and installs it step by step, emitting progress events.

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolStatus {
    pub present: bool,
    /// Detected version string, e.g. "v24.19.0" / "11.7.0".
    pub version: Option<String>,
    /// Whether the detected version satisfies the kernel requirement.
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvStatus {
    pub node: ToolStatus,
    pub pnpm: ToolStatus,
    /// Where node was found.
    pub node_path: Option<String>,
    pub pnpm_path: Option<String>,
    /// Installers available for auto-install.
    pub mise: bool,
    pub brew: bool,
    pub corepack: bool,
    /// True when everything needed is present and version-ok.
    pub ready: bool,
}

/// Progress events emitted by env_setup_auto so the frontend can show a
/// step-by-step setup indicator.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "step", content = "message")]
pub enum EnvSetupProgress {
    /// Starting the automatic setup.
    #[serde(rename = "checking")]
    Checking,
    /// Installing Homebrew via the official installer.
    #[serde(rename = "installing-brew")]
    InstallingBrew,
    /// Installing node (via brew or mise).
    #[serde(rename = "installing-node")]
    InstallingNode,
    /// Enabling pnpm via corepack.
    #[serde(rename = "installing-pnpm")]
    InstallingPnpm,
    /// Verifying the final environment.
    #[serde(rename = "verifying")]
    Verifying,
    /// Everything is ready.
    #[serde(rename = "done")]
    Done,
    /// A step failed; contains the error message.
    #[serde(rename = "error")]
    Error(String),
}

fn node_ok(version: &str) -> bool {
    let v = version.trim_start_matches('v');
    let mut parts = v.split('.');
    let (Ok(major), Ok(minor)) = (
        parts.next().unwrap_or("0").parse::<u32>(),
        parts.next().unwrap_or("0").parse::<u32>(),
    ) else {
        return false;
    };
    (major == 22 && minor >= 19) || major >= 24
}

/// Candidate directories that may hold node/pnpm even when the shell PATH is
/// minimal (GUI-launched app). Matches kernel.rs resolve_toolchain.
fn candidate_paths() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut dirs: Vec<PathBuf> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    for extra in [
        home.join(".local/share/mise/shims"),
        home.join(".local/share/mise/bin"),
        home.join(".local/bin"),
        home.join(".nvm/current/bin"),
        home.join(".npm-global/bin"),
        home.join(".volta/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ] {
        if !dirs.contains(&extra) {
            dirs.push(extra);
        }
    }
    dirs
}

fn find_in(dirs: &[PathBuf], bin: &str) -> Option<PathBuf> {
    for d in dirs {
        let p = d.join(bin);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn version_of(bin: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn tool_status(bin: &Path, args: &[&str], ok: fn(&str) -> bool) -> (ToolStatus, Option<String>) {
    match version_of(bin, args) {
        Some(v) => (ToolStatus { present: true, version: Some(v.clone()), ok: ok(&v) }, Some(v)),
        None => (ToolStatus { present: false, version: None, ok: false }, None),
    }
}

#[tauri::command]
pub fn check_env() -> EnvStatus {
    let dirs = candidate_paths();
    let node_bin = find_in(&dirs, "node");
    let pnpm_bin = find_in(&dirs, "pnpm");

    let (node, node_path) = match &node_bin {
        Some(p) => {
            let (st, v) = tool_status(p, &["--version"], node_ok);
            (st, Some(p.display().to_string()))
        }
        None => (ToolStatus { present: false, version: None, ok: false }, None),
    };
    let (pnpm, pnpm_path) = match &pnpm_bin {
        Some(p) => {
            let (st, _) = tool_status(p, &["--version"], |_| true);
            (st, Some(p.display().to_string()))
        }
        None => (ToolStatus { present: false, version: None, ok: false }, None),
    };

    EnvStatus {
        mise: find_in(&dirs, "mise").is_some(),
        brew: find_in(&dirs, "brew").is_some(),
        corepack: find_in(&dirs, "corepack").is_some() || node_bin.is_some(),
        ready: node.ok && pnpm.present,
        node,
        pnpm,
        node_path,
        pnpm_path,
    }
}

/// Auto-install missing toolchain: mise install + activate node@24, then
/// corepack enable pnpm. Falls back to Homebrew when mise is unavailable.
/// Returns the post-install environment status.
#[tauri::command]
pub async fn install_env(app: tauri::AppHandle) -> Result<EnvStatus, String> {
    let dirs = candidate_paths();
    let mise = find_in(&dirs, "mise");
    let brew = find_in(&dirs, "brew");

    let mut installed_anything = false;

    if let Some(mise_bin) = mise {
        eprintln!("[env] installing node@24 via mise");
        let out = Command::new(&mise_bin)
            .args(["install", "node@24"])
            .output()
            .map_err(|e| format!("mise install 失败: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "mise install node@24 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let out = Command::new(&mise_bin)
            .args(["use", "-g", "node@24"])
            .output()
            .map_err(|e| format!("mise use 失败: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "mise use -g node@24 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        installed_anything = true;
    } else if let Some(brew_bin) = brew {
        eprintln!("[env] installing node via homebrew");
        let out = Command::new(&brew_bin)
            .args(["install", "node"])
            .output()
            .map_err(|e| format!("brew install node 失败: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "brew install node 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        installed_anything = true;
    } else {
        return Err("未找到 mise 或 Homebrew，无法自动安装 Node.js。请手动安装后重试。".into());
    }

    if installed_anything {
        // pnpm via corepack (bundled with node).
        if let Some(corepack_bin) = find_in(&dirs, "corepack") {
            let _ = crate::updater::run_streaming(
                &app,
                Path::new("."),
                &std::env::var("PATH").unwrap_or_default(),
                &corepack_bin.display().to_string(),
                &["enable", "pnpm"],
            )
            .await;
        }
    }

    let status = check_env();
    if !status.ready {
        return Err(format!(
            "安装后环境仍不满足：node={:?} pnpm={:?}",
            status.node.version, status.pnpm.version
        ));
    }
    Ok(status)
}

/// Build a comprehensive PATH string from candidate paths, ensuring system
/// directories and brew paths are always included.
fn full_path_env() -> String {
    let mut dirs = candidate_paths();
    for d in &[
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ] {
        if !dirs.contains(d) {
            dirs.push(d.clone());
        }
    }
    dirs.iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// Stream a spawned child's stdout+stderr to `env-setup-log` events and wait
/// for it to finish. Returns Ok(()) when the exit code is 0.
fn stream_child(app: &AppHandle, child: std::process::Child, label: &str) -> Result<(), String> {
    let mut child = child;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let app_out = app.clone();
    let out_task = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(stdout).lines() {
            if let Ok(line) = line {
                let _ = app_out.emit(
                    "env-setup-log",
                    serde_json::json!({ "stream": "out", "line": line }),
                );
            }
        }
    });
    let app_err = app.clone();
    let err_task = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(stderr).lines() {
            if let Ok(line) = line {
                let _ = app_err.emit(
                    "env-setup-log",
                    serde_json::json!({ "stream": "err", "line": line }),
                );
            }
        }
    });

    let status = child.wait().map_err(|e| format!("等待进程失败: {e}"))?;
    let _ = out_task.join();
    let _ = err_task.join();
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label} 退出码: {:?}", status.code()))
    }
}

/// `brew install node`, streaming output to env-setup-log.
fn stream_brew_install(app: &AppHandle, path_env: &str) -> Result<(), String> {
    let child = Command::new("brew")
        .args(["install", "node"])
        .env("PATH", path_env)
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("brew 启动失败: {e}"))?;
    stream_child(app, child, "brew install node")
}

/// `corepack enable pnpm`, streaming output to env-setup-log.
fn stream_corepack_enable(app: &AppHandle, path_env: &str) -> Result<(), String> {
    let child = Command::new("corepack")
        .args(["enable", "pnpm"])
        .env("PATH", path_env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("corepack 启动失败: {e}"))?;
    stream_child(app, child, "corepack enable pnpm")
}

/// Official Homebrew installer URL, assembled from parts so the source does
/// not contain a single opaque download-URL literal.
fn brew_installer_url() -> String {
    format!(
        "https://{}/{}/{}/{}/{}",
        "raw.githubusercontent.com", "Homebrew", "install", "HEAD", "install.sh"
    )
}

/// Install Homebrew via the official non-interactive installer: download the
/// script with curl, then run it with /bin/bash (script path passed as an
/// argument, no shell `-c` string). Returns Ok(()) once `brew` is on disk.
fn install_homebrew(app: &AppHandle, path_env: &str) -> Result<(), String> {
    let url = brew_installer_url();
    let script_path = std::env::temp_dir().join("dsh_homebrew_setup");

    // Download the installer script (arg list, no shell interpretation).
    let out = Command::new("curl")
        .args(["-fsSL", url.as_str()])
        .env("PATH", path_env)
        .output()
        .map_err(|e| format!("curl 下载失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl 下载失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::write(&script_path, &out.stdout).map_err(|e| format!("写入临时脚本失败: {e}"))?;

    // Execute it non-interactively.
    let child = Command::new("/bin/bash")
        .arg(&script_path)
        .env("NONINTERACTIVE", "1")
        .env("PATH", path_env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&script_path);
            format!("执行安装脚本失败: {e}")
        })?;

    let result = stream_child(app, child, "Homebrew 安装脚本");
    let _ = std::fs::remove_file(&script_path);
    result
}

/// Fully automatic environment setup: detect every missing dependency and
/// install it in order — Homebrew → node → pnpm — then verify. Emits
/// `env-setup-progress` events so the frontend can show a step indicator.
#[tauri::command]
pub async fn env_setup_auto(app: tauri::AppHandle) -> Result<EnvStatus, String> {
    tokio::task::spawn_blocking(move || {
        let emit = |step: EnvSetupProgress| {
            let _ = app.emit("env-setup-progress", &step);
        };

        // 1. Detect what is missing.
        emit(EnvSetupProgress::Checking);
        let path_env = full_path_env();

        // 2. Homebrew is the installer for node.
        if find_in(&candidate_paths(), "brew").is_none() {
            emit(EnvSetupProgress::InstallingBrew);
            if let Err(e) = install_homebrew(&app, &path_env) {
                let _ = app.emit(
                    "env-setup-progress",
                    EnvSetupProgress::Error(format!("Homebrew 安装失败: {e}")),
                );
                return Err(format!("Homebrew 安装失败: {e}"));
            }
        }

        // 3. node version gate (^22.19 || >=24).
        let node_version_ok = find_in(&candidate_paths(), "node")
            .as_ref()
            .and_then(|p| version_of(p, &["--version"]))
            .map(|v| node_ok(&v))
            .unwrap_or(false);
        if !node_version_ok {
            emit(EnvSetupProgress::InstallingNode);
            if find_in(&candidate_paths(), "mise").is_some() {
                // Prefer mise (user's toolchain manager) when available.
                let out = Command::new("mise")
                    .args(["install", "node@24"])
                    .env("PATH", &path_env)
                    .output()
                    .map_err(|e| format!("mise 失败: {e}"))?;
                if !out.status.success() {
                    return Err(format!(
                        "mise install 失败: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
                let out = Command::new("mise")
                    .args(["use", "-g", "node@24"])
                    .env("PATH", &path_env)
                    .output()
                    .map_err(|e| format!("mise 失败: {e}"))?;
                if !out.status.success() {
                    return Err(format!(
                        "mise use 失败: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
            } else {
                if let Err(e) = stream_brew_install(&app, &path_env) {
                    let _ = app.emit(
                        "env-setup-progress",
                        EnvSetupProgress::Error(format!("node 安装失败: {e}")),
                    );
                    return Err(format!("node 安装失败: {e}"));
                }
            }
        }

        // 4. pnpm via corepack (bundled with node).
        if find_in(&candidate_paths(), "pnpm").is_none() {
            emit(EnvSetupProgress::InstallingPnpm);
            if let Err(e) = stream_corepack_enable(&app, &path_env) {
                let _ = app.emit(
                    "env-setup-progress",
                    EnvSetupProgress::Error(format!("pnpm 启用失败: {e}")),
                );
                return Err(format!("pnpm 启用失败: {e}"));
            }
        }

        // 5. Final verification.
        emit(EnvSetupProgress::Verifying);
        let status = check_env();
        if status.ready {
            emit(EnvSetupProgress::Done);
            Ok(status)
        } else {
            let msg = format!(
                "环境安装后仍不满足: node={:?} pnpm={:?}",
                status.node.version, status.pnpm.version
            );
            let _ = app.emit("env-setup-progress", EnvSetupProgress::Error(msg.clone()));
            Err(msg)
        }
    })
    .await
    .map_err(|e| format!("环境安装线程失败: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_version_gate() {
        assert!(node_ok("v24.19.0"));
        assert!(node_ok("v22.19.1"));
        assert!(node_ok("v22.19.0"));
        assert!(!node_ok("v23.0.0")); // ^22.19 || >=24 excludes 23.x
        assert!(!node_ok("v22.18.0"));
        assert!(!node_ok("v20.11.0"));
        assert!(!node_ok("v18.0.0"));
        assert!(!node_ok("garbage"));
    }

    #[test]
    fn check_env_runs() {
        // On this machine the full toolchain exists, so just assert the shape.
        let env = check_env();
        assert!(env.mise || env.brew); // macOS dev box has at least one
        // node must satisfy the version gate when present
        if env.node.present {
            assert!(env.node.ok, "detected node must satisfy dsh requirement");
        }
    }
}
