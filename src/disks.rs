//! Discovery of NTFS volumes and their current mount state.
//!
//! Disk metadata comes from `diskutil … -plist`, the mount state from the
//! kernel mount table. The mount table is authoritative: it is what decides
//! whether a volume is mounted, by whom and whether it is writable.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::cmd;
use crate::error::{Context, Error, Result};
use crate::mounts::{self, MountEntry};

pub const DISKUTIL: &str = "/usr/sbin/diskutil";
const DISKUTIL_TIMEOUT: Duration = Duration::from_secs(30);

/// Partition contents that can never hold an NTFS file system. Skipping them
/// avoids one `diskutil info` call per APFS/EFI partition on every scan.
const SKIPPED_CONTENT: &[&str] = &[
    "EFI",
    "Apple_APFS",
    "Apple_APFS_Container",
    "Apple_APFS_ISC",
    "Apple_APFS_Recovery",
    "Apple_Boot",
    "Apple_HFS",
    "Apple_HFSX",
    "Apple_KernelCoreDump",
    "Apple_partition_map",
    "Apple_Free",
    "Microsoft Reserved",
    "Linux Swap",
    "GUID_partition_scheme",
    "FDisk_partition_scheme",
    "Apple_partition_scheme",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    Unmounted,
    /// Mounted by macOS itself (FSKit / kernel driver) or a third party driver.
    Native {
        path: PathBuf,
        read_only: bool,
    },
    /// Mounted through FUSE, i.e. by ntfs-3g. `ours` marks mounts that live in
    /// Remounty's mount directory.
    Fuse {
        path: PathBuf,
        read_only: bool,
        ours: bool,
    },
}

