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

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

/// How long to wait for a response before giving up. `exchange` runs on the
/// UI/event-loop thread (`viewer.rs`, input handlers), so an unbounded read
/// would hang the whole sidebar if the herdr process accepts the connection
/// but never answers.
const IPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on a single response line, so a runaway/malformed reply can't grow
/// unbounded in memory.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

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

/// Stamp a sidebar pane's identity tokens with a fresh heartbeat timestamp
/// (launchers treat stale stamps as dead panes — see
/// `launch::HEARTBEAT_STALE_SECS`): always the pane's own view; in merged
/// mode also the other view's (one Sidebar pane satisfies both plugins'
/// launchers), otherwise the other view's token is cleared with an explicit
/// null VALUE — `pane.report_metadata` MERGES the token map, so an empty
/// map is a no-op (verified live, herdr 0.7.1).
pub fn report_identity(pane_id: &str, my: crate::state::View, merged: bool) {
    let _ = report_identity_checked(pane_id, my, merged);
}

/// Checked form used by launchers before they perform focus-emitting layout
/// operations. A TUI heartbeat uses [`report_identity`] because a transient
/// reporting failure should not end its event loop.
pub fn report_identity_checked(
    pane_id: &str,
    my: crate::state::View,
    merged: bool,
) -> std::io::Result<()> {
    let now = crate::state::unix_now().to_string();
    let mine = serde_json::json!({
        my.plugin_id(): now,
        crate::launch::STARTING_TOKEN: serde_json::Value::Null,
    });
    call_text(
        "pane.report_metadata",
        serde_json::json!({ "pane_id": pane_id, "source": my.plugin_id(), "tokens": mine }),
    )?;
    let other = my.other();
    let other_tokens = if merged {
        serde_json::json!({ other.plugin_id(): now })
    } else {
        serde_json::json!({ other.plugin_id(): serde_json::Value::Null })
    };
    call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": other.plugin_id(),
            "tokens": other_tokens,
        }),
    )?;
    Ok(())
}

/// Mark the brief interval before a TUI reaches its event loop. Windows calls
/// this after a raw split; directly spawned Unix TUIs call it immediately on
/// process startup. The first full identity report clears the marker.
pub fn report_starting_identity(
    pane_id: &str,
    my: crate::state::View,
    merged: bool,
) -> std::io::Result<()> {
    let now = crate::state::unix_now().to_string();
    call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": my.plugin_id(),
            "tokens": {
                my.plugin_id(): now,
                crate::launch::STARTING_TOKEN: "1",
            },
        }),
    )?;
    if merged {
        let other = my.other();
        call_text(
            "pane.report_metadata",
            serde_json::json!({
                "pane_id": pane_id,
                "source": other.plugin_id(),
                "tokens": { other.plugin_id(): now },
            }),
        )?;
    }
    Ok(())
}

/// Spawn a Unix plugin pane directly from its manifest argv. Unlike a raw
/// `pane.split` followed by terminal input, this never renders an interactive
/// shell prompt before the TUI. Herdr 0.8.2 resolves relative pane commands
/// against the process cwd, so the requested project cwd travels in an env
/// var and `main` changes directory after the direct spawn.
#[cfg(unix)]
pub fn open_plugin_pane(
    target_pane_id: &str,
    view: crate::state::View,
    cwd: &std::path::Path,
    merged: bool,
) -> std::io::Result<String> {
    let mut env = crate::state::spawn_env();
    if !cwd.as_os_str().is_empty()
        && let Some(env) = env.as_object_mut()
    {
        env.insert(
            crate::state::SPAWN_CWD_ENV.to_string(),
            serde_json::Value::String(cwd.display().to_string()),
        );
    }
    let response = call_text(
        "plugin.pane.open",
        serde_json::json!({
            "plugin_id": "herdr-sidebar",
            "entrypoint": view.entrypoint(),
            "placement": "split",
            "target_pane_id": target_pane_id,
            "direction": "right",
            "focus": false,
            "env": env,
        }),
    )?;
    let pane_id = crate::launch::plugin_pane_id(&response).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("plugin.pane.open returned no pane id: {response}"),
        )
    })?;
    if let Err(error) = report_identity_checked(&pane_id, view, merged) {
        let _ = call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
        return Err(error);
    }
    Ok(pane_id)
}

