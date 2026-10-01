//! App side of the privileged helper: status checks, installation (through
//! the macOS administrator dialog) and requests (through `sudo -n`, which
//! keeps Remounty as the "responsible" app so its Removable Volumes
//! permission applies to ntfs-3g).

use std::fmt;
use std::path::{Path, PathBuf};

use crate::authz;
use crate::cmd;
use crate::error::{Error, Result};
use crate::helper_proto::{self, exit};
use crate::privileged::{self, Outcome};

const SUDO: &str = "/usr/bin/sudo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    NotInstalled,
    Ready,
    /// Installed, but built from files that have changed since (Remounty,
    /// ntfs-3g or macFUSE were updated).
    NeedsUpdate(String),
    /// Partially installed or damaged.
    Broken(String),
}

impl Status {
    pub fn is_ready(&self) -> bool {
        *self == Status::Ready
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::NotInstalled => write!(f, "not installed"),
            Status::Ready => write!(f, "ready"),
            Status::NeedsUpdate(reason) => write!(f, "needs an update ({reason})"),
            Status::Broken(reason) => write!(f, "damaged ({reason})"),
        }
    }
}

/// Checks that the helper is installed and matches this version of the app.
pub fn status() -> Status {
    let bin = Path::new(helper_proto::HELPER_BIN).exists();
    let sudoers = Path::new(helper_proto::SUDOERS_FILE).exists();
    if !bin && !sudoers {
        return Status::NotInstalled;
    }
    if !bin || !sudoers {
        return Status::Broken("some parts are missing".into());
    }
    // `version` needs no privileges and prints the helper's protocol version.
    match cmd::run(
        Path::new(helper_proto::HELPER_BIN),
        &["version"],
        Some(std::time::Duration::from_secs(10)),
    ) {
        Ok(out) if out.success() && out.stdout.trim() == helper_proto::HELPER_VERSION => Status::Ready,
        Ok(out) if out.success() => Status::NeedsUpdate("Remounty was updated".into()),
        Ok(out) => Status::Broken(out.diagnostics()),
        Err(err) => Status::Broken(err.to_string()),
    }
}

/// The helper binary shipped next to the app executable.
pub fn bundled_helper() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|err| Error::new(format!("Cannot locate Remounty: {err}")))?;
    let helper = exe
        .parent()
        .map(|dir| dir.join(helper_proto::BUNDLED_HELPER_NAME))
        .ok_or_else(|| Error::new("Cannot locate Remounty's helper"))?;
    if !helper.is_file() {
        return Err(Error::new(format!(
            "The helper is missing from Remounty.app ({}). Please reinstall Remounty.",
            helper.display()
        )));
    }
    Ok(helper)
}

const INSTALL_SCRIPT: &str = r#"exec "$1" install "$2" "$3""#;
const UNINSTALL_SCRIPT: &str = r#"exec "$1" uninstall"#;

/// Installs or updates the helper (macOS administrator dialog).
pub fn install(user: &str, remember: u32) -> Result<Outcome> {
    let helper = bundled_helper()?;
    privileged::run_as_admin_dialog(
        "Remounty wants to install its helper for mounting NTFS volumes.",
        INSTALL_SCRIPT,
        &[&helper.to_string_lossy(), user, &remember.to_string()],
    )
}

/// Removes a damaged installation with the bundled helper (admin dialog).
pub fn uninstall_with_dialog() -> Result<Outcome> {
    let helper = bundled_helper()?;
    privileged::run_as_admin_dialog(
        "Remounty wants to remove its helper.",
        UNINSTALL_SCRIPT,
        &[&helper.to_string_lossy()],
    )
}

/// Runs the installed helper through sudo. With `authorize`, Remounty's
/// authorization session is handed over on stdin.
fn run(args: &[&str], authorize: bool) -> Result<Outcome> {
    let form = if authorize {
        Some(authz::session_external_form()?)
    } else {
        None
    };
    let mut argv = vec!["-n", helper_proto::HELPER_BIN];
    argv.extend_from_slice(args);
    // No timeout: the user may take their time in the authorization dialog,
    // and a mount must never be killed half way.
    let out = cmd::run_with(
        Path::new(SUDO),
        &argv,
        &cmd::Options {
            input: form.as_ref().map(|f| f.as_slice()),
            ..cmd::Options::default()
        },
    )?;
    Ok(classify(out.status.code(), &out.stdout, &out.stderr))
}

