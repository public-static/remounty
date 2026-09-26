//! Alerts, questions and notifications, shown through `osascript`.
//!
//! Running them out of process keeps the menu bar responsive and avoids
//! nested modal run loops inside the tray event loop. All text is passed as
//! `argv`, never spliced into AppleScript source.
//!
//! Dialogs are serialized on a single UI thread so they never pile up on
//! top of each other.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use crate::cmd;
use crate::error::{Error, Result};

const OSASCRIPT: &str = "/usr/bin/osascript";
/// Dialogs that nobody answers are dismissed after this long.
pub const DEFAULT_GIVE_UP: u32 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Informational,
    Warning,
    Critical,
}

impl Level {
    fn applescript(self) -> &'static str {
        match self {
            Level::Informational => "informational",
            Level::Warning => "warning",
            Level::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Button(String),
    /// Dismissed by timeout, or the dialog could not be shown.
    NoAnswer,
}

/// Shows an alert and returns the button that was clicked. The first button
/// in `buttons` is the default one (drawn on the right by macOS).
pub fn alert(level: Level, title: &str, message: &str, buttons: &[&str], give_up_after: u32) -> Answer {
    let level_line = format!("set theLevel to \"{}\"", level.applescript());
    let lines = [
        "on run argv",
        level_line.as_str(),
        "set theTitle to item 1 of argv",
        "set theMessage to item 2 of argv",
        "set theDefault to item 3 of argv",
        "set theGiveUp to (item 4 of argv) as integer",
        "set theButtons to items 5 thru -1 of argv",
        "activate",
        "if theLevel is \"critical\" then",
        "set r to display alert theTitle message theMessage as critical buttons theButtons default button theDefault giving up after theGiveUp",
        "else if theLevel is \"warning\" then",
        "set r to display alert theTitle message theMessage as warning buttons theButtons default button theDefault giving up after theGiveUp",
        "else",
        "set r to display alert theTitle message theMessage as informational buttons theButtons default button theDefault giving up after theGiveUp",
        "end if",
        "if gave up of r then return \"REMOUNTY-GAVEUP\"",
        "return \"REMOUNTY-BUTTON:\" & button returned of r",
        "end run",
    ];
    // display alert lays buttons out right-to-left; reverse so the first
    // requested button ends up as the rightmost (default) one.
    let default = buttons.first().copied().unwrap_or("OK");
    let mut ordered: Vec<&str> = buttons.iter().rev().copied().collect();
    if ordered.is_empty() {
        ordered.push("OK");
    }
    let give_up = give_up_after.to_string();
    let mut args: Vec<&str> = vec![sanitize_arg(title), message, default, &give_up];
    args.extend(ordered);
    match run_script(&lines, &args) {
        Ok(out) => {
            let out = out.trim();
            match out.strip_prefix("REMOUNTY-BUTTON:") {
                Some(button) => Answer::Button(button.to_string()),
                None => Answer::NoAnswer,
            }
        }
        Err(err) => {
            crate::log_error!("Could not show dialog “{title}”: {err}");
            Answer::NoAnswer
        }
    }
}

/// Posts a notification banner. Failures are only logged.
pub fn notify(title: &str, message: &str) {
    let lines = [
        "on run argv",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "end run",
    ];
    if let Err(err) = run_script(&lines, &[sanitize_arg(title), message]) {
        crate::log_warn!("Could not post notification: {err}");
    }
}

/// Lets the user pick a file; `None` if cancelled.
pub fn choose_file(prompt: &str, start_dir: &Path) -> Option<PathBuf> {
    let lines = [
        "on run argv",
        "activate",
        "try",
        "set f to choose file with prompt (item 1 of argv) default location (POSIX file (item 2 of argv)) with invisibles",
        "on error number -128",
        "return \"\"",
        "end try",
        "return POSIX path of f",
        "end run",
    ];
    let start = start_dir.to_string_lossy();
    match run_script(&lines, &[sanitize_arg(prompt), &start]) {
        Ok(out) => {
            let path = out.trim_end_matches('\n');
            (!path.is_empty()).then(|| PathBuf::from(path))
        }
        Err(err) => {
            crate::log_error!("File chooser failed: {err}");
            None
        }
    }
}

/// osascript would parse a first argument starting with '-' as an option.
fn sanitize_arg(s: &str) -> &str {
    let trimmed = s.trim_start_matches('-');
    if trimmed.is_empty() { "Remounty" } else { trimmed }
}

fn run_script(lines: &[&str], args: &[&str]) -> Result<String> {
    let mut argv: Vec<&str> = Vec::with_capacity(lines.len() * 2 + args.len());
    for line in lines {
        argv.push("-e");
        argv.push(line);
    }
    // osascript stops option parsing at the first non-option argument, so
    // only the first one must not look like an option (callers sanitize it).
    argv.extend_from_slice(args);
    let out = cmd::run(Path::new(OSASCRIPT), &argv, Some(Duration::from_secs(3600)))?;
    if out.success() {
        Ok(out.stdout)
    } else {
        Err(Error::new(out.diagnostics()))
    }
}

/// A serial queue for dialogs.
#[derive(Clone)]
pub struct UiQueue {
    tx: Sender<Box<dyn FnOnce() + Send>>,
}

impl UiQueue {
    pub fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        thread::Builder::new()
            .name("ui-dialogs".into())
            .spawn(move || {
                for job in rx {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                        crate::log_error!("A dialog job panicked");
                    }
                }
            })
            .map_err(|err| Error::new(format!("Could not start UI thread: {err}")))?;
        Ok(Self { tx })
    }

    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        if self.tx.send(Box::new(job)).is_err() {
            crate::log_error!("UI thread is not running; dialog dropped");
        }
    }

    /// Fire-and-forget alert.
    pub fn show(&self, level: Level, title: impl Into<String>, message: impl Into<String>) {
        let (title, message) = (title.into(), message.into());
        self.submit(move || {
            alert(level, &title, &message, &["OK"], DEFAULT_GIVE_UP);
        });
    }
}