impl MountState {
    pub fn path(&self) -> Option<&Path> {
        match self {
            MountState::Unmounted => None,
            MountState::Native { path, .. } | MountState::Fuse { path, .. } => Some(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// BSD name, e.g. `disk4s1`. Always validated by [`is_valid_bsd_name`].
    pub bsd_name: String,
    /// Volume label as reported by diskutil; may be empty.
    pub label: String,
    /// Name shown to the user. Unique within a scan: unnamed volumes and
    /// volumes sharing a label get their device name appended.
    pub name: String,
    /// Description of the physical drive, e.g. "WD Elements 25A3".
    pub media: String,
    /// Stable identifier: volume UUID if available, otherwise partition UUID.
    pub identity: Option<String>,
    pub size: u64,
    pub internal: bool,
    pub state: MountState,
}

impl Volume {
    pub fn device_node(&self) -> String {
        format!("/dev/{}", self.bsd_name)
    }

    /// The label, or "Untitled" for unnamed volumes.
    pub fn display_name(&self) -> String {
        let label = self.label.trim();
        if label.is_empty() {
            "Untitled".to_string()
        } else {
            label.to_string()
        }
    }

    /// Key that identifies this particular attachment of the volume.
    pub fn key(&self) -> VolumeKey {
        VolumeKey {
            bsd_name: self.bsd_name.clone(),
            identity: self.identity.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VolumeKey {
    pub bsd_name: String,
    pub identity: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub volumes: Vec<Volume>,
    /// FUSE mounts inside Remounty's mount directory whose device is gone.
    pub orphans: Vec<MountEntry>,
}

impl Scan {
    pub fn find(&self, key: &VolumeKey) -> Option<&Volume> {
        self.volumes.iter().find(|v| &v.key() == key)
    }
}

/// `disk4`, `disk4s1`, `disk12s3s1`, … — nothing else is ever accepted.
pub fn is_valid_bsd_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("disk") else {
        return false;
    };
    let mut expect_digit = true;
    let mut last_was_digit = false;
    for c in rest.chars() {
        if c.is_ascii_digit() {
            last_was_digit = true;
            expect_digit = false;
        } else if c == 's' && !expect_digit {
            expect_digit = true;
            last_was_digit = false;
        } else {
            return false;
        }
    }
    last_was_digit
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListPlist {
    #[serde(default)]
    all_disks_and_partitions: Vec<ListDisk>,
}

#[derive(Debug, Deserialize)]
struct ListDisk {
    #[serde(rename = "DeviceIdentifier")]
    device_identifier: String,
    #[serde(rename = "Content", default)]
    content: Option<String>,
    #[serde(rename = "Partitions", default)]
    partitions: Vec<ListPartition>,
    #[serde(rename = "APFSVolumes", default)]
    apfs_volumes: Vec<plist::Value>,
}

#[derive(Debug, Deserialize)]
struct ListPartition {
    #[serde(rename = "DeviceIdentifier")]
    device_identifier: String,
    #[serde(rename = "Content", default)]
    content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct InfoPlist {
    #[serde(rename = "DeviceIdentifier", default)]
    pub device_identifier: String,
    #[serde(rename = "DeviceNode", default)]
    pub device_node: String,
    #[serde(rename = "FilesystemType", default)]
    pub filesystem_type: Option<String>,
    #[serde(rename = "VolumeName", default)]
    pub volume_name: Option<String>,
    #[serde(rename = "VolumeUUID", default)]
    pub volume_uuid: Option<String>,
    #[serde(rename = "DiskUUID", default)]
    pub disk_uuid: Option<String>,
    #[serde(rename = "MountPoint", default)]
    pub mount_point: Option<String>,
    #[serde(rename = "WritableVolume", default)]
    pub writable_volume: Option<bool>,
    #[serde(rename = "TotalSize", default)]
    pub total_size: Option<u64>,
    #[serde(rename = "Size", default)]
    pub size: Option<u64>,
    #[serde(rename = "Internal", default)]
    pub internal: Option<bool>,
    #[serde(rename = "MediaName", default)]
    pub media_name: Option<String>,
    #[serde(rename = "BusProtocol", default)]
    pub bus_protocol: Option<String>,
    #[serde(rename = "Error", default)]
    pub error: Option<bool>,
    #[serde(rename = "ErrorMessage", default)]
    pub error_message: Option<String>,
}

impl InfoPlist {
    pub fn is_ntfs(&self) -> bool {
        self.filesystem_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("ntfs"))
    }

    pub fn identity(&self) -> Option<String> {
        [&self.volume_uuid, &self.disk_uuid]
            .into_iter()
            .flatten()
            .map(|s| s.trim().to_ascii_uppercase())
            .find(|s| !s.is_empty())
    }
}

/// Lists BSD names that may contain an NTFS file system.
fn candidates(list: &ListPlist) -> Vec<String> {
    let skipped = |content: &Option<String>| {
        content
            .as_deref()
            .is_some_and(|c| SKIPPED_CONTENT.iter().any(|s| s.eq_ignore_ascii_case(c)))
    };
    let mut out = Vec::new();
    for disk in &list.all_disks_and_partitions {
        if !disk.apfs_volumes.is_empty() {
            continue;
        }
        if disk.partitions.is_empty() {
            if !skipped(&disk.content) {
                out.push(disk.device_identifier.clone());
            }
            continue;
        }
        for part in &disk.partitions {
            if !skipped(&part.content) {
                out.push(part.device_identifier.clone());
            }
        }
    }
    out.retain(|name| is_valid_bsd_name(name));
    out
}

pub fn diskutil_info(bsd_name: &str) -> Result<InfoPlist> {
    if !is_valid_bsd_name(bsd_name) {
        return Err(Error::new(format!("Invalid device name {bsd_name:?}")));
    }
    let out = cmd::run(
        Path::new(DISKUTIL),
        &["info", "-plist", bsd_name],
        Some(DISKUTIL_TIMEOUT),
    )?;
    let info: InfoPlist = plist::from_bytes(out.stdout.as_bytes())
        .context(format!("Unexpected output from diskutil info {bsd_name}"))?;
    if info.error == Some(true) || !out.success() {
        let reason = info.error_message.clone().unwrap_or_else(|| out.diagnostics());
        return Err(Error::new(format!("{bsd_name}: {reason}")));
    }
    if info.device_identifier != bsd_name || info.device_node != format!("/dev/{bsd_name}") {
        return Err(Error::new(format!(
            "diskutil returned information for {:?} instead of {bsd_name}",
            info.device_identifier
        )));
    }
    Ok(info)
}

/// Performs a full scan of attached NTFS volumes.
pub fn scan(mount_root: Option<&Path>) -> Result<Scan> {
    let out = cmd::run(Path::new(DISKUTIL), &["list", "-plist"], Some(DISKUTIL_TIMEOUT))?;
    if !out.success() {
        return Err(Error::new(format!("diskutil list failed: {}", out.diagnostics())));
    }
    let list: ListPlist =
        plist::from_bytes(out.stdout.as_bytes()).context("Unexpected output from diskutil list")?;
    let entries = mounts::snapshot()?;

    let mut volumes = Vec::new();
    for bsd_name in candidates(&list) {
        match diskutil_info(&bsd_name) {
            Ok(info) if info.is_ntfs() => volumes.push(volume_from_info(&info, &entries, mount_root)),
            Ok(_) => {}
            // A disk that vanishes mid-scan is normal (e.g. it was just ejected).
            Err(err) => crate::log_warn!("Skipping {bsd_name}: {err}"),
        }
    }
    volumes.sort_by(|a, b| a.bsd_name.cmp(&b.bsd_name));
    let mut media_cache: HashMap<String, String> = HashMap::new();
    for vol in &mut volumes {
        let whole = whole_disk_of(&vol.bsd_name);
        vol.media = media_cache
            .entry(whole.clone())
            .or_insert_with(|| media_description(&whole, vol.internal))
            .clone();
    }
    assign_unique_names(&mut volumes);
    let orphans = find_orphans(&volumes, &entries, mount_root);
    Ok(Scan { volumes, orphans })
}

/// Re-reads a single volume. Used right before acting on it.
pub fn rescan_volume(bsd_name: &str, mount_root: Option<&Path>) -> Result<Volume> {
    let info = diskutil_info(bsd_name)?;
    if !info.is_ntfs() {
        return Err(Error::new(format!(
            "{bsd_name} no longer contains an NTFS volume"
        )));
    }
    let entries = mounts::snapshot()?;
    Ok(volume_from_info(&info, &entries, mount_root))
}

/// `disk6s1` → `disk6`.
pub fn whole_disk_of(bsd_name: &str) -> String {
    match bsd_name.strip_prefix("disk") {
        Some(rest) => {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            format!("disk{digits}")
        }
        None => bsd_name.to_string(),
    }
}

/// Human readable name of the drive a volume lives on.
fn media_description(whole_disk: &str, internal: bool) -> String {
    let info = diskutil_info(whole_disk).ok();
    let media = info
        .as_ref()
        .and_then(|i| i.media_name.as_deref())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let is_image = info
        .as_ref()
        .and_then(|i| i.bus_protocol.as_deref())
        .is_some_and(|p| p.eq_ignore_ascii_case("Disk Image"));
    match media {
        Some(media) => media,
        None if is_image => "Disk Image".into(),
        None if internal => "Internal Disk".into(),
        None => "External Disk".into(),
    }
}

/// Gives every volume a name the user can tell apart from the others:
/// the label if it is unique, otherwise "Label (diskXsY)".
pub fn assign_unique_names(volumes: &mut [Volume]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for vol in volumes.iter() {
        *counts.entry(vol.display_name().to_lowercase()).or_default() += 1;
    }
    for vol in volumes.iter_mut() {
        let unique = counts.get(&vol.display_name().to_lowercase()) == Some(&1);
        vol.name = if unique && !vol.label.trim().is_empty() {
            vol.display_name()
        } else {
            format!("{} ({})", vol.display_name(), vol.bsd_name)
        };
    }
}

pub fn volume_from_info(info: &InfoPlist, entries: &[MountEntry], mount_root: Option<&Path>) -> Volume {
    let device_node = format!("/dev/{}", info.device_identifier);
    let mut volume = Volume {
        bsd_name: info.device_identifier.clone(),
        label: info.volume_name.clone().unwrap_or_default(),
        name: String::new(),
        media: String::new(),
        identity: info.identity(),
        size: info.total_size.or(info.size).unwrap_or(0),
        internal: info.internal.unwrap_or(false),
        state: classify(
            &device_node,
            info.mount_point.as_deref(),
            info.writable_volume,
            entries,
            mount_root,
        ),
    };
    assign_unique_names(std::slice::from_mut(&mut volume));
    volume
}

/// Decides how a device is mounted, preferring the kernel mount table over
/// diskutil's (possibly stale) view.
pub fn classify(
    device_node: &str,
    diskutil_mount_point: Option<&str>,
    diskutil_writable: Option<bool>,
    entries: &[MountEntry],
    mount_root: Option<&Path>,
) -> MountState {
    let mut matching: Vec<&MountEntry> = entries.iter().filter(|e| e.from == device_node).collect();
    // A FUSE mount wins if, for whatever reason, both exist.
    matching.sort_by_key(|e| !e.is_fuse());
    if let Some(entry) = matching.first() {
        return state_for_entry(entry, mount_root);
    }
    // Some FUSE builds report a different source name; fall back to looking
    // up diskutil's mount point in the mount table.
    if let Some(mp) = diskutil_mount_point.filter(|mp| !mp.is_empty()) {
        if let Some(entry) = entries.iter().find(|e| e.on == Path::new(mp)) {
            return state_for_entry(entry, mount_root);
        }
        return MountState::Native {
            path: PathBuf::from(mp),
            read_only: !diskutil_writable.unwrap_or(false),
        };
    }
    MountState::Unmounted
}

fn state_for_entry(entry: &MountEntry, mount_root: Option<&Path>) -> MountState {
    if entry.is_fuse() {
        MountState::Fuse {
            path: entry.on.clone(),
            read_only: entry.read_only,
            ours: mount_root.is_some_and(|root| is_inside(&entry.on, root)),
        }
    } else {
        MountState::Native {
            path: entry.on.clone(),
            read_only: entry.read_only,
        }
    }
}

pub fn is_inside(path: &Path, root: &Path) -> bool {
    path.parent().is_some_and(|parent| parent == root)
}

fn find_orphans(volumes: &[Volume], entries: &[MountEntry], mount_root: Option<&Path>) -> Vec<MountEntry> {
    let Some(root) = mount_root else {
        return Vec::new();
    };
    let used: HashSet<&Path> = volumes.iter().filter_map(|v| v.state.path()).collect();
    entries
        .iter()
        .filter(|e| e.is_fuse() && is_inside(&e.on, root) && !used.contains(e.on.as_path()))
        .cloned()
        .collect()
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {name}")
    } else {
        format!("{value:.1} {name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(from: &str, on: &str, fs: &str, ro: bool) -> MountEntry {
        MountEntry {
            from: from.into(),
            on: on.into(),
            fs_type: fs.into(),
            read_only: ro,
        }
    }

    #[test]
    fn bsd_names() {
        for ok in ["disk0", "disk4s1", "disk12s3s1"] {
            assert!(is_valid_bsd_name(ok), "{ok}");
        }
        for bad in [
            "",
            "disk",
            "disks1",
            "disk1s",
            "disk1ss1",
            "/dev/disk1",
            "disk1s1;rm",
            "disk1 s1",
            "Disk1",
            "disk1s1\n",
        ] {
            assert!(!is_valid_bsd_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn classify_native_read_only() {
        let entries = [entry("/dev/disk4s1", "/Volumes/Test", "ntfs", true)];
        assert_eq!(
            classify("/dev/disk4s1", Some("/Volumes/Test"), Some(false), &entries, None),
            MountState::Native {
                path: "/Volumes/Test".into(),
                read_only: true
            }
        );
    }

    #[test]
    fn classify_fuse_ours_and_foreign() {
        let root = Path::new("/Users/u/.remounty");
        let entries = [entry("/dev/disk4s1", "/Users/u/.remounty/Test", "macfuse", false)];
        assert_eq!(
            classify("/dev/disk4s1", None, None, &entries, Some(root)),
            MountState::Fuse {
                path: "/Users/u/.remounty/Test".into(),
                read_only: false,
                ours: true
            }
        );
        let entries = [entry("/dev/disk4s1", "/Users/u/.mounty/Test", "macfuse", false)];
        assert_eq!(
            classify("/dev/disk4s1", None, None, &entries, Some(root)),
            MountState::Fuse {
                path: "/Users/u/.mounty/Test".into(),
                read_only: false,
                ours: false
            }
        );
    }

    #[test]
    fn classify_prefers_fuse_entry() {
        let entries = [
            entry("/dev/disk4s1", "/Volumes/Test", "ntfs", true),
            entry("/dev/disk4s1", "/x/Test", "macfuse", false),
        ];
        assert!(matches!(
            classify("/dev/disk4s1", None, None, &entries, None),
            MountState::Fuse { .. }
        ));
    }

    #[test]
    fn classify_by_mount_point_fallback() {
        let entries = [entry("ntfs-3g@disk4s1", "/Users/u/.remounty/T", "macfuse", false)];
        let root = Path::new("/Users/u/.remounty");
        assert_eq!(
            classify(
                "/dev/disk4s1",
                Some("/Users/u/.remounty/T"),
                None,
                &entries,
                Some(root)
            ),
            MountState::Fuse {
                path: "/Users/u/.remounty/T".into(),
                read_only: false,
                ours: true
            }
        );
    }

    #[test]
    fn classify_unmounted() {
        let entries = [entry("/dev/disk9s1", "/Volumes/Other", "ntfs", true)];
        assert_eq!(
            classify("/dev/disk4s1", Some(""), None, &entries, None),
            MountState::Unmounted
        );
        // "disk4s1" must not match "disk4s10".
        let entries = [entry("/dev/disk4s10", "/Volumes/Other", "ntfs", true)];
        assert_eq!(
            classify("/dev/disk4s1", None, None, &entries, None),
            MountState::Unmounted
        );
    }

    #[test]
    fn orphans_are_only_ours_and_unclaimed() {
        let root = Path::new("/r");
        let entries = [
            entry("/dev/disk4s1", "/r/A", "macfuse", false),
            entry("/dev/disk5s1", "/r/B", "macfuse", false),
            entry("/dev/disk6s1", "/elsewhere/C", "macfuse", false),
        ];
        let volumes = [Volume {
            bsd_name: "disk4s1".into(),
            label: "A".into(),
            name: "A".into(),
            media: String::new(),
            identity: None,
            size: 0,
            internal: false,
            state: MountState::Fuse {
                path: "/r/A".into(),
                read_only: false,
                ours: true,
            },
        }];
        let orphans = find_orphans(&volumes, &entries, Some(root));
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans.first().map(|o| o.on.clone()), Some(PathBuf::from("/r/B")));
    }

    #[test]
    fn candidates_skip_apfs_and_efi() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>AllDisksAndPartitions</key><array>
<dict><key>DeviceIdentifier</key><string>disk0</string><key>Content</key><string>GUID_partition_scheme</string>
<key>Partitions</key><array>
<dict><key>DeviceIdentifier</key><string>disk0s1</string><key>Content</key><string>EFI</string></dict>
<dict><key>DeviceIdentifier</key><string>disk0s2</string><key>Content</key><string>Apple_APFS</string></dict>
<dict><key>DeviceIdentifier</key><string>disk0s3</string><key>Content</key><string>Microsoft Basic Data</string></dict>
</array></dict>
<dict><key>DeviceIdentifier</key><string>disk3</string><key>Content</key><string>Apple_APFS_Container</string>
<key>APFSVolumes</key><array><dict><key>DeviceIdentifier</key><string>disk3s1</string></dict></array></dict>
<dict><key>DeviceIdentifier</key><string>disk7</string><key>Content</key><string>Windows_NTFS</string></dict>
</array></dict></plist>"#;
        let list: Option<ListPlist> = plist::from_bytes(xml.as_bytes()).ok();
        let names = list.map(|l| candidates(&l)).unwrap_or_default();
        assert_eq!(names, vec!["disk0s3".to_string(), "disk7".to_string()]);
    }

    #[test]
    fn info_identity_prefers_volume_uuid() {
        let info = InfoPlist {
            volume_uuid: Some("abc".into()),
            disk_uuid: Some("def".into()),
            ..Default::default()
        };
        assert_eq!(info.identity(), Some("ABC".into()));
        let info = InfoPlist {
            volume_uuid: Some("  ".into()),
            disk_uuid: Some("def".into()),
            ..Default::default()
        };
        assert_eq!(info.identity(), Some("DEF".into()));
        assert_eq!(InfoPlist::default().identity(), None);
    }

    #[test]
    fn display_name_falls_back() {
        let v = Volume {
            bsd_name: "disk6s1".into(),
            label: " ".into(),
            name: String::new(),
            media: String::new(),
            identity: None,
            size: 0,
            internal: false,
            state: MountState::Unmounted,
        };
        assert_eq!(v.display_name(), "Untitled");
    }

    fn vol(bsd: &str, label: &str) -> Volume {
        Volume {
            bsd_name: bsd.into(),
            label: label.into(),
            name: String::new(),
            media: String::new(),
            identity: None,
            size: 0,
            internal: false,
            state: MountState::Unmounted,
        }
    }

    #[test]
    fn names_are_unique() {
        let mut vols = vec![
            vol("disk4s1", ""),
            vol("disk5s1", ""),
            vol("disk6s1", "Data"),
            vol("disk6s2", "data"),
            vol("disk7s1", "Photos"),
            vol("disk8s1", "Untitled"),
        ];
        assign_unique_names(&mut vols);
        let names: Vec<&str> = vols.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Untitled (disk4s1)",
                "Untitled (disk5s1)",
                "Data (disk6s1)",
                "data (disk6s2)",
                "Photos",
                "Untitled (disk8s1)",
            ]
        );
        let mut single = [vol("disk9s1", "")];
        assign_unique_names(&mut single);
        assert_eq!(
            single.first().map(|v| v.name.clone()),
            Some("Untitled (disk9s1)".into())
        );
    }

    #[test]
    fn whole_disk_names() {
        assert_eq!(whole_disk_of("disk6s1"), "disk6");
        assert_eq!(whole_disk_of("disk12s3s1"), "disk12");
        assert_eq!(whole_disk_of("disk3"), "disk3");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(65_007_616), "65.0 MB");
        assert_eq!(format_size(5_794_421_547_008), "5.8 TB");
    }
}
