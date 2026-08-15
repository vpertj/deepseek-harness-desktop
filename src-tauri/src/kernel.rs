use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Status model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum KernelStatus {
    Stopped,
    Starting,
    Running { port: u16 },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct KernelInfo {
    pub status: KernelStatus,
    pub kernel_dir: Option<PathBuf>,
    /// Current git HEAD short sha of the kernel checkout, if any.
    pub revision: Option<String>,
    /// True when the working tree is dirty (local modifications).
    pub dirty: bool,
    /// True when the configured dir looks like a kernel checkout. Works even
    /// without git (a tarball install has no .git but is still valid).
    pub valid: bool,
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-testable)
// ---------------------------------------------------------------------------

/// Ask the OS for a free TCP port on loopback, then release it.
pub fn find_free_port() -> Result<u16, String> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("绑定空闲端口失败: {e}"))?;
    Ok(listener
        .local_addr()
        .map_err(|e| format!("读取端口失败: {e}"))?
        .port())
}

/// A directory qualifies as a deepseek-harness kernel checkout when it carries
/// the repo-root markers: package.json, pnpm-workspace.yaml and the dsh CLI entry.
pub fn is_kernel_dir(dir: &Path) -> bool {
    dir.join("package.json").is_file()
        && dir.join("pnpm-workspace.yaml").is_file()
        && dir.join("apps/cli/src/bin.ts").is_file()
}

/// Resolve pnpm and a usable PATH for spawned processes.
/// Returns (pnpm_abs_path, full_path_env).
///
/// GUI apps (Finder/launchd) get a minimal PATH, and login shells do NOT read
/// ~/.zshrc (where mise/nvm/fnm activate), so we probe common locations
/// directly instead of trusting `sh -lc`.
/// Cache of the last successful toolchain probe. Login-shell probing is slow
/// (zsh + oh-my-zsh init can take 1-2s), so we only pay it once per app run.
static TOOLCHAIN_CACHE: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

/// Resolve the pnpm executable and a PATH that covers node/pnpm. Fast path:
/// probe well-known locations directly (mise shims, pnpm dirs, homebrew) —
/// no shell involved. The login shell is only consulted as a fallback for
/// unusual installs. The result is cached for the process lifetime (a fresh
/// app launch re-probes, so new installs are picked up next run).
pub async fn resolve_toolchain() -> Result<(String, String), String> {
    if let Some(cached) = TOOLCHAIN_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return Ok(cached);
    }
    let home = std::env::var("HOME").unwrap_or_default();

    // 1. Fast path: well-known locations (covers mise/nvm/homebrew without
    //    spawning a login shell). This is the common case.
    let mut found: Option<String> = None;
    let mut shell_path = String::new();
    for candidate in [
        format!("{home}/.local/share/mise/shims/pnpm"),
        format!("{home}/.local/share/pnpm/pnpm"),
        format!("{home}/.npm-global/bin/pnpm"),
        "/opt/homebrew/bin/pnpm".to_string(),
        "/usr/local/bin/pnpm".to_string(),
    ] {
        if std::path::Path::new(&candidate).is_file() {
            found = Some(candidate);
            break;
        }
    }

    // 2. Slow path: ask the login shell (covers manually-installed pnpm).
    //    Timeout: a hung login shell must never block the start flow.
    if found.is_none() {
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            Command::new("sh").args(["-lc", "command -v pnpm; echo ---; printf '%s' \"$PATH\""]).output(),
        )
        .await
        .map_err(|_| "shell 探测超时")?
        .map_err(|e| format!("无法执行 shell 探测工具链: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let mut parts = text.split("---");
        let pnpm_in_shell = parts.next().unwrap_or("").trim().to_string();
        shell_path = parts.next().unwrap_or("").trim().to_string();
        if !pnpm_in_shell.is_empty() {
            found = Some(pnpm_in_shell);
        }
    }

    let pnpm = found.ok_or_else(|| {
        "未找到 pnpm。请先安装 Node.js 与 pnpm（corepack enable pnpm 或 npm i -g pnpm）。".to_string()
    })?;

    // Build a PATH that includes every dir that might hold node/pnpm.
    let mut dirs: Vec<String> = Vec::new();
    if let Some(parent) = std::path::Path::new(&pnpm).parent() {
        dirs.push(parent.display().to_string());
    }
    for extra in [
        format!("{home}/.local/share/mise/shims"),
        format!("{home}/.local/share/pnpm"),
        format!("{home}/.npm-global/bin"),
        format!("{home}/.nvm/versions/node"),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
    ] {
        if !dirs.contains(&extra) {
            dirs.push(extra);
        }
    }
    let mut full_path = shell_path;
    for d in dirs {
        if !full_path.contains(&d) {
            full_path = format!("{full_path}:{d}");
        }
    }
    if let Ok(cur) = std::env::var("PATH") {
        if !full_path.contains(&cur) {
            full_path = format!("{full_path}:{cur}");
        }
    }
    let result = (pnpm, full_path);
    *TOOLCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(result.clone());
    Ok(result)
}

