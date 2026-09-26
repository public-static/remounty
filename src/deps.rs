//! Detection of the external tools Remounty relies on: ntfs-3g and macFUSE.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const NTFS3G_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/ntfs-3g",
    "/usr/local/bin/ntfs-3g",
    "/opt/local/bin/ntfs-3g",
];

const MACFUSE_BUNDLE: &str = "/Library/Filesystems/macfuse.fs";

pub const INSTALL_HINT: &str = "Install them with Homebrew:\n\n\
    brew install --cask macfuse\n\
    brew install gromgit/fuse/ntfs-3g-mac\n\n\
    After installing macFUSE, allow its system extension in System Settings → \
    Privacy & Security and restart your Mac if asked.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependencies {
    pub ntfs3g: std::result::Result<PathBuf, String>,
    pub macfuse: bool,
}

impl Dependencies {
    pub fn ready(&self) -> bool {
        self.ntfs3g.is_ok() && self.macfuse
    }

    /// Short description of what is missing, if anything.
    pub fn problem(&self) -> Option<String> {
        match (&self.ntfs3g, self.macfuse) {
            (Ok(_), true) => None,
            (Err(_), false) => Some("ntfs-3g and macFUSE are not installed".into()),
            (Err(reason), true) => Some(reason.clone()),
            (Ok(_), false) => Some("macFUSE is not installed".into()),
        }
    }
}

pub fn detect(configured: Option<&Path>) -> Dependencies {
    Dependencies {
        ntfs3g: find_ntfs3g(configured),
        macfuse: Path::new(MACFUSE_BUNDLE).is_dir(),
    }
}

fn find_ntfs3g(configured: Option<&Path>) -> std::result::Result<PathBuf, String> {
    if let Some(path) = configured {
        return validate_ntfs3g(path).map_err(|err| err.to_string());
    }
    for candidate in NTFS3G_CANDIDATES {
        let path = Path::new(candidate);
        if path.exists() {
            return validate_ntfs3g(path).map_err(|err| err.to_string());
        }
    }
    Err("ntfs-3g is not installed".into())
}

/// ntfs-3g runs as root, so only accept a plain executable named `ntfs-3g`
/// (or `lowntfs-3g`) that cannot be modified by other users.
pub fn validate_ntfs3g(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::new(format!("{} is not an absolute path", path.display())));
    }
    let resolved = fs::canonicalize(path)
        .map_err(|err| Error::new(format!("ntfs-3g at {} is not usable: {err}", path.display())))?;
    let name_ok = matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("ntfs-3g" | "lowntfs-3g")
    );
    if !name_ok {
        return Err(Error::new(format!(
            "{} is not an ntfs-3g executable",
            path.display()
        )));
    }
    let meta = fs::metadata(&resolved)
        .map_err(|err| Error::new(format!("Cannot inspect {}: {err}", resolved.display())))?;
    if !meta.is_file() {
        return Err(Error::new(format!("{} is not a file", resolved.display())));
    }
    let mode = meta.permissions().mode();
    if mode & 0o111 == 0 {
        return Err(Error::new(format!("{} is not executable", resolved.display())));
    }
    if mode & 0o022 != 0 {
        return Err(Error::new(format!(
            "{} is writable by other users; refusing to run it as root",
            resolved.display()
        )));
    }
    // Hand the original (unresolved) path to callers: Homebrew symlinks keep
    // working across upgrades, and the target was checked above.
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_and_wrong_names() {
        assert!(validate_ntfs3g(Path::new("ntfs-3g")).is_err());
        assert!(validate_ntfs3g(Path::new("/bin/sh")).is_err());
        assert!(validate_ntfs3g(Path::new("/nonexistent/ntfs-3g")).is_err());
    }

    #[test]
    fn rejects_group_writable() {
        let dir = std::env::temp_dir().join(format!("remounty-deps-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let exe = dir.join("ntfs-3g");
        let _ = fs::write(&exe, b"#!/bin/sh\n");
        let _ = fs::set_permissions(&exe, fs::Permissions::from_mode(0o775));
        assert!(validate_ntfs3g(&exe).is_err());
        let _ = fs::set_permissions(&exe, fs::Permissions::from_mode(0o755));
        assert!(validate_ntfs3g(&exe).is_ok());
        let _ = fs::set_permissions(&exe, fs::Permissions::from_mode(0o644));
        assert!(validate_ntfs3g(&exe).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn problem_text() {
        let deps = Dependencies {
            ntfs3g: Err("x".into()),
            macfuse: false,
        };
        assert!(!deps.ready());
        assert!(deps.problem().is_some());
        let deps = Dependencies {
            ntfs3g: Ok("/a/ntfs-3g".into()),
            macfuse: true,
        };
        assert!(deps.ready());
        assert_eq!(deps.problem(), None);
    }
}
