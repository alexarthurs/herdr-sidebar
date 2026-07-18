//! The ✨ commit-message suggestion: ask the local `claude` CLI to summarize
//! the pending diff (like VS Code's sparkle button), falling back to a
//! filename-based heuristic when the CLI is missing, slow, or fails. Runs on a
//! background thread so the TUI stays responsive; the app polls the returned
//! channel from its refresh tick.

use std::io::Write;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Cap the diff sent to the model — huge diffs only slow generation down, and
/// the file list already names everything that changed.
const MAX_DIFF_BYTES: usize = 16 * 1024;

/// How long to wait for `claude` before killing it and falling back.
const TIMEOUT: Duration = Duration::from_secs(60);

const PROMPT: &str = "Write a git commit message for the diff on stdin: one imperative \
                      subject line under 72 characters, no quotes, no trailing period. \
                      Reply with ONLY the message line.";

/// Spawn generation for `diff`/`files`; the result arrives on the channel.
/// Always yields exactly one message (the fallback is used on any failure).
pub fn spawn(diff: String, files: Vec<String>) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let message = generate(&diff, &files);
        let _ = tx.send(message);
    });
    rx
}

fn generate(diff: &str, files: &[String]) -> String {
    match ask_claude(diff) {
        Some(message) => message,
        None => fallback(files),
    }
}

/// One subject line from the `claude` CLI, or `None` on any failure. Candidate
/// program names cover the native install (`claude`/`claude.exe`, found by
/// CreateProcess) and the npm shim (`claude.cmd`, which CreateProcess skips).
fn ask_claude(diff: &str) -> Option<String> {
    let mut input = String::with_capacity(diff.len().min(MAX_DIFF_BYTES));
    for c in diff.chars() {
        if input.len() + c.len_utf8() > MAX_DIFF_BYTES {
            input.push_str("\n[diff truncated]");
            break;
        }
        input.push(c);
    }

    #[cfg(windows)]
    let candidates = ["claude", "claude.cmd"];
    #[cfg(not(windows))]
    let candidates = ["claude"];

    for program in candidates {
        let spawned = std::process::Command::new(program)
            .args(["-p", "--model", "haiku", "--strict-mcp-config", PROMPT])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else { continue };
        if let Some(stdin) = child.stdin.take() {
            let mut stdin = stdin;
            if stdin.write_all(input.as_bytes()).is_err() {
                let _ = child.kill();
                continue;
            }
            // Dropping stdin closes it so claude sees EOF.
        }
        return wait_with_timeout(child);
    }
    None
}

/// Wait for the child up to [`TIMEOUT`]; kill it and give up on overrun.
/// Reading stdout AFTER exit is safe here because `-p` output is one short
/// line, far below any pipe buffer.
fn wait_with_timeout(mut child: std::process::Child) -> Option<String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    clean_reply(&out)
}

/// The reply line, stripped of the quoting/fencing chat models sometimes add
/// despite instructions; `None` when nothing usable came back. Startup log
/// noise (MCP warnings and the like) can precede the reply on stdout, so this
/// takes the LAST usable line and drops warning-looking lines outright.
fn clean_reply(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).rfind(|l| {
        let lower = l.to_lowercase();
        !l.is_empty()
            && !l.starts_with("```")
            && !lower.contains("warn")
            && !lower.contains("error")
    })?;
    let line = line.trim_matches(['"', '\'', '`']).trim_end_matches('.').trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Filename-based fallback: good enough to save retyping, honest about scope.
fn fallback(files: &[String]) -> String {
    let name = |path: &String| {
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    match files {
        [] => "Update".to_string(),
        [only] => format!("Update {}", name(only)),
        [first, rest @ ..] => format!("Update {} and {} more", name(first), rest.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_cleanup_strips_quotes_fences_and_periods() {
        assert_eq!(clean_reply("Add sidebar merge\n"), Some("Add sidebar merge".into()));
        assert_eq!(clean_reply("\"Fix the thing.\""), Some("Fix the thing".into()));
        assert_eq!(
            clean_reply("```\nRefactor launch flow\n```"),
            Some("Refactor launch flow".into())
        );
        assert_eq!(clean_reply("   \n\n"), None);
        // Log noise before (or instead of) the reply must never win.
        assert_eq!(
            clean_reply("RendererWarning resource UID duplicate\nAdd auth docs\n"),
            Some("Add auth docs".into())
        );
        assert_eq!(clean_reply("[WARN] something\nERROR: nope\n"), None);
    }

    #[test]
    fn fallback_names_the_files() {
        assert_eq!(fallback(&[]), "Update");
        assert_eq!(fallback(&["src/app.rs".into()]), "Update app.rs");
        assert_eq!(
            fallback(&["src/app.rs".into(), "b".into(), "c".into()]),
            "Update app.rs and 2 more"
        );
    }
}
