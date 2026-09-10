//! Transparent HTTP proxy in front of the kernel.
//!
//! The embedded kernel UI is served from its own port, but its directory
//! picker (`host.pickDirectory`) shells out to `osascript`, which cannot show
//! a dialog from this app's process tree. The shell therefore loads the
//! iframe from this proxy port instead: every request is forwarded verbatim
//! to the kernel EXCEPT `host.pickDirectory`, which is served by the app's
//! native folder dialog (tauri-plugin-dialog). The kernel UI is untouched.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Once;
use tauri::AppHandle;

/// Fixed local port the iframe loads. One proxy serves every kernel run; the
/// target kernel port is swapped atomically on start/stop.
pub const PROXY_PORT: u16 = 54001;

/// Current kernel port the proxy forwards to (0 = no kernel).
static KERNEL_PORT: AtomicU16 = AtomicU16::new(0);

/// The kernel's auth cookie (`dsh-auth-<authority-hash>=v1.<payload>`),
/// captured from the kernel's tokened 303 response.
///
/// The cookie is SameSite=Strict and authority-bound to the kernel port, so
/// a cross-origin iframe (the shell's proxy origin) will not present it on
/// the redirect that follows the tokened first navigation — the browser
/// lands on the kernel's 401 "authentication required" body. The proxy
/// therefore acts as the cookie carrier: capture Set-Cookie, inject it into
/// every forwarded request and WebSocket upgrade.
static AUTH_COOKIE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Store the `dsh-auth-...=...` pair (attributes stripped) as the current
/// auth cookie.
fn store_auth_cookie(set_cookie_value: &str) {
    let pair = set_cookie_value.split(';').next().unwrap_or("").trim();
    if pair.starts_with("dsh-auth-") && pair.contains('=') && pair.len() > "dsh-auth-".len() + 1 {
        if let Ok(mut slot) = AUTH_COOKIE.lock() {
            if slot.as_deref() != Some(pair) {
                eprintln!("[proxy] captured kernel auth cookie");
            }
            *slot = Some(pair.to_string());
        }
    }
}

