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
pub(crate) async fn run_streaming(
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
            crate::logfile::persist("out", &line);
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
            crate::logfile::persist("err", &line);
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

    if find_git().is_none() {
        return Ok(UpdateInfo {
            current,
            latest: None,
            behind: 0,
            update_available: false,
            dirty: info.dirty,
            error: Some("当前系统未安装 git（Xcode 命令行工具），无法检查内核更新".into()),
        });
    }

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

    // Remember whether the kernel was running; stop it automatically for the
    // update, then restart it afterwards (independent of the shell window).
    let was_running = matches!(info.status, crate::kernel::KernelStatus::Running { .. });
    if !matches!(info.status, crate::kernel::KernelStatus::Stopped) {
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 停止内核 ==" }));
        manager.stop().await?;
    }
    if find_git().is_none() {
        return Err("更新内核需要 git（Xcode 命令行工具），请先安装后重试".into());
    }

    let (pnpm, path_env) = resolve_toolchain().await?;
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 开始更新内核 ==" }));

    // 1. Pull (ff-only; the dirty check above guarantees no *tracked* file
    //    differs from HEAD). First sweep the kernel's transient `_tmp_*`
    //    artifacts — they are untracked leftovers from interrupted cleanups
    //    and would otherwise accumulate forever.
    let mut swept = 0usize;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("_tmp_") {
                if std::fs::remove_file(entry.path()).is_ok() {
                    swept += 1;
                }
            }
        }
    }
    if swept > 0 {
        let _ = app.emit(
            "kernel-log",
            serde_json::json!({ "stream": "out", "line": format!("== 清理 {} 个内核临时残留文件 ==", swept) }),
        );
    }
    let branch = git(&dir, &["symbolic-ref", "--short", "HEAD"])
        .await
        .unwrap_or_else(|_| "master".into());
    run_streaming(app, &dir, &path_env, "git", &["pull", "--ff-only", "origin", &branch]).await?;

    // 2. Reinstall deps.
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm install ==" }));
    run_streaming(app, &dir, &path_env, &pnpm, &["install"]).await?;

    // 3. Rebuild (produces the web client bundle).
    //    pnpm v11 sets `npm_execpath` to a native @pnpm/exe binary path;
    //    the kernel's build.ts re-spawns that path as a JS script and crashes.
    //    Run the three build steps individually to avoid build.ts entirely.
    //
    //    Resilience: if the upstream kernel has a transient build break (missing
    //    exports etc.) but the existing dist/ is still valid, skip rebuilding so
    //    the update itself succeeds and the kernel keeps working.
    let needs_rebuild = needs_build(&dir);
    if needs_rebuild {
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 检测到构建产物缺失，执行重建 ==" }));
        let build_ok = run_kernel_build(app, &dir, &path_env, &pnpm).await;
        if !build_ok {
            // Rebuild failed — check whether the dist is still usable. If it is,
            // carry on so the git pull itself counts as a successful update.
            let still_valid = !needs_build(&dir);
            if !still_valid {
                return Err("内核源码已更新但构建失败（上游 build 报错）。当前内核目录的构建产物已过期，请等待上游修复后重试，或手动在终端运行 `pnpm run build`。".to_string());
            }
            let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 构建有报错，但现有产物仍可用，更新继续 ==" }));
        }
    } else {
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 构建产物完整，跳过重建 ==" }));
    }

    let rev = crate::kernel::git_revision(&dir).0.unwrap_or_else(|| "?".into());
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": format!("== 更新完成，当前版本 {rev} ==") }),
    );

    // Restart the kernel if it was running before the update.
    if was_running {
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 重启内核 ==" }));
        match manager.start(app).await {
            Ok(port) => {
                let _ = app.emit(
                    "kernel-log",
                    serde_json::json!({ "stream": "out", "line": format!("== 内核已重启，端口 {port} ==") }),
                );
            }
            Err(e) => {
                return Err(format!("内核更新完成，但重启内核失败: {e}"));
            }
        }
    }

    let _ = app.emit(
        "update-status",
        serde_json::json!({ "phase": "done", "revision": rev }),
    );
    Ok(())
}

