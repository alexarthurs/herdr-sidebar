//! "Update & refresh" for managed (GitHub-installed) sidebars.
//!
//! The flow is split across two herdr actions so each step runs the right
//! binary: `update[-windows]` (the OLD build) installs the release, then asks
//! herdr to run `refresh-sidebars[-windows]` (now the NEW build), which
//! restarts the docked sidebars and records the final status. Everything that
//! talks to herdr, GitHub or the filesystem sits behind a small trait so the
//! decisions are tested without a live session or network.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const REPOSITORY: &str = "alexarthurs/herdr-sidebar/plugins/herdr-sidebar";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a status message stays relevant in Settings.
const OPERATION_TTL_SECS: u64 = 1800;
/// An installing/refreshing record whose lock is free and that is older than
/// this was interrupted (the process died or was killed).
const INTERRUPTED_AFTER_SECS: u64 = 60;

const INSTALLING: &str = "installing";
const REFRESHING: &str = "refreshing";
const FINISHED: &str = "finished";
const FAILED: &str = "failed";

fn update_action() -> &'static str {
    if cfg!(windows) {
        "update-windows"
    } else {
        "update"
    }
}

fn refresh_action() -> &'static str {
    if cfg!(windows) {
        "refresh-sidebars-windows"
    } else {
        "refresh-sidebars"
    }
}

#[derive(Clone, Default)]
struct Status {
    checked: Option<Instant>,
    busy: bool,
    latest: Option<String>,
    local: bool,
    error: bool,
    operation: Option<(u64, String)>,
    /// Some process holds the install lock (an update or refresh is live).
    held: bool,
    polled: Option<Instant>,
    /// The update action is being requested on a worker thread.
    requesting: bool,
    /// When requesting the update action last failed.
    request_failed: Option<u64>,
}

fn status() -> &'static Mutex<Status> {
    static STATUS: OnceLock<Mutex<Status>> = OnceLock::new();
    STATUS.get_or_init(|| Mutex::new(Status::default()))
}

fn api(method: &str, params: Value) -> Result<Value, String> {
    let raw = crate::ipc::call_text(method, params).map_err(|error| error.to_string())?;
    let value: Value =
        serde_json::from_str(crate::launch::strip_bom(&raw)).map_err(|error| error.to_string())?;
    value.get("result").cloned().ok_or_else(|| {
        value["error"]["message"]
            .as_str()
            .unwrap_or("Herdr request failed")
            .into()
    })
}

fn plugin() -> Result<Value, String> {
    let list = api("plugin.list", json!({"plugin_id":"herdr-sidebar"}))?;
    list["plugins"]
        .as_array()
        .and_then(|plugins| {
            plugins
                .iter()
                .find(|plugin| plugin["plugin_id"] == "herdr-sidebar")
        })
        .cloned()
        .ok_or("Sidebar is not registered".into())
}

fn managed(plugin: &Value) -> bool {
    plugin["source"]["kind"] == "github"
        && plugin["source"]["owner"] == "alexarthurs"
        && plugin["source"]["repo"] == "herdr-sidebar"
        && plugin["source"]["subdir"] == "plugins/herdr-sidebar"
}

fn version(value: &str) -> Option<[u64; 3]> {
    let parts: Vec<_> = value
        .strip_prefix('v')
        .unwrap_or(value)
        .split('.')
        .collect();
    if parts.len() != 3 {
        return None;
    }
    let mut result = [0; 3];
    for (index, part) in parts.into_iter().enumerate() {
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        result[index] = part.parse().ok()?;
    }
    Some(result)
}

fn release(value: &Value) -> Result<String, String> {
    let tag = value["tag_name"].as_str().ok_or("Release has no version")?;
    if value["draft"] != false
        || value["prerelease"] != false
        || !tag.starts_with('v')
        || version(tag).is_none()
    {
        return Err("Not a stable versioned release".into());
    }
    Ok(tag.into())
}

