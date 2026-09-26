//! The status item menu.
//!
//! Menu item ids encode the complete action (including the volume's device
//! name and identity), so a click on an outdated, still-open menu is still
//! unambiguous — and is re-validated against fresh disk state before anything
//! happens.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu};
use objc2_foundation::{NSAttributedString, NSAttributedStringKey, NSDictionary, NSString};
use tray_icon::menu::{CheckMenuItem, ContextMenu, Menu, MenuItem, PredefinedMenuItem};

use crate::disks::{self, MountState, Volume, VolumeKey};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    MountReadWrite(VolumeKey),
    Unmount(VolumeKey),
    MountReadOnly(VolumeKey),
    ShowInFinder(VolumeKey),
    ToggleAutomount(VolumeKey),
    UnmountStale(PathBuf),
    Rescan,
    ToggleAskOnAttach,
    ToggleStartAtLogin,
    ToggleTouchId,
    SetUpTouchId,
    LocateNtfs3g,
    Help,
    ShowNotice,
    OpenLog,
    Quit,
}

impl Action {
    pub fn to_id(&self) -> String {
        let vol = |tag: &str, key: &VolumeKey| {
            format!(
                "{tag}|{}|{}",
                key.bsd_name,
                key.identity.as_deref().unwrap_or("-")
            )
        };
        match self {
            Action::MountReadWrite(k) => vol("rw", k),
            Action::Unmount(k) => vol("unmount", k),
            Action::MountReadOnly(k) => vol("ro", k),
            Action::ShowInFinder(k) => vol("finder", k),
            Action::ToggleAutomount(k) => vol("auto", k),
            Action::UnmountStale(path) => format!("stale|{}", path.to_string_lossy()),
            Action::Rescan => "rescan".into(),
            Action::ToggleAskOnAttach => "ask".into(),
            Action::ToggleStartAtLogin => "login".into(),
            Action::ToggleTouchId => "touchid".into(),
            Action::SetUpTouchId => "touchid-setup".into(),
            Action::LocateNtfs3g => "locate".into(),
            Action::Help => "help".into(),
            Action::ShowNotice => "notice".into(),
            Action::OpenLog => "log".into(),
            Action::Quit => "quit".into(),
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        if let Some(path) = id.strip_prefix("stale|") {
            return Some(Action::UnmountStale(PathBuf::from(path)));
        }
        let mut parts = id.splitn(3, '|');
        let tag = parts.next()?;
        if let (Some(bsd), Some(identity)) = (parts.next(), parts.next()) {
            if !disks::is_valid_bsd_name(bsd) {
                return None;
            }
            let key = VolumeKey {
                bsd_name: bsd.to_string(),
                identity: (identity != "-").then(|| identity.to_string()),
            };
            return match tag {
                "rw" => Some(Action::MountReadWrite(key)),
                "unmount" => Some(Action::Unmount(key)),
                "ro" => Some(Action::MountReadOnly(key)),
                "finder" => Some(Action::ShowInFinder(key)),
                "auto" => Some(Action::ToggleAutomount(key)),
                _ => None,
            };
        }
        match tag {
            "rescan" => Some(Action::Rescan),
            "ask" => Some(Action::ToggleAskOnAttach),
            "login" => Some(Action::ToggleStartAtLogin),
            "touchid" => Some(Action::ToggleTouchId),
            "touchid-setup" => Some(Action::SetUpTouchId),
            "locate" => Some(Action::LocateNtfs3g),
            "help" => Some(Action::Help),
            "notice" => Some(Action::ShowNotice),
            "log" => Some(Action::OpenLog),
            "quit" => Some(Action::Quit),
            _ => None,
        }
    }
}

pub struct VolumeRow {
    pub volume: Volume,
    pub working: bool,
    pub automount: bool,
}

pub struct MenuModel {
    pub status: String,
    pub problem: Option<String>,
    pub volumes: Vec<VolumeRow>,
    pub stale: Vec<PathBuf>,
    pub busy: bool,
    pub deps_ready: bool,
    pub notice_accepted: bool,
    pub ask_on_attach: bool,
    pub start_at_login: bool,
    pub ntfs3g_missing: bool,
    pub touch_id_configured: bool,
    pub use_touch_id: bool,
}

pub struct BuiltMenu {
    pub menu: Menu,
    /// Titles of the volume header lines, styled by [`style_headers`].
    headers: Vec<String>,
}

/// Human readable state of a volume, used in the menu and tooltips.
pub fn describe_state(state: &MountState) -> &'static str {
    match state {
        MountState::Unmounted => "not mounted",
        MountState::Native { read_only: true, .. } => "read-only",
        MountState::Native { read_only: false, .. } => "read-write (other driver)",
        MountState::Fuse { read_only: true, .. } => "read-only (ntfs-3g)",
        MountState::Fuse {
            read_only: false,
            ours: true,
            ..
        } => "read-write",
        MountState::Fuse {
            read_only: false,
            ours: false,
            ..
        } => "read-write (ntfs-3g)",
    }
}

