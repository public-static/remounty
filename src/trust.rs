//! Deciding whether something is safe to run as root.
//!
//! The rule: a file may be executed or loaded by root only if nobody but
//! root can change it — the file itself, every directory on the way to it
//! (including symlinks and their targets), every non-system library it
//! loads (recursively) and the directory ntfs-3g loads plugins from. Only
//! root can alter such files, so there is no window between checking and
//! using them in which an unprivileged process could swap them.
//!
//! MacPorts installs ntfs-3g and macFUSE this way (under `/opt/local`,
//! owned by root). Homebrew does not (its files belong to the user).

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::cmd;

pub const MACFUSE_BUNDLE: &str = "/Library/Filesystems/macfuse.fs";
const PLUGIN_SUFFIX: &[u8] = b"/ntfs-plugin-%08lx.so";
const OTOOL: &str = "/usr/bin/otool";

pub type TrustResult = std::result::Result<(), String>;

/// Checks one path component with `lstat`: owned by root and, unless it is
/// a symlink, not writable by group or others.
fn check_entry(path: &Path) -> TrustResult {
    let meta =
        fs::symlink_metadata(path).map_err(|err| format!("Cannot inspect {}: {err}", path.display()))?;
    if meta.uid() != 0 {
        return Err(format!("{} is not owned by root", path.display()));
    }
    if !meta.file_type().is_symlink() && meta.permissions().mode() & 0o022 != 0 {
        return Err(format!("{} is writable by users other than root", path.display()));
    }
    Ok(())
}

/// `path`, every directory above it and — if symlinks are involved — the
/// resolved target and its directories must all be root-controlled.
pub fn check_path(path: &Path, expect_dir: bool) -> TrustResult {
    check_path_inner(path, expect_dir, 0)
}

fn check_path_inner(path: &Path, expect_dir: bool, depth: usize) -> TrustResult {
    if depth > 8 {
        return Err(format!("{} has too many levels of symlinks", path.display()));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(format!("{} is not a plain absolute path", path.display()));
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        check_entry(&prefix)?;
    }
    let canonical =
        fs::canonicalize(path).map_err(|err| format!("Cannot resolve {}: {err}", path.display()))?;
    if canonical != path {
        check_path_inner(&canonical, expect_dir, depth + 1)?;
    }
    let meta =
        fs::metadata(&canonical).map_err(|err| format!("Cannot inspect {}: {err}", canonical.display()))?;
    let kind_ok = if expect_dir { meta.is_dir() } else { meta.is_file() };
    if !kind_ok {
        return Err(format!(
            "{} is not a {}",
            path.display(),
            if expect_dir { "directory" } else { "regular file" }
        ));
    }
    Ok(())
}

/// For a directory that may not exist: if it exists it and its contents
/// must be root-controlled; otherwise its nearest existing ancestor must be,
/// so that only root could ever create it.
pub fn check_dir_or_uncreatable(dir: &Path) -> TrustResult {
    if fs::symlink_metadata(dir).is_ok() {
        check_path(dir, true)?;
        let entries = fs::read_dir(dir).map_err(|err| format!("Cannot read {}: {err}", dir.display()))?;
        for entry in entries.flatten() {
            check_entry(&entry.path())?;
        }
        return Ok(());
    }
    let mut ancestor = dir.parent();
    while let Some(candidate) = ancestor {
        if fs::symlink_metadata(candidate).is_ok() {
            return check_path(candidate, true);
        }
        ancestor = candidate.parent();
    }
    Err(format!("{} has no existing parent directory", dir.display()))
}

/// For a file that does not exist: its nearest existing ancestor must be
/// root-controlled, so nobody else could ever create it there.
pub fn check_uncreatable(path: &Path) -> TrustResult {
    let mut ancestor = path.parent();
    while let Some(candidate) = ancestor {
        if fs::symlink_metadata(candidate).is_ok() {
            return check_path(candidate, true);
        }
        ancestor = candidate.parent();
    }
    Err(format!("{} has no existing parent directory", path.display()))
}

pub fn is_system_library(path: &str) -> bool {
    path.starts_with("/usr/lib/") || path.starts_with("/System/")
}

/// Dependency lines of `otool -L` are indented; header lines (the file name,
/// and one per architecture in universal binaries) are not.
pub fn parse_otool_libraries(output: &str) -> Vec<String> {
    let mut deps: Vec<String> = Vec::new();
    for line in output.lines().filter(|l| l.starts_with(char::is_whitespace)) {
        let dep = line
            .trim()
            .split(" (compatibility")
            .next()
            .unwrap_or_default()
            .trim();
        if !dep.is_empty() && !deps.iter().any(|d| d == dep) {
            deps.push(dep.to_string());
        }
    }
    deps
}

/// `LC_RPATH` entries from `otool -l` output.
pub fn parse_otool_rpaths(output: &str) -> Vec<String> {
    let mut rpaths: Vec<String> = Vec::new();
    let mut in_rpath = false;
    for line in output.lines().map(str::trim) {
        if let Some(cmd) = line.strip_prefix("cmd ") {
            in_rpath = cmd.trim() == "LC_RPATH";
        } else if in_rpath && let Some(rest) = line.strip_prefix("path ") {
            let path = rest.split(" (offset").next().unwrap_or_default().trim();
            if !path.is_empty() && !rpaths.iter().any(|p| p == path) {
                rpaths.push(path.to_string());
            }
            in_rpath = false;
        }
    }
    rpaths
}