/// Clear both sidebar identity tokens after the event loop has finished its
/// final persistence.
pub fn clear_identity(pane_id: &str) {
    for view in [
        crate::state::View::Explorer,
        crate::state::View::SourceControl,
    ] {
        let _ = call_text(
            "pane.report_metadata",
            serde_json::json!({
                "pane_id": pane_id,
                "source": view.plugin_id(),
                "tokens": {
                    view.plugin_id(): serde_json::Value::Null,
                    crate::launch::STARTING_TOKEN: serde_json::Value::Null,
                },
            }),
        );
    }
}

// Windows' named-pipe `File` handle has no `set_read_timeout` via std (no
// overlapped I/O in this fix's scope), so it's bounded with a background
// thread instead. unix sockets support native read/write timeouts, which
// `roundtrip` uses directly below — no thread, so no risk of a blocked
// reader thread + open fd lingering past the timeout when the peer never
// responds (the thread-based approach would leak exactly that on every
// timeout against a wedged peer).
#[cfg(windows)]
fn roundtrip(path: &std::path::Path, request: &str) -> std::io::Result<String> {
    let pipe = format!(r"\\.\pipe\{}", path.display());
    let stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe)?;
    exchange_with_thread_timeout(stream, request, IPC_TIMEOUT)
}

#[cfg(unix)]
fn roundtrip(path: &std::path::Path, request: &str) -> std::io::Result<String> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    exchange(stream, request)
}

/// Write the request, then read one response line. `S` must already have its
/// own read/write timeout configured by the caller. (unix-only: the Windows
/// path bounds the read with `exchange_with_thread_timeout` instead, since a
/// named-pipe `File` has no native read timeout via std.)
#[cfg(unix)]
fn exchange<S: Read + Write>(mut stream: S, request: &str) -> std::io::Result<String> {
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_RESPONSE_BYTES)).read_line(&mut line)?;
    Ok(line)
}

/// Windows-only: bound the read with a background thread + `recv_timeout`
/// since `File` has no native read timeout via std. Note this can still
/// leak a blocked thread and an open pipe handle if the peer never responds
/// and never closes the pipe — accepted here as strictly better than the
/// previous unconditional hang, not as a full fix (that needs overlapped
/// I/O on the pipe handle).
#[cfg(windows)]
fn exchange_with_thread_timeout<S: Read + Write + Send + 'static>(
    mut stream: S,
    request: &str,
    timeout: Duration,
) -> std::io::Result<String> {
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stream.take(MAX_RESPONSE_BYTES))
            .read_line(&mut line)
            .map(|_| line);
        let _ = tx.send(result);
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "herdr socket response timed out",
        ))
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// A peer that accepts the connection but never reads or writes must not
    /// hang the caller — this is the exact shape of a wedged herdr host.
    /// Exercises the real `roundtrip` timeout setup (not a hand-rolled one)
    /// so the test would fail if a future change dropped the timeout calls.
    #[test]
    fn exchange_times_out_instead_of_hanging_on_an_unresponsive_peer() {
        let path = std::env::temp_dir()
            .join(format!("aa-ipc-timeout-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            // Accept and hold the connection open (binding it, not `let _`,
            // which would drop it immediately and close the socket) without
            // ever responding; the stream's own timeout is what bounds this.
            if let Ok((_conn, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(30));
            }
        });
        let stream = UnixStream::connect(&path).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        stream.set_write_timeout(Some(Duration::from_millis(200))).unwrap();
        let start = std::time::Instant::now();
        let result = exchange(stream, "{}");
        let elapsed = start.elapsed();
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err(), "an unresponsive peer must not report success");
        assert!(
            elapsed < Duration::from_secs(2),
            "must bail out around the timeout, not hang; took {elapsed:?}"
        );
    }
}
