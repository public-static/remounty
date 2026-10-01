//! Turning untrusted volume labels into safe mount options and path names.
//! Used by both the app and the privileged helper.

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

/// A volume identity as reported by diskutil: hex digits and dashes only.
pub fn is_safe_identity(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Text that is safe to show in an authorization dialog: no control
/// characters, limited length.
pub fn display_text(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    cleaned.chars().take(80).collect()
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
    fn display_text_is_bounded() {
        assert_eq!(display_text("a\nb\u{7}c"), "abc");
        assert_eq!(display_text(&"x".repeat(200)).chars().count(), 80);
    }
}
