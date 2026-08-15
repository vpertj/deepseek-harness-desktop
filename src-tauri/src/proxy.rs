//! Transparent HTTP proxy in front of the kernel.
//!
//! The embedded kernel UI is served from its own port, but its directory
//! picker (`host.pickDirectory`) shells out to `osascript`, which cannot show
//! a dialog from this app's process tree. The shell therefore loads the
//! iframe from this proxy port instead: every request is forwarded verbatim
//! to the kernel EXCEPT `host.pickDirectory`, which is served by the app's
//! native folder dialog (tauri-plugin-dialog). The kernel UI is untouched.

use std::io::Read;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Once;
use tauri::AppHandle;

/// Fixed local port the iframe loads. One proxy serves every kernel run; the
/// target kernel port is swapped atomically on start/stop.
pub const PROXY_PORT: u16 = 54001;

/// Current kernel port the proxy forwards to (0 = no kernel).
static KERNEL_PORT: AtomicU16 = AtomicU16::new(0);

/// Point the proxy at a running kernel (call after spawn, before iframe).
pub fn set_kernel_port(port: u16) {
    KERNEL_PORT.store(port, Ordering::SeqCst);
}

static PROXY_ONCE: Once = Once::new();

/// Start the proxy listener once per process (idempotent). Must be called
/// before the frontend loads the iframe.
pub fn ensure_started(app: AppHandle) -> Result<(), String> {
    let mut result = Ok(());
    PROXY_ONCE.call_once(|| {
        match std::net::TcpListener::bind(("127.0.0.1", PROXY_PORT)) {
            Ok(listener) => {
                let server = tiny_http::Server::from_listener(listener, None)
                    .expect("tiny_http server");
                std::thread::spawn(move || {
                    for request in server.incoming_requests() {
                        let app = app.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = handle_request(request, &app) {
                                eprintln!("[proxy] request failed: {e}");
                            }
                        });
                    }
                });
            }
            Err(e) => result = Err(format!("代理端口 {PROXY_PORT} 绑定失败: {e}")),
        }
    });
    result
}

fn handle_request(mut request: tiny_http::Request, app: &AppHandle) -> Result<(), String> {
    let url = request.url().to_string();
    let method = request.method().as_str().to_string();

    // WebSocket upgrade (the kernel UI streams events over ws://…/events.mux).
    let is_upgrade = request.headers().iter().any(|h| {
        let name: &str = h.field.as_str().as_ref();
        let value: &str = h.value.as_ref();
        name.eq_ignore_ascii_case("upgrade") && value.eq_ignore_ascii_case("websocket")
    });

    // Intercept the kernel's directory picker RPC and serve it with the
    // app's native dialog.
    if method == "POST" && url.starts_with("/api/host.pickDirectory") {
        return handle_pick_directory(request, app);
    }

    // Everything else: forward verbatim to the kernel.
    let port = KERNEL_PORT.load(Ordering::SeqCst);
    if port == 0 {
        let _ = request.respond(
            tiny_http::Response::from_string("kernel not running")
                .with_status_code(502),
        );
        return Ok(());
    }
    if is_upgrade {
        return handle_upgrade(request, port);
    }

    let inner_url = format!("http://127.0.0.1:{port}{url}");

    let mut body = Vec::new();
    request
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("读取请求体失败: {e}"))?;

    let agent = ureq::Agent::new_with_defaults();
    let resp = match method.as_str() {
        "GET" | "HEAD" => {
            let builder = apply_headers(agent.get(&inner_url), request.headers(), port);
            builder.call().map_err(|e| format!("转发 {method} {url} 失败: {e}"))?
        }
        "POST" => {
            let builder = apply_headers(agent.post(&inner_url), request.headers(), port);
            builder
                .send(&body)
                .map_err(|e| format!("转发 {method} {url} 失败: {e}"))?
        }
        "PUT" => {
            let builder = apply_headers(agent.put(&inner_url), request.headers(), port);
            builder
                .send(&body)
                .map_err(|e| format!("转发 {method} {url} 失败: {e}"))?
        }
        "PATCH" => {
            let builder = apply_headers(agent.patch(&inner_url), request.headers(), port);
            builder
                .send(&body)
                .map_err(|e| format!("转发 {method} {url} 失败: {e}"))?
        }
        "DELETE" => {
            let builder = apply_headers(agent.delete(&inner_url), request.headers(), port);
            builder
                .call()
                .map_err(|e| format!("转发 {method} {url} 失败: {e}"))?
        }
        "OPTIONS" => {
            let builder = apply_headers(agent.options(&inner_url), request.headers(), port);
            builder
                .call()
                .map_err(|e| format!("转发 {method} {url} 失败: {e}"))?
        }
        other => return Err(format!("不支持的请求方法: {other}")),
    };

    // Stream the response back (chunked), preserving SSE long connections.
    let status_code = resp.status().as_u16();
    let mut headers = Vec::new();
    for (name, value) in resp.headers().iter() {
        let n = name.as_str().to_ascii_lowercase();
        if matches!(n.as_str(), "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        if let Ok(h) = tiny_http::Header::from_bytes(name.as_str(), value.to_str().unwrap_or("")) {
            headers.push(h);
        }
    }
    let reader = resp.into_parts().1.into_reader();
    // data_length None → chunked transfer, so SSE streams through.
    let response = tiny_http::Response::new(tiny_http::StatusCode::from(status_code), headers, reader, None, None);
    request.respond(response).map_err(|e| format!("响应失败: {e}"))
}

