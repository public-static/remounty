//! "Start at Login" through a per-user LaunchAgent.

use std::fs;
use std::path::PathBuf;

use crate::error::{Context, Error, Result};
use crate::paths;

pub fn is_enabled() -> bool {
    paths::launch_agent_path().is_some_and(|p| p.exists())
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let path = paths::launch_agent_path().context("Home directory not found")?;
    if enabled {
        crate::settings::write_atomic(&path, &agent_plist(&program()?)?)
    } else {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::new(format!("Removing {}: {err}", path.display()))),
        }
    }
}

/// Keeps an enabled agent pointing at the current executable (e.g. after the
/// app was moved).
pub fn refresh() {
    if !is_enabled() {
        return;
    }
    if let Err(err) = set_enabled(true) {
        crate::log_warn!("Could not update login item: {err}");
    }
}

fn program() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Locating the Remounty executable")?;
    fs::canonicalize(&exe).context("Locating the Remounty executable")
}

fn agent_plist(program: &std::path::Path) -> Result<Vec<u8>> {
    let mut dict = plist::Dictionary::new();
    dict.insert("Label".into(), paths::BUNDLE_ID.into());
    dict.insert(
        "ProgramArguments".into(),
        plist::Value::Array(vec![program.to_string_lossy().into_owned().into()]),
    );
    dict.insert("RunAtLoad".into(), true.into());
    dict.insert("KeepAlive".into(), false.into());
    dict.insert("ProcessType".into(), "Interactive".into());
    dict.insert("LimitLoadToSessionType".into(), "Aqua".into());
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &plist::Value::Dictionary(dict)).context("Encoding login item")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_program() {
        let bytes = agent_plist(std::path::Path::new(
            "/Applications/Remounty.app/Contents/MacOS/remounty",
        ))
        .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Applications/Remounty.app/Contents/MacOS/remounty"));
        assert!(text.contains("RunAtLoad"));
        assert!(text.contains(paths::BUNDLE_ID));
    }
}
