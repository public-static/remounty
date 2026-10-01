//! Everything the app and the privileged helper must agree on: locations,
//! authorization right names, exit codes and the on-disk state.
//!
//! Layout of an installed helper (everything root-owned, not writable by
//! anyone else):
//!
//! ```text
//! /Library/PrivilegedHelperTools/remounty/
//!     remounty-helper        the helper itself
//!     mountpoints.json       mount points in /Volumes created by the helper
//! /etc/sudoers.d/remounty    lets the user run *only* the helper via sudo
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Bumped whenever the helper's behaviour or protocol changes; the app
/// offers to update an installed helper with a different version.
pub const HELPER_VERSION: &str = "1";

pub const HELPER_DIR: &str = "/Library/PrivilegedHelperTools/remounty";
pub const HELPER_BIN: &str = "/Library/PrivilegedHelperTools/remounty/remounty-helper";
pub const STATE_FILE: &str = "/Library/PrivilegedHelperTools/remounty/mountpoints.json";
pub const SUDOERS_FILE: &str = "/etc/sudoers.d/remounty";
/// Name of the helper executable inside the app bundle (next to the app binary).
pub const BUNDLED_HELPER_NAME: &str = "remounty-helper";

/// Where the helper creates mount points. Only root can write here, so no
/// other process can swap a mount point for a symlink.
pub const VOLUMES_DIR: &str = "/Volumes";

/// Right checked for every mount / unmount.
pub const MOUNT_RIGHT: &str = "io.github.remounty.mount";
/// Right checked for changing settings and uninstalling (always asks).
pub const ADMIN_RIGHT: &str = "io.github.remounty.admin";

/// Allowed values for "remember authentication", in seconds.
pub const REMEMBER_CHOICES: [u32; 4] = [0, 300, 3600, REMEMBER_UNTIL_LOGOUT];
/// What macOS itself uses for "until the session ends".
pub const REMEMBER_UNTIL_LOGOUT: u32 = 2_147_483_647;

/// Exit codes of the helper (and of the old root scripts they replace).
pub mod exit {
    pub const BAD_ARGS: i32 = 64;
    pub const INTERNAL: i32 = 70;
    pub const DEVICE_GONE: i32 = 71;
    pub const IDENTITY_MISMATCH: i32 = 72;
    pub const UNMOUNT_FAILED: i32 = 73;
    pub const NTFS3G_FAILED: i32 = 74;
    /// The user could not be authorized; nothing was executed.
    pub const AUTH_FAILED: i32 = 77;
    /// The user cancelled the authorization dialog; nothing was executed.
    pub const CANCELED: i32 = 78;
    /// The helper installation is missing, incomplete or not trustworthy.
    pub const UNSAFE_INSTALL: i32 = 80;
    /// The volume is not in the state the request expects.
    pub const WRONG_STATE: i32 = 81;
}

/// Mount points in /Volumes that the helper created and has not removed yet.
pub fn read_created() -> HashSet<PathBuf> {
    match std::fs::read(STATE_FILE) {
        Ok(bytes) => parse_created(&bytes),
        Err(_) => HashSet::new(),
    }
}

pub fn parse_created(bytes: &[u8]) -> HashSet<PathBuf> {
    let list: Vec<String> = serde_json::from_slice(bytes).unwrap_or_default();
    list.into_iter()
        .map(PathBuf::from)
        .filter(|p| is_volumes_child(p))
        .collect()
}

/// `/Volumes/<single component>` — nothing else is ever a helper mount point.
pub fn is_volumes_child(path: &Path) -> bool {
    path.parent() == Some(Path::new(VOLUMES_DIR))
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.is_empty() && n != "." && n != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_children_only() {
        assert!(is_volumes_child(Path::new("/Volumes/Data")));
        assert!(is_volumes_child(Path::new("/Volumes/Untitled (disk4s1)")));
        assert!(!is_volumes_child(Path::new("/Volumes")));
        assert!(!is_volumes_child(Path::new("/Volumes/a/b")));
        assert!(!is_volumes_child(Path::new("/Users/u/.remounty/x")));
        assert!(!is_volumes_child(Path::new("/Volumes/..")));
    }

    #[test]
    fn created_list_parsing() {
        let parsed = parse_created(br#"["/Volumes/A", "/etc", "/Volumes/B/c", "/Volumes/B"]"#);
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains(Path::new("/Volumes/A")));
        assert!(parse_created(b"garbage").is_empty());
    }
}