/// Copy request headers onto a ureq builder, dropping hop-by-hop ones and
/// rewriting Origin to the kernel's authority.
///
/// The kernel's /api trust fence requires `Origin == Host` on every browser
/// request. The iframe loads from the proxy port (54001), so the browser
/// sends `Origin: http://127.0.0.1:54001`; forwarding that verbatim would
/// make the kernel see Origin(54001) ≠ Host(kernel port) and 403 every
/// request. Rewriting Origin to the kernel authority keeps the fence happy
/// while everything else passes through untouched.
fn apply_headers<B>(
    builder: ureq::RequestBuilder<B>,
    headers: &[tiny_http::Header],
    kernel_port: u16,
) -> ureq::RequestBuilder<B> {
    let mut b = builder;
    for header in headers {
        let n: &str = header.field.as_str().as_ref();
        let n = n.to_ascii_lowercase();
        if matches!(
            n.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        if n == "origin" {
            b = b.header("origin", format!("http://127.0.0.1:{kernel_port}"));
            continue;
        }
        let v: &str = header.value.as_ref();
        b = b.header(&n, v);
    }
    b
}

/// Forward a WebSocket upgrade to the kernel over raw TCP and pump frames in
/// both directions. tiny_http's `upgrade` hands back the client's raw stream;
/// the kernel side is a plain TcpStream.
fn handle_upgrade(request: tiny_http::Request, port: u16) -> Result<(), String> {
    use std::io::{Read, Write};

    let mut kernel = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("连接内核失败: {e}"))?;

    // Rebuild the upgrade request for the kernel (keep Upgrade/Sec-WebSocket
    // headers; only drop hop-by-hop Host/Content-Length and rewrite Origin to
    // the kernel authority — the /api trust fence requires Origin == Host).
    let mut req = format!(
        "{} {} HTTP/1.1\r\n",
        request.method().as_str(),
        request.url()
    );
    for h in request.headers() {
        let n: &str = h.field.as_str().as_ref();
        if n.eq_ignore_ascii_case("host") || n.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let v: &str = h.value.as_ref();
        if n.eq_ignore_ascii_case("origin") {
            req += &format!("Origin: http://127.0.0.1:{port}\r\n");
            continue;
        }
        req += &format!("{}: {}\r\n", n, v);
    }
    req += &format!("Host: 127.0.0.1:{port}\r\n\r\n");
    kernel
        .write_all(req.as_bytes())
        .map_err(|e| format!("发送 upgrade 请求失败: {e}"))?;

    // Read the kernel's response head (up to \r\n\r\n).
    let mut head: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        kernel
            .read_exact(&mut byte)
            .map_err(|e| format!("读取内核响应失败: {e}"))?;
        head.push(byte[0]);
        if head.len() > 65536 {
            return Err("upgrade 响应头过长".into());
        }
    }

    // Forward the kernel's response headers (101 + Sec-WebSocket-Accept) to
    // the client through tiny_http's upgrade.
    let head_str = String::from_utf8_lossy(&head);
    let mut response = tiny_http::Response::empty(tiny_http::StatusCode::from(101));
    for line in head_str.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            let n = name.trim().to_ascii_lowercase();
            if matches!(n.as_str(), "connection" | "transfer-encoding" | "content-length") {
                continue;
            }
            if let Ok(h) = tiny_http::Header::from_bytes(name.trim(), value.trim()) {
                response = response.with_header(h);
            }
        }
    }
    let client = request.upgrade("websocket", response);

    // Bidirectional pump. tiny_http's upgrade returns one boxed Read+Write
    // object; we split it with raw pointers because the two directions run on
    // separate threads. CustomStream's reader/writer are internally
    // independent, so this is safe in practice and the threads are joined
    // before the box is dropped.
    struct SplitStream(*mut dyn tiny_http::ReadWrite);
    unsafe impl Send for SplitStream {}
    impl std::io::Read for SplitStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            unsafe { (*self.0).read(buf) }
        }
    }
    impl std::io::Write for SplitStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            unsafe { (*self.0).write(buf) }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            unsafe { (*self.0).flush() }
        }
    }

    let raw = Box::into_raw(Box::new(client));
    let to_client = SplitStream(raw);
    let to_kernel = SplitStream(raw);

    let mut kernel_read = kernel.try_clone().map_err(|e| e.to_string())?;
    let mut kernel_write = kernel;

    let h1 = std::thread::spawn(move || {
        let mut client = to_client;
        let _ = std::io::copy(&mut kernel_read, &mut client);
        let _ = client.flush();
    });
    let h2 = std::thread::spawn(move || {
        let mut client = to_kernel;
        let _ = std::io::copy(&mut client, &mut kernel_write);
        let _ = kernel_write.flush();
    });
    let _ = (h1.join(), h2.join());
    unsafe {
        drop(Box::from_raw(raw));
    }
    Ok(())
}

