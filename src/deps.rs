//! Detection of the external tools Remounty relies on: ntfs-3g and macFUSE.
//!
//! Both are run as root, so they are only accepted when nobody but root can
//! modify them (see [`crate::trust`]) — as installed by MacPorts.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::trust;

pub const NTFS3G_CANDIDATES: &[&str] = &["/opt/local/bin/ntfs-3g"];

pub const INSTALL_HINT: &str = "Install them with MacPorts:\n\n\
    sudo port install macfuse +fs_link ntfs-3g\n\n\
    After installing macFUSE, allow its system extension in System Settings → \
    Privacy & Security and restart your Mac if asked.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependencies {
    pub ntfs3g: std::result::Result<PathBuf, String>,
    pub macfuse: std::result::Result<(), String>,
}

impl Dependencies {
    pub fn ready(&self) -> bool {
        self.ntfs3g.is_ok() && self.macfuse.is_ok()
    }

    /// Short description of what is wrong, if anything.
    pub fn problem(&self) -> Option<String> {
        match (&self.ntfs3g, &self.macfuse) {
            (Ok(_), Ok(())) => None,
            (Err(reason), _) => Some(first_line(reason)),
            (Ok(_), Err(reason)) => Some(first_line(reason)),
        }
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or(text)
        .trim_end_matches('.')
        .to_string()
}

pub fn detect(configured: Option<&Path>) -> Dependencies {
    Dependencies {
        ntfs3g: find_ntfs3g(configured),
        macfuse: if Path::new(trust::MACFUSE_BUNDLE).exists() {
            trust::verify_macfuse()
        } else {
            Err("macFUSE is not installed".into())
        },
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

/// Accepts only an ntfs-3g that is safe to run as root, together with the
/// libraries it loads. The helper repeats this check before every mount.
pub fn validate_ntfs3g(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::new(format!("{} is not an absolute path", path.display())));
    }
    trust::verify_ntfs3g(path).map_err(Error::new)
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
    fn problem_text() {
        let deps = Dependencies {
            ntfs3g: Err("x is not owned by root.\n\nmore".into()),
            macfuse: Err("y".into()),
        };
        assert!(!deps.ready());
        assert_eq!(deps.problem(), Some("x is not owned by root".into()));
        let deps = Dependencies {
            ntfs3g: Ok("/opt/local/bin/ntfs-3g".into()),
            macfuse: Ok(()),
        };
        assert!(deps.ready());
        assert_eq!(deps.problem(), None);
    }
}
