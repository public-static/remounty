//! Running the few steps that need root through the standard macOS
//! administrator prompt (`do shell script … with administrator privileges`).
//!
//! Remounty never sees or stores the password. The shell scripts executed as
//! root are compile-time constants; every variable part is handed over as a
//! separate `argv` item and quoted by AppleScript's `quoted form of`, so no
//! volume name, path or option can ever be interpreted as shell code.

use std::path::Path;

use crate::cmd;
use crate::error::{Error, Result};

const OSASCRIPT: &str = "/usr/bin/osascript";

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

/// Exit codes of [`MOUNT_SCRIPT`] / [`UNMOUNT_SCRIPT`].
pub mod exit {
    pub const BAD_ARGS: i32 = 64;
    pub const INTERNAL: i32 = 70;
    pub const DEVICE_GONE: i32 = 71;
    pub const IDENTITY_MISMATCH: i32 = 72;
    pub const UNMOUNT_FAILED: i32 = 73;
    pub const NTFS3G_FAILED: i32 = 74;
    /// AppleScript's "User canceled." error number.
    pub const USER_CANCELED: i32 = -128;
}

/// Mounts an NTFS device with ntfs-3g, optionally unmounting the native
/// read-only mount first.
///
/// Arguments: `mode` (`remount` | `mount`), device node, mount point,
/// ntfs-3g path, ntfs-3g options, expected identity (may be empty).
///
/// The native mount is never force-unmounted: if something is using the
/// volume, the unmount fails and nothing else happens. The identity check
/// makes sure the device node still belongs to the volume the user picked.
pub const MOUNT_SCRIPT: &str = r#"set -u
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
[ $# -eq 6 ] || { echo "internal error: expected 6 arguments, got $#" >&2; exit 64; }
mode=$1 dev=$2 mp=$3 ntfs3g=$4 opts=$5 expected=$6
case "$mode" in remount|mount) ;; *) echo "invalid mode $mode" >&2; exit 64;; esac
case "$dev" in /dev/disk[0-9]*) ;; *) echo "invalid device $dev" >&2; exit 64;; esac
[ -d "$mp" ] || { echo "mount point $mp does not exist" >&2; exit 64; }
[ -x "$ntfs3g" ] || { echo "$ntfs3g is not executable" >&2; exit 64; }
info=$(/usr/sbin/diskutil info "$dev" 2>&1) || { printf '%s\n' "$info" >&2; exit 71; }
if [ -n "$expected" ]; then
  printf '%s\n' "$info" | /usr/bin/grep -qiF -- "$expected" || {
    echo "$dev is no longer the volume that was selected" >&2; exit 72; }
fi
log=$(/usr/bin/mktemp /tmp/remounty.XXXXXX) || exit 70
if [ "$mode" = remount ]; then
  if ! /usr/sbin/diskutil unmount "$dev" >"$log" 2>&1; then
    /bin/cat "$log" >&2; /bin/rm -f "$log"; exit 73
  fi
fi
"$ntfs3g" "$dev" "$mp" -o "$opts" </dev/null >"$log" 2>&1
rc=$?
if [ "$rc" -ne 0 ]; then
  echo "ntfs-3g-exit: $rc" >&2
  /bin/cat "$log" >&2
  /bin/rm -f "$log"
  exit 74
fi
/bin/cat "$log"
/bin/rm -f "$log"
exit 0
"#;

/// Unmounts a mount point (never forced). Argument: mount point.
pub const UNMOUNT_SCRIPT: &str = r#"set -u
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
[ $# -eq 1 ] || { echo "internal error: expected 1 argument, got $#" >&2; exit 64; }
mp=$1
[ -d "$mp" ] || { echo "$mp does not exist" >&2; exit 64; }
if out=$(/usr/sbin/diskutil unmount "$mp" 2>&1); then exit 0; fi
if out2=$(/sbin/umount "$mp" 2>&1); then exit 0; fi
printf '%s\n%s\n' "$out" "$out2" >&2
exit 73
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success { stdout: String },
    Canceled,
    Failed { code: i32, message: String },
}

/// Runs `script` as root with `args`.
///
/// With `prefer_touch_id` (and Touch ID enabled for sudo, see
/// [`touch_id_configured`]) the user authenticates with Touch ID through
/// `sudo`; if that is not possible or is cancelled, the standard password
/// dialog (showing `prompt`) is used instead.
///
/// There is deliberately no timeout: the user may take as long as they like
/// to authenticate, and killing the process half way through a mount could
/// leave the volume in an unknown state.
pub fn run_as_admin(prompt: &str, script: &str, args: &[&str], prefer_touch_id: bool) -> Result<Outcome> {
    if prefer_touch_id && touch_id_configured() {
        match run_with_sudo(script, args) {
            Ok(SudoAttempt::Done(outcome)) => return Ok(outcome),
            Ok(SudoAttempt::NotAuthenticated(reason)) => {
                crate::log_info!("Touch ID was not used ({reason}); asking for the password instead")
            }
            Err(err) => crate::log_warn!("{err}; asking for the password instead"),
        }
    }
    run_with_password_dialog(RUNNER, prompt, script, args)
}