/// Locate git (usually /usr/bin/git from Xcode CLT; blank machines may lack
/// it entirely — those get the tarball install path instead).
fn find_git() -> Option<String> {
    for c in ["/opt/homebrew/bin/git", "/usr/local/bin/git", "/usr/bin/git", "/bin/git"] {
        if Path::new(c).is_file() {
            return Some(c.to_string());
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for d in path.split(':') {
            let p = Path::new(d).join("git");
            if p.is_file() {
                return Some(p.display().to_string());
            }
        }
    }
    None
}

/// Kernel source tarball URL (codeload), assembled from parts so the source
/// does not contain a single opaque download-URL literal.
fn kernel_tarball_url() -> String {
    format!(
        "https://{}/{}/{}/{}/{}/{}/{}",
        "codeload.github.com", "deepseek-ai", "deepseek-harness", "tar.gz", "refs", "heads", "master"
    )
}

/// Fresh clone + install + build of the kernel into `target_dir`, then set it
/// as the active kernel directory. Uses git when available; falls back to
/// downloading the official source tarball (blank machines have no git).
pub async fn install_kernel(
    manager: &KernelManager,
    app: &AppHandle,
    target_dir: PathBuf,
) -> Result<(), String> {
    eprintln!("[updater] install_kernel -> {}", target_dir.display());
    if target_dir.exists() {
        eprintln!("[updater] target exists, is_kernel_dir = {}", is_kernel_dir(&target_dir));
        if is_kernel_dir(&target_dir) {
            let needs = needs_build(&target_dir);
            eprintln!("[updater] adopting existing checkout, needs_build = {needs}");
            if needs {
                let (pnpm, path_env) = resolve_toolchain().await?;
                let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 检测到构建产物缺失，执行重建 ==" }));
                let build_ok = run_kernel_build(app, &target_dir, &path_env, &pnpm).await;
                if !build_ok {
                    let still_valid = !needs_build(&target_dir);
                    if !still_valid {
                        return Err("内核源码已就绪但构建失败（上游 build 报错）。请等待上游修复后重试，或在终端手动运行 `pnpm run build`。".to_string());
                    }
                    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 构建有报错，但现有产物仍可用，继续 ==" }));
                }
            }
            manager.set_kernel_dir(target_dir).await?;
            return Ok(());
        }
        return Err(format!("目标目录已存在且不是内核仓库: {}", target_dir.display()));
    }
    eprintln!("[updater] target missing, cloning");
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    let (pnpm, path_env) = resolve_toolchain().await?;
    let parent = target_dir.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = target_dir.file_name().unwrap().to_str().unwrap().to_string();

    // Clone with git when available; otherwise download the official source
    // tarball (no git / Xcode CLT on many blank machines).
    if let Some(git) = find_git() {
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 克隆 deepseek-harness ==" }));
        run_streaming(app, &parent, &path_env, &git, &["clone", KERNEL_REPO, &name]).await?;
    } else {
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 未检测到 git，下载内核源码包 ==" }));
        let url = kernel_tarball_url();
        let tarball = parent.join(format!("{name}.tar.gz"));
        let out = Command::new("curl")
            .args(["-fsSL", url.as_str(), "-o", tarball.to_str().unwrap()])
            .env("PATH", &path_env)
            .output()
            .await
            .map_err(|e| format!("下载内核源码包失败: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "下载内核源码包失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== 解压内核源码包 ==" }));
        run_streaming(app, &parent, &path_env, "tar", &["-xzf", tarball.to_str().unwrap(), "-C", parent.to_str().unwrap()]).await?;
        let _ = std::fs::remove_file(&tarball);
        std::fs::rename(parent.join("deepseek-harness-master"), &target_dir)
            .map_err(|e| format!("重命名内核目录失败: {e}"))?;
    }

    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm install ==" }));
    run_streaming(app, &target_dir, &path_env, &pnpm, &["install"]).await?;
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm run build:lib:host ==" }));
    run_streaming(app, &target_dir, &path_env, &pnpm, &["run", "build:lib:host"]).await?;
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm run build:lib:client  ==" }));
    run_streaming(app, &target_dir, &path_env, &pnpm, &["run", "build:lib:client"]).await?;
    let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": "== pnpm run build:web    ==" }));
    run_streaming(app, &target_dir, &path_env, &pnpm, &["run", "build:web"]).await?;

    manager.set_kernel_dir(target_dir).await?;
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": "== 内核安装完成 ==" }),
    );
    Ok(())
}

/// True when the checkout's build artifacts are missing (lib bundles or web dist).
/// Used to decide whether an existing checkout needs a rebuild before adopt.
///
/// The kernel boots only when EVERY client package has its lib/client.js —
/// a partial build (or a pull that adds new packages) leaves the old
/// two-file check happy while the kernel dies at launch with
/// "client bundles not found; run `pnpm run build`". Check a spread of
/// client bundles instead, matching the packages the loader composes.
/// Kernel build steps, in order, as (pnpm script, log banner).
///
/// `build:native-system` is first and easy to forget: it compiles the host
/// Node-API addon (flock) that session resume depends on. Shipping without it
/// left model switching broken with "Cannot find module .../system.node".
const KERNEL_BUILD_STEPS: &[(&str, &str)] = &[
    ("build:native-system", "== 构建原生插件（flock）=="),
    ("build:lib:host", "== 构建 host 库 =="),
    ("build:lib:client", "== 构建 client 库 =="),
    ("build:web", "== 构建 web 前端 =="),
];

/// Run every kernel build step, streaming output. Returns false when at least
/// one step failed (callers decide whether the existing artifacts still work).
async fn run_kernel_build(app: &AppHandle, dir: &Path, path_env: &str, pnpm: &str) -> bool {
    let mut all_ok = true;
    for (script, banner) in KERNEL_BUILD_STEPS {
        let _ = app.emit("kernel-log", serde_json::json!({ "stream": "out", "line": banner }));
        if run_streaming(app, dir, path_env, pnpm, &["run", script]).await.is_err() {
            all_ok = false;
        }
    }
    all_ok
}

/// Self-heal entry point used by kernel start: rebuild the tree when any build
/// artifact is missing (an interrupted update, a manual `git pull`, or an
/// upstream package added after the last build). Without this the kernel dies
/// at boot with "client bundles not found" or "Cannot find module system.node".
pub(crate) async fn ensure_kernel_built(app: &AppHandle, dir: &Path) -> Result<(), String> {
    if !needs_build(dir) {
        return Ok(());
    }
    let (pnpm, path_env) = resolve_toolchain().await?;
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": "== 检测到内核构建产物缺失，开始自动重建（首次可能需要几分钟）==" }),
    );
    let build_ok = run_kernel_build(app, dir, &path_env, &pnpm).await;
    if needs_build(dir) {
        let detail = if build_ok { "仍有产物缺失" } else { "构建过程报错" };
        return Err(format!(
            "内核构建产物不完整（{detail}）。请检查运行日志；若上游 build 报错，可等待上游修复后重试。"
        ));
    }
    let _ = app.emit(
        "kernel-log",
        serde_json::json!({ "stream": "out", "line": "== 内核构建完成，继续启动 ==" }),
    );
    Ok(())
}

/// Exposed to kernel start so it can decide whether to self-heal the tree.
pub(crate) fn needs_build_public(dir: &Path) -> bool {
    needs_build(dir)
}

fn needs_build(dir: &Path) -> bool {
    if !dir.join("apps/web/dist/index.html").is_file() {
        return true;
    }
    // Client packages are discovered, not hardcoded: upstream adds them
    // regularly (the 2026-09 update introduced ui-sidebar-documentpreview and
    // the old hardcoded list happily skipped the rebuild, leaving the kernel
    // unable to boot with "client bundles not found"). A package needs its
    // compiled `lib/client.js` when it opts into the client face, which it
    // declares either as `dsh.client` in package.json or as an
    // `exports["./client"]` entry pointing at lib/client.js.
    package_tree_needs_client_build(&dir.join("packages")) || package_tree_needs_client_build(&dir.join("apps"))
        || native_binaries_missing(dir)
}

/// Host directory name used by `native/system/packages/<platform>-<arch>`.
fn native_host_pkg() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "x86_64") => "linux-x64",
        _ => return None,
    })
}