fn item(action: &Action, text: &str, enabled: bool) -> MenuItem {
    MenuItem::with_id(action.to_id(), text, enabled, None)
}

fn label(text: &str) -> MenuItem {
    MenuItem::new(text, false, None)
}

pub fn build(model: &MenuModel) -> Result<BuiltMenu> {
    let menu = Menu::new();
    let mut headers = Vec::new();
    let add = |i: &dyn tray_icon::menu::IsMenuItem| {
        menu.append(i)
            .map_err(|err| Error::new(format!("Building menu: {err}")))
    };
    let separator = PredefinedMenuItem::separator();

    add(&label(&model.status))?;
    if let Some(problem) = &model.problem {
        add(&label(&format!("⚠︎ {problem}")))?;
        if model.ntfs3g_missing {
            add(&item(&Action::LocateNtfs3g, "Locate ntfs-3g…", true))?;
        }
        add(&item(&Action::Help, "Installation Instructions…", true))?;
    }
    if !model.notice_accepted {
        add(&item(
            &Action::ShowNotice,
            "Read the Safety Notice to Get Started…",
            true,
        ))?;
    }
    add(&separator)?;

    let can_act = !model.busy && model.notice_accepted;
    if model.volumes.is_empty() {
        add(&label("No NTFS volumes found"))?;
    }
    for (index, row) in model.volumes.iter().enumerate() {
        if index > 0 {
            add(&PredefinedMenuItem::separator())?;
        }
        let vol = &row.volume;
        let key = vol.key();
        let state = if row.working {
            "working…"
        } else {
            describe_state(&vol.state)
        };
        let header = volume_header(vol, state);
        add(&label(&header))?;
        headers.push(header);
        let enabled = can_act && !row.working;
        match &vol.state {
            MountState::Native { read_only: true, .. } => {
                add(&item(
                    &Action::MountReadWrite(key.clone()),
                    "    Re-mount Read-Write",
                    enabled && model.deps_ready,
                ))?;
                add(&item(
                    &Action::ShowInFinder(key.clone()),
                    "    Show in Finder",
                    true,
                ))?;
            }
            MountState::Unmounted => {
                add(&item(
                    &Action::MountReadWrite(key.clone()),
                    "    Mount Read-Write",
                    enabled && model.deps_ready,
                ))?;
                add(&item(
                    &Action::MountReadOnly(key.clone()),
                    "    Mount Read-Only",
                    enabled,
                ))?;
            }
            MountState::Fuse { .. } => {
                add(&item(
                    &Action::ShowInFinder(key.clone()),
                    "    Show in Finder",
                    true,
                ))?;
                add(&item(&Action::Unmount(key.clone()), "    Unmount", enabled))?;
            }
            MountState::Native { read_only: false, .. } => {
                add(&item(
                    &Action::ShowInFinder(key.clone()),
                    "    Show in Finder",
                    true,
                ))?;
            }
        }
        let auto = CheckMenuItem::with_id(
            Action::ToggleAutomount(key.clone()).to_id(),
            "    Re-mount Automatically",
            vol.identity.is_some() && model.notice_accepted,
            row.automount,
            None,
        );
        add(&auto)?;
    }

    if !model.stale.is_empty() {
        add(&PredefinedMenuItem::separator())?;
        for path in &model.stale {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let header = format!("{name} — disk disconnected");
            add(&label(&header))?;
            headers.push(header);
            add(&item(
                &Action::UnmountStale(path.clone()),
                "    Clean Up Mount",
                can_act,
            ))?;
        }
    }

    add(&PredefinedMenuItem::separator())?;
    add(&item(&Action::Rescan, "Rescan Volumes", true))?;
    add(&CheckMenuItem::with_id(
        Action::ToggleAskOnAttach.to_id(),
        "Ask When an NTFS Volume Is Attached",
        true,
        model.ask_on_attach,
        None,
    ))?;
    add(&CheckMenuItem::with_id(
        Action::ToggleStartAtLogin.to_id(),
        "Start at Login",
        true,
        model.start_at_login,
        None,
    ))?;
    if model.touch_id_configured {
        add(&CheckMenuItem::with_id(
            Action::ToggleTouchId.to_id(),
            "Use Touch ID",
            true,
            model.use_touch_id,
            None,
        ))?;
    } else {
        add(&item(&Action::SetUpTouchId, "Enable Touch ID…", !model.busy))?;
    }
    add(&PredefinedMenuItem::separator())?;
    add(&item(&Action::Help, "Help & Safety Notice…", true))?;
    add(&item(&Action::OpenLog, "Open Log", true))?;
    add(&PredefinedMenuItem::separator())?;
    add(&item(&Action::Quit, "Quit Remounty", true))?;
    Ok(BuiltMenu { menu, headers })
}

