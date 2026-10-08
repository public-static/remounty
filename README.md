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

Install ntfs-3g and macFUSE with [MacPorts](https://www.macports.org):

```sh
sudo port install macfuse +fs_link ntfs-3g +extra_progs
```

* `+fs_link` (macfuse) creates the `/Library/Filesystems/macfuse.fs` link that
  macOS needs to find macFUSE.
* `+extra_progs` (ntfs-3g) also installs the optional NTFS utilities that are
  not built by default: `ntfsck` (consistency check), `ntfsdecrypt`,
  `ntfsdump_logfile`, `ntfsfallocate`, `ntfsmftalloc`, `ntfsmove`,
  `ntfstruncate` and `ntfswipe`. Remounty itself only uses `ntfs-3g`, so the
  variant is optional; the extras are handy for inspecting or repairing a
  volume by hand. Several of them modify the disk directly, so only use them
  on unmounted volumes you have a backup of. After installing macFUSE, allow its system extension in *System
Settings → Privacy & Security* and restart if asked.

Remounty only runs ntfs-3g and macFUSE (as root) when **nobody but root can
modify them**. MacPorts installs them that way under `/opt/local`. Homebrew
doesn't: its files belong to your user account, so any program you run could
replace them. Remounty refuses such installations and explains why. It looks
for `ntfs-3g` at `/opt/local/bin/ntfs-3g`; use *Locate ntfs-3g…* for another
root-owned location.

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
  mount in `/Volumes/<name>`. The volume shows up in the Finder sidebar.
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

## The privileged helper

Mounting needs root. The first time you mount read-write, Remounty offers to
install a small helper, once, with the standard macOS administrator dialog:

| What | Where |
|------|-------|
| the helper | `/Library/PrivilegedHelperTools/remounty/` (owned by root) |
| a sudoers rule that lets you start **only** this helper | `/etc/sudoers.d/remounty` (checked with `visudo`) |
| two authorization rules | `app.remounty.mount`, `app.remounty.admin` |

After that, every mount and unmount shows the **macOS authorization dialog**.
The helper itself asks macOS for your approval, so another program that
starts the helper directly gets the same dialog and cannot skip it.
*Helper → Remember Authorization* sets how long macOS remembers your approval
for Remounty: never, 5 minutes, 1 hour or until logout. macOS keeps that
approval inside Remounty's own session (`shared = false`), so other programs
can't reuse it. Remounty never sees or stores your password. Touch ID isn't
offered, because macOS reserves it in that dialog for Apple's own apps.
*Helper → Uninstall Helper…* removes everything again.

**Why a helper?**

* **Removable disks.** Commands started through the standard administrator
  dialog aren't attributed to Remounty, so macOS's Removable Volumes
  protection stops `ntfs-3g` from opening USB disks ("Operation not
  permitted"). The helper is started with `sudo` as Remounty's child process,
  so Remounty's permission applies. On first use macOS asks whether Remounty
  may access removable volumes; answer **Allow**.
* **Only root-controlled code runs as root.** Before every mount the helper
  checks that `ntfs-3g`, every non-system library it loads (followed
  recursively, including all `@rpath` / `@loader_path` search locations),
  the folder it loads plugins from, and the macFUSE bundle are root-owned and
  writable only by root. That includes each file, every folder up to `/`, and
  the targets of any symlinks. A location that doesn't exist must be one only
  root could create. If anything fails, the helper refuses and tells you
  what. Because only root can change files that pass this check, nothing can
  swap them between the check and their use.
* **Mount points.** Mount points are created by the helper in `/Volumes`,
  which only root can modify, so no other program can swap a mount point for
  a link to a system folder.

**Updates.** When you update ntfs-3g or macFUSE with MacPorts, nothing needs
to be done: the helper checks the installed files before every mount. After
Remounty itself is updated, the menu offers *Update Helper…*.

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
  must still be the same volume (matched by UUID, which the helper checks
  again) and in the expected state. Stale menu clicks are harmless.
* **Verifies after acting.** A mount is confirmed through the kernel mount
  table, including that it really is writable.
* **Restores after failure.** If a re-mount fails after the read-only mount was
  removed, the read-only mount is put back.
* **The helper trusts nothing.** Every request needs a macOS authorization.
  Arguments are validated strictly: device names must match `diskN[sN…]`,
  and volume labels are cleaned before going into `-o volname=` so they can't
  inject mount options. `ntfs-3g` runs with an empty environment and no shell
  is involved. The helper can't be reinstalled through its own sudoers rule.
* **Only runs protected code as root.** See *The privileged helper* above.
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
| `lib.rs` | startup, single instance, `tao` event loop (the app's `main.rs` just calls it) |
| `helper.rs`, `helper_install.rs` | the privileged helper (`remounty-helper`): mount, unmount, cleanup, install, uninstall |
| `helper_client.rs`, `helper_proto.rs` | app side of the helper; shared paths and exit codes |
| `trust.rs` | checks that ntfs-3g, its libraries and macFUSE can only be modified by root |
| `authz.rs` | Authorization Services (session in the app, check in the helper) |
| `naming.rs` | turning volume labels into safe mount options and folder names |
| `app.rs` | state and event handling on the main thread (menu, icon, queue, prompts) |
| `watcher.rs` | DiskArbitration callbacks, debounced scans, mount table polling |
| `disks.rs` | NTFS discovery via `diskutil … -plist`, mount state classification |
| `mounts.rs` | kernel mount table via `getfsstat(2)` |
| `ops.rs` | mount / unmount operations with pre- and post-checks |
| `privileged.rs` | the macOS administrator dialog (used to install the helper) |
| `worker.rs` | serial operation thread |
| `ui.rs` | alerts, notifications, file chooser (out of process via `osascript`) |
| `menu.rs`, `icon.rs` | menu model and procedurally drawn status icon |
| `settings.rs`, `login.rs`, `deps.rs` | preferences, LaunchAgent, dependency checks |

NTFS-3G options match Mounty's (`local`, `auto_xattr`, `windows_names`,
`streams_interface=openxattr`, `noatime`, `uid`/`gid`, …), plus `norecover`.