fn otool(flag: &str, file: &Path) -> std::result::Result<String, String> {
    let out = cmd::run_with(
        Path::new(OTOOL),
        &[flag, &file.to_string_lossy()],
        &cmd::Options {
            env: &[("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")],
            clear_env: true,
            input: None,
            timeout: Some(Duration::from_secs(60)),
        },
    )
    .map_err(|err| format!("Cannot inspect the libraries of {}: {err}", file.display()))?;
    if !out.success() {
        return Err(format!(
            "Cannot inspect the libraries of {} (are the Xcode Command Line Tools installed?): {}",
            file.display(),
            out.diagnostics()
        ));
    }
    Ok(out.stdout)
}

/// Expands `@loader_path` / `@executable_path` the way dyld does.
fn expand(path: &str, loader: &Path, executable: &Path) -> Option<PathBuf> {
    let dir_of = |p: &Path| p.parent().map(Path::to_path_buf);
    if let Some(rest) = path.strip_prefix("@loader_path") {
        return dir_of(loader).map(|d| normalize(&d.join(rest.trim_start_matches('/'))));
    }
    if let Some(rest) = path.strip_prefix("@executable_path") {
        return dir_of(executable).map(|d| normalize(&d.join(rest.trim_start_matches('/'))));
    }
    path.starts_with('/').then(|| normalize(Path::new(path)))
}

/// Resolves `..` and `.` lexically (the paths may not exist).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Every location dyld could load a dependency from must be controlled by
/// root; returns the existing library files to inspect further.
fn resolve_dependency(
    dep: &str,
    loader: &Path,
    executable: &Path,
    rpaths: &[String],
) -> std::result::Result<Vec<PathBuf>, String> {
    let candidates: Vec<PathBuf> = if let Some(name) = dep.strip_prefix("@rpath/") {
        rpaths
            .iter()
            .filter_map(|rpath| expand(rpath, loader, executable))
            .map(|dir| dir.join(name))
            .collect()
    } else {
        let expanded = expand(dep, loader, executable)
            .ok_or_else(|| format!("{} loads {dep}, which cannot be verified", loader.display()))?;
        vec![expanded]
    };
    if candidates.is_empty() {
        return Err(format!(
            "{} loads {dep}, which cannot be resolved",
            loader.display()
        ));
    }
    let mut existing = Vec::new();
    for candidate in candidates {
        let text = candidate.to_string_lossy();
        if is_system_library(&text) {
            continue;
        }
        if fs::symlink_metadata(&candidate).is_ok() {
            check_path(&candidate, false)?;
            existing.push(candidate);
        } else {
            check_uncreatable(&candidate)?;
        }
    }
    Ok(existing)
}

/// The directory an ntfs-3g binary loads plugins from, if it supports them.
pub fn plugin_dir(binary: &[u8]) -> Option<PathBuf> {
    let at = binary
        .windows(PLUGIN_SUFFIX.len())
        .position(|w| w == PLUGIN_SUFFIX)?;
    let head = binary.get(..at)?;
    let start = head.iter().rposition(|&b| b == 0).map_or(0, |nul| nul + 1);
    let dir = std::str::from_utf8(head.get(start..)?).ok()?;
    dir.starts_with('/').then(|| PathBuf::from(dir))
}

/// Verifies that `path` is an ntfs-3g executable that is safe to run as
/// root, together with everything it loads. Returns the path on success.
pub fn verify_ntfs3g(path: &Path) -> std::result::Result<PathBuf, String> {
    let name_ok = matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("ntfs-3g" | "lowntfs-3g")
    );
    if !name_ok {
        return Err(format!("{} is not an ntfs-3g executable", path.display()));
    }
    check_path(path, false).map_err(|reason| protected_hint(&reason))?;
    let mode = fs::metadata(path).map(|m| m.permissions().mode()).unwrap_or(0);
    if mode & 0o111 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }

    // Every library it loads, recursively, must be protected as well.
    let mut queue = vec![path.to_path_buf()];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    while let Some(file) = queue.pop() {
        let deps = parse_otool_libraries(&otool("-L", &file)?);
        let rpaths = parse_otool_rpaths(&otool("-l", &file)?);
        for dep in deps {
            if is_system_library(&dep) {
                continue;
            }
            for library in
                resolve_dependency(&dep, &file, path, &rpaths).map_err(|reason| protected_hint(&reason))?
            {
                if seen.insert(library.clone()) {
                    queue.push(library);
                }
            }
        }
    }

    let bytes = fs::read(path).map_err(|err| format!("Cannot read {}: {err}", path.display()))?;
    if let Some(dir) = plugin_dir(&bytes) {
        check_dir_or_uncreatable(&dir).map_err(|reason| {
            protected_hint(&format!("ntfs-3g's plugin directory is not protected: {reason}"))
        })?;
    }
    Ok(path.to_path_buf())
}

