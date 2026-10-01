//! Installing and removing the privileged helper.
//!
//! `install` must be started as root through the macOS administrator dialog
//! (never through sudo, so the sudoers rule cannot be used to reinstall the
//! helper). It copies the helper binary from Remounty.app into a staging
//! directory, swaps it into place, installs the two authorization rights
//! and, last, the sudoers rule (validated with `visudo`).
//!
//! ntfs-3g and macFUSE are not copied: the helper runs them where MacPorts
//! installed them, after verifying before every mount that nobody but root
//! can modify them (see [`crate::trust`]).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::cmd;
use crate::helper::{self, Fail, HelperResult, fail};
use crate::helper_proto::{self, exit};

const STAGING_DIR: &str = "/Library/PrivilegedHelperTools/.remounty-staging";
const OLD_DIR: &str = "/Library/PrivilegedHelperTools/.remounty-old";
const SUDOERS_TMP: &str = "/etc/sudoers.d/.remounty.tmp";

/// `install <user> <remember seconds>`
pub fn install(args: &[String]) -> HelperResult {
    if helper::invoked_via_sudo() {
        return Err(fail(
            exit::BAD_ARGS,
            "The helper can only be installed through the macOS administrator dialog.",
        ));
    }
    let [user, remember] = args else {
        return Err(fail(exit::BAD_ARGS, "usage: install <user> <remember seconds>"));
    };
    validate_user(user)?;
    let remember: u32 = remember
        .parse()
        .ok()
        .filter(|s| helper_proto::REMEMBER_CHOICES.contains(s))
        .ok_or_else(|| fail(exit::BAD_ARGS, "unsupported remember duration"))?;
    let self_exe = std::env::current_exe().map_err(|err| fail(exit::INTERNAL, err.to_string()))?;
    if fs::canonicalize(&self_exe).ok().as_deref() == Some(Path::new(helper_proto::HELPER_BIN)) {
        return Err(fail(exit::BAD_ARGS, "Install from the copy inside Remounty.app."));
    }

    let staging = Path::new(STAGING_DIR);
    remove_tree(staging)?;
    make_dir(staging)?;
    if let Err(failure) = copy_file(&self_exe, &staging.join(helper_proto::BUNDLED_HELPER_NAME), 0o755) {
        let _ = remove_tree(staging);
        return Err(failure);
    }
    // Keep the record of existing mounts across reinstalls.
    let old_state = Path::new(helper_proto::STATE_FILE);
    if old_state.is_file() {
        let _ = copy_file(old_state, &staging.join("mountpoints.json"), 0o644);
    }

    swap_into_place(staging)?;
    helper::verify_installation()
        .and_then(|()| {
            write_right(
                helper_proto::MOUNT_RIGHT,
                "Mount and unmount NTFS volumes with Remounty.",
                remember,
            )
        })
        .and_then(|()| write_right(helper_proto::ADMIN_RIGHT, "Change Remounty's helper settings.", 0))
        .and_then(|()| install_sudoers(user))
        .inspect_err(|_| rollback())?;
    let _ = remove_tree(Path::new(OLD_DIR));
    println!("INSTALLED");
    Ok(())
}