fn quiet(command: &mut Command) -> &mut Command {
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

fn latest() -> Result<String, String> {
    let output = quiet(Command::new("curl").args([
        "-q",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "5",
        "--max-time",
        "15",
        "--max-filesize",
        "1048576",
        "--user-agent",
        "herdr-sidebar",
        "https://api.github.com/repos/alexarthurs/herdr-sidebar/releases/latest",
    ]))
    .output()
    .map_err(|error| format!("Update check needs curl: {error}"))?;
    if !output.status.success() {
        return Err("Could not check releases; try again later".into());
    }
    let value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Invalid release: {error}"))?;
    release(&value)
}

pub fn check() {
    let Ok(mut current) = status().lock() else {
        return;
    };
    if current.busy
        || current
            .checked
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1800))
    {
        return;
    }
    current.busy = true;
    drop(current);
    if std::thread::Builder::new()
        .name("sidebar-update-check".into())
        .spawn(|| {
            let result = plugin().and_then(|plugin| {
                if managed(&plugin) {
                    latest().map(|tag| (Some(tag), false))
                } else {
                    Ok((None, true))
                }
            });
            if let Ok(mut current) = status().lock() {
                current.busy = false;
                current.checked = Some(Instant::now());
                current.error = result.is_err();
                if let Ok((tag, local)) = result {
                    current.latest = tag;
                    current.local = local;
                }
            }
        })
        .is_err()
        && let Ok(mut current) = status().lock()
    {
        current.busy = false;
        current.error = true;
    }
}

fn updates_dir() -> PathBuf {
    crate::rundir::dir("updates")
}

fn open_lock(dir: &Path) -> Result<File, String> {
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("install.lock"))
        .map_err(|error| error.to_string())
}

/// Whether an update or refresh currently holds the install lock. The OS
/// releases it when that process dies, so this cannot go stale.
fn lock_held(dir: &Path) -> bool {
    if !crate::rundir::is_private(dir) {
        return false;
    }
    let Ok(file) = open_lock(dir) else {
        return false;
    };
    // Probing must never block anyone for longer than this call.
    let held = file.try_lock().is_err();
    drop(file);
    held
}