/// macFUSE must be installed in root-controlled locations too: libfuse
/// starts `mount_macfuse` (setuid root) from this bundle.
pub fn verify_macfuse() -> TrustResult {
    check_path(Path::new(MACFUSE_BUNDLE), true).map_err(|reason| protected_hint(&reason))
}

fn protected_hint(reason: &str) -> String {
    format!(
        "{reason}.\n\nRemounty only runs ntfs-3g and macFUSE as root when nobody but root can \
         modify them. Install both with MacPorts (sudo port install macfuse +fs_link ntfs-3g)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_paths_are_trusted() {
        assert!(check_path(Path::new("/usr/bin/true"), false).is_ok());
        assert!(check_path(Path::new("/usr/bin"), true).is_ok());
        // /etc is a root-owned symlink to /private/etc.
        assert!(check_path(Path::new("/etc/hosts"), false).is_ok());
        assert!(check_path(Path::new("/usr/bin/true"), true).is_err());
    }

    #[test]
    fn user_files_are_not_trusted() {
        let dir = std::env::temp_dir().join(format!("remounty-trust-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("ntfs-3g");
        let _ = fs::write(&file, b"x");
        assert!(check_path(&file, false).is_err());
        assert!(check_dir_or_uncreatable(&dir.join("plugins")).is_err());
        assert!(verify_ntfs3g(&file).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_relative_and_dotted_paths() {
        assert!(check_path(Path::new("usr/bin/true"), false).is_err());
        assert!(check_path(Path::new("/usr/bin/../bin/true"), false).is_err());
    }

    #[test]
    fn uncreatable_directory() {
        // /usr/bin exists and is root-only, so nobody else can create this.
        assert!(check_dir_or_uncreatable(Path::new("/usr/bin/remounty-does-not-exist/x")).is_ok());
    }

    #[test]
    fn otool_parsing() {
        let out = "/x/bin/ntfs-3g:\n\t/opt/local/lib/libfuse.2.dylib (compatibility version 12.0.0, current version 12.9.0)\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1356.0.0)\n";
        assert_eq!(
            parse_otool_libraries(out),
            vec![
                "/opt/local/lib/libfuse.2.dylib".to_string(),
                "/usr/lib/libSystem.B.dylib".to_string()
            ]
        );
        let fat = "/x/libfuse.2.dylib (architecture x86_64):\n\t/x/libfuse.2.dylib (compatibility version 12.0.0)\n\t/usr/lib/libiconv.2.dylib (compatibility version 7.0.0)\n/x/libfuse.2.dylib (architecture arm64):\n\t/x/libfuse.2.dylib (compatibility version 12.0.0)\n\t/usr/lib/libiconv.2.dylib (compatibility version 7.0.0)\n";
        assert_eq!(
            parse_otool_libraries(fat),
            vec![
                "/x/libfuse.2.dylib".to_string(),
                "/usr/lib/libiconv.2.dylib".to_string()
            ]
        );
        assert!(is_system_library("/usr/lib/libSystem.B.dylib"));
        assert!(!is_system_library("/opt/local/lib/libintl.8.dylib"));
    }

    #[test]
    fn rpath_parsing_and_expansion() {
        let out = "Load command 31\n          cmd LC_RPATH\n      cmdsize 32\n         path /usr/lib/swift (offset 12)\nLoad command 32\n          cmd LC_RPATH\n      cmdsize 48\n         path @executable_path/../Frameworks (offset 12)\nLoad command 33\n          cmd LC_LOAD_DYLIB\n         name /x (offset 24)\n";
        assert_eq!(
            parse_otool_rpaths(out),
            vec![
                "/usr/lib/swift".to_string(),
                "@executable_path/../Frameworks".to_string()
            ]
        );
        let exe = Path::new("/opt/local/bin/ntfs-3g");
        let lib = Path::new("/opt/local/lib/libx.dylib");
        assert_eq!(
            expand("@executable_path/../Frameworks", lib, exe),
            Some(PathBuf::from("/opt/local/Frameworks"))
        );
        assert_eq!(
            expand("@loader_path/Frameworks", lib, exe),
            Some(PathBuf::from("/opt/local/lib/Frameworks"))
        );
        assert_eq!(expand("relative", lib, exe), None);
        // A search path in a user-writable place is refused.
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        assert!(resolve_dependency("@rpath/libevil.dylib", lib, exe, &[tmp]).is_err());
        // System search paths are fine.
        assert!(
            resolve_dependency("@rpath/libswiftCore.dylib", lib, exe, &["/usr/lib/swift".into()]).is_ok()
        );
    }

    #[test]
    fn finds_plugin_dir() {
        let bin = b"\0Plugin path: /opt/local/lib/ntfs-3g\0/opt/local/lib/ntfs-3g/ntfs-plugin-%08lx.so\0";
        assert_eq!(plugin_dir(bin), Some(PathBuf::from("/opt/local/lib/ntfs-3g")));
        assert_eq!(plugin_dir(b"no plugins"), None);
    }
}
