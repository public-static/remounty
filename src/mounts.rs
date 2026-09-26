//! Snapshot of the kernel mount table via `getfsstat(2)`.

use std::path::PathBuf;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MountEntry {
    /// Source, e.g. `/dev/disk4s1`.
    pub from: String,
    /// Mount point.
    pub on: PathBuf,
    /// File system type name, e.g. `ntfs`, `macfuse`.
    pub fs_type: String,
    pub read_only: bool,
}

impl MountEntry {
    /// Whether the mount is served by a FUSE file system (ntfs-3g).
    pub fn is_fuse(&self) -> bool {
        is_fuse_type(&self.fs_type)
    }
}

pub fn is_fuse_type(fs_type: &str) -> bool {
    let t = fs_type.to_ascii_lowercase();
    t.contains("fuse") || t == "ntfs-3g"
}

pub fn snapshot() -> Result<Vec<MountEntry>> {
    // The number of mounts can change between the two calls, so retry with
    // some headroom until the buffer was large enough.
    for _ in 0..4 {
        // SAFETY: a null buffer asks only for the number of mounted file systems.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(Error::new(format!(
                "Reading the mount table failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let capacity = usize::try_from(count).unwrap_or(0) + 8;
        // SAFETY: `statfs` is plain old data; all-zero is a valid value.
        let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; capacity];
        let bytes = capacity * std::mem::size_of::<libc::statfs>();
        let Ok(bytes) = libc::c_int::try_from(bytes) else {
            return Err(Error::new("Mount table is unexpectedly large"));
        };
        // SAFETY: `buf` holds `capacity` entries, exactly `bytes` long.
        let filled = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        if filled < 0 {
            return Err(Error::new(format!(
                "Reading the mount table failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let filled = usize::try_from(filled).unwrap_or(0);
        if filled >= capacity {
            continue;
        }
        return Ok(buf.iter().take(filled).map(entry_from_statfs).collect());
    }
    Err(Error::new(
        "The mount table kept changing while it was being read",
    ))
}

fn entry_from_statfs(st: &libc::statfs) -> MountEntry {
    MountEntry {
        from: c_chars_to_string(&st.f_mntfromname),
        on: PathBuf::from(c_chars_to_string(&st.f_mntonname)),
        fs_type: c_chars_to_string(&st.f_fstypename),
        read_only: (st.f_flags & libc::MNT_RDONLY as u32) != 0,
    }
}

fn c_chars_to_string(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A cheap fingerprint used to notice mount table changes between scans.
pub fn fingerprint(entries: &[MountEntry]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut sorted: Vec<&MountEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.on.cmp(&b.on).then_with(|| a.from.cmp(&b.from)));
    let mut hasher = DefaultHasher::new();
    sorted.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn root_is_mounted() {
        let entries = snapshot().unwrap_or_default();
        assert!(entries.iter().any(|e| e.on == Path::new("/")));
    }

    #[test]
    fn fuse_detection() {
        assert!(is_fuse_type("macfuse"));
        assert!(is_fuse_type("osxfuse"));
        assert!(!is_fuse_type("ntfs"));
        assert!(!is_fuse_type("apfs"));
    }

    #[test]
    fn fingerprint_ignores_order() {
        let a = MountEntry {
            from: "a".into(),
            on: "/a".into(),
            fs_type: "x".into(),
            read_only: false,
        };
        let b = MountEntry {
            on: "/b".into(),
            ..a.clone()
        };
        assert_eq!(
            fingerprint(&[a.clone(), b.clone()]),
            fingerprint(&[b.clone(), a.clone()])
        );
        let c = MountEntry {
            read_only: true,
            ..a.clone()
        };
        assert_ne!(fingerprint(&[a, b.clone()]), fingerprint(&[c, b]));
    }
}
