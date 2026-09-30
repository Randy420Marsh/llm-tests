//! A tiny read-only HTTP server for viewing saved results in a browser.
//!
//! No dependencies: `std::net` only. It binds to 127.0.0.1 unless told otherwise, serves the viewer page
//! and the `*.json` result files of one folder, and nothing else (no directory listing, no other
//! files, no uploads).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::report;

pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Start serving `dir`. Port 0 picks a free port.
    pub fn start(dir: PathBuf, bind: &str, port: u16) -> std::io::Result<Server> {
        let listener = TcpListener::bind((bind, port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let dir = dir.clone();
                        std::thread::spawn(move || {
                            let _ = handle(stream, &dir);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(25)),
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        });
        Ok(Server { addr, stop })
    }

    #[allow(dead_code)] // used by the tests
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        let host = if self.addr.ip().is_unspecified() { "localhost".to_string() } else { self.addr.ip().to_string() };
        format!("http://{}:{}/", host, self.addr.port())
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

fn respond(status: &'static str, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
    Response { status, content_type, body: body.into() }
}

/// Decode %XX escapes (browsers encode `+` in a path segment as %2B)
fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn route(method: &str, path: &str, dir: &Path) -> Response {
    if method != "GET" && method != "HEAD" {
        return respond("405 Method Not Allowed", "text/plain", "read-only server");
    }
    let decoded = percent_decode(path.split('?').next().unwrap_or("/"));
    let path = decoded.as_str();
    match path {
        "/" | "/index.html" => respond("200 OK", "text/html; charset=utf-8", report::render_live()),
        "/favicon.ico" => respond("204 No Content", "text/plain", ""),
        "/api/results" => {
            let metas: Vec<_> = report::list(dir).into_iter().map(|(m, _)| m).collect();
            respond("200 OK", "application/json", serde_json::to_vec(&metas).unwrap_or_default())
        }
        "/api/status" => {
            let count = report::list(dir).len();
            let body = serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "dir": dir.to_string_lossy(), "count": count });
            respond("200 OK", "application/json", body.to_string())
        }
        p if p.starts_with("/api/result/") => {
            let name = &p["/api/result/".len()..];
            if !report::is_safe_name(name) {
                return respond("400 Bad Request", "text/plain", "bad file name");
            }
            match std::fs::read(dir.join(name)) {
                Ok(bytes) => respond("200 OK", "application/json", bytes),
                Err(_) => respond("404 Not Found", "text/plain", "not found"),
            }
        }
        _ => respond("404 Not Found", "text/plain", "not found"),
    }
}

fn handle(mut stream: TcpStream, dir: &Path) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let resp = route(method, path, dir);
    let mut out = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src data:\r\nConnection: close\r\n\r\n",
        resp.status,
        resp.content_type,
        resp.body.len()
    )
    .into_bytes();
    if method != "HEAD" {
        out.extend_from_slice(&resp.body);
    }
    stream.write_all(&out)?;
    stream.flush()
}

/// Open `url` in the default browser
pub fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).creation_flags(0x0800_0000).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::tests::make_result;

    fn get(addr: SocketAddr, request: &str) -> (String, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
        (head.to_string(), body.to_string())
    }

    #[test]
    fn serves_viewer_list_and_files_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let (_l, v) = make_result(dir.path(), serde_json::json!({"memory": {"results": []}}));
        let srv = Server::start(dir.path().to_path_buf(), "127.0.0.1", 0).unwrap();
        let a = srv.addr();
        assert!(srv.url().starts_with("http://127.0.0.1:"));

        let (head, body) = get(a, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 200"));
        assert!(head.contains("Content-Security-Policy"));
        assert!(body.contains("Latency Tester") && body.contains("/api/results"));
        assert!(!body.contains("__EMBEDDED__"), "placeholder must not reach the browser");

        let (head, body) = get(a, "GET /api/results HTTP/1.1\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 200") && head.contains("application/json"));
        let list: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["valid"], true);
        let name = list[0]["name"].as_str().unwrap().to_string();

        let (head, body) = get(a, &format!("GET /api/result/{} HTTP/1.1\r\n\r\n", name));
        assert!(head.starts_with("HTTP/1.1 200"));
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(doc["signature"]["sig"], v.signature.sig);

        // everything else is refused
        // a browser sends '+' as %2B; encoded traversal must still be refused after decoding
        let plus_name = format!("GET /api/result/{} HTTP/1.1\r\n\r\n", name.replace('+', "%2B"));
        assert!(get(a, &plus_name).0.starts_with("HTTP/1.1 200"), "percent-encoded plus must work");
        assert!(get(a, "GET /api/result/%2e%2e%2fsigning_key.bin HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 400"));
        assert!(get(a, "GET /api/result/../signing_key.bin HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 400"));
        assert!(get(a, "GET /api/result/signing_key.bin HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 400"));
        assert!(get(a, "GET /api/result/missing.json HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 404"));
        assert!(get(a, "GET /etc/passwd HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 404"));
        assert!(get(a, "POST /api/results HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 405"));
        assert!(get(a, "DELETE /api/result/x.json HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 405"));
        let (head, body) = get(a, "HEAD / HTTP/1.1\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 200") && body.is_empty());
        let (_, status) = get(a, "GET /api/status HTTP/1.1\r\n\r\n");
        assert!(status.contains("\"count\":1"));
    }

    #[test]
    fn stops_accepting_after_stop() {
        let dir = tempfile::tempdir().unwrap();
        let srv = Server::start(dir.path().to_path_buf(), "127.0.0.1", 0).unwrap();
        let a = srv.addr();
        assert!(TcpStream::connect(a).is_ok());
        srv.stop();
        std::thread::sleep(Duration::from_millis(200));
        // the listener thread has dropped the socket; new connections are refused
        assert!(TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_err());
    }

    #[test]
    fn survives_garbage_requests() {
        let dir = tempfile::tempdir().unwrap();
        let srv = Server::start(dir.path().to_path_buf(), "127.0.0.1", 0).unwrap();
        let a = srv.addr();
        let (head, _) = get(a, "\u{0}\u{1}garbage\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 405") || head.starts_with("HTTP/1.1 404"));
        // and it still answers afterwards
        assert!(get(a, "GET / HTTP/1.1\r\n\r\n").0.starts_with("HTTP/1.1 200"));
    }
}
