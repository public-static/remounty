//! Well-known locations used by Remounty.

use std::ffi::CStr;
use std::path::PathBuf;

pub const APP_NAME: &str = "Remounty";
pub const BUNDLE_ID: &str = "app.remounty";
/// Identifier used by earlier builds (login item migration).
pub const LEGACY_BUNDLE_ID: &str = "io.github.remounty";

/// The current user's home directory.
///
/// `$HOME` is preferred; the password database is used as a fallback so a
/// missing or relative `$HOME` never leads to paths relative to the cwd.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home);
        if path.is_absolute() {
            return Some(path);
        }
    }
    home_from_passwd()
}

fn home_from_passwd() -> Option<PathBuf> {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers reference live, correctly sized buffers owned by
    // this function; `result` is only dereferenced when non-null.
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: `pw_dir` is a NUL terminated string inside `buf`.
    let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
    let path = PathBuf::from(dir.to_string_lossy().into_owned());
    path.is_absolute().then_some(path)
}

pub fn support_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join("Library/Application Support").join(APP_NAME))
}

pub fn logs_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join("Library/Logs"))
}

/// Where versions before the privileged helper created mount points.
pub fn legacy_mount_root() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".remounty"))
}

/// The current user's short name (from the password database, not `$USER`).
pub fn user_name() -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: `passwd` is plain old data; all-zero is a valid value.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers reference live, correctly sized buffers owned by
    // this function; `result` is only dereferenced when non-null.
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: `pw_name` is a NUL terminated string inside `buf`.
    let name = unsafe { CStr::from_ptr(pwd.pw_name) };
    Some(name.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
}

pub fn launch_agent_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{BUNDLE_ID}.plist")))
}

pub fn legacy_launch_agent_path() -> Option<PathBuf> {
    home_dir().map(|h| {
        h.join("Library/LaunchAgents")
            .join(format!("{LEGACY_BUNDLE_ID}.plist"))
    })
}
