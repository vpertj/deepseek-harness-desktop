use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter};

/// One live terminal session (shell spawned in a pty inside the kernel dir).
struct TerminalSession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
}

static TERMINAL: Mutex<Option<TerminalSession>> = Mutex::new(None);

fn terminal_dir() -> Option<std::path::PathBuf> {
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