fn read_operation(dir: &Path) -> Option<(u64, String)> {
    if !crate::rundir::is_private(dir) {
        return None;
    }
    let bytes = std::fs::read(dir.join("operation.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn row() -> (&'static str, String, bool) {
    if let Ok(mut current) = status().lock()
        && current
            .polled
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
    {
        current.polled = Some(Instant::now());
        let dir = updates_dir();
        if let Some(operation) = read_operation(&dir) {
            current.operation = Some(operation);
        }
        current.held = lock_held(&dir);
    }
    let current = status()
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    describe(&current, crate::state::unix_now())
}

fn describe(current: &Status, now: u64) -> (&'static str, String, bool) {
    if current.local {
        return ("Updates", "local checkout".into(), false);
    }
    if current.requesting {
        return ("Updates", "requesting…".into(), false);
    }
    if let Some((at, operation)) = &current.operation
        && now.saturating_sub(*at) < OPERATION_TTL_SECS
    {
        let in_flight = operation == INSTALLING || operation == REFRESHING;
        if in_flight && (current.held || now.saturating_sub(*at) < INTERRUPTED_AFTER_SECS) {
            return if operation == INSTALLING {
                ("Updates", "installing…".into(), false)
            } else {
                ("Updates", "refreshing…".into(), false)
            };
        }
        if in_flight {
            return ("Update interrupted", "retry".into(), true);
        }
        if operation == FAILED {
            return ("Update failed", "retry / see logs".into(), true);
        }
    }
    if current
        .request_failed
        .is_some_and(|at| now.saturating_sub(at) < OPERATION_TTL_SECS)
    {
        return ("Update failed", "retry".into(), true);
    }
    if current.busy {
        return ("Updates", "checking…".into(), false);
    }
    if current.error {
        return ("Check updates", "retry".into(), true);
    }
    if let Some(tag) = &current.latest {
        if version(tag) > version(VERSION) {
            return ("Update & refresh", tag.clone(), true);
        }
        return ("Updates", "up to date".into(), false);
    }
    ("Check updates", String::new(), true)
}

/// The Settings row's action. Requests the update action on a worker thread:
/// herdr runs actions asynchronously, but the IPC round trip still must not
/// sit on the TUI's event loop.
pub fn activate() -> Result<(), String> {
    let mut current = status().lock().map_err(|_| "Update state unavailable")?;
    if current.local || current.busy || current.requesting {
        return Err("Updates unavailable while checking or using a local checkout".into());
    }
    let retry = current.error
        || current.latest.is_none()
        || current
            .operation
            .as_ref()
            .is_some_and(|(_, op)| op == FAILED);
    if retry && current.latest.is_none() {
        current.checked = None;
        drop(current);
        check();
        return Ok(());
    }
    current.requesting = true;
    current.request_failed = None;
    drop(current);
    let spawned = std::thread::Builder::new()
        .name("sidebar-update-request".into())
        .spawn(|| {
            let result = api(
                "plugin.action.invoke",
                json!({"plugin_id":"herdr-sidebar", "action_id": update_action()}),
            );
            if let Ok(mut current) = status().lock() {
                current.requesting = false;
                if result.is_err() {
                    current.request_failed = Some(crate::state::unix_now());
                }
            }
        });
    if spawned.is_err()
        && let Ok(mut current) = status().lock()
    {
        current.requesting = false;
        current.request_failed = Some(crate::state::unix_now());
    }
    Ok(())
}

/// Everything the update flow needs from the outside world.
trait UpdateHost {
    fn plugin(&mut self) -> Result<Value, String>;
    fn latest(&mut self) -> Result<String, String>;
    fn install(&mut self, tag: &str) -> Result<(), String>;
    fn invoke(&mut self, action: &str) -> Result<(), String>;
    fn record(&mut self, operation: &str) -> Result<(), String>;
}

/// Install the latest release if it is newer, then hand off to the refresh
/// action. The refresh runs even when nothing needed installing: a retry
/// after an install whose refresh failed must still restart the sidebars
/// that keep offering the update, or the button would never do anything.
fn run_with(host: &mut impl UpdateHost) -> Result<(), String> {
    host.record(INSTALLING)?;
    let installed = host.plugin()?;
    if !managed(&installed) {
        return Err("Refusing to overwrite a local or third-party checkout".into());
    }
    let tag = host.latest()?;
    let installed_version = installed["version"]
        .as_str()
        .and_then(version)
        .ok_or("Invalid installed version")?;
    let wanted = version(&tag).ok_or("Invalid release version")?;
    if wanted > installed_version {
        host.install(&tag)?;
        let updated = host.plugin()?;
        if !managed(&updated) || updated["version"].as_str().and_then(version) != Some(wanted) {
            return Err("Updated version could not be verified; sidebars left running".into());
        }
    }
    // FINISHED is written by the refresh itself once it has actually run.
    host.record(REFRESHING)?;
    host.invoke(refresh_action())
}

struct LiveUpdate {
    dir: PathBuf,
}

impl UpdateHost for LiveUpdate {
    fn plugin(&mut self) -> Result<Value, String> {
        plugin()
    }

    fn latest(&mut self) -> Result<String, String> {
        latest()
    }

    fn install(&mut self, tag: &str) -> Result<(), String> {
        let herdr = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
        let log = File::create(self.dir.join("install.log")).map_err(|error| error.to_string())?;
        let result = quiet(
            Command::new(herdr).args(["plugin", "install", REPOSITORY, "--ref", tag, "--yes"]),
        )
        .current_dir(&self.dir)
        .stdout(log.try_clone().map_err(|error| error.to_string())?)
        .stderr(log)
        .status()
        .map_err(|error| error.to_string())?;
        if result.success() {
            Ok(())
        } else {
            Err(format!(
                "Update failed; see {}",
                self.dir.join("install.log").display()
            ))
        }
    }

    fn invoke(&mut self, action: &str) -> Result<(), String> {
        api(
            "plugin.action.invoke",
            json!({"plugin_id":"herdr-sidebar", "action_id": action}),
        )
        .map(drop)
    }

    fn record(&mut self, operation: &str) -> Result<(), String> {
        write_operation(&self.dir, operation)
    }
}

fn write_operation(dir: &Path, value: &str) -> Result<(), String> {
    let text = serde_json::to_string(&(crate::state::unix_now(), value))
        .map_err(|error| error.to_string())?;
    crate::viewer::write_scratch_file_in(&dir.join("operation.json"), &text, dir)
        .map_err(|error| error.to_string())
}

fn write_error(dir: &Path, error: &str) {
    let _ = std::fs::write(dir.join("error.txt"), error);
}

/// The `update[-windows]` action.
pub fn run() -> Result<(), String> {
    let dir = updates_dir();
    crate::rundir::ensure_private(&dir).map_err(|error| error.to_string())?;
    // Leave the plugin root before installing: herdr runs actions with
    // cwd = plugin root, and Windows refuses to rename a directory that is
    // any process's working directory — which herdr's install has to do.
    std::env::set_current_dir(&dir).map_err(|error| error.to_string())?;
    // Blocking, not try_lock: a Settings pane probing `lock_held` for an
    // instant must never make a real update refuse to start.
    let lock = open_lock(&dir)?;
    lock.lock().map_err(|error| error.to_string())?;
    let mut host = LiveUpdate { dir: dir.clone() };
    let result = run_with(&mut host);
    if let Err(error) = &result {
        let _ = host.record(FAILED);
        write_error(&dir, error);
    }
    result
}

/// What the refresh writes when it finishes, if it finishes an update.
fn completion(
    operation: Option<&str>,
    refreshed: &Result<crate::ensure::RefreshReport, String>,
) -> Option<(&'static str, Option<String>)> {
    if operation != Some(REFRESHING) {
        return None;
    }
    Some(match refreshed {
        Ok(report) if report.is_complete() => (FINISHED, None),
        Ok(report) => (
            FAILED,
            Some(format!(
                "refreshed {} sidebar(s); {} kept running: {}",
                report.refreshed,
                report.kept,
                report.errors.join("; ")
            )),
        ),
        Err(error) => (FAILED, Some(error.clone())),
    })
}

/// The `refresh-sidebars[-windows]` action. When it completes an update it
/// records the outcome, so Settings reports a finished refresh — not merely
/// a queued one.
pub fn refresh() -> std::io::Result<()> {
    let dir = updates_dir();
    let guard = crate::rundir::ensure_private(&dir)
        .ok()
        .and_then(|_| open_lock(&dir).ok())
        .filter(|file| file.lock().is_ok());
    let result = crate::ensure::refresh_all().map_err(|error| error.to_string());
    if guard.is_some()
        && let Some((state, error)) = completion(
            read_operation(&dir).as_ref().map(|(_, op)| op.as_str()),
            &result,
        )
    {
        let _ = write_operation(&dir, state);
        if let Some(error) = error {
            write_error(&dir, &error);
        }
    }
    drop(guard);
    match result {
        Ok(report) if report.is_complete() => Ok(()),
        Ok(report) => Err(std::io::Error::other(report.errors.join("; "))),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ensure::RefreshReport;

    #[test]
    fn versions_are_numeric_and_release_tags_are_strict() {
        assert!(version("v0.15.0") > version("0.9.9"));
        for bad in ["v1.2", "1.2.3-beta", "v01.2.3", "../v1.2.3", "1.2.3;cmd"] {
            assert!(version(bad).is_none());
        }
        assert_eq!(
            release(&json!({"tag_name":"v1.2.3","draft":false,"prerelease":false})).unwrap(),
            "v1.2.3"
        );
        assert!(release(&json!({"tag_name":"v1.2.3","draft":true,"prerelease":false})).is_err());
    }

    #[test]
    fn only_official_managed_installs_can_update() {
        let mut plugin = json!({"source":{"kind":"github","owner":"alexarthurs","repo":"herdr-sidebar","subdir":"plugins/herdr-sidebar"}});
        assert!(managed(&plugin));
        plugin["source"]["kind"] = json!("local");
        assert!(!managed(&plugin));
        plugin["source"]["kind"] = json!("github");
        plugin["source"]["owner"] = json!("someone-else");
        assert!(!managed(&plugin));
    }

    #[test]
    fn local_checkout_never_offers_an_update_even_after_a_failed_operation() {
        let now = 10_000;
        let status = Status {
            local: true,
            operation: Some((now, FAILED.into())),
            ..Status::default()
        };
        assert_eq!(
            describe(&status, now),
            ("Updates", "local checkout".into(), false)
        );
    }

    #[test]
    fn update_button_only_offers_newer_versions() {
        let now = 10_000;
        let mut status = Status {
            latest: Some(VERSION.into()),
            ..Status::default()
        };
        assert_eq!(
            describe(&status, now),
            ("Updates", "up to date".into(), false)
        );
        status.latest = Some("v999.0.0".into());
        assert_eq!(
            describe(&status, now),
            ("Update & refresh", "v999.0.0".into(), true)
        );
        status.busy = true;
        assert!(!describe(&status, now).2);
    }

    /// Issue 6: an install/refresh record outlives its process only until the
    /// OS lock says nobody holds it; then it reads as interrupted, retryable.
    #[test]
    fn a_dead_updater_reads_as_interrupted_not_installing_forever() {
        let now = 10_000;
        let mut status = Status {
            latest: Some("v999.0.0".into()),
            operation: Some((now - 5, INSTALLING.into())),
            ..Status::default()
        };
        assert_eq!(describe(&status, now).1, "installing…");
        status.operation = Some((now - INTERRUPTED_AFTER_SECS, INSTALLING.into()));
        status.held = true;
        assert_eq!(describe(&status, now).1, "installing…", "lock still held");
        status.held = false;
        assert_eq!(
            describe(&status, now),
            ("Update interrupted", "retry".into(), true)
        );
        status.operation = Some((now - INTERRUPTED_AFTER_SECS, REFRESHING.into()));
        assert!(
            describe(&status, now).2,
            "an abandoned refresh is retryable too"
        );
        status.held = true;
        assert_eq!(describe(&status, now).1, "refreshing…");
    }

    #[test]
    fn a_failed_request_is_retryable_and_requesting_is_not() {
        let now = 10_000;
        let mut status = Status {
            latest: Some("v999.0.0".into()),
            requesting: true,
            ..Status::default()
        };
        assert_eq!(
            describe(&status, now),
            ("Updates", "requesting…".into(), false)
        );
        status.requesting = false;
        status.request_failed = Some(now - 1);
        assert_eq!(
            describe(&status, now),
            ("Update failed", "retry".into(), true)
        );
    }

    #[derive(Default)]
    struct Fake {
        versions: Vec<&'static str>,
        source: Option<Value>,
        latest: Option<Result<String, String>>,
        install_fails: bool,
        invoke_fails: bool,
        installs: Vec<String>,
        invoked: Vec<String>,
        recorded: Vec<String>,
    }

    impl UpdateHost for Fake {
        fn plugin(&mut self) -> Result<Value, String> {
            let version = if self.versions.len() > 1 {
                self.versions.remove(0)
            } else {
                self.versions[0]
            };
            let mut plugin = self.source.clone().unwrap_or_else(|| {
                json!({"source":{"kind":"github","owner":"alexarthurs",
                       "repo":"herdr-sidebar","subdir":"plugins/herdr-sidebar"}})
            });
            plugin["version"] = json!(version);
            Ok(plugin)
        }

        fn latest(&mut self) -> Result<String, String> {
            self.latest.clone().unwrap_or(Ok("v0.15.0".into()))
        }

        fn install(&mut self, tag: &str) -> Result<(), String> {
            self.installs.push(tag.into());
            if self.install_fails {
                Err("install failed".into())
            } else {
                Ok(())
            }
        }

        fn invoke(&mut self, action: &str) -> Result<(), String> {
            self.invoked.push(action.into());
            if self.invoke_fails {
                Err("invoke failed".into())
            } else {
                Ok(())
            }
        }

        fn record(&mut self, operation: &str) -> Result<(), String> {
            self.recorded.push(operation.into());
            Ok(())
        }
    }

    #[test]
    fn a_newer_release_installs_verifies_and_hands_off_to_refresh() {
        let mut host = Fake {
            versions: vec!["0.14.0", "0.15.0"],
            ..Fake::default()
        };
        run_with(&mut host).unwrap();
        assert_eq!(host.installs, vec!["v0.15.0"]);
        assert_eq!(host.invoked, vec![refresh_action()]);
        assert_eq!(host.recorded, vec![INSTALLING, REFRESHING]);
    }

    /// Issue 3: the release is already installed (an earlier refresh failed):
    /// nothing is reinstalled, but the refresh still runs.
    #[test]
    fn an_already_installed_release_still_refreshes_on_retry() {
        let mut host = Fake {
            versions: vec!["0.15.0"],
            ..Fake::default()
        };
        run_with(&mut host).unwrap();
        assert!(host.installs.is_empty());
        assert_eq!(host.invoked, vec![refresh_action()]);
        assert_eq!(host.recorded.last().map(String::as_str), Some(REFRESHING));
    }

    #[test]
    fn a_failed_or_unverified_install_never_refreshes() {
        let mut failed = Fake {
            versions: vec!["0.14.0"],
            install_fails: true,
            ..Fake::default()
        };
        assert!(run_with(&mut failed).is_err());
        assert!(failed.invoked.is_empty());

        let mut unverified = Fake {
            versions: vec!["0.14.0", "0.14.0"],
            ..Fake::default()
        };
        let error = run_with(&mut unverified).unwrap_err();
        assert!(error.contains("could not be verified"), "{error}");
        assert!(unverified.invoked.is_empty());
        assert!(!unverified.recorded.iter().any(|op| op == REFRESHING));
    }

    #[test]
    fn unmanaged_installs_and_bad_releases_are_refused_before_installing() {
        let mut local = Fake {
            versions: vec!["0.14.0"],
            source: Some(json!({"source":{"kind":"local"}})),
            ..Fake::default()
        };
        assert!(run_with(&mut local).is_err());
        assert!(local.installs.is_empty() && local.invoked.is_empty());

        let mut offline = Fake {
            versions: vec!["0.14.0"],
            latest: Some(Err("offline".into())),
            ..Fake::default()
        };
        assert!(run_with(&mut offline).is_err());
        assert!(offline.installs.is_empty() && offline.invoked.is_empty());
    }

    #[test]
    fn a_failed_refresh_handoff_is_an_error() {
        let mut host = Fake {
            versions: vec!["0.15.0"],
            invoke_fails: true,
            ..Fake::default()
        };
        assert!(run_with(&mut host).is_err());
    }

    /// The status says "finished" only once the refresh has actually run and
    /// every sidebar was restarted; a standalone refresh records nothing.
    #[test]
    fn only_a_completed_refresh_finishes_an_update() {
        let complete = Ok(RefreshReport {
            refreshed: 2,
            ..RefreshReport::default()
        });
        assert_eq!(
            completion(Some(REFRESHING), &complete),
            Some((FINISHED, None))
        );
        assert_eq!(completion(None, &complete), None);
        assert_eq!(completion(Some(FINISHED), &complete), None);

        let kept = Ok(RefreshReport {
            refreshed: 1,
            kept: 1,
            errors: vec!["w1:t2: a sidebar kept running (unsaved draft?)".into()],
        });
        let (state, error) = completion(Some(REFRESHING), &kept).unwrap();
        assert_eq!(state, FAILED);
        assert!(error.unwrap().contains("kept running"));
        assert_eq!(
            completion(Some(REFRESHING), &Err("no socket".into())).map(|(s, _)| s),
            Some(FAILED)
        );
    }
}