/// "Untitled (disk4s1) — read-only · 65.0 MB · Disk Image"
pub fn volume_header(vol: &Volume, state: &str) -> String {
    let mut header = format!("{} — {state} · {}", vol.name, disks::format_size(vol.size));
    if !vol.media.is_empty() {
        header.push_str(" · ");
        header.push_str(&vol.media);
    }
    header
}

/// Makes the volume header lines look like regular (bold) text instead of
/// greyed-out disabled items. They stay disabled, so they are not
/// highlighted on hover and cannot be clicked. Must run on the main thread;
/// if anything is unexpected the headers simply keep the default look.
pub fn style_headers(built: &BuiltMenu) {
    if built.headers.is_empty() || MainThreadMarker::new().is_none() {
        return;
    }
    let ptr = built.menu.ns_menu();
    if ptr.is_null() {
        return;
    }
    // SAFETY: muda returns the NSMenu backing this menu; it stays alive as
    // long as `built.menu`, which outlives this function call.
    let ns_menu: &NSMenu = unsafe { &*ptr.cast::<NSMenu>() };
    let size = NSFont::menuFontOfSize(0.0).pointSize();
    let font = NSFont::boldSystemFontOfSize(size);
    let color = NSColor::labelColor();
    // SAFETY: reading immutable framework constants.
    let keys: [&NSAttributedStringKey; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [&AnyObject; 2] = [&font, &color];
    let attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> =
        NSDictionary::from_slices(&keys, &values);
    for item in ns_menu.itemArray().iter() {
        let title = item.title().to_string();
        if item.isEnabled() || !built.headers.contains(&title) {
            continue;
        }
        // SAFETY: the attributes are a font and a colour, as AppKit expects.
        let styled = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &NSString::from_str(&title),
                Some(&attributes),
            )
        };
        item.setAttributedTitle(Some(&styled));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let key = VolumeKey {
            bsd_name: "disk4s1".into(),
            identity: Some("ABC-1".into()),
        };
        let no_id = VolumeKey {
            bsd_name: "disk4s1".into(),
            identity: None,
        };
        let actions = [
            Action::MountReadWrite(key.clone()),
            Action::Unmount(key.clone()),
            Action::MountReadOnly(no_id.clone()),
            Action::ShowInFinder(key.clone()),
            Action::ToggleAutomount(key),
            Action::UnmountStale(PathBuf::from("/Users/u/.remounty/a|b")),
            Action::Rescan,
            Action::ToggleAskOnAttach,
            Action::ToggleStartAtLogin,
            Action::ToggleTouchId,
            Action::SetUpTouchId,
            Action::LocateNtfs3g,
            Action::Help,
            Action::ShowNotice,
            Action::OpenLog,
            Action::Quit,
        ];
        for action in actions {
            assert_eq!(Action::from_id(&action.to_id()), Some(action));
        }
    }

    #[test]
    fn rejects_malformed_ids() {
        assert_eq!(Action::from_id("rw|../../x|-"), None);
        assert_eq!(Action::from_id("rw|disk4s1"), None);
        assert_eq!(Action::from_id("format|disk4s1|-"), None);
        assert_eq!(Action::from_id(""), None);
        assert_eq!(Action::from_id("12"), None);
    }
}