/// Read the kernel checkout's git revision (short sha) and dirtiness.
pub fn git_revision(dir: &Path) -> (Option<String>, bool) {
    let head = match std::fs::read_to_string(dir.join(".git/HEAD")) {
        Ok(raw) => raw.trim().to_string(),
        Err(_) => return (None, false),
    };
    let sha = if let Some(branch) = head.strip_prefix("ref: ") {
        let path = dir.join(".git").join(branch);
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    } else if head.len() == 40 {
        Some(head)
    } else {
        None
    };
    let short = sha.map(|s| s.chars().take(7).collect());
    // Dirty = any tracked file differs from HEAD.
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    (short, dirty)
}

// ---------------------------------------------------------------------------
// Kernel manager
// ---------------------------------------------------------------------------

pub struct KernelManager {
    inner: Arc<Mutex<KernelInner>>,
}

struct KernelInner {
    kernel_dir: Option<PathBuf>,
    status: KernelStatus,
    child: Option<tokio::process::Child>,
}

impl Default for KernelManager {
    fn default() -> Self {
        let settings = crate::config::Settings::load();
        eprintln!(
            "[kernel] KernelManager::default: kernel_dir = {:?} (settings path: {})",
            settings.kernel_dir,
            crate::config::Settings::config_path().display()
        );
        KernelManager {
            inner: Arc::new(Mutex::new(KernelInner {
                kernel_dir: settings.effective_kernel_dir(),
                status: KernelStatus::Stopped,
                child: None,
            })),
        }
    }
}

impl KernelManager {
    pub async fn status(&self) -> KernelInfo {
        let inner = self.inner.lock().await;
        let (revision, dirty) = match &inner.kernel_dir {
            Some(dir) if is_kernel_dir(dir) => git_revision(dir),
            _ => (None, false),
        };
        let valid = inner.kernel_dir.as_ref().map(|d| is_kernel_dir(d)).unwrap_or(false);
        KernelInfo {
            status: inner.status.clone(),
            kernel_dir: inner.kernel_dir.clone(),
            revision,
            dirty,
            valid,
        }
    }

    /// Synchronous status snapshot (for tray menu updates etc.).
    pub fn status_blocking(&self) -> KernelInfo {
        let inner = self.inner.blocking_lock();
        let (revision, dirty) = match &inner.kernel_dir {
            Some(dir) if is_kernel_dir(dir) => git_revision(dir),
            _ => (None, false),
        };
        let valid = inner.kernel_dir.as_ref().map(|d| is_kernel_dir(d)).unwrap_or(false);
        KernelInfo {
            status: inner.status.clone(),
            kernel_dir: inner.kernel_dir.clone(),
            revision,
            dirty,
            valid,
        }
    }