/// `uninstall` — through sudo it requires an administrator authorization.
pub fn uninstall(args: &[String]) -> HelperResult {
    if !args.is_empty() {
        return Err(fail(exit::BAD_ARGS, "usage: uninstall"));
    }
    if helper::invoked_via_sudo() {
        helper::require_authorization(helper_proto::ADMIN_RIGHT, "Remounty wants to remove its helper.")?;
    }
    let created = helper_proto::read_created();
    let mounted: HashSet<PathBuf> = crate::mounts::snapshot()
        .map(|entries| entries.into_iter().map(|e| e.on).collect())
        .unwrap_or_default();
    if let Some(active) = created.iter().find(|p| mounted.contains(*p)) {
        return Err(fail(
            exit::WRONG_STATE,
            format!("{} is still mounted. Unmount it first.", active.display()),
        ));
    }
    // Revoke access first, then remove the files.
    remove_file_if_exists(Path::new(helper_proto::SUDOERS_FILE))?;
    for right in [helper_proto::MOUNT_RIGHT, helper_proto::ADMIN_RIGHT] {
        let _ = run_tool("/usr/bin/security", &["authorizationdb", "remove", right], None);
    }
    for path in &created {
        let _ = fs::remove_dir(path);
    }
    remove_tree(Path::new(helper_proto::HELPER_DIR))?;
    let _ = remove_tree(Path::new(STAGING_DIR));
    let _ = remove_tree(Path::new(OLD_DIR));
    println!("UNINSTALLED");
    Ok(())
}

fn validate_user(user: &str) -> HelperResult {
    let chars_ok = !user.is_empty()
        && user.len() <= 64
        && !user.starts_with('-')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !chars_ok {
        return Err(fail(exit::BAD_ARGS, format!("invalid user name {user:?}")));
    }
    let c_user = std::ffi::CString::new(user).map_err(|_| fail(exit::BAD_ARGS, "invalid user name"))?;
    // SAFETY: getpwnam with a valid C string; only checked for null.
    let pw = unsafe { libc::getpwnam(c_user.as_ptr()) };
    if pw.is_null() {
        return Err(fail(exit::BAD_ARGS, format!("unknown user {user:?}")));
    }
    Ok(())
}

fn swap_into_place(staging: &Path) -> HelperResult {
    let final_dir = Path::new(helper_proto::HELPER_DIR);
    let old = Path::new(OLD_DIR);
    remove_tree(old)?;
    if fs::symlink_metadata(final_dir).is_ok() {
        fs::rename(final_dir, old)
            .map_err(|err| fail(exit::INTERNAL, format!("Cannot replace the old helper: {err}")))?;
    }
    if let Err(err) = fs::rename(staging, final_dir) {
        let _ = fs::rename(old, final_dir);
        return Err(fail(exit::INTERNAL, format!("Cannot install the helper: {err}")));
    }
    Ok(())
}

fn rollback() {
    let _ = remove_tree(Path::new(helper_proto::HELPER_DIR));
    if fs::symlink_metadata(OLD_DIR).is_ok() {
        let _ = fs::rename(OLD_DIR, helper_proto::HELPER_DIR);
    }
}

/// Installs (or replaces) an authorization right. `allow-root` is false so
/// the helper (running as root) cannot pass the check without the user, and
/// `shared` is false so only Remounty's own session benefits from `timeout`.
pub fn write_right(name: &str, comment: &str, timeout: u32) -> HelperResult {
    let mut dict = plist::Dictionary::new();
    dict.insert("class".into(), "user".into());
    dict.insert("group".into(), "admin".into());
    dict.insert("authenticate-user".into(), true.into());
    dict.insert("allow-root".into(), false.into());
    dict.insert("session-owner".into(), false.into());
    dict.insert("shared".into(), false.into());
    dict.insert("timeout".into(), plist::Value::Integer(u64::from(timeout).into()));
    dict.insert("tries".into(), plist::Value::Integer(10_000u64.into()));
    dict.insert("version".into(), plist::Value::Integer(1u64.into()));
    dict.insert("comment".into(), comment.into());
    let mut xml = Vec::new();
    plist::Value::Dictionary(dict)
        .to_writer_xml(&mut xml)
        .map_err(|err| fail(exit::INTERNAL, err.to_string()))?;
    run_tool(
        "/usr/bin/security",
        &["authorizationdb", "write", name],
        Some(&xml),
    )
    .map(|_| ())
}

