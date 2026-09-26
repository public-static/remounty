//! Opening locations in Finder / Console.
//!
//! Finder opens *volume roots* requested through `open` (LaunchServices) in a
//! bare window without toolbar and sidebar — for every volume, native ones
//! included. Asking Finder itself for a new browser window avoids that, but
//! needs the one-time "Remounty wants to control Finder" permission. Without
//! it, the plain `open` is used as a fallback.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::cmd;
use crate::ui::{Level, UiQueue};

const OPEN: &str = "/usr/bin/open";
const OSASCRIPT: &str = "/usr/bin/osascript";

/// The path is passed as an argument, never spliced into the script.
const FINDER_WINDOW_SCRIPT: &[&str] = &[
    "on run argv",
    "tell application \"Finder\"",
    "activate",
    "set w to make new Finder window to (POSIX file (item 1 of argv) as alias)",
    "set toolbar visible of w to true",
    "end tell",
    "return \"ok\"",
    "end run",
];

/// AppleScript error numbers meaning "not allowed to control Finder".
const NOT_PERMITTED: &[&str] = &["-1743", "-1744"];

static PERMISSION_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

/// Opens a folder (e.g. a volume) in a normal Finder window. Errors are shown
/// to the user.
pub fn open_folder(path: &Path, ui: UiQueue) {
    if !path.is_absolute() {
        ui.show(
            Level::Warning,
            "Cannot open this location",
            path.display().to_string(),
        );
        return;
    }
    let target = path.to_path_buf();
    spawn(move || {
        let path_str = target.to_string_lossy().into_owned();
        match open_browser_window(&path_str) {
            Ok(()) => return,
            Err(err) => {
                crate::log_info!("Finder window request failed ({err}); falling back to open");
                if NOT_PERMITTED.iter().any(|code| err.contains(code))
                    && !PERMISSION_HINT_SHOWN.swap(true, Ordering::SeqCst)
                {
                    ui.show(
                        Level::Informational,
                        "Tip: allow Remounty to control Finder",
                        "Volumes open in a bare Finder window unless Remounty may ask Finder for \
                         a normal one. You can allow this in System Settings → Privacy & \
                         Security → Automation → Remounty → Finder.",
                    );
                }
            }
        }
        run_open(&[&path_str], &target, &ui);
    });
}

pub fn open_with_console(path: &Path, ui: UiQueue) {
    if !path.is_absolute() {
        ui.show(
            Level::Warning,
            "Cannot open this location",
            path.display().to_string(),
        );
        return;
    }
    let target = path.to_path_buf();
    spawn(move || {
        let path_str = target.to_string_lossy().into_owned();
        run_open(&["-a", "Console", &path_str], &target, &ui);
    });
}

fn open_browser_window(path: &str) -> std::result::Result<(), String> {
    let mut argv: Vec<&str> = Vec::new();
    for line in FINDER_WINDOW_SCRIPT {
        argv.push("-e");
        argv.push(line);
    }
    // Absolute paths start with '/', so osascript cannot mistake it for an option.
    argv.push(path);
    // Generous timeout: the first call may wait for the permission prompt.
    match cmd::run(Path::new(OSASCRIPT), &argv, Some(Duration::from_secs(300))) {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(out.diagnostics()),
        Err(err) => Err(err.to_string()),
    }
}

/// `open` only ever receives absolute paths (starting with '/'), so no
/// argument can be mistaken for an option.
fn run_open(args: &[&str], target: &Path, ui: &UiQueue) {
    let error = match cmd::run(Path::new(OPEN), args, Some(Duration::from_secs(30))) {
        Ok(out) if out.success() => return,
        Ok(out) => out.diagnostics(),
        Err(err) => err.to_string(),
    };
    crate::log_warn!("open {} failed: {error}", target.display());
    ui.show(
        Level::Warning,
        format!("Could not open {}", target.display()),
        error,
    );
}

fn spawn(job: impl FnOnce() + Send + 'static) {
    if let Err(err) = thread::Builder::new().name("open".into()).spawn(job) {
        crate::log_warn!("Could not start a thread to open Finder: {err}");
    }
}