fn run_with_password_dialog(runner: &[&str], prompt: &str, script: &str, args: &[&str]) -> Result<Outcome> {
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

enum SudoAttempt {
    Done(Outcome),
    /// sudo could not authenticate the user (Touch ID cancelled, not
    /// available, no admin rights, …). Nothing was executed.
    NotAuthenticated(String),
}

const SUDO: &str = "/usr/bin/sudo";
pub const PAM_SUDO_LOCAL: &str = "/etc/pam.d/sudo_local";

/// Runs the script through `sudo`, which authenticates with Touch ID when
/// `pam_tid.so` is enabled. `-k` ignores cached credentials so every
/// operation is authorised individually; `-A` with an askpass program that
/// always fails makes sure sudo never waits for a password on a terminal —
/// if Touch ID does not succeed, authentication simply fails.
fn run_with_sudo(script: &str, args: &[&str]) -> Result<SudoAttempt> {
    let mut argv: Vec<&str> = vec!["-k", "-A", "--", "/bin/sh", "-c", script, "remounty"];
    argv.extend_from_slice(args);
    let out = cmd::run_with_env(
        Path::new(SUDO),
        &argv,
        &[("SUDO_ASKPASS", "/usr/bin/false")],
        None,
    )?;
    Ok(classify_sudo_result(out.status.code(), &out.stdout, &out.stderr))
}

fn classify_sudo_result(code: Option<i32>, stdout: &str, stderr: &str) -> SudoAttempt {
    match code {
        Some(0) => SudoAttempt::Done(Outcome::Success {
            stdout: stdout.trim_end_matches('\n').to_string(),
        }),
        // sudo reports its own failures with exit status 1 and "sudo: …"
        // messages. The scripts never exit with 1 themselves.
        Some(1) if stderr.lines().any(|l| l.starts_with("sudo:")) => {
            SudoAttempt::NotAuthenticated(stderr.trim().to_string())
        }
        Some(code) => SudoAttempt::Done(Outcome::Failed {
            code,
            message: stderr.trim_end_matches('\n').to_string(),
        }),
        None => SudoAttempt::Done(Outcome::Failed {
            code: exit::INTERNAL,
            message: "The privileged helper was terminated by a signal.".into(),
        }),
    }
}

/// Whether Touch ID is enabled for sudo via `/etc/pam.d/sudo_local`.
pub fn touch_id_configured() -> bool {
    std::fs::read_to_string(PAM_SUDO_LOCAL)
        .map(|text| pam_enables_touch_id(&text))
        .unwrap_or(false)
}

pub fn sudo_local_exists() -> bool {
    std::fs::symlink_metadata(PAM_SUDO_LOCAL).is_ok()
}

fn pam_enables_touch_id(text: &str) -> bool {
    text.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("auth") && words.any(|w| w == "pam_tid.so")
    })
}

/// Creates `/etc/pam.d/sudo_local` enabling Touch ID for sudo — Apple's
/// supported mechanism, which survives system updates. An existing file is
/// never modified. The file only *adds* Touch ID as an alternative; the
/// normal password path in `/etc/pam.d/sudo` stays untouched.
pub const SETUP_TOUCH_ID_SCRIPT: &str = r#"set -u
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
[ $# -eq 0 ] || { echo "internal error: unexpected arguments" >&2; exit 64; }
f=/etc/pam.d/sudo_local
if [ -e "$f" ] || [ -L "$f" ]; then echo "$f already exists and was not changed." >&2; exit 65; fi
tmp=$(/usr/bin/mktemp /etc/pam.d/.remounty.XXXXXX) || exit 70
/bin/cat > "$tmp" <<'PAM'
# sudo_local: local config file which survives system update and is included for sudo
# Touch ID for sudo, enabled by Remounty. Delete this file to turn it off again.
auth       sufficient     pam_tid.so
PAM
if ! /usr/sbin/chown root:wheel "$tmp" || ! /bin/chmod 444 "$tmp" || ! /bin/mv -n "$tmp" "$f"; then
  /bin/rm -f "$tmp"; exit 70
fi
if [ -e "$tmp" ]; then
  /bin/rm -f "$tmp"; echo "$f appeared while setting up and was not changed." >&2; exit 65
fi
exit 0
"#;

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
        if code == exit::USER_CANCELED {
            return Ok(Outcome::Canceled);
        }
        return Ok(Outcome::Failed { code, message: rest });
    }
    Err(Error::new(format!(
        "Unexpected output from osascript: {}",
        text.trim()
    )))
}

