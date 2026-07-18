//! Minimal client for herdr's socket API: newline-delimited JSON, one
//! request/response per connection (`{"id":..,"method":"pane.split","params":{..}}`).
//!
//! Exists so the ensure sidecar never spawns the `herdr` CLI: on Windows 11 with
//! Windows Terminal as the default console host, every console child of a hook
//! briefly flashes a terminal window — even when spawned with CREATE_NO_WINDOW
//! (herdr already does that; the flashes were verified live). Socket I/O spawns
//! nothing.
//!
//! On Windows the socket is a named pipe at `\\.\pipe\<HERDR_SOCKET_PATH>`
//! (herdr feeds the whole path through interprocess' namespaced naming), which a
//! plain `File` can speak. On unix it is an ordinary unix domain socket.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// `HERDR_SOCKET_PATH` (injected into hook/action commands), falling back to
/// herdr's default socket location.
pub fn socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return Some(path.into());
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("herdr").join("herdr.sock"))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Send one request; return the raw response line (same JSON shape the herdr
/// CLI prints, so `launch::*` parsers work on it unchanged).
pub fn call_text(method: &str, params: serde_json::Value) -> std::io::Result<String> {
    let path = socket_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no herdr socket path")
    })?;
    let request = serde_json::json!({
        "id": format!("herdr-sidebar:{method}"),
        "method": method,
        "params": params,
    });
    roundtrip(&path, &request.to_string())
}

#[cfg(windows)]
fn roundtrip(path: &std::path::Path, request: &str) -> std::io::Result<String> {
    let pipe = format!(r"\\.\pipe\{}", path.display());
    let stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe)?;
    exchange(stream, request)
}

#[cfg(unix)]
fn roundtrip(path: &std::path::Path, request: &str) -> std::io::Result<String> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    exchange(stream, request)
}

fn exchange<S: std::io::Read + Write>(mut stream: S, request: &str) -> std::io::Result<String> {
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(line)
}