/// The stored cookie pair, if captured.
fn auth_cookie() -> Option<String> {
    AUTH_COOKIE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Merge the stored auth cookie into an outgoing Cookie header value.
/// Keeps any cookies the client already sent.
fn merge_cookie(incoming: Option<&str>, stored: Option<&str>) -> Option<String> {
    let stored = stored?;
    match incoming {
        Some(existing) if existing.contains("dsh-auth-") => Some(existing.to_string()),
        Some(existing) if !existing.trim().is_empty() => {
            Some(format!("{existing}; {stored}"))
        }
        _ => Some(stored.to_string()),
    }
}

/// Point the proxy at a running kernel (call after spawn, before iframe).
/// The auth cookie is cleared: it is bound to the previous kernel port's
/// authority, and a fresh one is captured from the new kernel's first
/// tokened response.
pub fn set_kernel_port(port: u16) {
    KERNEL_PORT.store(port, Ordering::SeqCst);
    if let Ok(mut slot) = AUTH_COOKIE.lock() {
        *slot = None;
    }
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

/// Shared HTTP agent. Creating an `ureq` per request throws away its
/// connection pool, so every forwarded request paid fresh connection setup.
/// One process-wide agent (clone is cheap, pool is shared) reuses
/// keep-alive connections to the kernel across requests.
///
/// Two non-default settings make the proxy transparent:
/// - `http_status_as_error(false)`: ureq turns 4xx/5xx into `Err` by
///   default; the kernel's trust fence answers 401 to tokenless probes and
///   those responses must reach the iframe verbatim.
/// - `max_redirects(0)`: the kernel's 303 (token → cookie) must be handed
///   to the browser, not followed internally by ureq.
static KERNEL_AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();

fn kernel_agent() -> ureq::Agent {
    KERNEL_AGENT
        .get_or_init(|| {
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .build()
                .new_agent()
        })
        .clone()
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

    let agent = kernel_agent();
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
        // Capture the kernel's auth cookie so later requests (including
        // WebSocket upgrades and cookieless browser navigations) carry it.
        if n == "set-cookie" {
            store_auth_cookie(value.to_str().unwrap_or(""));
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
    let mut cookie: Option<String> = None;
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
        if n == "cookie" {
            cookie = Some(header.value.as_str().to_string());
            continue;
        }
        let v: &str = header.value.as_ref();
        b = b.header(&n, v);
    }
    // Inject the captured kernel auth cookie (SameSite=Strict keeps the
    // browser from sending it on cross-origin iframe navigations).
    if let Some(merged) = merge_cookie(cookie.as_deref(), auth_cookie().as_deref()) {
        b = b.header("cookie", &merged);
    }
    b
}

/// Forward a WebSocket upgrade to the kernel over raw TCP and pump frames in
/// both directions. tiny_http's `upgrade` hands back the client's raw stream;
/// the kernel side is a plain TcpStream.
///
/// Every kernel→client write must be flushed immediately: tiny_http's upgraded
/// writer is a 1024-byte `BufWriter` (`tiny_http::client` builds it with
/// `with_capacity(1024, ..)`), and `std::io::copy` never flushes it. The
/// kernel's mux socket sends a 4-byte WebSocket ping every 2 s and terminates
/// the connection after 2 missed heartbeats (`MAX_MISSED_HEARTBEATS` in the
/// gateway's `stream-server.ts`). With a non-flushing copy those pings stay
/// buffered, the kernel kills the socket roughly every 5 s, and the embedded UI
/// loops on "自动重连中" → "连接成功".
fn handle_upgrade(request: tiny_http::Request, port: u16) -> Result<(), String> {
    use std::io::{Read, Write};

    let mut kernel = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("连接内核失败: {e}"))?;

    // Rebuild the upgrade request for the kernel (keep Upgrade/Sec-WebSocket
    // headers; only drop hop-by-hop Host/Content-Length and rewrite Origin to
    // the kernel authority — the /api trust fence requires Origin == Host).
    // The captured auth cookie is injected: SameSite=Strict keeps the browser
    // from sending it on cross-origin iframe requests.
    let mut req = format!(
        "{} {} HTTP/1.1\r\n",
        request.method().as_str(),
        request.url()
    );
    let mut incoming_cookie: Option<String> = None;
    for h in request.headers() {
        let n: &str = h.field.as_str().as_ref();
        if n.eq_ignore_ascii_case("host") || n.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if n.eq_ignore_ascii_case("cookie") {
            incoming_cookie = Some(h.value.as_str().to_string());
            continue;
        }
        let v: &str = h.value.as_ref();
        if n.eq_ignore_ascii_case("origin") {
            req += &format!("Origin: http://127.0.0.1:{port}\r\n");
            continue;
        }
        req += &format!("{}: {}\r\n", n, v);
    }
    if let Some(merged) = merge_cookie(incoming_cookie.as_deref(), auth_cookie().as_deref()) {
        req += &format!("Cookie: {merged}\r\n");
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
    // Our own writes towards the kernel (mux requests, WebSocket pongs) are
    // small control frames; Nagle would hold them behind an unacknowledged
    // segment and make the pong miss the kernel's 2 s heartbeat window.
    let _ = kernel_write.set_nodelay(true);
    let _ = kernel_read.set_nodelay(true);

    let h1 = std::thread::spawn(move || {
        // kernel → client. `to_client` is tiny_http's BufWriter-backed upgrade
        // writer, so each frame must be flushed to survive the heartbeat.
        let mut client = to_client;
        pump(&mut kernel_read, &mut client, true);
    });
    let h2 = std::thread::spawn(move || {
        // client → kernel. Both ends are unbuffered, so no per-write flush is
        // needed; the trailing flush in `pump` is a cheap no-op.
        let mut client = to_kernel;
        pump(&mut client, &mut kernel_write, false);
    });
    let _ = (h1.join(), h2.join());
    unsafe {
        drop(Box::from_raw(raw));
    }
    Ok(())
}

/// Copy one direction of an upgraded connection until either side closes.
///
/// `flush_each` must be `true` whenever the destination is tiny_http's upgraded
/// writer. That writer buffers 1024 bytes and nothing flushes it while the
/// connection is live, so small frames (a WebSocket ping is 4 bytes) would
/// never leave the process. Flushing after every read keeps a peer's heartbeat
/// satisfied and makes streamed fragments visible without waiting for the
/// buffer to fill.
fn pump<R: std::io::Read, W: std::io::Write>(from: &mut R, to: &mut W, flush_each: bool) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if to.write_all(&buf[..read]).is_err() {
            break;
        }
        if flush_each && to.flush().is_err() {
            break;
        }
    }
    let _ = to.flush();
}

