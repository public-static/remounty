//! The privileged helper (`remounty-helper`), run as root through a sudoers
//! rule that allows exactly this binary and nothing else.
//!
//! Because any process of the user can start it through sudo, the helper
//! trusts none of its inputs:
//!
//! * every mount / unmount requires a macOS authorization, checked by the
//!   helper itself (the system dialog is shown when needed);
//! * arguments are validated strictly; the volume is re-checked with
//!   diskutil (NTFS, identity, current state) right before acting;
//! * mount points are created by the helper in /Volumes, which only root
//!   can modify, so nobody can redirect a mount through a symlink;
//! * before every mount it verifies that ntfs-3g, every library it loads,
//!   its plugin directory and macFUSE can only be modified by root (see
//!   [`crate::trust`]) and refuses otherwise;
//! * ntfs-3g runs with an empty environment.
//!
//! Output protocol: on success a line `MOUNTED:<path>` (mount) is printed;
//! failures print a message on stderr and exit with a code from
//! [`helper_proto::exit`].

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::authz::{self, Decision, EXTERNAL_FORM_LEN};
use crate::disks::{self, MountState};
use crate::helper_proto::{self, exit};
use crate::{cmd, helper_install, mounts, naming, trust};

const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A helper failure: exit code plus message for stderr.
#[derive(Debug)]
pub struct Fail {
    pub code: i32,
    pub message: String,
}

pub fn fail(code: i32, message: impl Into<String>) -> Fail {
    Fail {
        code,
        message: message.into(),
    }
}

pub type HelperResult = std::result::Result<(), Fail>;

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = std::panic::catch_unwind(|| dispatch(&args));
    let code = match outcome {
        Ok(Ok(())) => 0,
        Ok(Err(failure)) => {
            eprintln!("{}", failure.message.trim_end());
            failure.code
        }
        Err(_) => {
            eprintln!("remounty-helper: internal error");
            exit::INTERNAL
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn dispatch(args: &[String]) -> HelperResult {
    let Some((command, rest)) = args.split_first() else {
        return Err(fail(
            exit::BAD_ARGS,
            "usage: remounty-helper <command> [arguments]",
        ));
    };
    if command == "version" {
        println!("{}", helper_proto::HELPER_VERSION);
        return Ok(());
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(fail(exit::BAD_ARGS, "remounty-helper must run as root"));
    }
    match command.as_str() {
        "mount" => cmd_mount(rest),
        "unmount" => cmd_unmount(rest),
        "cleanup" => cmd_cleanup(rest),
        "set-remember" => cmd_set_remember(rest),
        "install" => helper_install::install(rest),
        "uninstall" => helper_install::uninstall(rest),
        other => Err(fail(exit::BAD_ARGS, format!("unknown command {other:?}"))),
    }
}

/// sudo always sets SUDO_UID for the command it runs. Without it, the helper
/// was started by root directly (the macOS administrator dialog).
pub fn invoked_via_sudo() -> bool {
    std::env::var_os("SUDO_UID").is_some()
}

fn caller_ids() -> std::result::Result<(u32, u32), Fail> {
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| {
                fail(
                    exit::BAD_ARGS,
                    format!("{name} is missing; run the helper through sudo"),
                )
            })
    };
    Ok((read("SUDO_UID")?, read("SUDO_GID")?))
}

