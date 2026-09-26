# Remounty

A macOS menu bar app that re-mounts read-only NTFS volumes **read-write** using
[NTFS-3G](https://github.com/tuxera/ntfs-3g) and [macFUSE](https://macfuse.github.io/).
It is an alternative to [Mounty for NTFS](https://mounty.app/), written in Rust.
It works like Mounty and adds two things:

* **Startup scan.** Volumes that were attached *before* Remounty started are
  listed and can be re-mounted. Mounty only sees volumes attached after launch.
  Volumes marked "Re-mount Automatically" are re-mounted at startup too.
* **Stricter safety.** See [Safety design](#safety-design).

## Requirements

```sh
brew install --cask macfuse
brew install gromgit/fuse/ntfs-3g-mac
```

After installing macFUSE, allow its system extension in *System Settings →
Privacy & Security* and restart if asked. Remounty looks for `ntfs-3g` in
`/opt/homebrew/bin`, `/usr/local/bin` and `/opt/local/bin`. If yours is
elsewhere, use *Locate ntfs-3g…* in the menu.

## Build

```sh
./scripts/bundle.sh          # → target/release/Remounty.app
cp -R target/release/Remounty.app /Applications/
```

`cargo test` runs the unit tests. `remounty --scan` prints the NTFS volumes
Remounty sees and their state, without changing anything.

## Usage

The menu bar icon is a drive split into two halves:

| Left half | Meaning |
|-----------|---------|
| white     | nothing to do |
| orange    | a volume can be re-mounted read-write |
| green     | an operation is in progress |

| Right half | Meaning |
|------------|---------|
| white      | no volume is writable through Remounty |
| blue       | at least one volume is mounted read-write by Remounty |

The menu lists every NTFS volume with its state and these actions:

* **Re-mount Read-Write**: replaces macOS's read-only mount with an NTFS-3G
  mount in `~/.remounty/<name>`. The volume shows up in the Finder sidebar.
* **Unmount**: unmounts the NTFS-3G mount and waits until NTFS-3G has written
  everything to disk.
* **Mount Read-Only / Mount Read-Write** (for unmounted volumes).
* **Show in Finder**.
* **Re-mount Automatically**: a per-volume setting, remembered by volume UUID.
  It applies when the volume is attached and when Remounty starts.

When a new NTFS volume is attached, Remounty asks whether to re-mount it.
You can turn this off with *Ask When an NTFS Volume Is Attached*. Other items:
*Start at Login*, *Rescan Volumes*, *Help & Safety Notice…* and *Open Log*
(`~/Library/Logs/Remounty.log`).

Each mount asks for an administrator password in the standard macOS dialog.
Remounty never sees or stores the password. Unmounting usually needs no
password.

**Touch ID.** macOS offers Touch ID in that dialog only to Apple's own apps.
*Enable Touch ID…* in the menu turns on Touch ID for `sudo` in Apple's
supported way: it creates `/etc/pam.d/sudo_local` containing one line,
`auth sufficient pam_tid.so`, and macOS keeps that file across updates. You
confirm this once with your password. From then on, Remounty authenticates
through `sudo -k` with Touch ID. If Touch ID fails or you cancel it, you get
the password dialog instead. Remounty never modifies an existing
`sudo_local`. To undo the change, delete the file.

**Volume names.** A volume with a unique label keeps it. Unnamed volumes,
and volumes that share a label, get their device name added, for example
"Untitled (disk4s1)". The same name is used in the menu, in notifications,
for the mount folder, and in Finder, so two unnamed disks can always be told
apart. The menu also shows each volume's size and drive model.

**Finder and notifications.** Finder opens *any* volume root that another
app asks it to open (including native ones, via `open`) in a bare window
without toolbar or sidebar. Remounty instead asks Finder for a normal browser
window, which needs the one-time "Remounty wants to control Finder"
permission. If you deny it, volumes open in the bare window. Clicking a
"now writable" notification opens the volume. Modern notifications require a
properly signed app; for ad-hoc signed builds Remounty falls back to the
older notification API, and only uses `osascript` when it isn't running from
an app bundle.

## Safety design

Writing to NTFS from macOS relies on third-party drivers, so Remounty is built
to never make things worse:

* **Never forces anything.** Unmounts are never forced. If a file is open, the
  operation stops with an "in use" message and nothing changes.
* **Refuses risky volumes.** NTFS-3G runs with `norecover`. A volume that
  Windows left unclean is refused instead of having its journal wiped.
  `remove_hiberfile` is never used, so a hibernated (Fast Startup) volume
  stays read-only, and the user is told why.
* **Verifies before acting.** Before every operation, the volume is re-read. It
  must still be the same volume (matched by UUID, which the root script checks
  again) and in the expected state. Stale menu clicks are harmless.
* **Verifies after acting.** A mount is confirmed through the kernel mount
  table, including that it really is writable.
* **Restores after failure.** If a re-mount fails after the read-only mount was
  removed, the read-only mount is put back.
* **No shell injection.** Root work is a fixed shell script run via
  `do shell script … with administrator privileges`. Device names, paths,
  options and volume names are passed as separate arguments and quoted by
  AppleScript. Volume labels are cleaned before going into `-o volname=` so
  they cannot inject mount options. Device names must match `diskN[sN…]`.
* **Only runs a trustworthy `ntfs-3g`.** It must be an executable file named
  `ntfs-3g` that is not group- or world-writable.
* **Never deletes data.** Mount-point folders are removed with `rmdir`, which
  only deletes empty folders and never a mount point.
* **One operation at a time.** Operations run one at a time on a worker thread.
  A second copy of the app refuses to start. Quitting is blocked while an
  operation runs. Quitting never unmounts anything, because NTFS-3G runs as an
  independent process.
* **No panics or exits.** The code never calls `unwrap`, `expect`, `panic!`
  or `process::exit`; clippy lints enforce this. Errors are shown to the user
  and logged. Worker threads and the event loop also catch unexpected panics,
  so a bug is reported instead of leaving an operation half done.
* **Corrupt settings don't stop the app.** A corrupt settings file is moved
  aside, and Remounty starts with defaults.

Still: **keep backups**, eject volumes before unplugging them, and turn off
Windows Fast Startup on disks you share with Windows.

## Architecture

| Module | Role |
|--------|------|
| `main.rs` | startup, single instance, `tao` event loop |
| `app.rs` | state and event handling on the main thread (menu, icon, queue, prompts) |
| `watcher.rs` | DiskArbitration callbacks, debounced scans, mount table polling |
| `disks.rs` | NTFS discovery via `diskutil … -plist`, mount state classification |
| `mounts.rs` | kernel mount table via `getfsstat(2)` |
| `ops.rs` | mount / unmount operations with pre- and post-checks |
| `privileged.rs` | the root shell scripts and the `osascript` runner |
| `worker.rs` | serial operation thread |
| `ui.rs` | alerts, notifications, file chooser (out of process via `osascript`) |
| `menu.rs`, `icon.rs` | menu model and procedurally drawn status icon |
| `settings.rs`, `login.rs`, `deps.rs` | preferences, LaunchAgent, dependency checks |

NTFS-3G options match Mounty's (`local`, `auto_xattr`, `windows_names`,
`streams_interface=openxattr`, `noatime`, `uid`/`gid`, …), plus `norecover`.
