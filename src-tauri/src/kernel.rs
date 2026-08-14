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

/// Resolve pnpm and a usable PATH from the user's login shell.
/// Returns (pnpm_abs_path, full_path_env).
pub async fn resolve_toolchain() -> Result<(String, String), String> {
    // `-lc` picks up the login shell profile where pnpm/nvm usually live.
    let out = Command::new("sh")
        .args(["-lc", "command -v pnpm; echo ---; printf '%s' \"$PATH\""])
        .output()
        .await
        .map_err(|e| format!("无法执行 shell 探测工具链: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut parts = text.split("---");
    let pnpm = parts.next().unwrap_or("").trim().to_string();
    let shell_path = parts.next().unwrap_or("").trim().to_string();
    if pnpm.is_empty() {
        return Err(
            "未找到 pnpm。请先安装 Node.js 与 pnpm（corepack enable pnpm 或 npm i -g pnpm）。"
                .to_string(),
        );
    }
    let mut full_path = shell_path;
    if let Ok(cur) = std::env::var("PATH") {
        if !full_path.contains(&cur) {
            full_path = format!("{full_path}:{cur}");
        }
    }
    // Common fallback dirs for node/pnpm binaries.
    let home = std::env::var("HOME").unwrap_or_default();
    for extra in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        &format!("{home}/.local/share/pnpm"),
        &format!("{home}/.npm-global/bin"),
    ] {
        if !full_path.contains(extra) {
            full_path = format!("{full_path}:{extra}");
        }
    }
    Ok((pnpm, full_path))
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
        KernelManager {
            inner: Arc::new(Mutex::new(KernelInner {
                kernel_dir: crate::config::Settings::load().kernel_dir,
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
        KernelInfo {
            status: inner.status.clone(),
            kernel_dir: inner.kernel_dir.clone(),
            revision,
            dirty,
        }
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
        let (pnpm, path_env) = resolve_toolchain().await?;
        let port = find_free_port()?;

        inner.status = KernelStatus::Starting;
        inner.child = None;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("cd {} && exec '{}' dsh web --port {port}", dir.display(), pnpm))
            .env("PATH", &path_env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Put the whole tree in its own process group so stop() can kill it all.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| {
                let mut g = self.inner.blocking_lock();
                g.status = KernelStatus::Stopped;
                format!("启动内核进程失败: {e}")
            })?;

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
                let _ = app_log.emit("kernel-log", serde_json::json!({ "stream": "out", "line": line }));
            }
        });
        let err_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
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
    manager.start(&app).await
}

#[tauri::command]
pub async fn kernel_stop(manager: tauri::State<'_, KernelManager>) -> Result<(), String> {
    manager.stop().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_port_is_bindable() {
        let port = find_free_port().unwrap();
        assert!(port > 0);
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        drop(listener);
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
    async fn toolchain_resolves_on_dev_machine() {
        let (pnpm, path) = resolve_toolchain().await.unwrap();
        assert!(pnpm.contains("pnpm"), "pnpm 路径: {pnpm}");
        assert!(!path.is_empty());
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
}