/// Reads the app's authorization session (32 bytes) from stdin and asks
/// macOS to authorize `right`, showing the system dialog if needed.
pub fn require_authorization(right: &str, prompt: &str) -> HelperResult {
    let mut form = [0u8; EXTERNAL_FORM_LEN];
    std::io::stdin()
        .read_exact(&mut form)
        .map_err(|_| fail(exit::AUTH_FAILED, "No authorization was provided."))?;
    match authz::authorize(&form, right, prompt) {
        Ok(Decision::Granted) => Ok(()),
        Ok(Decision::Canceled) => Err(fail(exit::CANCELED, "Authorization was cancelled.")),
        Ok(Decision::Denied) => Err(fail(exit::AUTH_FAILED, "Authorization was denied.")),
        Err(err) => Err(fail(exit::AUTH_FAILED, err.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Installation integrity

/// The helper binary and its directory must be controlled by root only.
pub fn verify_installation() -> HelperResult {
    trust::check_path(Path::new(helper_proto::HELPER_DIR), true)
        .and_then(|()| trust::check_path(Path::new(helper_proto::HELPER_BIN), false))
        .map_err(|reason| {
            fail(
                exit::UNSAFE_INSTALL,
                format!("{reason}. Reinstall the helper from Remounty's menu."),
            )
        })
}

/// ntfs-3g (with its libraries and plugin directory) and macFUSE must be
/// controlled by root only; otherwise running them as root is refused.
fn verify_tools(ntfs3g: &Path) -> HelperResult {
    trust::verify_ntfs3g(ntfs3g)
        .and_then(|_| trust::verify_macfuse())
        .map_err(|reason| fail(exit::UNSAFE_INSTALL, reason))
}

// ---------------------------------------------------------------------------
// State: mount points created by the helper

fn write_created(set: &HashSet<PathBuf>) -> HelperResult {
    let mut list: Vec<String> = set.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    list.sort();
    let json = serde_json::to_vec_pretty(&list).map_err(|err| fail(exit::INTERNAL, err.to_string()))?;
    let tmp = Path::new(helper_proto::HELPER_DIR).join(".mountpoints.tmp");
    fs::write(&tmp, json)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)))
        .and_then(|()| fs::rename(&tmp, helper_proto::STATE_FILE))
        .map_err(|err| fail(exit::INTERNAL, format!("Cannot record mount points: {err}")))
}

fn remember_created(path: &Path) -> HelperResult {
    let mut set = helper_proto::read_created();
    set.insert(path.to_path_buf());
    write_created(&set)
}

fn forget_created(path: &Path) {
    let mut set = helper_proto::read_created();
    if set.remove(path)
        && let Err(err) = write_created(&set)
    {
        eprintln!("{}", err.message);
    }
}

/// Creates a new, empty mount point `/Volumes/<name>` (or `<name> 2`, …).
/// `create_dir` fails if anything already exists there, including a symlink.
fn create_mount_point(name: &str) -> std::result::Result<PathBuf, Fail> {
    // Only safe if nobody but root can add, rename or remove entries here.
    trust::check_path(Path::new(helper_proto::VOLUMES_DIR), true).map_err(|reason| {
        fail(
            exit::UNSAFE_INSTALL,
            format!("{reason}; refusing to create a mount point"),
        )
    })?;
    let base = naming::sanitize_dir_name(name);
    for n in 1..=99 {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base} {n}")
        };
        let path = Path::new(helper_proto::VOLUMES_DIR).join(candidate);
        match fs::DirBuilder::new().mode(0o755).create(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(fail(
                    exit::INTERNAL,
                    format!("Cannot create {}: {err}", path.display()),
                ));
            }
        }
    }
    Err(fail(
        exit::INTERNAL,
        format!("No free mount point name for “{name}”"),
    ))
}

fn is_mounted(path: &Path) -> bool {
    mounts::snapshot()
        .map(|entries| entries.iter().any(|e| e.on == path))
        .unwrap_or(true)
}