    /// Non-blocking status snapshot. Safe to call from inside the async
    /// runtime (tray menu refresh): never blocks, never panics — returns
    /// `None` if the lock is momentarily held.
    pub fn status_snapshot(&self) -> Option<KernelInfo> {
        let inner = self.inner.try_lock().ok()?;
        let (revision, dirty) = match &inner.kernel_dir {
            Some(dir) if is_kernel_dir(dir) => git_revision(dir),
            _ => (None, false),
        };
        let valid = inner.kernel_dir.as_ref().map(|d| is_kernel_dir(d)).unwrap_or(false);
        Some(KernelInfo {
            status: inner.status.clone(),
            kernel_dir: inner.kernel_dir.clone(),
            revision,
            dirty,
            valid,
        })
    }

    pub fn kernel_dir(&self) -> Option<PathBuf> {
        self.inner.try_lock().ok()?.kernel_dir.clone()
    }

    pub async fn set_kernel_dir(&self, dir: PathBuf) -> Result<KernelInfo, String> {
        if !dir.is_dir() {
            return Err(format!("目录不存在: {}", dir.display()));
        }
        if !is_kernel_dir(&dir) {
            return Err(format!(
                "{} 不是有效的 deepseek-harness 仓库（缺少 package.json / pnpm-workspace.yaml / apps/cli/src/bin.ts）",
                dir.display()
            ));
        }
        let mut inner = self.inner.lock().await;
        if !matches!(inner.status, KernelStatus::Stopped) {
            return Err("请先停止内核再更换目录".to_string());
        }
        inner.kernel_dir = Some(dir.clone());
        let mut settings = crate::config::Settings::load();
        settings.kernel_dir = Some(dir);
        settings.save()?;
        drop(inner);
        Ok(self.status().await)
    }

