//! Background threads that notice disk changes and produce fresh scans.
//!
//! * The DiskArbitration thread runs its own CFRunLoop and forwards
//!   appear / disappear / description-changed callbacks as bare triggers.
//! * The scanner thread debounces triggers, additionally polls the (cheap)
//!   kernel mount table, and runs full scans off the main thread.
//!
//! If DiskArbitration is unavailable, polling alone keeps the app working.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use core_foundation_sys::array::CFArrayRef;
use core_foundation_sys::base::CFAllocatorRef;
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::runloop::{CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRun, kCFRunLoopDefaultMode};
use core_foundation_sys::string::CFStringRef;

use crate::disks::{self, Scan};
use crate::error::Result;
use crate::mounts;

const DEBOUNCE: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const FULL_SCAN_INTERVAL: Duration = Duration::from_secs(300);

#[repr(C)]
struct OpaqueSession {
    _private: [u8; 0],
}
#[repr(C)]
struct OpaqueDisk {
    _private: [u8; 0],
}
type DASessionRef = *mut OpaqueSession;
type DADiskRef = *mut OpaqueDisk;

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    fn DASessionCreate(allocator: CFAllocatorRef) -> DASessionRef;
    fn DASessionScheduleWithRunLoop(session: DASessionRef, run_loop: CFRunLoopRef, mode: CFStringRef);
    fn DARegisterDiskAppearedCallback(
        session: DASessionRef,
        matching: CFDictionaryRef,
        callback: extern "C" fn(DADiskRef, *mut c_void),
        context: *mut c_void,
    );
    fn DARegisterDiskDisappearedCallback(
        session: DASessionRef,
        matching: CFDictionaryRef,
        callback: extern "C" fn(DADiskRef, *mut c_void),
        context: *mut c_void,
    );
    fn DARegisterDiskDescriptionChangedCallback(
        session: DASessionRef,
        matching: CFDictionaryRef,
        watch: CFArrayRef,
        callback: extern "C" fn(DADiskRef, CFArrayRef, *mut c_void),
        context: *mut c_void,
    );
}

/// Sender used by the C callbacks (they cannot capture state safely).
static DA_TRIGGER: OnceLock<Sender<Trigger>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    DiskEvent,
    Requested,
}

extern "C" fn on_disk_event(_disk: DADiskRef, _context: *mut c_void) {
    if let Some(tx) = DA_TRIGGER.get() {
        let _ = tx.send(Trigger::DiskEvent);
    }
}

extern "C" fn on_disk_changed(_disk: DADiskRef, _keys: CFArrayRef, _context: *mut c_void) {
    if let Some(tx) = DA_TRIGGER.get() {
        let _ = tx.send(Trigger::DiskEvent);
    }
}

fn start_disk_arbitration(tx: Sender<Trigger>) {
    if DA_TRIGGER.set(tx).is_err() {
        crate::log_warn!("DiskArbitration watcher already running");
        return;
    }
    let spawned = thread::Builder::new().name("disk-arbitration".into()).spawn(|| {
        // SAFETY: plain CoreFoundation / DiskArbitration calls on this
        // thread's own run loop. The session is intentionally kept alive
        // for the lifetime of the process; callbacks only touch a static.
        unsafe {
            let session = DASessionCreate(std::ptr::null());
            if session.is_null() {
                crate::log_error!("DASessionCreate failed; falling back to polling only");
                return;
            }
            let null_match: CFDictionaryRef = std::ptr::null();
            DARegisterDiskAppearedCallback(session, null_match, on_disk_event, std::ptr::null_mut());
            DARegisterDiskDisappearedCallback(session, null_match, on_disk_event, std::ptr::null_mut());
            DARegisterDiskDescriptionChangedCallback(
                session,
                null_match,
                std::ptr::null(),
                on_disk_changed,
                std::ptr::null_mut(),
            );
            DASessionScheduleWithRunLoop(session, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            crate::log_info!("Watching disks via DiskArbitration");
            CFRunLoopRun();
        }
        crate::log_warn!("DiskArbitration run loop exited");
    });
    if let Err(err) = spawned {
        crate::log_error!("Could not start DiskArbitration thread: {err}");
    }
}

/// Handle used to request a rescan.
#[derive(Clone)]
pub struct Scanner {
    tx: Sender<Trigger>,
}

impl Scanner {
    pub fn request_scan(&self) {
        if self.tx.send(Trigger::Requested).is_err() {
            crate::log_error!("Scanner thread is not running");
        }
    }
}

/// Starts watching; `on_scan` is called (on the scanner thread) with every
/// scan result, starting with an immediate initial scan of all attached disks.
pub fn start(
    mount_root: Option<PathBuf>,
    on_scan: impl Fn(Result<Scan>) + Send + 'static,
) -> Result<Scanner> {
    let (tx, rx) = mpsc::channel();
    start_disk_arbitration(tx.clone());
    thread::Builder::new()
        .name("scanner".into())
        .spawn(move || scanner_loop(rx, mount_root, on_scan))
        .map_err(|err| crate::error::Error::new(format!("Could not start scanner thread: {err}")))?;
    let scanner = Scanner { tx };
    scanner.request_scan();
    Ok(scanner)
}

fn scanner_loop(rx: Receiver<Trigger>, mount_root: Option<PathBuf>, on_scan: impl Fn(Result<Scan>)) {
    let mut last_fingerprint: Option<u64> = None;
    let mut last_full_scan = Instant::now();
    loop {
        let should_scan = match rx.recv_timeout(POLL_INTERVAL) {
            Ok(_) => {
                // Disk events arrive in bursts; wait for the burst to settle.
                thread::sleep(DEBOUNCE);
                while rx.try_recv().is_ok() {}
                true
            }
            Err(RecvTimeoutError::Timeout) => {
                let changed = match mounts::snapshot() {
                    Ok(entries) => Some(mounts::fingerprint(&entries)) != last_fingerprint,
                    Err(err) => {
                        crate::log_warn!("{err}");
                        false
                    }
                };
                changed || last_full_scan.elapsed() >= FULL_SCAN_INTERVAL
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if !should_scan {
            continue;
        }
        last_fingerprint = mounts::snapshot().ok().map(|e| mounts::fingerprint(&e));
        last_full_scan = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            disks::scan(mount_root.as_deref())
        }))
        .unwrap_or_else(|_| Err(crate::error::Error::new("Internal error while scanning disks")));
        on_scan(result);
    }
}