/// Extracts the ntfs-3g exit status that [`MOUNT_SCRIPT`] reports on failure.
pub fn ntfs3g_exit_code(message: &str) -> Option<i32> {
    message
        .lines()
        .find_map(|l| l.trim().strip_prefix("ntfs-3g-exit:"))
        .and_then(|v| v.trim().parse().ok())
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

    #[test]
    fn pam_detection() {
        assert!(pam_enables_touch_id("auth       sufficient     pam_tid.so\n"));
        assert!(!pam_enables_touch_id("#auth       sufficient     pam_tid.so\n"));
        assert!(!pam_enables_touch_id("# comment\n"));
        assert!(!pam_enables_touch_id(""));
    }

    #[test]
    fn sudo_results() {
        assert!(matches!(
            classify_sudo_result(Some(1), "", "sudo: a password is required\n"),
            SudoAttempt::NotAuthenticated(_)
        ));
        assert!(matches!(
            classify_sudo_result(Some(1), "", "sudo: 1 incorrect password attempt\n"),
            SudoAttempt::NotAuthenticated(_)
        ));
        assert!(matches!(
            classify_sudo_result(Some(0), "out\n", ""),
            SudoAttempt::Done(Outcome::Success { ref stdout }) if stdout == "out"
        ));
        assert!(matches!(
            classify_sudo_result(Some(74), "", "ntfs-3g-exit: 15\n"),
            SudoAttempt::Done(Outcome::Failed { code: 74, .. })
        ));
        // Exit status 1 without sudo's own message is a script failure.
        assert!(matches!(
            classify_sudo_result(Some(1), "", "something else"),
            SudoAttempt::Done(Outcome::Failed { code: 1, .. })
        ));
        assert!(matches!(
            classify_sudo_result(None, "", ""),
            SudoAttempt::Done(Outcome::Failed { .. })
        ));
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
        let outcome = run_with_password_dialog(&unprivileged, "Remounty test", script, &hostile);
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

    /// The mount script hands every argument to ntfs-3g unchanged, even for
    /// hostile mount point names. Uses a fake ntfs-3g that records its
    /// arguments, the always-present /dev/disk0 in "mount" mode (which only
    /// reads `diskutil info`) and a scratch directory under the temp dir.
    #[test]
    fn mount_script_passes_arguments_verbatim() {
        let base = std::env::temp_dir().join(format!("remounty-args-{}", std::process::id()));
        let mp = base.join("-o ro,$(id) `x` 'q' \"d\" ;|& Ünï");
        let fake = base.join("ntfs-3g");
        let record = base.join("args.txt");
        let _ = std::fs::create_dir_all(&mp);
        let fake_body = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > '{}'\n",
            record.display()
        );
        let _ = std::fs::write(&fake, fake_body);
        let _ = std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755));
        let mp_str = mp.to_string_lossy().into_owned();
        let fake_str = fake.to_string_lossy().into_owned();
        let opts = "volname=x,local";
        let out = cmd::run(
            Path::new("/bin/sh"),
            &[
                "-c",
                MOUNT_SCRIPT,
                "remounty",
                "mount",
                "/dev/disk0",
                &mp_str,
                &fake_str,
                opts,
                "",
            ],
            None,
        );
        assert_eq!(out.ok().and_then(|o| o.status.code()), Some(0));
        let recorded = std::fs::read(&record).unwrap_or_default();
        let args: Vec<String> = recorded
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["/dev/disk0".to_string(), mp_str, "-o".into(), opts.into()]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn extracts_ntfs3g_code() {
        assert_eq!(
            ntfs3g_exit_code("ntfs-3g-exit: 15\nThe disk contains an unclean file system"),
            Some(15)
        );
        assert_eq!(ntfs3g_exit_code("nothing"), None);
    }

    /// Runs the mount script unprivileged with hostile arguments to prove
    /// they are rejected before anything is executed.
    #[test]
    fn mount_script_validates_arguments() {
        let sh = Path::new("/bin/sh");
        let run = |args: &[&str]| {
            let mut argv = vec!["-c", MOUNT_SCRIPT, "remounty"];
            argv.extend_from_slice(args);
            cmd::run(sh, &argv, None).ok().and_then(|o| o.status.code())
        };
        assert_eq!(run(&["remount"]), Some(exit::BAD_ARGS));
        assert_eq!(
            run(&["erase", "/dev/disk99s1", "/tmp", "/bin/echo", "", ""]),
            Some(exit::BAD_ARGS)
        );
        assert_eq!(
            run(&["mount", "/tmp/x; rm -rf /", "/tmp", "/bin/echo", "", ""]),
            Some(exit::BAD_ARGS)
        );
        assert_eq!(
            run(&["mount", "/dev/disk99s1", "/nonexistent-dir", "/bin/echo", "", ""]),
            Some(exit::BAD_ARGS)
        );
        // A device that does not exist is reported, never acted upon.
        assert_eq!(
            run(&["mount", "/dev/disk999s9", "/tmp", "/bin/echo", "", ""]),
            Some(exit::DEVICE_GONE)
        );
    }

    #[test]
    fn unmount_script_validates_arguments() {
        let sh = Path::new("/bin/sh");
        let out = cmd::run(sh, &["-c", UNMOUNT_SCRIPT, "remounty", "/nonexistent-dir"], None);
        assert_eq!(out.ok().and_then(|o| o.status.code()), Some(exit::BAD_ARGS));
    }
}
