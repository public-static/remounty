//! The mount operations. Each one re-reads the current state of the volume
//! right before acting, refuses to do anything unexpected, and after a failure
//! tries to return the volume to the state it was found in.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::cmd;
use crate::deps::Dependencies;
use crate::disks::{self, MountState, Volume, VolumeKey};
use crate::error::{Error, Result};
use crate::mounts;
use crate::privileged::{self, Outcome, exit};

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
    /// Unmount a leftover ntfs-3g mount in Remounty's directory whose disk is gone.
    UnmountStale { path: PathBuf },
    /// Enable Touch ID for sudo (creates /etc/pam.d/sudo_local).
    SetUpTouchId,
}

impl Operation {
    pub fn key(&self) -> Option<&VolumeKey> {
        match self {
            Operation::MountReadWrite { key, .. }
            | Operation::Unmount { key, .. }
            | Operation::MountReadOnly { key, .. } => Some(key),
            Operation::UnmountStale { .. } | Operation::SetUpTouchId => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Operation::MountReadWrite { name, .. } => format!("Re-mounting “{name}” read-write…"),
            Operation::Unmount { name, .. } => format!("Unmounting “{name}”…"),
            Operation::MountReadOnly { name, .. } => format!("Mounting “{name}” read-only…"),
            Operation::UnmountStale { path } => format!("Unmounting {}…", path.display()),
            Operation::SetUpTouchId => "Setting up Touch ID…".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Done {
        message: String,
        /// Location to open when the user clicks the notification.
        open_path: Option<PathBuf>,
    },
    /// The user dismissed the password prompt; nothing was changed.
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
}

pub struct Context<'a> {
    pub deps: &'a Dependencies,
    pub mount_root: Option<&'a Path>,
    /// Authenticate with Touch ID (via sudo) when it is set up.
    pub prefer_touch_id: bool,
}

pub fn execute(op: &Operation, ctx: &Context<'_>) -> Report {
    crate::log_info!("Starting: {op:?}");
    let report = match op {
        Operation::MountReadWrite { key, name, label } => mount_read_write(key, name, label, ctx),
        Operation::Unmount { key, name, label } => unmount(key, name, label, ctx),
        Operation::MountReadOnly { key, name, label } => mount_read_only(key, name, label, ctx),
        Operation::UnmountStale { path } => unmount_stale(path, ctx),
        Operation::SetUpTouchId => set_up_touch_id(),
    };
    crate::log_info!("Finished: {report:?}");
    report
}

/// Re-reads the volume and makes sure it is still the one the user picked.
fn current_volume(
    key: &VolumeKey,
    name: &str,
    label: &str,
    ctx: &Context<'_>,
) -> std::result::Result<Volume, Report> {
    let vol = disks::rescan_volume(&key.bsd_name, ctx.mount_root).map_err(|err| {
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
    if !ctx.deps.macfuse {
        return Report::failed("macFUSE is not installed", crate::deps::INSTALL_HINT);
    }
    let Some(root) = ctx.mount_root else {
        return Report::failed("Cannot mount", "Your home directory could not be determined.");
    };
    let vol = match current_volume(key, name, label, ctx) {
        Ok(vol) => vol,
        Err(report) => return report,
    };
    let mode = match &vol.state {
        MountState::Native { read_only: true, .. } => "remount",
        MountState::Unmounted => "mount",
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

    // The mount point (and the name Finder shows) is the same unique name
    // the menu uses, so two unnamed volumes can be told apart everywhere.
    let mount_point = match prepare_mount_point(root, name) {
        Ok(path) => path,
        Err(err) => return Report::failed(format!("Cannot mount “{name}”"), err.to_string()),
    };
    // SAFETY: getuid/getgid have no preconditions and cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let options = ntfs3g_options(name, uid, gid);
    let expected = vol
        .identity
        .clone()
        .filter(|id| is_safe_identity(id))
        .unwrap_or_default();
    let device = vol.device_node();
    let mount_point_str = mount_point.to_string_lossy().into_owned();
    let ntfs3g_str = ntfs3g.to_string_lossy().into_owned();
    let prompt = format!("Remounty wants to mount “{name}” with write access.");

    let outcome = privileged::run_as_admin(
        &prompt,
        privileged::MOUNT_SCRIPT,
        &[mode, &device, &mount_point_str, &ntfs3g_str, &options, &expected],
        ctx.prefer_touch_id,
    );

    let report = match outcome {
        Ok(Outcome::Success { stdout }) => {
            if !stdout.trim().is_empty() {
                crate::log_info!("ntfs-3g output: {}", stdout.trim());
            }
            verify_read_write(key, name, &mount_point, ctx)
        }
        Ok(Outcome::Canceled) => Report::Canceled,
        Ok(Outcome::Failed { code, message }) => explain_mount_failure(name, code, &message),
        Err(err) => Report::failed(format!("Could not mount “{name}”"), err.to_string()),
    };

    if !matches!(report, Report::Done { .. }) {
        remove_mount_point(&mount_point, root);
        if matches!(vol.state, MountState::Native { .. }) {
            return with_restore_note(report, key, name, ctx);
        }
    }
    report
}

fn verify_read_write(key: &VolumeKey, name: &str, mount_point: &Path, ctx: &Context<'_>) -> Report {
    // The mount table can lag for a moment behind ntfs-3g's exit.
    let mut last = None;
    for _ in 0..20 {
        match disks::rescan_volume(&key.bsd_name, ctx.mount_root) {
            Ok(vol) => {
                if let MountState::Fuse { path, read_only, .. } = &vol.state
                    && path == mount_point
                {
                    return if *read_only {
                        Report::failed(
                            format!("“{name}” was mounted read-only"),
                            "ntfs-3g refused write access, most likely because Windows is \
                                 hibernated or used Fast Startup on this volume. Your data was \
                                 not modified. Shut Windows down completely (or run \
                                 “powercfg /h off” in Windows) and try again. The volume stays \
                                 readable in the meantime.",
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
    let detail = match code {
        exit::DEVICE_GONE => "The disk is no longer available. Was it disconnected?".to_string(),
        exit::IDENTITY_MISMATCH => "The device now holds a different volume than the one \
            selected. Nothing was changed."
            .to_string(),
        exit::UNMOUNT_FAILED => format!(
            "macOS could not unmount the read-only volume, most likely because a program or \
             Finder window is using it. Close it and try again. Nothing was changed.\n\n{}",
            message.trim()
        ),
        exit::NTFS3G_FAILED => {
            let reason = privileged::ntfs3g_exit_code(message)
                .map(ntfs3g_reason)
                .unwrap_or("ntfs-3g reported an error.");
            let output: String = message
                .lines()
                .filter(|l| !l.starts_with("ntfs-3g-exit:"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{reason}\n\n{}", output.trim())
        }
        exit::BAD_ARGS | exit::INTERNAL => format!(
            "Remounty refused to continue because of an internal inconsistency. Nothing was \
             changed.\n\n{}",
            message.trim()
        ),
        _ => format!("Unexpected error ({code}):\n{}", message.trim()),
    };
    Report::failed(title, detail.trim().to_string())
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
/// read-only before is mounted read-only again.
fn with_restore_note(report: Report, key: &VolumeKey, name: &str, ctx: &Context<'_>) -> Report {
    let Ok(vol) = disks::rescan_volume(&key.bsd_name, ctx.mount_root) else {
        return report;
    };
    if vol.state != MountState::Unmounted || vol.identity != key.identity {
        return report;
    }
    crate::log_warn!("{} was left unmounted; mounting it read-only again", key.bsd_name);
    let note = match diskutil_as_user(&["mount", &vol.bsd_name]) {
        Ok(()) => format!("“{name}” has been mounted read-only again."),
        Err(err) => format!(
            "“{name}” is currently unmounted and could not be mounted read-only again ({err}). \
             Use “Mount Read-Only” from the menu or reconnect the disk."
        ),
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

fn unmount(key: &VolumeKey, name: &str, label: &str, ctx: &Context<'_>) -> Report {
    let vol = match current_volume(key, name, label, ctx) {
        Ok(vol) => vol,
        Err(report) => return report,
    };
    let MountState::Fuse { path, .. } = &vol.state else {
        return Report::failed(
            format!("“{name}” is not mounted with ntfs-3g"),
            "Nothing was changed.",
        );
    };
    let prompt = format!("Remounty wants to unmount “{name}”.");
    let report = unmount_path(path, &vol.device_node(), &prompt, name, ctx.prefer_touch_id);
    if matches!(report, Report::Done { .. })
        && let Some(root) = ctx.mount_root
    {
        remove_mount_point(path, root);
    }
    report
}

fn unmount_stale(path: &Path, ctx: &Context<'_>) -> Report {
    let Some(root) = ctx.mount_root else {
        return Report::failed("Cannot unmount", "Your home directory could not be determined.");
    };
    let is_stale_fuse = mounts::snapshot()
        .map(|entries| entries.iter().any(|e| e.on == path && e.is_fuse()))
        .unwrap_or(false);
    if !disks::is_inside(path, root) || !is_stale_fuse {
        return Report::failed(
            "Nothing to unmount",
            format!("{} is not an ntfs-3g mount.", path.display()),
        );
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prompt = format!("Remounty wants to unmount “{name}”.");
    let report = unmount_path(path, "", &prompt, &name, ctx.prefer_touch_id);
    if matches!(report, Report::Done { .. }) {
        remove_mount_point(path, root);
    }
    report
}

/// Unmounts without ever forcing it: a busy volume is reported, not yanked.
fn unmount_path(path: &Path, device: &str, prompt: &str, name: &str, prefer_touch_id: bool) -> Report {
    let path_str = path.to_string_lossy().into_owned();
    let first_attempt = diskutil_as_user(&["unmount", &path_str]);
    // A timed-out attempt may still have completed; never escalate for a
    // volume that is already gone.
    let still_mounted_after_attempt = || {
        mounts::snapshot()
            .map(|entries| entries.iter().any(|e| e.on == path))
            .unwrap_or(true)
    };
    if let Err(err) = &first_attempt
        && still_mounted_after_attempt()
    {
        crate::log_info!("Unprivileged unmount failed ({err}); asking for administrator rights");
        if is_busy_message(err.message()) {
            return busy_report(name, err.message());
        }
        match privileged::run_as_admin(prompt, privileged::UNMOUNT_SCRIPT, &[&path_str], prefer_touch_id) {
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
    let still_mounted = mounts::snapshot()
        .map(|entries| entries.iter().any(|e| e.on == path))
        .unwrap_or(true);
    if still_mounted {
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
    Report::Done {
        message: format!("“{name}” was unmounted and all changes were written to the disk."),
        open_path: None,
    }
}

fn set_up_touch_id() -> Report {
    if privileged::touch_id_configured() {
        return Report::Done {
            message: "Touch ID is already enabled.".into(),
            open_path: None,
        };
    }
    if privileged::sudo_local_exists() {
        return Report::failed(
            "Touch ID cannot be enabled automatically",
            format!(
                "{} already exists, and Remounty does not modify existing security settings. \
                 To use Touch ID, add this line to that file yourself:\n\n\
                 auth       sufficient     pam_tid.so",
                privileged::PAM_SUDO_LOCAL
            ),
        );
    }
    let prompt = "Remounty wants to enable Touch ID for administrator requests.";
    match privileged::run_as_admin(prompt, privileged::SETUP_TOUCH_ID_SCRIPT, &[], false) {
        Ok(Outcome::Success { .. }) if privileged::touch_id_configured() => Report::Done {
            message: "Touch ID is now enabled for Remounty.".into(),
            open_path: None,
        },
        Ok(Outcome::Success { .. }) => Report::failed(
            "Touch ID could not be enabled",
            "The setup finished, but Touch ID is still not configured.",
        ),
        Ok(Outcome::Canceled) => Report::Canceled,
        Ok(Outcome::Failed { message, .. }) => {
            Report::failed("Touch ID could not be enabled", message.trim().to_string())
        }
        Err(err) => Report::failed("Touch ID could not be enabled", err.to_string()),
    }
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

fn mount_read_only(key: &VolumeKey, name: &str, label: &str, ctx: &Context<'_>) -> Report {
    let vol = match current_volume(key, name, label, ctx) {
        Ok(vol) => vol,
        Err(report) => return report,
    };
    if vol.state != MountState::Unmounted {
        return Report::failed(format!("“{name}” is already mounted"), "Nothing was changed.");
    }
    match diskutil_as_user(&["mount", &vol.bsd_name]) {
        Ok(()) => Report::Done {
            message: format!("“{name}” was mounted read-only."),
            open_path: disks::rescan_volume(&vol.bsd_name, ctx.mount_root)
                .ok()
                .and_then(|v| v.state.path().map(Path::to_path_buf)),
        },
        Err(err) => Report::failed(format!("Could not mount “{name}”"), err.to_string()),
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

/// Options passed to ntfs-3g. Mirrors Mounty's proven set, plus `norecover`
/// so a volume with an unclean journal is refused instead of having its
/// Windows log file wiped.
pub fn ntfs3g_options(volume_name: &str, uid: u32, gid: u32) -> String {
    format!(
        "volname={},local,negative_vncache,auto_xattr,auto_cache,noatime,windows_names,\
         streams_interface=openxattr,inherit,allow_other,big_writes,norecover,uid={uid},gid={gid}",
        sanitize_volname(volume_name)
    )
}

/// FUSE parses `-o` as a comma separated list with backslash escapes, so the
/// label must not contain either (otherwise it could inject options).
pub fn sanitize_volname(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == ',' || c == '\\' || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = cleaned.chars().take(63).collect();
    if truncated.is_empty() {
        "Untitled".to_string()
    } else {
        truncated
    }
}

/// Turns a volume label into a safe single path component.
pub fn sanitize_dir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let truncated: String = trimmed.chars().take(64).collect();
    let truncated = truncated.trim().to_string();
    if truncated.is_empty() {
        "Untitled".to_string()
    } else {
        truncated
    }
}

fn is_safe_identity(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Creates (as the current user) an empty directory to mount on. Existing
/// directories are only reused if they are empty and not mounted on.
pub fn prepare_mount_point(root: &Path, volume_name: &str) -> Result<PathBuf> {
    ensure_root(root)?;
    let mounted: Vec<PathBuf> = mounts::snapshot()?.into_iter().map(|e| e.on).collect();
    let base = sanitize_dir_name(volume_name);
    for n in 1..=99 {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base} {n}")
        };
        let path = root.join(&candidate);
        match fs::symlink_metadata(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)
                    .map_err(|err| Error::new(format!("Cannot create {}: {err}", path.display())))?;
                return Ok(path);
            }
            Err(err) => return Err(Error::new(format!("Cannot inspect {}: {err}", path.display()))),
            Ok(meta) => {
                let reusable = meta.is_dir()
                    && !mounted.contains(&path)
                    && fs::read_dir(&path)
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false);
                if reusable {
                    return Ok(path);
                }
            }
        }
    }
    Err(Error::new(format!(
        "No free mount point name for “{volume_name}” in {}",
        root.display()
    )))
}

fn ensure_root(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(Error::new(format!(
            "{} exists but is not a directory; please move it away",
            root.display()
        ))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(root).map_err(|err| Error::new(format!("Cannot create {}: {err}", root.display())))
        }
        Err(err) => Err(Error::new(format!("Cannot inspect {}: {err}", root.display()))),
    }
}

/// Removes a mount point directory Remounty created. `remove_dir` only ever
/// deletes *empty* directories and fails on mount points, so this can never
/// remove user data.
pub fn remove_mount_point(path: &Path, root: &Path) {
    if !disks::is_inside(path, root) {
        return;
    }
    if let Err(err) = fs::remove_dir(path) {
        crate::log_warn!("Leaving {} in place: {err}", path.display());
    }
}

/// Removes empty, unused mount point directories left behind by earlier runs.
pub fn clean_stale_mount_points(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
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
            // Fails harmlessly unless the directory is empty.
            let _ = fs::remove_dir(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volname_cannot_inject_options() {
        assert_eq!(sanitize_volname("Test Vol, x"), "Test Vol x");
        assert_eq!(sanitize_volname("a,allow_root,uid=0"), "a allow_root uid=0");
        assert_eq!(sanitize_volname("back\\slash"), "back slash");
        assert_eq!(sanitize_volname(" \n "), "Untitled");
        let opts = ntfs3g_options("x,ro,remove_hiberfile", 501, 20);
        assert!(!opts.contains(",ro,"));
        assert!(!opts.contains(",remove_hiberfile"));
        assert!(opts.contains("norecover"));
        assert!(opts.ends_with("uid=501,gid=20"));
    }

    #[test]
    fn dir_names_are_single_components() {
        assert_eq!(sanitize_dir_name("../../etc"), "_.._etc");
        assert_eq!(sanitize_dir_name("a/b:c"), "a_b_c");
        assert_eq!(sanitize_dir_name(".hidden"), "hidden");
        assert_eq!(sanitize_dir_name(""), "Untitled");
        assert_eq!(sanitize_dir_name("..."), "Untitled");
        assert_eq!(sanitize_dir_name("Test Vol, x"), "Test Vol, x");
        assert!(sanitize_dir_name(&"x".repeat(500)).chars().count() <= 64);
    }

    #[test]
    fn safe_identity() {
        assert!(is_safe_identity("1163E33B-7486-4255-A385-3F1F6BDE1CD6"));
        assert!(!is_safe_identity(""));
        assert!(!is_safe_identity("abc; rm"));
    }

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
        let ps = "/opt/homebrew/bin/ntfs-3g /dev/disk4s1 /Users/u/.remounty/T -o volname=T\n\
                  /usr/bin/vim /dev/disk4s1\n";
        assert!(ps_lists_ntfs3g(ps, "/dev/disk4s1"));
        assert!(!ps_lists_ntfs3g(ps, "/dev/disk4s10"));
        assert!(!ps_lists_ntfs3g("/usr/bin/vim /dev/disk5s1", "/dev/disk5s1"));
    }

    #[test]
    fn mount_point_preparation() {
        let root = std::env::temp_dir().join(format!("remounty-mp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let first = prepare_mount_point(&root, "Data").ok();
        assert_eq!(first, Some(root.join("Data")));
        // Empty, unmounted directory is reused.
        assert_eq!(prepare_mount_point(&root, "Data").ok(), Some(root.join("Data")));
        // A non-empty directory is never mounted over.
        let _ = fs::write(root.join("Data").join("file"), b"keep me");
        assert_eq!(prepare_mount_point(&root, "Data").ok(), Some(root.join("Data 2")));
        // Cleanup keeps non-empty directories.
        remove_mount_point(&root.join("Data"), &root);
        assert!(root.join("Data").join("file").exists());
        clean_stale_mount_points(&root);
        assert!(!root.join("Data 2").exists());
        assert!(root.join("Data").exists());
        // Paths outside the root are never touched.
        let outside = std::env::temp_dir().join(format!("remounty-out-{}", std::process::id()));
        let _ = fs::create_dir_all(&outside);
        remove_mount_point(&outside, &root);
        assert!(outside.exists());
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failure_explanations() {
        let report = explain_mount_failure("X", exit::NTFS3G_FAILED, "ntfs-3g-exit: 15\nunclean");
        assert!(matches!(report, Report::Failed { .. }));
        if let Report::Failed { detail, .. } = report {
            assert!(detail.contains("not cleanly unmounted"));
            assert!(detail.contains("unclean"));
            assert!(!detail.contains("ntfs-3g-exit"));
        }
    }
}