    /// Spawn `pnpm dsh web --port <free>` inside the kernel dir and stream
    /// its output to the frontend. Returns the chosen port.
    pub async fn start(&self, app: &AppHandle) -> Result<u16, String> {
        // Take over any dsh web instance the user started outside this app
        // (own terminal, scripts, other tools) before spawning ours.
        let taken = kill_external_dsh_web();
        if taken > 0 {
            eprintln!("[kernel] 接管 {taken} 个外部 dsh web 实例");
        }
        let mut inner = self.inner.lock().await;
        if matches!(inner.status, KernelStatus::Running { .. } | KernelStatus::Starting) {
            return Err("内核已在运行".to_string());
        }
        let Some(dir) = inner.kernel_dir.clone() else {
            return Err("尚未设置内核目录，请先在设置中选择 deepseek-harness 仓库目录".to_string());
        };
        if !is_kernel_dir(&dir) {
            return Err(format!("内核目录无效: {}", dir.display()));
        }
        let port = find_free_port()?;

        inner.status = KernelStatus::Starting;
        inner.child = None;

        eprintln!("[kernel] spawning dsh web on port {port} in {}", dir.display());
        let spawn_result = spawn_web(&dir, port).await;
        let mut child = match spawn_result {
            Ok(c) => c,
            Err(e) => {
                let mut g = self.inner.lock().await;
                g.status = KernelStatus::Stopped;
                eprintln!("[kernel] spawn failed: {e}");
                return Err(e);
            }
        };
        eprintln!("[kernel] spawned pid {:?}", child.id());

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Share a handle to the manager so background tasks can update status.
        let shared = Arc::clone(&self.inner);

        // Reader tasks: stream log lines + detect exit via stderr EOF.
        let app_log = app.clone();
        let app_err = app.clone();
        let out_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                crate::logfile::persist("out", &line);
                let _ = app_log.emit("kernel-log", serde_json::json!({ "stream": "out", "line": line }));
            }
        });
        let err_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[kernel:stderr] {line}");
                crate::logfile::persist("err", &line);
                let _ = app_err.emit("kernel-log", serde_json::json!({ "stream": "err", "line": line }));
            }
        });

        // Supervisor: wait for the port to answer, then flip status.
        let app_sup = app.clone();
        let sup_shared = Arc::clone(&shared);
        let sup_dir = dir.clone();
        let sup = tokio::spawn(async move {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
            let mut healthy = false;
            while std::time::Instant::now() < deadline {
                if tcp_probe(port).await {
                    healthy = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            if healthy {
                sup_shared.lock().await.status = KernelStatus::Running { port };
                // Point the shell proxy at this kernel and make sure the
                // proxy listener is up (the iframe loads the proxy port).
                crate::proxy::set_kernel_port(port);
                let _ = crate::proxy::ensure_started(app_sup.clone());
                crate::tray::refresh_menu(&app_sup);
                let _ = app_sup.emit(
                    "kernel-status",
                    serde_json::json!({ "state": "running", "port": port }),
                );
            } else {
                sup_shared.lock().await.status = KernelStatus::Error {
                    message: format!(
                        "内核 90 秒内未就绪（端口 {port} 无响应）。日志见下方输出。\n内核目录: {}",
                        sup_dir.display()
                    ),
                };
                let _ = app_sup.emit(
                    "kernel-status",
                    serde_json::json!({ "state": "error", "message": format!(
                        "内核 90 秒内未就绪（端口 {port} 无响应）。日志见下方输出。\n内核目录: {}",
                        sup_dir.display()
                    )}),
                );
            }
        });

        let pid = child.id().expect("child spawned with id");
        inner.child = Some(child);
        drop(inner);

        // When both readers hit EOF the process tree is gone -> Stopped.
        let app_done = app.clone();
        let done_shared = Arc::clone(&shared);
        let done = tokio::spawn(async move {
            let (r1, r2) = tokio::join!(out_task, err_task);
            let _ = (r1, r2);
            done_shared.lock().await.status = KernelStatus::Stopped;
            crate::tray::refresh_menu(&app_done);
            let _ = app_done.emit(
                "kernel-status",
                serde_json::json!({ "state": "stopped", "message": "内核进程已退出" }),
            );
        });
        let _ = done;
        let _ = sup;
        let _ = pid;
        Ok(port)
    }

    /// Terminate the kernel process group (SIGTERM, then SIGKILL after 5s).
    pub async fn stop(&self) -> Result<(), String> {
        crate::proxy::set_kernel_port(0);
        let mut inner = self.inner.lock().await;
        let Some(mut child) = inner.child.take() else {
            inner.status = KernelStatus::Stopped;
            return Ok(());
        };
        let pid = child.id().ok_or("子进程无 PID")? as i32;
        drop(inner);
        #[cfg(unix)]
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        // Wait up to 5s for graceful exit, then force kill.
        for _ in 0..25 {
            let mut guard = self.inner.lock().await;
            match child.try_wait() {
                Ok(Some(_)) => {
                    guard.child = None;
                    guard.status = KernelStatus::Stopped;
                    return Ok(());
                }
                Ok(None) => {}
                Err(e) => {
                    guard.child = None;
                    guard.status = KernelStatus::Stopped;
                    return Err(format!("等待内核退出失败: {e}"));
                }
            }
            drop(guard);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let mut guard = self.inner.lock().await;
        let _ = child.kill().await;
        guard.child = None;
        guard.status = KernelStatus::Stopped;
        Ok(())
    }
}

/// Spawn `pnpm dsh web --port <port>` inside a kernel checkout.
/// The process tree gets its own process group (so it can be killed wholesale).
pub(crate) async fn spawn_web(dir: &Path, port: u16) -> Result<tokio::process::Child, String> {
    let (pnpm, path_env) = resolve_toolchain().await?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!(
            "cd '{}' && exec '{}' dsh web --port {port} --trusted-host 127.0.0.1:{}",
            dir.display(),
            pnpm,
            crate::proxy::PROXY_PORT
        ))
        .env("PATH", &path_env)
        .env("DSH_DESKTOP_OWNED", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn().map_err(|e| format!("启动内核进程失败: {e}"))
}

/// Minimal TCP + HTTP probe: connect and check for an HTTP 200 on GET /.
async fn tcp_probe(port: u16) -> bool {
    let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    use tokio::io::AsyncWriteExt;
    let req = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 1024];
    let n = match tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut buf)).await {
        Ok(Ok(n)) => n,
        _ => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn kernel_status(manager: tauri::State<'_, KernelManager>) -> Result<KernelInfo, String> {
    Ok(manager.status().await)
}

/// Open the app's native folder picker. Used by the shell to let the user
/// pick a workspace directory without relying on the kernel's osascript
/// dialog (which cannot show in this embedded environment). The shell then
/// creates the workspace through the kernel's own HTTP API.
#[tauri::command]
pub async fn pick_workspace_dir(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app.dialog().file().blocking_pick_folder();
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
pub async fn kernel_set_dir(
    manager: tauri::State<'_, KernelManager>,
    dir: String,
) -> Result<KernelInfo, String> {
    manager.set_kernel_dir(PathBuf::from(dir)).await
}

#[tauri::command]
pub async fn kernel_start(
    manager: tauri::State<'_, KernelManager>,
    app: AppHandle,
) -> Result<u16, String> {
    eprintln!("[cmd] kernel_start invoked");
    let r = manager.start(&app).await;
    eprintln!("[cmd] kernel_start -> {:?}", r);
    r
}

#[tauri::command]
pub async fn kernel_stop(manager: tauri::State<'_, KernelManager>) -> Result<(), String> {
    manager.stop().await
}

// ---------------------------------------------------------------------------
// Multi-kernel profiles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ProfileDto {
    pub name: String,
    pub dir: PathBuf,
    pub active: bool,
    pub revision: Option<String>,
}

/// List all configured kernel profiles.
#[tauri::command]
pub async fn kernel_profiles() -> Result<Vec<ProfileDto>, String> {
    let settings = crate::config::Settings::load();
    let active = settings
        .active_profile
        .clone()
        .or_else(|| settings.profiles.first().map(|p| p.name.clone()));
    Ok(settings
        .profiles
        .iter()
        .map(|p| {
            let revision = if crate::kernel::is_kernel_dir(&p.dir) {
                crate::kernel::git_revision(&p.dir)
            } else {
                (None, false)
            }
            .0;
            ProfileDto {
                name: p.name.clone(),
                dir: p.dir.clone(),
                active: active.as_deref() == Some(p.name.as_str()),
                revision,
            }
        })
        .collect())
}

/// Add a new kernel profile (validates the directory). Becomes active when it
/// is the only profile or when no profile was active before.
#[tauri::command]
pub async fn kernel_add_profile(
    name: String,
    dir: String,
    manager: tauri::State<'_, KernelManager>,
) -> Result<Vec<ProfileDto>, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("请输入配置名称".to_string());
    }
    let dir = std::path::PathBuf::from(dir);
    if !crate::kernel::is_kernel_dir(&dir) {
        return Err(format!(
            "{} 不是有效的 deepseek-harness 仓库（缺少 package.json / pnpm-workspace.yaml / apps/cli/src/bin.ts）",
            dir.display()
        ));
    }
    let mut settings = crate::config::Settings::load();
    let first = settings.profiles.is_empty();
    settings.upsert_profile(name, dir);
    if first {
        settings.active_profile = settings.profiles.first().map(|p| p.name.clone());
    }
    settings.save()?;
    manager.set_kernel_dir(settings.effective_kernel_dir().unwrap_or_default()).await?;
    kernel_profiles().await
}