/// Serve `host.pickDirectory` with the app's native folder picker, answering
/// the same RPC envelope the kernel would produce.
///
/// Uses the NON-blocking `pick_folder` API: it shows the dialog on the main
/// thread without occupying the event loop (the blocking variant deadlocks
/// macOS here — AppKit dialogs need the main thread, but a blocking call on
/// the main thread freezes the app). The result arrives via callback, so we
/// hand the request to the callback thread and wait on a channel.
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

    // Non-blocking dialog; the callback fires on the main thread after the
    // user picks or cancels. Wait up to 2 minutes for it.
    //
    // Bind to the main window as its parent: without a parent the dialog is
    // an independent window that can end up BEHIND the shell window (the
    // iframe keeps focus), looking like nothing happened. As a sheet it is
    // attached to the shell window and always on top.
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    {
        use tauri::Manager;
        use tauri_plugin_dialog::DialogExt;
        let mut builder = app.dialog().file();
        if let Some(window) = app.get_webview_window("main") {
            builder = builder.set_parent(&window);
        }
        builder.pick_folder(move |picked| {
            let _ = tx.send(picked.map(|p| p.to_string()));
        });
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

#[cfg(test)]
mod tests {
    use super::pump;
    use std::io::{BufWriter, Read, Write};
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Sink the test can inspect while the pump is still running.
    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl SharedSink {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A peer that releases one chunk per `send` and then blocks, so the test
    /// can observe delivery *before* the stream ends.
    struct ChannelReader(Receiver<Vec<u8>>);

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.recv() {
                Ok(chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    Ok(n)
                }
                Err(_) => Ok(0),
            }
        }
    }

    fn wait_for_bytes(sink: &SharedSink, want: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if sink.bytes().len() >= want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// A 4-byte WebSocket ping written into tiny_http's 1024-byte upgrade
    /// writer must reach the client while the connection is still open.
    /// Regression guard: `std::io::copy` leaves it buffered, so the kernel
    /// records a missed heartbeat, terminates the mux socket, and the UI loops
    /// on "自动重连中" / "连接成功".
    #[test]
    fn pump_flushes_small_frames_before_the_stream_ends() {
        let (tx, rx) = channel();
        let sink = SharedSink::default();
        let writer = BufWriter::with_capacity(1024, sink.clone());
        let worker = std::thread::spawn(move || {
            let mut source = ChannelReader(rx);
            let mut writer = writer;
            pump(&mut source, &mut writer, true);
        });

        tx.send(vec![0x89, 0x00, 0x00, 0x00]).unwrap(); // one ping frame
        assert!(
            wait_for_bytes(&sink, 4),
            "small frame stayed buffered while the connection was open"
        );
        assert_eq!(sink.bytes(), vec![0x89, 0x00, 0x00, 0x00]);

        drop(tx); // EOF ends the pump
        worker.join().unwrap();
    }

    /// `flush_each = false` is the client→kernel direction: the data must still
    /// arrive, and the pump must terminate on EOF.
    #[test]
    fn pump_forwards_without_per_write_flush() {
        let (tx, rx) = channel();
        let sink = SharedSink::default();
        let mut writer = BufWriter::with_capacity(1024, sink.clone());
        let mut source = ChannelReader(rx);

        tx.send(vec![1, 2, 3, 4, 5]).unwrap();
        drop(tx);
        pump(&mut source, &mut writer, false);

        assert_eq!(sink.bytes(), vec![1, 2, 3, 4, 5]);
    }
}
