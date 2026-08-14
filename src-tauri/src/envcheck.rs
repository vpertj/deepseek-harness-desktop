use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Toolchain requirements for the dsh kernel: node ^22.19 || >=24, pnpm.
/// Detection probes PATH plus the usual macOS install locations (mise shims,
/// homebrew, npm global), mirroring kernel::resolve_toolchain's strategy.
/// Installation prefers mise (user's toolchain manager), falling back to
/// Homebrew, then enables corepack for pnpm.

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