/// Remove a profile. The active profile cannot be removed; removing a
/// non-active one is always allowed.
#[tauri::command]
pub async fn kernel_remove_profile(
    name: String,
    manager: tauri::State<'_, KernelManager>,
) -> Result<Vec<ProfileDto>, String> {
    let mut settings = crate::config::Settings::load();
    if settings.active_profile.as_deref() == Some(name.as_str()) {
        return Err("不能删除当前使用的配置，请先切换到其他配置".to_string());
    }
    settings.remove_profile(&name);
    settings.save()?;
    manager.set_kernel_dir(settings.effective_kernel_dir().unwrap_or_default()).await?;
    kernel_profiles().await
}

/// Switch the active profile. If the kernel is running it is stopped first
/// (the user restarts it afterwards), so switching never leaves two kernels.
#[tauri::command]
pub async fn kernel_set_active(
    name: String,
    manager: tauri::State<'_, KernelManager>,
) -> Result<Vec<ProfileDto>, String> {
    let mut settings = crate::config::Settings::load();
    if !settings.profiles.iter().any(|p| p.name == name) {
        return Err(format!("配置不存在: {name}"));
    }
    settings.active_profile = Some(name.clone());
    settings.save()?;
    // Stop any running kernel so switching never leaves two instances.
    let _ = manager.stop().await;
    manager.set_kernel_dir(settings.effective_kernel_dir().unwrap_or_default()).await?;
    kernel_profiles().await
}