/// Serve `host.pickDirectory` with the app's native folder picker, answering
/// the same RPC envelope the kernel would produce.
///
/// The dialog MUST run on the main thread: macOS AppKit dialogs fail
/// silently when opened from a background thread (the proxy handler thread
/// is one), so we hop to the main thread via `run_on_main_thread` and wait
/// on a channel for the result.
fn handle_pick_directory(mut request: tiny_http::Request, app: &AppHandle) -> Result<(), String> {
    let mut body = Vec::new();
    request
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("读取请求体失败: {e}"))?;
    let rpc_id = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("rpcId").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    // Hop to the main thread for the dialog; wait up to 2 minutes for the
    // user to pick or cancel. If the hop itself fails (app shutting down),
    // answer with null so the kernel UI treats it as a cancellation.
    let (tx, rx) = std::sync::mpsc::channel();
    let app_for_main = app.clone();
    if app
        .run_on_main_thread(move || {
            use tauri_plugin_dialog::DialogExt;
            let picked = app_for_main.dialog().file().blocking_pick_folder();
            let _ = tx.send(picked.map(|p| p.to_string()));
        })
        .is_err()
    {
        eprintln!("[proxy] run_on_main_thread failed for pickDirectory");
    }
    let picked = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .unwrap_or(None);

    let value = match &picked {
        Some(p) => serde_json::json!({ "path": p }),
        None => serde_json::json!({ "path": null }),
    };
    let payload = serde_json::json!({
        "type": "server-response",
        "rpcId": rpc_id,
        "result": { "ok": true, "value": value },
    });
    let response = tiny_http::Response::from_string(payload.to_string())
        .with_status_code(200)
        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap());
    request.respond(response).map_err(|e| format!("响应失败: {e}"))
}
