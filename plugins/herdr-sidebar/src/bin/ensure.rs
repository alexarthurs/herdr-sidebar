//! Windowless sidebar sidecar. GUI subsystem on Windows: it must NEVER own a
//! console — a console process launched from a herdr focus hook flashes a
//! Windows Terminal window on Windows 11 even under CREATE_NO_WINDOW, and this
//! runs on every tab/workspace focus. All herdr interaction is socket I/O.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let toggle = std::env::args().any(|arg| arg == "--toggle");
    // Errors are deliberately silent: there is no console to print to, herdr
    // logs the exit, and the next focus event retries anyway.
    let _ = herdr_sidebar::ensure::run(toggle);
}