/// Kill dsh web processes owned by this app that survived a previous run
/// (crashed/force-quit leaves them orphaned; each relaunch would otherwise
/// accumulate another kernel process). The env marker `DSH_DESKTOP_OWNED`
/// distinguishes our kernels (both the pnpm wrapper and the node dsh web)
/// from manually started ones.
pub fn kill_stale_owned() {
    let Ok(out) = std::process::Command::new("sh")
        .arg("-c")
        .arg("ps axo pid=,command= -E | grep DSH_DESKTOP_OWNED | grep -v grep | awk '{print $1}'")
        .output()
    else {
        return;
    };
    let pids = String::from_utf8_lossy(&out.stdout);
    for pid in pids.split_whitespace() {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid)
            .output();
        eprintln!("[kernel] killed stale kernel pid {pid}");
    }
}

/// Stop `dsh web` instances the user started outside this app (terminal,
/// scripts, other tools) so the shell takes over the single kernel. Owned
/// instances (tagged DSH_DESKTOP_OWNED) and this process are skipped.
/// Kills the whole process tree (pnpm wrapper + node child), not just the
/// matched parent. Returns the number of processes stopped.
pub fn kill_external_dsh_web() -> usize {
    let Ok(out) = std::process::Command::new("ps")
        .args(["axo", "pid=,ppid=,command=", "-E"])
        .output()
    else {
        return 0;
    };
    let mut procs: Vec<(i32, i32, String)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.trim().splitn(3, char::is_whitespace);
        let (Some(p), Some(pp), Some(cmd)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if let (Ok(pid), Ok(ppid)) = (p.parse::<i32>(), pp.parse::<i32>()) {
            procs.push((pid, ppid, cmd.to_string()));
        }
    }

    // Root targets: user-started dsh web processes.
    let mut all: Vec<i32> = Vec::new();
    for (pid, _, cmd) in &procs {
        if cmd.contains("DSH_DESKTOP_OWNED") {
            continue;
        }
        // Skip only grep processes from the ps pipeline itself (a bare
        // substring check would also drop legit env vars like .../ugrep).
        if cmd.split_whitespace().next().unwrap_or("").contains("grep") {
            continue;
        }
        if cmd.contains("dsh web") || cmd.contains("bin.ts web") {
            if *pid > 1 && *pid != std::process::id() as i32 {
                all.push(*pid);
            }
        }
    }
    // Expand to every descendant so the pnpm wrapper's node child dies too.
    let mut changed = true;
    while changed {
        changed = false;
        for (pid, ppid, _) in &procs {
            if all.contains(ppid) && !all.contains(pid) {
                all.push(*pid);
                changed = true;
            }
        }
    }

    let mut killed = 0;
    // Children first, parents last.
    for pid in all.iter().rev() {
        if *pid <= 1 || *pid == std::process::id() as i32 {
            continue;
        }
        unsafe {
            libc::kill(*pid, libc::SIGTERM);
        }
        killed += 1;
        eprintln!("[kernel] 接管: 停止外部 dsh web pid {pid}");
    }
    killed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that grab a free port: find_free_port has a
    /// TOCTOU window (release-then-rebind) that flakes under parallelism.
    static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn free_port_is_bindable() {
        let _guard = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The port can be snatched between release and rebind by another
        // process (e.g. the running app's kernel), so retry a few times.
        let mut ok = false;
        for _ in 0..5 {
            let port = find_free_port().unwrap();
            assert!(port > 0);
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                drop(listener);
                ok = true;
                break;
            }
        }
        assert!(ok, "5 次尝试内未能重绑空闲端口");
    }

    #[test]
    fn kernel_dir_detection() {
        let tmp = std::env::temp_dir().join(format!("dsh-kernel-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("apps/cli/src")).unwrap();
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        std::fs::write(tmp.join("pnpm-workspace.yaml"), "").unwrap();
        std::fs::write(tmp.join("apps/cli/src/bin.ts"), "").unwrap();
        assert!(is_kernel_dir(&tmp));

        let empty = std::env::temp_dir().join(format!("dsh-kernel-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!is_kernel_dir(&empty));
        assert!(!is_kernel_dir(Path::new("/nonexistent")));
    }

    #[tokio::test]
    async fn toolchain_resolves_in_gui_and_shell_env() {
        // GUI-like: minimal PATH → must find pnpm via direct probing (mise shims).
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", "/usr/bin:/bin");
        let gui = resolve_toolchain().await.expect("GUI 环境也应能找到 pnpm");
        std::env::set_var("PATH", &old);
        assert!(
            gui.1.contains("mise/shims") || gui.1.contains("local/share"),
            "GUI PATH 应含用户工具链目录: {}",
            gui.1
        );
        assert!(gui.0.contains("pnpm"), "pnpm 路径: {}", gui.0);

        // Normal shell env still works.
        let normal = resolve_toolchain().await.expect("shell 环境能找到 pnpm");
        assert!(!normal.0.is_empty());
        assert!(!normal.1.is_empty());
    }

    #[test]
    fn git_revision_of_this_repo() {
        // The kernel checkout used during dev lives next to this repo.
        let candidate = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../deepseek-harness");
        if candidate.exists() {
            let (rev, dirty) = git_revision(&candidate);
            assert!(rev.is_some(), "应能读到 HEAD revision");
            let _ = dirty;
        }
    }

    #[tokio::test]
    async fn spawn_healthcheck_and_kill_web() {
        let _guard = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Real kernel checkout next to this repo; skip elsewhere.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../deepseek-harness");
        if !is_kernel_dir(&dir) {
            eprintln!("跳过: 未找到内核目录 {}", dir.display());
            return;
        }
        let port = find_free_port().unwrap();
        let mut child = spawn_web(&dir, port).await.expect("spawn dsh web");
        let pid = child.id().expect("pid") as i32;

        // Wait for health (up to 60s).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut healthy = false;
        while std::time::Instant::now() < deadline {
            if tcp_probe(port).await {
                healthy = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        assert!(healthy, "dsh web 未在 60s 内就绪 (port {port})");

        // Kill the whole process group and confirm the port frees up.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if !tcp_probe(port).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert!(!tcp_probe(port).await, "端口 {port} 在 kill 后仍被占用");
        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn spawn_web_with_spaces_in_path() {
        let _guard = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The kernel dir must survive spaces in the path (e.g. "Application
        // Support"). Symlink the real checkout into a space-containing path.
        let real = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../deepseek-harness");
        if !is_kernel_dir(&real) {
            eprintln!("跳过: 未找到内核目录 {}", real.display());
            return;
        }
        let link = std::env::temp_dir().join(format!("dsh kernel test {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&link);
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let port = find_free_port().unwrap();
        let mut child = spawn_web(&link, port).await.expect("spawn dsh web via spaced path");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut healthy = false;
        while std::time::Instant::now() < deadline {
            if tcp_probe(port).await {
                healthy = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        assert!(healthy, "带空格路径下 dsh web 未就绪 (port {port})");
        let _ = child.kill().await;
        let _ = std::fs::remove_dir_all(&link);
    }
}