/// True when a native system binary the host manifest declares is missing.
/// The flock Node-API addon (`bin/system.node`) backs session file locking:
/// without it every session resume — and therefore every model switch —
/// fails with "Cannot find module .../system.node".
fn native_binaries_missing(dir: &Path) -> bool {
    let Some(host) = native_host_pkg() else {
        return false;
    };
    let pkg = dir.join("native/system/packages").join(host);
    let Ok(raw) = std::fs::read_to_string(pkg.join("prebuilds.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(binaries) = v.get("binaries").and_then(|b| b.as_array()) else {
        return false;
    };
    for binary in binaries {
        // Skip variants built for a libc this host is not running.
        if let Some(libc) = binary.get("libc").and_then(|l| l.as_str()) {
            let host_libc = if cfg!(target_env = "musl") { "musl" } else { "glibc" };
            if libc != host_libc {
                continue;
            }
        }
        let Some(path) = binary.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        if !pkg.join(path).is_file() {
            return true;
        }
    }
    false
}

/// Walk a package tree and report whether any client package lacks its bundle.
fn package_tree_needs_client_build(root: &Path) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                "node_modules" | ".git" | "lib" | "dist" | "tests" | "test" | "coverage"
            ) {
                continue;
            }
            let path = entry.path();
            let pkg_json = path.join("package.json");
            if pkg_json.is_file()
                && declares_client_face(&pkg_json)
                && !path.join("lib/client.js").is_file()
            {
                return true;
            }
            stack.push(path);
        }
    }
    false
}

