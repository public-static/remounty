//! The operations. Each one re-reads the current state of the volume right
//! before acting, refuses to do anything unexpected, and after a failure
//! tries to return the volume to the state it was found in. Everything that
//! needs root goes through the privileged helper.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::cmd;
use crate::deps::Dependencies;
use crate::disks::{self, MountState, Volume, VolumeKey};
use crate::error::{Error, Result};
use crate::helper_client::{self, Status};
use crate::helper_proto::{self, exit};
use crate::privileged::Outcome;
use crate::{mounts, naming, paths};

const USER_DISKUTIL_TIMEOUT: Duration = Duration::from_secs(90);
const DAEMON_EXIT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Mount read-write with ntfs-3g (unmounting the native read-only mount first if needed).
    MountReadWrite {
        key: VolumeKey,
        name: String,
        label: String,
    },
    /// Unmount an ntfs-3g mount.
    Unmount {
        key: VolumeKey,
        name: String,
        label: String,
    },
    /// Mount with the built-in (read-only) macOS driver.
    MountReadOnly {
        key: VolumeKey,
        name: String,
        label: String,
    },
    /// Unmount a leftover helper mount whose disk is gone.
    UnmountStale { path: PathBuf },
    /// Install or update the privileged helper.
    InstallHelper,
    /// Remove the privileged helper.
    UninstallHelper,
    /// Change how long a mount authorization is remembered.
    SetRemember { seconds: u32 },
}

impl Operation {
    pub fn key(&self) -> Option<&VolumeKey> {
        match self {
            Operation::MountReadWrite { key, .. }
            | Operation::Unmount { key, .. }
            | Operation::MountReadOnly { key, .. } => Some(key),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Operation::MountReadWrite { name, .. } => format!("Re-mounting “{name}” read-write…"),
            Operation::Unmount { name, .. } => format!("Unmounting “{name}”…"),
            Operation::MountReadOnly { name, .. } => format!("Mounting “{name}” read-only…"),
            Operation::UnmountStale { path } => format!("Unmounting {}…", path.display()),
            Operation::InstallHelper => "Installing the helper…".into(),
            Operation::UninstallHelper => "Removing the helper…".into(),
            Operation::SetRemember { .. } => "Changing the authorization setting…".into(),
        }
    }

