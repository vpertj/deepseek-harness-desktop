use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// One live terminal session (shell spawned in a pty inside the kernel dir).
struct TerminalSession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
}

static TERMINAL: Mutex<Option<TerminalSession>> = Mutex::new(None);

/// Decode a dsh session-store directory name back into a filesystem path.
/// dsh encodes the workspace path by replacing "/" with "-" and dropping the
/// leading slash: "--Users-tianjun-Desktop-prog-newtonlab--" ->
/// "/Users/tianjun/Desktop/prog/newtonlab".
fn decode_workspace(name: &str) -> std::path::PathBuf {
    let trimmed = name.trim_matches('-');
    let path = trimmed.replace('-', "/");
    std::path::PathBuf::from(format!("/{path}"))
}

/// The shell's working directory: the dsh workspace most recently used
/// (newest session store under ~/.dsh/sessions), falling back to the kernel
/// directory when no workspace session exists yet.
fn terminal_dir() -> Option<std::path::PathBuf> {
    let sessions = dirs::home_dir()?.join(".dsh").join("sessions");
    if let Ok(rd) = std::fs::read_dir(&sessions) {
        let mut latest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
        for entry in rd.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("--") {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if latest.as_ref().is_none_or(|(t, _)| mtime > *t) {
                let dir = decode_workspace(&name);
                if dir.is_dir() {
                    latest = Some((mtime, dir));
                }
            }
        }
        if let Some((_, dir)) = latest {
            return Some(dir);
        }
    }
    crate::config::Settings::load().effective_kernel_dir()
}

/// Open a shell pty rooted at the kernel directory. Only one session at a time.
#[tauri::command]
pub fn terminal_open(app: AppHandle) -> Result<(), String> {
    let mut guard = TERMINAL.lock().unwrap();
    if guard.is_some() {
        return Ok(()); // already open
    }
    let Some(dir) = terminal_dir() else {
        return Err("尚未设置内核目录".to_string());
    };

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("创建 pty 失败: {e}"))?;

    let mut cmd = CommandBuilder::new("/bin/zsh");
    cmd.args(["-l"]);
    cmd.cwd(dir);
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

/// Pop the terminal out into its own window. Kills any live session so the
/// popped-out window starts a fresh shell, then opens "terminal-win" which
/// renders the terminal standalone.
#[tauri::command]
pub fn terminal_popout(app: AppHandle) -> Result<(), String> {
    if let Some(mut session) = TERMINAL.lock().unwrap().take() {
        let _ = session.child.kill();
    }
    let win = tauri::WebviewWindowBuilder::new(
        &app,
        "terminal-win",
        tauri::WebviewUrl::App("index.html?view=terminal".into()),
    )
    .title("终端")
    .inner_size(680.0, 420.0)
    .min_inner_size(400.0, 240.0)
    .build()
    .map_err(|e| format!("创建终端窗口失败: {e}"))?;
    win.on_window_event(|event| {
        // When the standalone terminal window is gone, kill its shell so no
        // orphan pty stays behind (re-opening creates a fresh one).
        if matches!(event, tauri::WindowEvent::Destroyed) {
            if let Some(mut session) = TERMINAL.lock().unwrap().take() {
                let _ = session.child.kill();
            }
        }
    });
    let _ = win.set_focus();
    Ok(())
}

/// Dock the terminal back: close the popped-out window and tell the main
/// window to re-show its bottom panel (the panel opens a fresh pty).
#[tauri::command]
pub fn terminal_dock_back(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("terminal-win") {
        let _ = win.close();
    }
    let _ = app.emit("terminal-docked", serde_json::json!({}));
    Ok(())
}
