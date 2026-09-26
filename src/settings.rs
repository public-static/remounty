//! Persistent user preferences (`~/Library/Application Support/Remounty/settings.json`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The user has read and accepted the data-safety notice.
    pub disclaimer_accepted: bool,
    /// Ask whether to re-mount when a new NTFS volume is attached.
    pub ask_on_attach: bool,
    /// Volumes (by identity) that are re-mounted without asking.
    pub automount: Vec<AutomountEntry>,
    /// Explicit ntfs-3g location chosen by the user.
    pub ntfs3g_path: Option<PathBuf>,
    /// Authenticate with Touch ID when it is enabled for sudo.
    pub use_touch_id: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomountEntry {
    pub identity: String,
    /// Last known name; only informational.
    pub name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            disclaimer_accepted: false,
            ask_on_attach: true,
            automount: Vec::new(),
            ntfs3g_path: None,
            use_touch_id: true,
        }
    }
}

impl Settings {
    pub fn is_automount(&self, identity: Option<&str>) -> bool {
        identity.is_some_and(|id| self.automount.iter().any(|e| e.identity == id))
    }

    pub fn set_automount(&mut self, identity: &str, name: &str, enabled: bool) {
        self.automount.retain(|e| e.identity != identity);
        if enabled {
            self.automount.push(AutomountEntry {
                identity: identity.to_string(),
                name: name.to_string(),
            });
        }
    }
}

pub struct Store {
    path: Option<PathBuf>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            path: crate::paths::support_dir().map(|d| d.join("settings.json")),
        }
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Loads settings. A missing file yields defaults; an unreadable or corrupt
    /// file is moved aside (never deleted) and defaults are used.
    pub fn load(&self) -> Settings {
        let Some(path) = &self.path else {
            crate::log_warn!("No home directory; settings will not be saved");
            return Settings::default();
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
            Err(err) => {
                crate::log_error!("Could not read {}: {err}", path.display());
                return Settings::default();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(settings) => settings,
            Err(err) => {
                let backup = path.with_extension("json.corrupt");
                crate::log_error!(
                    "Settings file is corrupt ({err}); moving it to {}",
                    backup.display()
                );
                let _ = fs::rename(path, &backup);
                Settings::default()
            }
        }
    }

    /// Saves settings atomically (write to a temp file, fsync, rename).
    pub fn save(&self, settings: &Settings) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        write_atomic(
            path,
            &serde_json::to_vec_pretty(settings).context("Encoding settings")?,
        )
    }
}

pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("Settings path has no parent directory")?;
    fs::create_dir_all(dir).context(format!("Creating {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.context(format!("Writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("remounty-test-{}-{name}", std::process::id()))
            .join("settings.json")
    }

    #[test]
    fn round_trip() {
        let path = temp_path("rt");
        let store = Store::at(path.clone());
        let mut settings = Settings {
            disclaimer_accepted: true,
            ..Settings::default()
        };
        settings.set_automount("ABC", "Data", true);
        assert!(store.save(&settings).is_ok());
        assert_eq!(store.load(), settings);
        let _ = fs::remove_dir_all(path.parent().unwrap_or(Path::new("/nonexistent")));
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let path = temp_path("corrupt");
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(&path, b"{ not json");
        let store = Store::at(path.clone());
        assert_eq!(store.load(), Settings::default());
        assert!(path.with_extension("json.corrupt").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let parsed: Option<Settings> = serde_json::from_str(r#"{"disclaimer_accepted":true}"#).ok();
        assert_eq!(
            parsed,
            Some(Settings {
                disclaimer_accepted: true,
                ..Settings::default()
            })
        );
    }

    #[test]
    fn automount_toggle() {
        let mut s = Settings::default();
        s.set_automount("A", "x", true);
        s.set_automount("A", "x", true);
        assert_eq!(s.automount.len(), 1);
        assert!(s.is_automount(Some("A")));
        assert!(!s.is_automount(None));
        s.set_automount("A", "x", false);
        assert!(!s.is_automount(Some("A")));
    }
}
