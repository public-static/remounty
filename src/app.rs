//! Application state and event handling. Lives on the main thread; all slow
//! work (scanning, mounting, dialogs) happens on background threads that
//! report back through [`AppEvent`]s.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::deps::{self, Dependencies};
use crate::disks::{self, MountState, Scan, Volume, VolumeKey};
use crate::error::{Error, Result};
use crate::icon::{self, Activity, IconState};
use crate::menu::{self, Action, MenuModel, VolumeRow};
use crate::ops::{self, Operation, Report};
use crate::settings::{Settings, Store};
use crate::ui::{self, Answer, Level, UiQueue};
use crate::watcher::{self, Scanner};
use crate::worker::Worker;
use crate::{finder, login, notify, paths, privileged};

pub type Sink = Arc<dyn Fn(AppEvent) + Send + Sync>;

#[derive(Debug)]
pub enum AppEvent {
    Menu(String),
    Scanned(Result<Scan>),
    OpFinished(Operation, Report),
    NoticeAnswered(NoticeAnswer),
    NewVolumeAnswered {
        key: VolumeKey,
        name: String,
        choice: NewVolumeChoice,
    },
    Ntfs3gChosen(Option<PathBuf>),
    /// A notification about this location was clicked.
    OpenPath(PathBuf),
    TouchIdSetupConfirmed(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeAnswer {
    Accepted,
    Quit,
    Dismissed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewVolumeChoice {
    Remount,
    Always,
    NotNow,
}

const BTN_UNDERSTAND: &str = "OK, I Understand";
const BTN_QUIT: &str = "Quit Remounty";
const BTN_REMOUNT: &str = "Re-mount";
const BTN_ALWAYS: &str = "Always Re-mount";
const BTN_NOT_NOW: &str = "Not Now";
const BTN_ENABLE: &str = "Enable Touch ID";
const BTN_CANCEL: &str = "Cancel";

const NOTICE_TITLE: &str = "Important Notice — Please Read Carefully";
const NOTICE_TEXT: &str = "Remounty re-mounts NTFS volumes with write access using the free \
NTFS-3G driver and macFUSE, which you need to install separately.\n\n\
Writing to NTFS from macOS relies on third-party software. Remounty is careful — it never \
forces an unmount, refuses volumes that Windows left hibernated or not cleanly unmounted, \
and verifies every step — but data loss can never be ruled out completely.\n\n\
• Always keep a good backup of your data.\n\
• Always eject volumes before disconnecting them.\n\
• Turn off Windows Fast Startup for disks you share with Windows.\n\n\
Using Remounty is at your own risk.";

const HELP_TITLE: &str = "Remounty — Help";

pub struct App {
    tray: TrayIcon,
    sink: Sink,
    ui: UiQueue,
    scanner: Scanner,
    worker: Worker,
    store: Store,
    settings: Settings,
    deps: Dependencies,
    mount_root: Option<PathBuf>,

    scan: Scan,
    scan_error: Option<String>,
    scanned_once: bool,

    running: Option<Operation>,
    queue: VecDeque<Operation>,

    /// Volumes present in the previous scan.
    known: HashSet<VolumeKey>,
    /// Newly seen volumes waiting to become read-only-mounted, mapped to
    /// whether the user may be asked about them (false for volumes that
    /// were already attached when Remounty started).
    pending: HashMap<VolumeKey, bool>,
    /// Volumes with an open "re-mount?" question.
    asking: HashSet<VolumeKey>,
    notice_open: bool,
    last_icon: Option<IconState>,
    /// Stale mounts the user has already been warned about.
    warned_orphans: HashSet<PathBuf>,
    /// Errors already shown once (so a persistent failure does not flood
    /// the user with alerts every few seconds).
    scan_error_reported: bool,
    menu_error_reported: bool,
}

impl App {
    pub fn start(sink: Sink) -> Result<Self> {
        let store = Store::new();
        let settings = store.load();
        let deps = deps::detect(settings.ntfs3g_path.as_deref());
        let mount_root = paths::mount_root();
        if let Some(root) = &mount_root {
            ops::clean_stale_mount_points(root);
        }
        login::refresh();
        log_dependencies(&deps);
        {
            let sink = sink.clone();
            notify::init(move |path| sink(AppEvent::OpenPath(path)));
        }

        let initial_icon = IconState {
            activity: Activity::Idle,
            mounted_by_us: false,
        };
        let tray = TrayIconBuilder::new()
            .with_icon(make_icon(initial_icon)?)
            .with_icon_as_template(false)
            .with_tooltip(paths::APP_NAME)
            .with_menu_on_left_click(true)
            .build()
            .map_err(|err| Error::new(format!("Could not create the menu bar item: {err}")))?;

        let ui = UiQueue::start()?;
        let worker = {
            let sink = sink.clone();
            Worker::start(move |op, report| sink(AppEvent::OpFinished(op, report)))?
        };
        let scanner = {
            let sink = sink.clone();
            watcher::start(mount_root.clone(), move |result| sink(AppEvent::Scanned(result)))?
        };

        let mut app = Self {
            tray,
            sink,
            ui,
            scanner,
            worker,
            store,
            settings,
            deps,
            mount_root,
            scan: Scan::default(),
            scan_error: None,
            scanned_once: false,
            running: None,
            queue: VecDeque::new(),
            known: HashSet::new(),
            pending: HashMap::new(),
            asking: HashSet::new(),
            notice_open: false,
            last_icon: Some(initial_icon),
            warned_orphans: HashSet::new(),
            scan_error_reported: false,
            menu_error_reported: false,
        };
        if !app.settings.disclaimer_accepted {
            app.show_notice();
        }
        if let Some(problem) = app.deps.problem() {
            app.ui.show(
                Level::Warning,
                format!("Remounty cannot re-mount volumes yet: {problem}"),
                format!(
                    "Remounty needs macFUSE and NTFS-3G to write to NTFS volumes. {}",
                    deps::INSTALL_HINT
                ),
            );
        }
        app.refresh();
        Ok(app)
    }

    /// Handles one event. Returns `true` when the app should quit.
    pub fn handle(&mut self, event: AppEvent) -> bool {
        let quit = match event {
            AppEvent::Menu(id) => match Action::from_id(&id) {
                Some(action) => self.on_action(action),
                None => {
                    crate::log_warn!("Ignoring unknown menu item {id:?}");
                    false
                }
            },
            AppEvent::Scanned(result) => {
                self.on_scan(result);
                false
            }
            AppEvent::OpFinished(op, report) => {
                self.on_op_finished(op, report);
                false
            }
            AppEvent::NoticeAnswered(answer) => self.on_notice_answer(answer),
            AppEvent::NewVolumeAnswered { key, name, choice } => {
                self.on_new_volume_answer(key, &name, choice);
                false
            }
            AppEvent::Ntfs3gChosen(path) => {
                self.on_ntfs3g_chosen(path);
                false
            }
            AppEvent::OpenPath(path) => {
                self.open_path(&path);
                false
            }
            AppEvent::TouchIdSetupConfirmed(confirmed) => {
                if confirmed {
                    self.enqueue(Operation::SetUpTouchId);
                }
                false
            }
        };
        if !quit {
            self.refresh();
        }
        quit
    }

    // -----------------------------------------------------------------------
    // Scans and newly attached volumes

    fn on_scan(&mut self, result: Result<Scan>) {
        self.deps = deps::detect(self.settings.ntfs3g_path.as_deref());
        let scan = match result {
            Ok(scan) => scan,
            Err(err) => {
                crate::log_error!("Scan failed: {err}");
                if !self.scan_error_reported {
                    self.scan_error_reported = true;
                    self.ui.show(
                        Level::Warning,
                        "Remounty cannot read the list of disks",
                        format!(
                            "{err}\n\nThe menu may be out of date. Remounty keeps retrying \
                             and will tell you nothing more about this until it works again."
                        ),
                    );
                }
                self.scan_error = Some(err.to_string());
                return;
            }
        };
        if self.scan_error_reported {
            self.scan_error_reported = false;
            notify::post(paths::APP_NAME, "Reading the list of disks works again.", None);
        }
        self.scan_error = None;
        self.warn_about_orphans(&scan);
        let current: HashSet<VolumeKey> = scan.volumes.iter().map(Volume::key).collect();
        for key in &current {
            if !self.known.contains(key) {
                let vol = scan.find(key);
                crate::log_info!(
                    "Found {} ({}) — {}",
                    vol.map(Volume::display_name).unwrap_or_default(),
                    key.bsd_name,
                    vol.map(|v| menu::describe_state(&v.state)).unwrap_or_default()
                );
                self.pending.insert(key.clone(), self.scanned_once);
            }
        }
        self.pending.retain(|k, _| current.contains(k));
        self.asking.retain(|k| current.contains(k));
        self.known = current;
        self.scan = scan;
        self.scanned_once = true;
        // Volumes unmounted elsewhere (e.g. ejected in Finder) leave an empty
        // mount point behind. Only tidy up while no operation is running, so
        // this can never race with a mount point that is about to be used.
        if self.running.is_none()
            && self.queue.is_empty()
            && let Some(root) = &self.mount_root
        {
            ops::clean_stale_mount_points(root);
        }
        self.process_pending();
    }

    fn process_pending(&mut self) {
        if !self.settings.disclaimer_accepted {
            return;
        }
        let pending: Vec<(VolumeKey, bool)> = self.pending.iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (key, may_ask) in pending {
            let Some(vol) = self.scan.find(&key).cloned() else {
                self.pending.remove(&key);
                continue;
            };
            match vol.state {
                MountState::Unmounted => continue, // wait for macOS to mount it
                MountState::Native { read_only: true, .. } => {}
                _ => {
                    self.pending.remove(&key);
                    continue;
                }
            }
            self.pending.remove(&key);
            let name = vol.name.clone();
            if self.settings.is_automount(vol.identity.as_deref()) {
                if self.deps.ready() {
                    crate::log_info!("Automatically re-mounting {name}");
                    self.enqueue(Operation::MountReadWrite {
                        key,
                        name,
                        label: vol.label.clone(),
                    });
                } else {
                    let problem = self.deps.problem().unwrap_or_default();
                    self.ui.show(
                        Level::Warning,
                        format!("“{name}” cannot be re-mounted automatically"),
                        format!("{problem}.\n\n{}", deps::INSTALL_HINT),
                    );
                }
            } else if may_ask && self.settings.ask_on_attach && self.deps.ready() {
                self.ask_about_new_volume(&vol);
            }
        }
    }

    fn ask_about_new_volume(&mut self, vol: &Volume) {
        let key = vol.key();
        if !self.asking.insert(key.clone()) {
            return;
        }
        let name = vol.name.clone();
        let title = format!("“{name}” detected");
        let message = format!(
            "The NTFS volume “{name}” ({}, {}) is mounted read-only.\n\nRe-mount it read-write?",
            disks::format_size(vol.size),
            vol.media
        );
        let can_always = vol.identity.is_some();
        let sink = self.sink.clone();
        self.ui.submit(move || {
            let buttons: Vec<&str> = if can_always {
                vec![BTN_REMOUNT, BTN_ALWAYS, BTN_NOT_NOW]
            } else {
                vec![BTN_REMOUNT, BTN_NOT_NOW]
            };
            let choice = match ui::alert(Level::Informational, &title, &message, &buttons, 120) {
                Answer::Button(b) if b == BTN_REMOUNT => NewVolumeChoice::Remount,
                Answer::Button(b) if b == BTN_ALWAYS => NewVolumeChoice::Always,
                _ => NewVolumeChoice::NotNow,
            };
            sink(AppEvent::NewVolumeAnswered { key, name, choice });
        });
    }

    fn on_new_volume_answer(&mut self, key: VolumeKey, name: &str, choice: NewVolumeChoice) {
        self.asking.remove(&key);
        if choice == NewVolumeChoice::NotNow {
            return;
        }
        if choice == NewVolumeChoice::Always
            && let Some(identity) = &key.identity
        {
            self.settings.set_automount(identity, name, true);
            self.save_settings();
        }
        let Some(vol) = self.scan.find(&key).cloned() else {
            notify::post(
                &format!("“{name}” is no longer attached"),
                "Nothing was changed.",
                None,
            );
            return;
        };
        self.enqueue(Operation::MountReadWrite {
            key,
            name: vol.name,
            label: vol.label,
        });
    }

    /// Warns once about ntfs-3g mounts whose disk vanished without being
    /// unmounted (e.g. the cable was pulled).
    fn warn_about_orphans(&mut self, scan: &Scan) {
        let current: HashSet<PathBuf> = scan.orphans.iter().map(|o| o.on.clone()).collect();
        for path in current.difference(&self.warned_orphans) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            crate::log_warn!("Disk of {} disappeared while mounted", path.display());
            self.ui.show(
                Level::Critical,
                format!("“{name}” was disconnected without being unmounted"),
                "The disk disappeared while it was mounted with write access. Changes made just \
                 before that may be lost and the volume may need checking (“chkdsk /f” on \
                 Windows).\n\nUse “Clean Up Mount” in the Remounty menu to remove the leftover \
                 mount. Always unmount or eject volumes before disconnecting them.",
            );
        }
        self.warned_orphans = current;
    }

    // -----------------------------------------------------------------------
    // Operations

    fn enqueue(&mut self, op: Operation) {
        let duplicate =
            self.running
                .iter()
                .chain(self.queue.iter())
                .any(|queued| match (queued.key(), op.key()) {
                    (Some(a), Some(b)) => a == b,
                    _ => *queued == op,
                });
        if duplicate {
            crate::log_info!("Ignoring duplicate request: {op:?}");
            return;
        }
        self.queue.push_back(op);
        self.start_next();
    }

    fn start_next(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(op) = self.queue.pop_front() else {
            return;
        };
        match self.worker.submit(
            op.clone(),
            self.deps.clone(),
            self.mount_root.clone(),
            self.settings.use_touch_id,
        ) {
            Ok(()) => self.running = Some(op),
            Err(err) => {
                crate::log_error!("{err}");
                self.queue.clear();
                self.ui.show(
                    Level::Critical,
                    "Remounty cannot perform operations",
                    format!("{err}. Please restart Remounty."),
                );
            }
        }
    }

    fn on_op_finished(&mut self, op: Operation, report: Report) {
        if self.running.as_ref() == Some(&op) {
            self.running = None;
        }
        match report {
            Report::Done { message, open_path } => notify::post(paths::APP_NAME, &message, open_path),
            Report::Canceled => crate::log_info!("Canceled by user: {op:?}"),
            Report::Failed { title, detail } => {
                crate::log_error!("{title}: {detail}");
                self.ui.show(Level::Warning, title, detail);
            }
        }
        self.scanner.request_scan();
        self.start_next();
    }

    // -----------------------------------------------------------------------
    // Menu actions

    fn on_action(&mut self, action: Action) -> bool {
        match action {
            Action::MountReadWrite(key) => self.volume_op(&key, |key, name, label| {
                Operation::MountReadWrite { key, name, label }
            }),
            Action::Unmount(key) => {
                self.volume_op(&key, |key, name, label| Operation::Unmount { key, name, label })
            }
            Action::MountReadOnly(key) => self.volume_op(&key, |key, name, label| Operation::MountReadOnly {
                key,
                name,
                label,
            }),
            Action::UnmountStale(path) => {
                if self.require_notice() {
                    self.enqueue(Operation::UnmountStale { path });
                }
            }
            Action::ShowInFinder(key) => {
                match self
                    .scan
                    .find(&key)
                    .and_then(|v| v.state.path())
                    .map(Path::to_path_buf)
                {
                    Some(path) => self.open_path(&path),
                    None => self.ui.show(
                        Level::Informational,
                        "The volume is not mounted",
                        "It cannot be shown in Finder. Rescanning…",
                    ),
                }
                self.scanner.request_scan();
            }
            Action::ToggleAutomount(key) => self.toggle_automount(&key),
            Action::Rescan => self.scanner.request_scan(),
            Action::ToggleAskOnAttach => {
                self.settings.ask_on_attach = !self.settings.ask_on_attach;
                self.save_settings();
            }
            Action::ToggleTouchId => {
                self.settings.use_touch_id = !self.settings.use_touch_id;
                self.save_settings();
            }
            Action::SetUpTouchId => self.confirm_touch_id_setup(),
            Action::ToggleStartAtLogin => {
                let enable = !login::is_enabled();
                if let Err(err) = login::set_enabled(enable) {
                    self.ui
                        .show(Level::Warning, "Could not change the login item", err.to_string());
                }
            }
            Action::LocateNtfs3g => {
                let sink = self.sink.clone();
                self.ui.submit(move || {
                    let chosen = ui::choose_file(
                        "Choose the ntfs-3g executable (for example /opt/homebrew/bin/ntfs-3g)",
                        Path::new("/opt/homebrew/bin"),
                    );
                    sink(AppEvent::Ntfs3gChosen(chosen));
                });
            }
            Action::Help => self.show_help(),
            Action::ShowNotice => self.show_notice(),
            Action::OpenLog => match crate::logging::log_path() {
                Some(path) => finder::open_with_console(&path, self.ui.clone()),
                None => self.ui.show(
                    Level::Warning,
                    "No log file",
                    "Logging is not available because the log directory could not be created.",
                ),
            },
            Action::Quit => return self.try_quit(),
        }
        false
    }

    fn volume_op(&mut self, key: &VolumeKey, make: impl FnOnce(VolumeKey, String, String) -> Operation) {
        if !self.require_notice() {
            return;
        }
        match self.scan.find(key) {
            Some(vol) => {
                let op = make(key.clone(), vol.name.clone(), vol.label.clone());
                self.enqueue(op);
            }
            None => {
                self.ui.show(
                    Level::Warning,
                    "The volume is not available",
                    "The disk may have been disconnected. Nothing was changed.",
                );
                self.scanner.request_scan();
            }
        }
    }

    fn open_path(&self, path: &Path) {
        finder::open_folder(path, self.ui.clone());
    }

    fn confirm_touch_id_setup(&mut self) {
        if privileged::sudo_local_exists() {
            // Let the operation explain why it cannot proceed.
            self.enqueue(Operation::SetUpTouchId);
            return;
        }
        let sink = self.sink.clone();
        self.ui.submit(move || {
            let answer = ui::alert(
                Level::Informational,
                "Use Touch ID instead of typing your password?",
                "macOS only offers Touch ID in the standard password dialog to Apple's own apps. \
                 Remounty can use Touch ID through “sudo” instead, which requires enabling Touch \
                 ID for sudo once. This is Apple's supported way: Remounty creates the file \
                 /etc/pam.d/sudo_local containing one line (auth sufficient pam_tid.so), and macOS \
                 keeps it across updates.\n\n\
                 Note: this also enables Touch ID for “sudo” in Terminal. Your password keeps \
                 working everywhere. To undo it, delete /etc/pam.d/sudo_local.\n\n\
                 You will be asked for your password one last time to make this change.",
                &[BTN_ENABLE, BTN_CANCEL],
                ui::DEFAULT_GIVE_UP,
            );
            let confirmed = answer == Answer::Button(BTN_ENABLE.to_string());
            sink(AppEvent::TouchIdSetupConfirmed(confirmed));
        });
    }

    fn toggle_automount(&mut self, key: &VolumeKey) {
        if !self.require_notice() {
            return;
        }
        let Some(identity) = key.identity.clone() else {
            return;
        };
        let name = self.scan.find(key).map(|v| v.name.clone()).unwrap_or_default();
        let enable = !self.settings.is_automount(Some(&identity));
        self.settings.set_automount(&identity, &name, enable);
        self.save_settings();
    }

    fn try_quit(&mut self) -> bool {
        if self.running.is_some() || !self.queue.is_empty() {
            self.ui.show(
                Level::Informational,
                "Remounty is busy",
                "Please wait until the current operation has finished, then quit again.",
            );
            return false;
        }
        crate::log_info!("Quit requested");
        true
    }

    fn on_ntfs3g_chosen(&mut self, path: Option<PathBuf>) {
        let Some(path) = path else {
            return;
        };
        match deps::validate_ntfs3g(&path) {
            Ok(valid) => {
                self.settings.ntfs3g_path = Some(valid);
                self.save_settings();
                self.deps = deps::detect(self.settings.ntfs3g_path.as_deref());
                log_dependencies(&self.deps);
                self.process_pending();
            }
            Err(err) => self
                .ui
                .show(Level::Warning, "This is not a usable ntfs-3g", err.to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // Notice & help

    fn require_notice(&mut self) -> bool {
        if self.settings.disclaimer_accepted {
            return true;
        }
        self.show_notice();
        false
    }

    fn show_notice(&mut self) {
        if self.notice_open {
            return;
        }
        self.notice_open = true;
        let sink = self.sink.clone();
        self.ui.submit(move || {
            let answer = match ui::alert(
                Level::Warning,
                NOTICE_TITLE,
                NOTICE_TEXT,
                &[BTN_UNDERSTAND, BTN_QUIT],
                ui::DEFAULT_GIVE_UP,
            ) {
                Answer::Button(b) if b == BTN_UNDERSTAND => NoticeAnswer::Accepted,
                Answer::Button(b) if b == BTN_QUIT => NoticeAnswer::Quit,
                _ => NoticeAnswer::Dismissed,
            };
            sink(AppEvent::NoticeAnswered(answer));
        });
    }

    fn on_notice_answer(&mut self, answer: NoticeAnswer) -> bool {
        self.notice_open = false;
        match answer {
            NoticeAnswer::Accepted => {
                if !self.settings.disclaimer_accepted {
                    self.settings.disclaimer_accepted = true;
                    self.save_settings();
                }
                self.process_pending();
                false
            }
            NoticeAnswer::Quit => self.try_quit(),
            NoticeAnswer::Dismissed => false,
        }
    }

    fn show_help(&self) {
        let deps_line = match (&self.deps.ntfs3g, self.deps.macfuse) {
            (Ok(path), true) => format!("✓ ntfs-3g: {}\n✓ macFUSE installed", path.display()),
            _ => format!(
                "⚠︎ {}\n\nRemounty needs macFUSE and NTFS-3G. {}",
                self.deps.problem().unwrap_or_default(),
                deps::INSTALL_HINT
            ),
        };
        let root = self
            .mount_root
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.remounty".into());
        let text = format!(
            "Remounty lists every attached NTFS volume — including those that were attached \
             before it started — and re-mounts them read-write with NTFS-3G.\n\n\
             {deps_line}\n\n\
             Writable volumes are mounted in {root} and appear in the Finder sidebar. \
             You will be asked for an administrator password for each mount; Remounty never \
             stores it.\n\n\
             If a volume is refused, it was usually hibernated by Windows (Fast Startup) or \
             not ejected safely. Shut Windows down fully or run “chkdsk /f” there first.\n\n\
             Menu bar icon:\n\
             • left half orange — a volume can be re-mounted\n\
             • left half green — working\n\
             • right half blue — a volume is writable through Remounty\n\n\
             {NOTICE_TEXT}"
        );
        self.ui.show(Level::Informational, HELP_TITLE, text);
    }

    // -----------------------------------------------------------------------
    // Presentation

    fn save_settings(&mut self) {
        if let Err(err) = self.store.save(&self.settings) {
            crate::log_error!("{err}");
            self.ui
                .show(Level::Warning, "Settings could not be saved", err.to_string());
        }
    }

    fn icon_state(&self) -> IconState {
        // Unmounted volumes do not count: the user (or Finder) unmounted
        // them on purpose, so the icon should not nag about them.
        let actionable = self.deps.ready()
            && self
                .scan
                .volumes
                .iter()
                .any(|v| matches!(v.state, MountState::Native { read_only: true, .. }));
        let activity = if self.running.is_some() {
            Activity::Working
        } else if actionable {
            Activity::Available
        } else {
            Activity::Idle
        };
        let mounted_by_us = self.scan.volumes.iter().any(|v| {
            matches!(
                v.state,
                MountState::Fuse {
                    ours: true,
                    read_only: false,
                    ..
                }
            )
        });
        IconState {
            activity,
            mounted_by_us,
        }
    }

    fn status_line(&self) -> String {
        if let Some(op) = &self.running {
            let queued = self.queue.len();
            return if queued > 0 {
                format!("{} ({queued} more queued)", op.describe())
            } else {
                op.describe()
            };
        }
        if self.scan_error.is_some() {
            return "⚠︎ Scanning disks failed — see the log".into();
        }
        if !self.scanned_once {
            return "Scanning disks…".into();
        }
        let count = |f: fn(&MountState) -> bool| self.scan.volumes.iter().filter(|v| f(&v.state)).count();
        let read_only = count(|s| matches!(s, MountState::Native { read_only: true, .. }));
        let writable = count(|s| matches!(s, MountState::Fuse { read_only: false, .. }));
        match (read_only, writable) {
            (0, 0) if self.scan.volumes.is_empty() => "No NTFS volumes attached".into(),
            (0, 0) => format!("{} NTFS volume(s) attached", self.scan.volumes.len()),
            (r, 0) => format!("{r} volume(s) can be re-mounted read-write"),
            (0, w) => format!("{w} volume(s) writable"),
            (r, w) => format!("{w} writable, {r} read-only"),
        }
    }

    fn refresh(&mut self) {
        let running_key = self.running.as_ref().and_then(Operation::key);
        let volumes = self
            .scan
            .volumes
            .iter()
            .map(|v| VolumeRow {
                working: running_key == Some(&v.key()),
                automount: self.settings.is_automount(v.identity.as_deref()),
                volume: v.clone(),
            })
            .collect();
        let model = MenuModel {
            status: self.status_line(),
            problem: self.deps.problem(),
            volumes,
            stale: self.scan.orphans.iter().map(|o| o.on.clone()).collect(),
            busy: self.running.is_some(),
            deps_ready: self.deps.ready(),
            notice_accepted: self.settings.disclaimer_accepted,
            ask_on_attach: self.settings.ask_on_attach,
            start_at_login: login::is_enabled(),
            ntfs3g_missing: self.deps.ntfs3g.is_err(),
            touch_id_configured: privileged::touch_id_configured(),
            use_touch_id: self.settings.use_touch_id,
        };
        match menu::build(&model) {
            Ok(built) => {
                menu::style_headers(&built);
                self.tray.set_menu(Some(Box::new(built.menu)));
                self.menu_error_reported = false;
            }
            Err(err) => {
                crate::log_error!("{err}");
                if !self.menu_error_reported {
                    self.menu_error_reported = true;
                    self.ui.show(
                        Level::Warning,
                        "The Remounty menu could not be updated",
                        format!("{err}\n\nThe menu may show outdated information."),
                    );
                }
            }
        }
        if let Err(err) = self
            .tray
            .set_tooltip(Some(format!("{} — {}", paths::APP_NAME, model.status)))
        {
            crate::log_warn!("Could not set tooltip: {err}");
        }
        let state = self.icon_state();
        if self.last_icon != Some(state) {
            match make_icon(state).and_then(|icon| {
                self.tray
                    .set_icon(Some(icon))
                    .map_err(|err| Error::new(format!("Setting icon: {err}")))
            }) {
                Ok(()) => self.last_icon = Some(state),
                Err(err) => crate::log_error!("{err}"),
            }
        }
    }
}

fn make_icon(state: IconState) -> Result<Icon> {
    Icon::from_rgba(icon::render(state), icon::WIDTH, icon::HEIGHT)
        .map_err(|err| Error::new(format!("Creating icon: {err}")))
}

fn log_dependencies(deps: &Dependencies) {
    match &deps.ntfs3g {
        Ok(path) => crate::log_info!("ntfs-3g: {}", path.display()),
        Err(reason) => crate::log_warn!("ntfs-3g: {reason}"),
    }
    crate::log_info!("macFUSE installed: {}", deps.macfuse);
}
