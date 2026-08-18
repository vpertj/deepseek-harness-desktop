use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter};

/// One live terminal session (shell spawned in a pty).
struct TerminalSession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
}

static TERMINAL: Mutex<Option<TerminalSession>> = Mutex::new(None);

/// Query the kernel's session.list API (via the proxy at PROXY_PORT) and return
/// the cwd of the most recently updated session. Falls back to the kernel
/// directory from settings.
fn resolve_terminal_cwd() -> std::path::PathBuf {
    let proxy_url = format!("http://127.0.0.1:{}/api/session.list", crate::proxy::PROXY_PORT);
    let body = serde_json::to_string(&serde_json::json!({
        "type": "client-request",
        "rpcId": "tcwd",
        "method": "session.list",
        "payload": {}
    })).unwrap_or_default();

    let mut resp = match ureq::post(&proxy_url)
        .header("Content-Type", "application/json")
        .send(body.as_str())
    {
        Ok(r) => r,
        Err(_) => {
            return crate::config::Settings::load()
                .effective_kernel_dir()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        }
    };

    let body_text: String = match resp.body_mut().read_to_string() {
        Ok(s) => s,
        Err(_) => return crate::config::Settings::load().effective_kernel_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
    };

    let Ok(data) = serde_json::from_str::<serde_json::Value>(&body_text) else {
        return crate::config::Settings::load()
            .effective_kernel_dir()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    };

    if let Some(items) = data
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("items"))
        .and_then(|i| i.as_array())
    {
        let mut best: Option<(u64, &str)> = None;
        for s in items {
            if let (Some(ts), Some(cwd)) = (
                s.get("updatedAt").and_then(|v| v.as_u64()),
                s.get("cwd").and_then(|v| v.as_str()),
            ) {
                if best.as_ref().is_none_or(|(t, _)| ts > *t) {
                    best = Some((ts, cwd));
                }
            }
        }
        if let Some((_ts, cwd)) = best {
            let p = std::path::PathBuf::from(cwd);
            if p.is_dir() {
                return p;
            }
        }
    }

    crate::config::Settings::load()
        .effective_kernel_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// Open a shell pty. If cwd is provided (non-empty), use it directly; otherwise
/// resolve the active workspace cwd from the kernel's session list.
/// Always creates a fresh pty — kills any existing session first so the
/// terminal follows workspace switches.
#[tauri::command]
pub fn terminal_open(app: AppHandle, cwd: Option<String>) -> Result<(), String> {
    // Kill any existing session so we always start fresh at the requested dir.
    {
        let mut guard = TERMINAL.lock().unwrap();
        if let Some(mut s) = guard.take() {
            let _ = s.child.kill();
        }
    }

    let dir = match cwd {
        Some(ref c) if !c.is_empty() => std::path::PathBuf::from(c),
        _ => resolve_terminal_cwd(),
    };

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("创建 pty 失败: {e}"))?;

    let mut cmd = CommandBuilder::new("/bin/zsh");
    cmd.args(["-l"]);
    cmd.cwd(&dir);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("启动 shell 失败: {e}"))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("创建读取端失败: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("创建写入端失败: {e}"))?;
    let master = pair.master;

    // Pump pty output to the frontend as terminal-output events.
    let pump_app = app.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = pump_app.emit("terminal-output", serde_json::json!({ "data": text }));
                }
            }
        }
        let _ = pump_app.emit("terminal-output", serde_json::json!({ "data": "\r\n[进程已结束]\r\n" }));
    });

    let mut guard = TERMINAL.lock().unwrap();
    *guard = Some(TerminalSession { writer, child, master });
    Ok(())
}

/// Write keystrokes / input to the terminal.
#[tauri::command]
pub fn terminal_write(data: String) -> Result<(), String> {
    let mut guard = TERMINAL.lock().unwrap();
    let Some(session) = guard.as_mut() else {
        return Err("终端未打开".to_string());
    };
    session
        .writer
        .write_all(data.as_bytes())
        .map_err(|e| format!("写入终端失败: {e}"))?;
    session.writer.flush().map_err(|e| format!("刷新终端失败: {e}"))
}

/// Resize the pty (called by the frontend xterm fit addon).
#[tauri::command]
pub fn terminal_resize(rows: u16, cols: u16) -> Result<(), String> {
    let guard = TERMINAL.lock().unwrap();
    if let Some(session) = guard.as_ref() {
        session
            .master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("调整终端大小失败: {e}"))?;
    }
    Ok(())
}

/// Close the terminal (kills the shell).
#[tauri::command]
pub fn terminal_close() -> Result<(), String> {
    let mut guard = TERMINAL.lock().unwrap();
    if let Some(mut session) = guard.take() {
        let _ = session.child.kill();
    }
    Ok(())
}