    /// Whether this operation changes the helper itself.
    pub fn affects_helper(&self) -> bool {
        matches!(
            self,
            Operation::InstallHelper | Operation::UninstallHelper | Operation::SetRemember { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Done {
        message: String,
        /// Location to open when the user clicks the notification.
        open_path: Option<PathBuf>,
    },
    /// The user dismissed the authorization dialog; nothing was changed.
    Canceled,
    Failed {
        title: String,
        detail: String,
    },
}

impl Report {
    fn failed(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Report::Failed {
            title: title.into(),
            detail: detail.into(),
        }
    }

    fn done(message: impl Into<String>) -> Self {
        Report::Done {
            message: message.into(),
            open_path: None,
        }
    }
}

pub struct Context<'a> {
    pub deps: &'a Dependencies,
}

pub fn execute(op: &Operation, ctx: &Context<'_>) -> Report {
    crate::log_info!("Starting: {op:?}");
    let report = match op {
        Operation::MountReadWrite { key, name, label } => mount_read_write(key, name, label, ctx),
        Operation::Unmount { key, name, label } => unmount(key, name, label),
        Operation::MountReadOnly { key, name, label } => mount_read_only(key, name, label),
        Operation::UnmountStale { path } => unmount_stale(path),
        Operation::InstallHelper => install_helper(),
        Operation::UninstallHelper => uninstall_helper(),
        Operation::SetRemember { seconds } => set_remember(*seconds),
    };
    crate::log_info!("Finished: {report:?}");
    report
}

/// Re-reads the volume and makes sure it is still the one the user picked.
fn current_volume(key: &VolumeKey, name: &str, label: &str) -> std::result::Result<Volume, Report> {
    let vol = disks::rescan_volume(&key.bsd_name).map_err(|err| {
        Report::failed(
            format!("“{name}” is not available"),
            format!("{err}\n\nWas the disk disconnected?"),
        )
    })?;
    // Without any UUID the device name is the only identity, so additionally
    // require the same volume label before touching it.
    let same_volume = vol.identity == key.identity && (key.identity.is_some() || vol.label == label);
    if !same_volume {
        return Err(Report::failed(
            format!("“{name}” changed"),
            format!(
                "The device {} now holds a different volume than the one you selected. \
                 Nothing was changed. Please choose the volume again from the menu.",
                key.bsd_name
            ),
        ));
    }
    Ok(vol)
}

// ---------------------------------------------------------------------------
// Mount read-write

fn mount_read_write(key: &VolumeKey, name: &str, label: &str, ctx: &Context<'_>) -> Report {
    let ntfs3g = match &ctx.deps.ntfs3g {
        Ok(path) => path.clone(),
        Err(reason) => return Report::failed("ntfs-3g is not available", reason.clone()),
    };
    if let Err(reason) = &ctx.deps.macfuse {
        return Report::failed("macFUSE is not available", reason.clone());
    }
    let status = helper_client::status();
    if !status.is_ready() {
        return Report::failed(
            "Remounty's helper is not ready",
            format!(
                "The helper is {status}. Use “Install Helper…” or “Update Helper…” in the \
                 Remounty menu, then try again. Nothing was changed."
            ),
        );
    }
    let vol = match current_volume(key, name, label) {
        Ok(vol) => vol,
        Err(report) => return report,
    };
    let remount = match &vol.state {
        MountState::Native { read_only: true, .. } => true,
        MountState::Unmounted => false,
        MountState::Native {
            read_only: false,
            path,
        } => {
            return Report::failed(
                format!("“{name}” is already writable"),
                format!(
                    "It is mounted read-write at {} by another NTFS driver.",
                    path.display()
                ),
            );
        }
        MountState::Fuse { path, .. } => {
            return Report::failed(
                format!("“{name}” is already mounted with ntfs-3g"),
                format!("It is mounted at {}.", path.display()),
            );
        }
    };
    let identity = vol.identity.as_deref().filter(|id| naming::is_safe_identity(id));

    // The mount point (and the name Finder shows) is the same unique name
    // the menu uses, so two unnamed volumes can be told apart everywhere.
    let report = match helper_client::mount(&vol.bsd_name, identity, remount, name, &ntfs3g) {
        Ok(Outcome::Success { stdout }) => match helper_client::mounted_path(&stdout) {
            Some(path) => verify_read_write(key, name, &path),
            None => Report::failed(
                format!("Could not verify “{name}”"),
                format!("The helper reported success without a mount point:\n{stdout}"),
            ),
        },
        Ok(Outcome::Canceled) => Report::Canceled,
        Ok(Outcome::Failed { code, message }) => explain_mount_failure(name, code, &message),
        Err(err) => Report::failed(format!("Could not mount “{name}”"), err.to_string()),
    };
    if remount && !matches!(report, Report::Done { .. }) {
        return with_restore_note(report, key, name);
    }
    report
}

fn verify_read_write(key: &VolumeKey, name: &str, mount_point: &Path) -> Report {
    // The mount table can lag for a moment behind ntfs-3g's exit.
    let mut last = None;
    for _ in 0..20 {
        match disks::rescan_volume(&key.bsd_name) {
            Ok(vol) => {
                if let MountState::Fuse { path, read_only, .. } = &vol.state
                    && path == mount_point
                {
                    return if *read_only {
                        Report::failed(
                            format!("“{name}” was mounted read-only"),
                            "ntfs-3g refused write access, most likely because Windows is \
                             hibernated or used Fast Startup on this volume. Your data was not \
                             modified. Shut Windows down completely (or run “powercfg /h off” \
                             in Windows) and try again. The volume stays readable in the meantime.",
                        )
                    } else {
                        Report::Done {
                            message: format!("“{name}” is now writable. Click to open it."),
                            open_path: Some(mount_point.to_path_buf()),
                        }
                    };
                }
                last = Some(vol.state);
            }
            Err(err) => crate::log_warn!("Verification scan failed: {err}"),
        }
        thread::sleep(Duration::from_millis(250));
    }
    Report::failed(
        format!("Could not verify “{name}”"),
        format!(
            "ntfs-3g reported success, but the volume does not appear at {} (current state: {:?}).",
            mount_point.display(),
            last
        ),
    )
}

fn explain_mount_failure(name: &str, code: i32, message: &str) -> Report {
    let title = format!("Could not mount “{name}” read-write");
    // The helper appends whether it put the read-only mount back.
    let message: String = message
        .lines()
        .filter(|l| !l.starts_with("restored-read-only:"))
        .collect::<Vec<_>>()
        .join("\n");
    let detail = match code {
        exit::DEVICE_GONE => "The disk is no longer available. Was it disconnected?".to_string(),
        exit::IDENTITY_MISMATCH => {
            "The device now holds a different volume than the one selected. Nothing was changed.".to_string()
        }
        exit::UNMOUNT_FAILED => format!(
            "macOS could not unmount the read-only volume, most likely because a program or \
             Finder window is using it. Close it and try again. Nothing was changed.\n\n{}",
            message.trim()
        ),
        exit::NTFS3G_FAILED => {
            // ntfs-3g maps *every* EPERM to its "hibernated" exit code, including
            // macOS refusing to let it open the device at all. Tell those apart.
            let reason = if device_open_denied(&message) {
                DEVICE_ACCESS_DENIED
            } else {
                ntfs3g_exit_code(&message)
                    .map(ntfs3g_reason)
                    .unwrap_or("ntfs-3g reported an error.")
            };
            let output: String = message
                .lines()
                .filter(|l| !l.starts_with("ntfs-3g-exit:"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{reason}\n\n{}", output.trim())
        }
        exit::AUTH_FAILED => format!("Authorization failed. Nothing was changed.\n\n{}", message.trim()),
        exit::UNSAFE_INSTALL => format!(
            "Remounty's helper refused to run because its installation is not intact. Reinstall \
             it with “Update Helper…” in the Remounty menu. Nothing was changed.\n\n{}",
            message.trim()
        ),
        exit::WRONG_STATE => format!("{}\n\nNothing was changed.", message.trim()),
        exit::BAD_ARGS | exit::INTERNAL => format!(
            "Remounty refused to continue because of an internal inconsistency. Nothing was \
             changed.\n\n{}",
            message.trim()
        ),
        _ => format!("Unexpected error ({code}):\n{}", message.trim()),
    };
    Report::failed(title, detail.trim().to_string())
}

const DEVICE_ACCESS_DENIED: &str = "macOS did not allow ntfs-3g to access the disk, so the volume \
    was not opened and nothing on it was changed. This is a macOS privacy protection for external \
    and removable disks, not a problem with the volume.\n\n\
    Open System Settings → Privacy & Security → Files & Folders and allow “Removable Volumes” for \
    Remounty, or add Remounty to Full Disk Access, then try again.";

/// ntfs-3g could not even open the device node ("Error opening '/dev/…':
/// Operation not permitted") — access was denied before anything was read.
pub fn device_open_denied(message: &str) -> bool {
    message
        .lines()
        .any(|l| l.contains("Error opening '/dev/") && l.contains("Operation not permitted"))
}

/// Extracts the ntfs-3g exit status the helper reports on failure.
pub fn ntfs3g_exit_code(message: &str) -> Option<i32> {
    message
        .lines()
        .find_map(|l| l.trim().strip_prefix("ntfs-3g-exit:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Meaning of ntfs-3g's exit codes (`ntfs_volume_status` in ntfs-3g).
pub fn ntfs3g_reason(code: i32) -> &'static str {
    match code {
        11 => "ntfs-3g rejected the mount options.",
        12 => "The partition does not contain a valid NTFS file system.",
        13 => "The NTFS file system is inconsistent. Repair it on Windows with “chkdsk /f” first.",
        14 => {
            "Windows is hibernated (or used Fast Startup) on this volume. Writing to it now \
             could destroy data. Shut Windows down completely, or disable Fast Startup with \
             “powercfg /h off”, then try again."
        }
        15 => {
            "The volume was not cleanly unmounted by Windows. To protect your data it will not \
             be opened for writing. Attach it to Windows, run “chkdsk /f” and eject it safely, \
             then try again."
        }
        16 => "The volume is locked or in use by another program.",
        17 => "The volume is part of a RAID / dynamic disk, which ntfs-3g cannot mount.",
        19 => "ntfs-3g did not get the privileges it needs.",
        20 => "ntfs-3g ran out of memory.",
        21 => {
            "macFUSE could not be used. Make sure its system extension is allowed in System \
             Settings → Privacy & Security, then restart your Mac."
        }
        22 => "ntfs-3g considers this setup insecure and refused to mount.",
        _ => "ntfs-3g could not mount the volume.",
    }
}

/// After a failed read-write attempt, make sure a volume that was mounted
/// read-only before is mounted read-only again (the helper already tries;
/// this is the second line of defence).
fn with_restore_note(report: Report, key: &VolumeKey, name: &str) -> Report {
    let Ok(vol) = disks::rescan_volume(&key.bsd_name) else {
        return report;
    };
    if vol.identity != key.identity {
        return report;
    }
    let note = match vol.state {
        MountState::Unmounted => {
            crate::log_warn!("{} was left unmounted; mounting it read-only again", key.bsd_name);
            match diskutil_as_user(&["mount", &vol.bsd_name]) {
                Ok(()) => format!("“{name}” has been mounted read-only again."),
                Err(err) => format!(
                    "“{name}” is currently unmounted and could not be mounted read-only again \
                     ({err}). Use “Mount Read-Only” from the menu or reconnect the disk."
                ),
            }
        }
        MountState::Native { read_only: true, .. } if !matches!(report, Report::Canceled) => {
            format!("“{name}” is mounted read-only, as before.")
        }
        _ => return report,
    };
    match report {
        Report::Failed { title, detail } => Report::Failed {
            title,
            detail: format!("{detail}\n\n{note}"),
        },
        Report::Canceled => Report::failed(format!("“{name}” was unmounted"), note),
        done => done,
    }
}

// ---------------------------------------------------------------------------
// Unmount

fn unmount(key: &VolumeKey, name: &str, label: &str) -> Report {
    let vol = match current_volume(key, name, label) {
        Ok(vol) => vol,
        Err(report) => return report,
    };
    let MountState::Fuse { path, ours, .. } = &vol.state else {
        return Report::failed(
            format!("“{name}” is not mounted with ntfs-3g"),
            "Nothing was changed.",
        );
    };
    unmount_path(path, &vol.device_node(), name, *ours)
}

fn unmount_stale(path: &Path) -> Report {
    let is_ours = helper_proto::read_created().contains(path);
    let is_fuse = mounts::snapshot()
        .map(|entries| entries.iter().any(|e| e.on == path && e.is_fuse()))
        .unwrap_or(false);
    if !is_ours || !is_fuse {
        return Report::failed(
            "Nothing to unmount",
            format!("{} is not an ntfs-3g mount created by Remounty.", path.display()),
        );
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    unmount_path(path, "", &name, true)
}

/// Unmounts without ever forcing it: a busy volume is reported, not yanked.
/// Tries without privileges first; the helper is only needed if that fails.
fn unmount_path(path: &Path, device: &str, name: &str, ours: bool) -> Report {
    let path_str = path.to_string_lossy().into_owned();
    let first_attempt = diskutil_as_user(&["unmount", &path_str]);
    // A timed-out attempt may still have completed; never escalate for a
    // volume that is already gone.
    if let Err(err) = &first_attempt
        && is_mounted(path)
    {
        if is_busy_message(err.message()) {
            return busy_report(name, err.message());
        }
        if !ours {
            return Report::failed(format!("Could not unmount “{name}”"), err.to_string());
        }
        crate::log_info!("Unprivileged unmount failed ({err}); asking the helper");
        match helper_client::unmount(path) {
            Ok(Outcome::Success { .. }) => {}
            Ok(Outcome::Canceled) => return Report::Canceled,
            Ok(Outcome::Failed { message, .. }) => {
                if is_busy_message(&message) {
                    return busy_report(name, &message);
                }
                return Report::failed(format!("Could not unmount “{name}”"), message.trim().to_string());
            }
            Err(err) => return Report::failed(format!("Could not unmount “{name}”"), err.to_string()),
        }
    }
    if is_mounted(path) {
        return Report::failed(
            format!("“{name}” is still mounted"),
            "The unmount command reported success, but the volume is still mounted.",
        );
    }
    if !device.is_empty() && !wait_for_ntfs3g_exit(device) {
        return Report::failed(
            format!("“{name}” is still being finalised"),
            "The volume was unmounted, but ntfs-3g is still writing its last changes. Wait a \
             little before disconnecting the disk.",
        );
    }
    if ours {
        cleanup_mount_points();
    }
    Report::done(format!(
        "“{name}” was unmounted and all changes were written to the disk."
    ))
}

/// Asks the helper to remove mount points in /Volumes that are no longer in
/// use. Harmless if it fails (the folders are empty and root-owned).
pub fn cleanup_mount_points() {
    match helper_client::cleanup() {
        Ok(Outcome::Success { .. }) => {}
        Ok(other) => crate::log_warn!("Helper cleanup: {other:?}"),
        Err(err) => crate::log_warn!("Helper cleanup: {err}"),
    }
}

fn is_mounted(path: &Path) -> bool {
    mounts::snapshot()
        .map(|entries| entries.iter().any(|e| e.on == path))
        .unwrap_or(true)
}

fn busy_report(name: &str, detail: &str) -> Report {
    Report::failed(
        format!("“{name}” is in use"),
        format!(
            "The volume could not be unmounted because a program is still using it. Close any \
             files, apps or Finder windows that use it and try again. Nothing was changed.\n\n{}",
            detail.trim()
        ),
    )
}

pub fn is_busy_message(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("busy") || m.contains("in use") || m.contains("dissent")
}

/// ntfs-3g flushes and marks the volume clean when its daemon exits after
/// the unmount. Wait for that so "done" really means "safe".
fn wait_for_ntfs3g_exit(device: &str) -> bool {
    let deadline = Instant::now() + DAEMON_EXIT_TIMEOUT;
    loop {
        match ntfs3g_running_for(device) {
            Ok(false) => return true,
            Ok(true) => {}
            Err(err) => {
                crate::log_warn!("Could not check for ntfs-3g process: {err}");
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn ntfs3g_running_for(device: &str) -> Result<bool> {
    let out = cmd::run(
        Path::new("/bin/ps"),
        &["-axww", "-o", "command="],
        Some(Duration::from_secs(10)),
    )?;
    if !out.success() {
        return Err(Error::new(out.diagnostics()));
    }
    Ok(ps_lists_ntfs3g(&out.stdout, device))
}

pub fn ps_lists_ntfs3g(ps_output: &str, device: &str) -> bool {
    ps_output.lines().any(|line| {
        let mut tokens = line.split_whitespace();
        let is_ntfs3g = tokens
            .next()
            .and_then(|exe| Path::new(exe).file_name())
            .is_some_and(|n| n == "ntfs-3g" || n == "lowntfs-3g");
        is_ntfs3g && line.split_whitespace().any(|t| t == device)
    })
}

// ---------------------------------------------------------------------------
// Mount read-only (native driver)

fn mount_read_only(key: &VolumeKey, name: &str, label: &str) -> Report {
    let vol = match current_volume(key, name, label) {
        Ok(vol) => vol,
        Err(report) => return report,
    };
    if vol.state != MountState::Unmounted {
        return Report::failed(format!("“{name}” is already mounted"), "Nothing was changed.");
    }
    match diskutil_as_user(&["mount", &vol.bsd_name]) {
        Ok(()) => Report::Done {
            message: format!("“{name}” was mounted read-only."),
            open_path: disks::rescan_volume(&vol.bsd_name)
                .ok()
                .and_then(|v| v.state.path().map(Path::to_path_buf)),
        },
        Err(err) => Report::failed(format!("Could not mount “{name}”"), err.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Helper management

fn install_helper() -> Report {
    let title = "The helper could not be installed";
    let Some(user) = paths::user_name() else {
        return Report::failed(title, "Your user name could not be determined.");
    };
    // Keep the user's "remember" choice across updates.
    let remember = helper_client::current_remember()
        .filter(|s| helper_proto::REMEMBER_CHOICES.contains(s))
        .unwrap_or(0);
    match helper_client::install(&user, remember) {
        Ok(Outcome::Success { .. }) => match helper_client::status() {
            Status::Ready => Report::done("Remounty's helper is installed."),
            other => Report::failed(title, format!("After installing, the helper is {other}.")),
        },
        Ok(Outcome::Canceled) => Report::Canceled,
        Ok(Outcome::Failed { message, .. }) => Report::failed(title, message.trim().to_string()),
        Err(err) => Report::failed(title, err.to_string()),
    }
}

fn uninstall_helper() -> Report {
    let title = "The helper could not be removed";
    // A working helper removes itself (asking for authorization); a damaged
    // one is removed through the administrator dialog.
    let outcome = match helper_client::uninstall() {
        Ok(Outcome::Failed {
            code: exit::UNSAFE_INSTALL,
            ..
        }) => helper_client::uninstall_with_dialog(),
        other => other,
    };
    match outcome {
        Ok(Outcome::Success { .. }) => Report::done("Remounty's helper was removed."),
        Ok(Outcome::Canceled) => Report::Canceled,
        Ok(Outcome::Failed { message, .. }) => Report::failed(title, message.trim().to_string()),
        Err(err) => Report::failed(title, err.to_string()),
    }
}

fn set_remember(seconds: u32) -> Report {
    match helper_client::set_remember(seconds) {
        Ok(Outcome::Success { .. }) => Report::done(match seconds {
            0 => "Remounty will ask for authorization every time.".to_string(),
            helper_proto::REMEMBER_UNTIL_LOGOUT => {
                "Your authorization is remembered until you log out.".to_string()
            }
            s => format!("Your authorization is remembered for {} minutes.", s / 60),
        }),
        Ok(Outcome::Canceled) => Report::Canceled,
        Ok(Outcome::Failed { message, .. }) => {
            Report::failed("The setting could not be changed", message.trim().to_string())
        }
        Err(err) => Report::failed("The setting could not be changed", err.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn diskutil_as_user(args: &[&str]) -> Result<()> {
    let out = cmd::run(Path::new(disks::DISKUTIL), args, Some(USER_DISKUTIL_TIMEOUT))?;
    if out.success() {
        Ok(())
    } else {
        Err(Error::new(out.diagnostics()))
    }
}

/// Removes empty, unused folders left in `~/.remounty` by earlier versions,
/// which mounted there. `remove_dir` only deletes empty directories and
/// fails on mount points, so this cannot remove data.
pub fn clean_legacy_mount_root() {
    let Some(root) = paths::legacy_mount_root() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let mounted: Vec<PathBuf> = mounts::snapshot()
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.on)
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_real_dir = fs::symlink_metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        if is_real_dir && !mounted.contains(&path) {
            let _ = fs::remove_dir(&path);
        }
    }
    let _ = fs::remove_dir(&root);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_detection() {
        assert!(is_busy_message(
            "Volume Data on disk4s1 failed to unmount: dissented by PID 42"
        ));
        assert!(is_busy_message("umount: unmount(/x): Resource busy"));
        assert!(!is_busy_message("Permission denied"));
    }

    #[test]
    fn ps_matching() {
        let ps = "/Library/PrivilegedHelperTools/remounty/bin/ntfs-3g /dev/disk4s1 /Volumes/T -o volname=T\n\
                  /usr/bin/vim /dev/disk4s1\n";
        assert!(ps_lists_ntfs3g(ps, "/dev/disk4s1"));
        assert!(!ps_lists_ntfs3g(ps, "/dev/disk4s10"));
        assert!(!ps_lists_ntfs3g("/usr/bin/vim /dev/disk5s1", "/dev/disk5s1"));
    }

    #[test]
    fn extracts_ntfs3g_code() {
        assert_eq!(
            ntfs3g_exit_code("ntfs-3g-exit: 15\nThe disk contains an unclean file system"),
            Some(15)
        );
        assert_eq!(ntfs3g_exit_code("nothing"), None);
    }

    #[test]
    fn device_permission_is_not_reported_as_hibernation() {
        let message = "ntfs-3g-exit: 14\nError opening '/dev/disk4s2': Operation not permitted\n\
                       Failed to mount '/dev/disk4s2': Operation not permitted\n\
                       The NTFS partition is in an unsafe state. Please resume and shutdown";
        let report = explain_mount_failure("X", exit::NTFS3G_FAILED, message);
        assert!(matches!(report, Report::Failed { .. }));
        if let Report::Failed { detail, .. } = report {
            assert!(detail.contains("Removable Volumes"));
            assert!(!detail.starts_with("Windows is hibernated"));
        }
        // A genuine hibernation report keeps its explanation.
        let hibernated = "ntfs-3g-exit: 14\nWindows is hibernated, refused to mount.";
        if let Report::Failed { detail, .. } = explain_mount_failure("X", exit::NTFS3G_FAILED, hibernated) {
            assert!(detail.starts_with("Windows is hibernated"));
        }
    }

    #[test]
    fn failure_explanations() {
        let report = explain_mount_failure(
            "X",
            exit::NTFS3G_FAILED,
            "ntfs-3g-exit: 15\nunclean\nrestored-read-only: yes",
        );
        assert!(matches!(report, Report::Failed { .. }));
        if let Report::Failed { detail, .. } = report {
            assert!(detail.contains("not cleanly unmounted"));
            assert!(detail.contains("unclean"));
            assert!(!detail.contains("ntfs-3g-exit"));
            assert!(!detail.contains("restored-read-only"));
        }
    }
}