fn install_sudoers(user: &str) -> HelperResult {
    let content = sudoers_content(user);
    let tmp = Path::new(SUDOERS_TMP);
    remove_file_if_exists(tmp)?;
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .and_then(|mut f| f.write_all(content.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| fs::set_permissions(tmp, fs::Permissions::from_mode(0o440)));
    if let Err(err) = written {
        let _ = fs::remove_file(tmp);
        return Err(fail(
            exit::INTERNAL,
            format!("Cannot write the sudoers rule: {err}"),
        ));
    }
    if let Err(failure) = run_tool("/usr/sbin/visudo", &["-c", "-f", SUDOERS_TMP], None) {
        let _ = fs::remove_file(tmp);
        return Err(failure);
    }
    fs::rename(tmp, helper_proto::SUDOERS_FILE).map_err(|err| {
        let _ = fs::remove_file(tmp);
        fail(exit::INTERNAL, format!("Cannot install the sudoers rule: {err}"))
    })
}

pub fn sudoers_content(user: &str) -> String {
    format!(
        "# Installed by Remounty. Lets {user} run Remounty's helper as root without typing a\n\
         # password. The helper itself asks macOS to authorize every mount (password dialog).\n\
         # Remove it with \"Uninstall Helper\" in Remounty's menu.\n\
         {user} ALL = (root) NOPASSWD: {}\n",
        helper_proto::HELPER_BIN
    )
}

// ---------------------------------------------------------------------------
// Small file utilities (all paths are fixed, root-owned locations)

fn run_tool(tool: &str, args: &[&str], input: Option<&[u8]>) -> std::result::Result<String, Fail> {
    let out = cmd::run_with(
        Path::new(tool),
        args,
        &cmd::Options {
            env: &[("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")],
            clear_env: true,
            input,
            timeout: Some(std::time::Duration::from_secs(120)),
        },
    )
    .map_err(|err| fail(exit::INTERNAL, format!("{tool}: {err}")))?;
    if !out.success() {
        return Err(fail(
            exit::INTERNAL,
            format!("{tool} failed: {}", out.diagnostics()),
        ));
    }
    Ok(out.stdout)
}

/// Copies a file and makes it `root:wheel`. On APFS `fs::copy` clones the
/// file, and a clone made by root keeps the *original* owner, so ownership
/// must always be set explicitly.
fn copy_file(from: &Path, to: &Path, mode: u32) -> HelperResult {
    fs::copy(from, to)
        .and_then(|_| std::os::unix::fs::chown(to, Some(0), Some(0)))
        .and_then(|()| fs::set_permissions(to, fs::Permissions::from_mode(mode)))
        .map_err(|err| fail(exit::INTERNAL, format!("Cannot copy {}: {err}", from.display())))
}

fn make_dir(path: &Path) -> HelperResult {
    fs::create_dir(path)
        .and_then(|()| std::os::unix::fs::chown(path, Some(0), Some(0)))
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o755)))
        .map_err(|err| fail(exit::INTERNAL, format!("Cannot create {}: {err}", path.display())))
}

fn remove_tree(path: &Path) -> HelperResult {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
    .map_err(|err| fail(exit::INTERNAL, format!("Cannot remove {}: {err}", path.display())))
}

fn remove_file_if_exists(path: &Path) -> HelperResult {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(fail(
            exit::INTERNAL,
            format!("Cannot remove {}: {err}", path.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudoers_rule_is_limited_to_the_helper() {
        let text = sudoers_content("alice");
        assert!(text.contains(
            "alice ALL = (root) NOPASSWD: /Library/PrivilegedHelperTools/remounty/remounty-helper\n"
        ));
        assert_eq!(text.lines().filter(|l| !l.starts_with('#')).count(), 1);
    }

    #[test]
    fn user_names_are_validated() {
        assert!(validate_user("root").is_ok());
        assert!(validate_user("-x").is_err());
        assert!(validate_user("a b").is_err());
        assert!(validate_user("a\nALL ALL=(ALL) ALL").is_err());
        assert!(validate_user("no-such-user-remounty").is_err());
    }
}