fn classify(code: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
    match code {
        Some(0) => Outcome::Success {
            stdout: stdout.trim_end_matches('\n').to_string(),
        },
        Some(exit::CANCELED) => Outcome::Canceled,
        // sudo itself failed (e.g. the sudoers rule is missing); nothing ran.
        Some(1) if stderr.lines().any(|l| l.starts_with("sudo:")) => Outcome::Failed {
            code: exit::UNSAFE_INSTALL,
            message: format!(
                "Remounty's helper is not allowed to run. Please reinstall the helper from the \
                 Remounty menu.\n\n{}",
                stderr.trim()
            ),
        },
        Some(code) => Outcome::Failed {
            code,
            message: stderr.trim_end_matches('\n').to_string(),
        },
        None => Outcome::Failed {
            code: exit::INTERNAL,
            message: "The helper was terminated by a signal.".into(),
        },
    }
}

pub fn mount(
    bsd_name: &str,
    identity: Option<&str>,
    remount: bool,
    name: &str,
    ntfs3g: &Path,
) -> Result<Outcome> {
    let identity = identity.unwrap_or("-");
    let mode = if remount { "remount" } else { "mount" };
    let ntfs3g = ntfs3g.to_string_lossy();
    run(&["mount", bsd_name, identity, mode, name, &ntfs3g], true)
}

/// The mount point from a successful mount's output.
pub fn mounted_path(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("MOUNTED:"))
        .map(PathBuf::from)
        .filter(|p| helper_proto::is_volumes_child(p))
}

pub fn unmount(path: &Path) -> Result<Outcome> {
    run(&["unmount", &path.to_string_lossy()], true)
}

pub fn cleanup() -> Result<Outcome> {
    run(&["cleanup"], false)
}

pub fn set_remember(secs: u32) -> Result<Outcome> {
    run(&["set-remember", &secs.to_string()], true)
}

pub fn uninstall() -> Result<Outcome> {
    run(&["uninstall"], true)
}

/// Current "remember authentication" duration, read from the rights database.
pub fn current_remember() -> Option<u32> {
    let out = cmd::run(
        Path::new("/usr/bin/security"),
        &["authorizationdb", "read", helper_proto::MOUNT_RIGHT],
        Some(std::time::Duration::from_secs(10)),
    )
    .ok()?;
    if !out.success() {
        return None;
    }
    let value: plist::Value = plist::from_bytes(out.stdout.as_bytes()).ok()?;
    let timeout = value.as_dictionary()?.get("timeout")?.as_unsigned_integer()?;
    u32::try_from(timeout).ok()
}

/// Whether any helper-created mount point is no longer mounted (so
/// `cleanup` has something to do).
pub fn needs_cleanup() -> bool {
    let created = helper_proto::read_created();
    if created.is_empty() {
        return false;
    }
    let mounted: std::collections::HashSet<PathBuf> = crate::mounts::snapshot()
        .map(|entries| entries.into_iter().map(|e| e.on).collect())
        .unwrap_or_default();
    created.iter().any(|p| !mounted.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_helper_results() {
        assert_eq!(
            classify(Some(0), "MOUNTED:/Volumes/X\n", ""),
            Outcome::Success {
                stdout: "MOUNTED:/Volumes/X".into()
            }
        );
        assert_eq!(classify(Some(exit::CANCELED), "", "cancelled"), Outcome::Canceled);
        assert!(matches!(
            classify(Some(1), "", "sudo: a password is required\n"),
            Outcome::Failed {
                code: exit::UNSAFE_INSTALL,
                ..
            }
        ));
        assert!(matches!(
            classify(Some(exit::NTFS3G_FAILED), "", "ntfs-3g-exit: 15\n"),
            Outcome::Failed {
                code: exit::NTFS3G_FAILED,
                ..
            }
        ));
        assert!(matches!(
            classify(None, "", ""),
            Outcome::Failed {
                code: exit::INTERNAL,
                ..
            }
        ));
    }

    #[test]
    fn mounted_path_must_be_in_volumes() {
        assert_eq!(
            mounted_path("noise\nMOUNTED:/Volumes/Новый том\n"),
            Some(PathBuf::from("/Volumes/Новый том"))
        );
        assert_eq!(mounted_path("MOUNTED:/etc"), None);
        assert_eq!(mounted_path(""), None);
    }
}
