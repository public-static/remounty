//! Running a step as root through the standard macOS administrator dialog
//! (`do shell script … with administrator privileges`).
//!
//! Only used to install, update or repair the helper: commands run this way
//! are not attributed to Remounty by macOS's privacy protection, so they
//! cannot open removable disks — mounting goes through the helper instead.
//!
//! Remounty never sees the password. The scripts are compile-time constants;
//! every variable part is a separate `argv` item quoted by AppleScript's
//! `quoted form of`, so no path or name can be interpreted as shell code.

use std::path::Path;

use crate::cmd;
use crate::error::{Error, Result};

const OSASCRIPT: &str = "/usr/bin/osascript";
/// AppleScript's "User canceled." error number.
const USER_CANCELED: i32 = -128;

/// AppleScript that assembles `/bin/sh -c <script> remounty <args…>` with
/// every piece quoted, runs it as root and reports the outcome on stdout.
const RUNNER: &[&str] = &[
    "on run argv",
    "set thePrompt to item 1 of argv",
    "set cmd to \"/bin/sh -c \" & quoted form of (item 2 of argv) & \" remounty\"",
    "repeat with i from 3 to count of argv",
    "set cmd to cmd & \" \" & quoted form of (item i of argv)",
    "end repeat",
    "try",
    "set out to do shell script cmd with prompt thePrompt with administrator privileges",
    "return \"REMOUNTY-OK\" & linefeed & out",
    "on error errMsg number errNum",
    "return \"REMOUNTY-ERR \" & (errNum as text) & linefeed & errMsg",
    "end try",
    "end run",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success { stdout: String },
    Canceled,
    Failed { code: i32, message: String },
}

/// Runs `script` as root with `args` after the macOS administrator dialog
/// (showing `prompt`). No timeout: the user may take their time.
pub fn run_as_admin_dialog(prompt: &str, script: &str, args: &[&str]) -> Result<Outcome> {
    run_with_runner(RUNNER, prompt, script, args)
}

fn run_with_runner(runner: &[&str], prompt: &str, script: &str, args: &[&str]) -> Result<Outcome> {
    // osascript would treat a leading '-' as an option.
    let prompt = prompt.trim_start_matches('-');
    let mut argv: Vec<&str> = Vec::new();
    for line in runner {
        argv.push("-e");
        argv.push(line);
    }
    argv.push(prompt);
    argv.push(script);
    argv.extend_from_slice(args);
    let out = cmd::run(Path::new(OSASCRIPT), &argv, None)?;
    if !out.success() {
        return Err(Error::new(format!(
            "Could not request administrator privileges: {}",
            out.diagnostics()
        )));
    }
    parse_outcome(&out.stdout)
}

fn parse_outcome(stdout: &str) -> Result<Outcome> {
    let text = stdout.replace('\r', "\n");
    let (first, rest) = text.split_once('\n').unwrap_or((text.as_str(), ""));
    let rest = rest.trim_end_matches('\n').to_string();
    if first == "REMOUNTY-OK" {
        return Ok(Outcome::Success { stdout: rest });
    }
    if let Some(code) = first.strip_prefix("REMOUNTY-ERR ") {
        let code: i32 = code
            .trim()
            .parse()
            .map_err(|_| Error::new(format!("Unexpected status from osascript: {first}")))?;
        if code == USER_CANCELED {
            return Ok(Outcome::Canceled);
        }
        return Ok(Outcome::Failed { code, message: rest });
    }
    Err(Error::new(format!(
        "Unexpected output from osascript: {}",
        text.trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success() {
        assert_eq!(
            parse_outcome("REMOUNTY-OK\nhello\n").ok(),
            Some(Outcome::Success {
                stdout: "hello".into()
            })
        );
        assert_eq!(
            parse_outcome("REMOUNTY-OK\n").ok(),
            Some(Outcome::Success {
                stdout: String::new()
            })
        );
    }

    #[test]
    fn parses_cancel_and_failure() {
        assert_eq!(
            parse_outcome("REMOUNTY-ERR -128\nUser canceled.\n").ok(),
            Some(Outcome::Canceled)
        );
        assert_eq!(
            parse_outcome("REMOUNTY-ERR 74\rntfs-3g-exit: 15\rbad\n").ok(),
            Some(Outcome::Failed {
                code: 74,
                message: "ntfs-3g-exit: 15\nbad".into()
            })
        );
        assert!(parse_outcome("garbage").is_err());
        assert!(parse_outcome("REMOUNTY-ERR x\n").is_err());
    }

    /// Runs the AppleScript runner end to end — but without administrator
    /// privileges — to prove hostile arguments reach the shell unchanged.
    #[test]
    fn runner_passes_arguments_verbatim() {
        let unprivileged: Vec<&str> = RUNNER
            .iter()
            .map(|line| {
                if line.contains("with administrator privileges") {
                    "set out to do shell script cmd"
                } else {
                    line
                }
            })
            .collect();
        let hostile = [
            "plain",
            "with space",
            "quote'single",
            "quote\"double",
            "$(touch /tmp/remounty-pwned)",
            "`id`",
            "a;b|c&d",
            "-o ro",
            "--",
            "",
            "back\\slash",
            "Ünïcödé 🙂",
            "multi\nline",
            "*?[glob]",
            "~",
        ];
        let script = "for a in \"$@\"; do printf '<%s>' \"$a\"; done";
        let outcome = run_with_runner(&unprivileged, "Remounty test", script, &hostile);
        let expected: String = hostile
            .iter()
            .map(|a| format!("<{}>", a.replace('\n', "\r")))
            .collect();
        assert!(matches!(outcome, Ok(Outcome::Success { .. })), "{outcome:?}");
        if let Ok(Outcome::Success { stdout }) = outcome {
            assert_eq!(stdout.replace('\n', "\r"), expected);
        }
        assert!(!Path::new("/tmp/remounty-pwned").exists());
    }
}