fn diskutil(args: &[&str]) -> std::result::Result<(), String> {
    let options = cmd::Options {
        env: &[("PATH", SYSTEM_PATH)],
        clear_env: true,
        input: None,
        timeout: Some(std::time::Duration::from_secs(120)),
    };
    match cmd::run_with(Path::new(disks::DISKUTIL), args, &options) {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(out.diagnostics()),
        Err(err) => Err(err.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Commands

/// `mount <bsd-name> <identity|-> <remount|mount> <name> <ntfs-3g>` +
/// authorization on stdin.
fn cmd_mount(args: &[String]) -> HelperResult {
    let [bsd, identity, mode, name, ntfs3g] = args else {
        return Err(fail(
            exit::BAD_ARGS,
            "usage: mount <device> <identity|-> <remount|mount> <name> <ntfs-3g>",
        ));
    };
    let ntfs3g = Path::new(ntfs3g);
    if !disks::is_valid_bsd_name(bsd) {
        return Err(fail(exit::BAD_ARGS, format!("invalid device {bsd:?}")));
    }
    if identity != "-" && !naming::is_safe_identity(identity) {
        return Err(fail(exit::BAD_ARGS, "invalid identity"));
    }
    if mode != "remount" && mode != "mount" {
        return Err(fail(exit::BAD_ARGS, format!("invalid mode {mode:?}")));
    }
    if name.is_empty() || name.len() > 256 {
        return Err(fail(exit::BAD_ARGS, "invalid name"));
    }
    let (uid, gid) = caller_ids()?;
    verify_installation()?;
    verify_tools(ntfs3g)?;
    require_authorization(
        helper_proto::MOUNT_RIGHT,
        &format!(
            "Remounty wants to mount “{}” with write access.",
            naming::display_text(name)
        ),
    )?;

    // Re-check the volume right before acting.
    let info = disks::diskutil_info(bsd).map_err(|err| fail(exit::DEVICE_GONE, err.to_string()))?;
    if !info.is_ntfs() {
        return Err(fail(
            exit::WRONG_STATE,
            format!("{bsd} does not contain an NTFS volume"),
        ));
    }
    if identity != "-" && info.identity().as_deref() != Some(identity.as_str()) {
        return Err(fail(
            exit::IDENTITY_MISMATCH,
            format!("{bsd} is no longer the volume that was selected"),
        ));
    }
    let device = format!("/dev/{bsd}");
    let entries = mounts::snapshot().map_err(|err| fail(exit::INTERNAL, err.to_string()))?;
    let state = disks::classify(
        &device,
        info.mount_point.as_deref(),
        info.writable_volume,
        &entries,
        &helper_proto::read_created(),
    );
    let remount = match (mode.as_str(), &state) {
        ("remount", MountState::Native { read_only: true, .. }) => true,
        ("mount", MountState::Unmounted) => false,
        _ => {
            return Err(fail(
                exit::WRONG_STATE,
                format!("{bsd} is not in the expected state ({state:?}). Nothing was changed."),
            ));
        }
    };

    if remount {
        // Never forced: if anything uses the volume, stop here.
        diskutil(&["unmount", bsd]).map_err(|err| fail(exit::UNMOUNT_FAILED, err))?;
    }

    let mount_point = match create_mount_point(name).and_then(|mp| remember_created(&mp).map(|()| mp)) {
        Ok(mp) => mp,
        Err(failure) => return Err(with_restore(failure, remount, bsd)),
    };
    let options = naming::ntfs3g_options(name, uid, gid);
    let mount_point_str = mount_point.to_string_lossy().into_owned();
    let run = cmd::run_with(
        ntfs3g,
        &[&device, &mount_point_str, "-o", &options],
        &cmd::Options {
            env: &[("PATH", SYSTEM_PATH)],
            clear_env: true,
            input: None,
            timeout: None,
        },
    );
    let failure = match run {
        Ok(out) if out.success() => None,
        Ok(out) => Some(format!(
            "ntfs-3g-exit: {}\n{}",
            out.status.code().unwrap_or(-1),
            out.diagnostics()
        )),
        Err(err) => Some(format!("ntfs-3g-exit: -1\n{err}")),
    };
    if let Some(message) = failure {
        let _ = fs::remove_dir(&mount_point);
        forget_created(&mount_point);
        return Err(with_restore(fail(exit::NTFS3G_FAILED, message), remount, bsd));
    }
    println!("MOUNTED:{mount_point_str}");
    Ok(())
}

/// After a failure that happened once the read-only mount was removed,
/// mount the volume read-only again with the macOS driver.
fn with_restore(failure: Fail, remounted: bool, bsd: &str) -> Fail {
    if !remounted {
        return failure;
    }
    let note = match diskutil(&["mount", bsd]) {
        Ok(()) => "restored-read-only: yes".to_string(),
        Err(err) => format!("restored-read-only: no ({err})"),
    };
    Fail {
        code: failure.code,
        message: format!("{}\n{note}", failure.message),
    }
}

/// `unmount <path>` + authorization on stdin. Only helper-created mounts.
fn cmd_unmount(args: &[String]) -> HelperResult {
    let [path] = args else {
        return Err(fail(exit::BAD_ARGS, "usage: unmount <mount point>"));
    };
    let path = PathBuf::from(path);
    if !helper_proto::is_volumes_child(&path) || !helper_proto::read_created().contains(&path) {
        return Err(fail(
            exit::WRONG_STATE,
            format!("{} was not mounted by Remounty", path.display()),
        ));
    }
    let is_fuse_mount = mounts::snapshot()
        .map(|entries| entries.iter().any(|e| e.on == path && e.is_fuse()))
        .unwrap_or(false);
    if !is_fuse_mount {
        return Err(fail(
            exit::WRONG_STATE,
            format!("{} is not mounted", path.display()),
        ));
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    require_authorization(
        helper_proto::MOUNT_RIGHT,
        &format!("Remounty wants to unmount “{}”.", naming::display_text(&name)),
    )?;
    let path_str = path.to_string_lossy().into_owned();
    // Never forced.
    if let Err(first) = diskutil(&["unmount", &path_str]) {
        let second = cmd::run_with(
            Path::new("/sbin/umount"),
            &[&path_str],
            &cmd::Options {
                env: &[("PATH", SYSTEM_PATH)],
                clear_env: true,
                input: None,
                timeout: Some(std::time::Duration::from_secs(120)),
            },
        );
        match second {
            Ok(out) if out.success() => {}
            Ok(out) => {
                return Err(fail(
                    exit::UNMOUNT_FAILED,
                    format!("{first}\n{}", out.diagnostics()),
                ));
            }
            Err(err) => return Err(fail(exit::UNMOUNT_FAILED, format!("{first}\n{err}"))),
        }
    }
    if is_mounted(&path) {
        return Err(fail(exit::UNMOUNT_FAILED, "The volume is still mounted."));
    }
    let _ = fs::remove_dir(&path);
    forget_created(&path);
    Ok(())
}

/// `cleanup`: removes helper-created mount points that are no longer in use
/// (e.g. after an eject in Finder). `remove_dir` only removes empty
/// directories and fails on active mount points, so this cannot harm data.
/// Needs no authorization for that reason.
fn cmd_cleanup(args: &[String]) -> HelperResult {
    if !args.is_empty() {
        return Err(fail(exit::BAD_ARGS, "usage: cleanup"));
    }
    let created = helper_proto::read_created();
    let mut kept = HashSet::new();
    for path in &created {
        if is_mounted(path) {
            kept.insert(path.clone());
            continue;
        }
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!("Keeping {}: {err}", path.display());
                kept.insert(path.clone());
            }
        }
    }
    if kept != created {
        write_created(&kept)?;
    }
    Ok(())
}

/// `set-remember <seconds>`: how long an authorization for mounting stays
/// valid for Remounty. Requires an administrator authorization.
fn cmd_set_remember(args: &[String]) -> HelperResult {
    let [secs] = args else {
        return Err(fail(exit::BAD_ARGS, "usage: set-remember <seconds>"));
    };
    let secs: u32 = secs
        .parse()
        .ok()
        .filter(|s| helper_proto::REMEMBER_CHOICES.contains(s))
        .ok_or_else(|| fail(exit::BAD_ARGS, "unsupported duration"))?;
    if invoked_via_sudo() {
        require_authorization(
            helper_proto::ADMIN_RIGHT,
            "Remounty wants to change how long your authorization is remembered.",
        )?;
    }
    helper_install::write_right(
        helper_proto::MOUNT_RIGHT,
        "Mount and unmount NTFS volumes with Remounty.",
        secs,
    )
}