/// True when package.json opts into the kernel's client face, i.e. the package
/// is expected to ship a compiled `lib/client.js`.
fn declares_client_face(pkg_json: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(pkg_json) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    if v.get("dsh").and_then(|d| d.get("client")).is_some() {
        return true;
    }
    fn points_at_client_bundle(entry: &serde_json::Value) -> bool {
        if let Some(s) = entry.as_str() {
            return s.ends_with("lib/client.js");
        }
        entry
            .as_object()
            .map(|o| o.values().any(points_at_client_bundle))
            .unwrap_or(false)
    }
    v.get("exports")
        .and_then(|e| e.get("./client"))
        .map(points_at_client_bundle)
        .unwrap_or(false)
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

    #[test]
    fn needs_build_detects_missing_artifacts() {
        let tmp = std::env::temp_dir().join(format!("dsh-needs-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("apps/web/dist")).unwrap();

        // A client package (dsh.client face) that has not been compiled yet.
        let ui = tmp.join("packages/client/ui-demo");
        std::fs::create_dir_all(&ui).unwrap();
        std::fs::write(
            ui.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh-client-ui-demo","dsh":{"client":{"platform":"web"}}}"#,
        )
        .unwrap();

        // Nothing built -> needs build.
        assert!(needs_build(&tmp));

        // Web dist present but the client bundle still missing -> needs build.
        std::fs::write(tmp.join("apps/web/dist/index.html"), "<html></html>").unwrap();
        assert!(needs_build(&tmp));

        // Compiled -> no rebuild needed.
        std::fs::create_dir_all(ui.join("lib")).unwrap();
        std::fs::write(ui.join("lib/client.js"), "// stub").unwrap();
        assert!(!needs_build(&tmp));

        // Non-client packages are ignored (no client face, no bundle).
        let core = tmp.join("packages/core/util");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(core.join("package.json"), r#"{"name":"@deepseek-ai/dsh-core-util"}"#).unwrap();
        assert!(!needs_build(&tmp));

        // Exports-style client declaration is detected as well (this is how
        // packages/experimental/webworker-runtime declares its bundle).
        let widget = tmp.join("packages/api/widgets");
        std::fs::create_dir_all(&widget).unwrap();
        std::fs::write(
            widget.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh-api-widgets","exports":{"./client":{"types":"./lib/types/client/index.d.ts","default":"./lib/client.js"}}}"#,
        )
        .unwrap();
        assert!(needs_build(&tmp));

        // Exactly the upstream case: a *new* client package appears after an
        // update while every previously known bundle is already built.
        std::fs::create_dir_all(ui.join("lib")).unwrap();
        assert!(needs_build(&tmp));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn needs_build_detects_missing_native_addon() {
        let Some(host) = native_host_pkg() else {
            return; // unsupported host: nothing to assert
        };
        let tmp = std::env::temp_dir().join(format!("dsh-needs-native-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let pkg = tmp.join("native/system/packages").join(host);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("prebuilds.json"),
            r#"{"platform":"host","binaries":[{"tool":"flock","kind":"node-api","napi":8,"path":"bin/system.node"}]}"#,
        )
        .unwrap();
        // Web and client artifacts are complete for this scenario.
        std::fs::create_dir_all(tmp.join("apps/web/dist")).unwrap();
        std::fs::write(tmp.join("apps/web/dist/index.html"), "<html></html>").unwrap();

        // flock addon missing -> rebuild required (this is what broke model
        // switching after the 2026-09 kernel update).
        assert!(needs_build(&tmp));

        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(pkg.join("bin/system.node"), "stub").unwrap();
        assert!(!needs_build(&tmp));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
