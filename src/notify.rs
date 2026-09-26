//! Native notifications.
//!
//! Notifications are posted as Remounty itself, so clicking one opens the
//! volume it is about instead of Script Editor. Three tiers, best first:
//!
//! 1. UserNotifications framework — needs a properly signed app; macOS
//!    refuses ad-hoc signed builds ("Notifications are not allowed").
//! 2. The older NSUserNotificationCenter — deprecated, but works for any
//!    app bundle, including ad-hoc signed ones.
//! 3. `osascript` (clicks open Script Editor) — only when not running from an
//!    app bundle at all, where the frameworks above are unusable.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, define_class, msg_send};
#[allow(deprecated)]
use objc2_foundation::{
    NSBundle, NSDictionary, NSError, NSString, NSUUID, NSUserNotification, NSUserNotificationCenter,
    NSUserNotificationCenterDelegate,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

const PATH_KEY: &str = "path";

const STATE_UNAVAILABLE: u8 = 0;
const STATE_PENDING: u8 = 1;
const STATE_GRANTED: u8 = 2;
const STATE_DENIED: u8 = 3;
const STATE_LEGACY: u8 = 4;

static STATE: AtomicU8 = AtomicU8::new(STATE_UNAVAILABLE);
static ON_CLICK: OnceLock<Box<dyn Fn(PathBuf) + Send + Sync>> = OnceLock::new();

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RemountyNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler
                .call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            if let (Some(path), Some(on_click)) = (path_from_response(response), ON_CLICK.get()) {
                on_click(path);
            }
            completion_handler.call(());
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RemountyLegacyNotificationDelegate"]
    struct LegacyDelegate;

    unsafe impl NSObjectProtocol for LegacyDelegate {}

    #[allow(deprecated)]
    unsafe impl NSUserNotificationCenterDelegate for LegacyDelegate {
        #[unsafe(method(userNotificationCenter:shouldPresentNotification:))]
        fn should_present(
            &self,
            _center: &NSUserNotificationCenter,
            _notification: &NSUserNotification,
        ) -> bool {
            true
        }

        #[unsafe(method(userNotificationCenter:didActivateNotification:))]
        fn did_activate(&self, center: &NSUserNotificationCenter, notification: &NSUserNotification) {
            if let (Some(path), Some(on_click)) = (path_from_legacy(notification), ON_CLICK.get()) {
                on_click(path);
            }
            center.removeDeliveredNotification(notification);
        }
    }
);

#[allow(deprecated)]
fn path_from_legacy(notification: &NSUserNotification) -> Option<PathBuf> {
    let info = notification.userInfo()?;
    let value = info.objectForKey(&NSString::from_str(PATH_KEY))?;
    let path = PathBuf::from(value.downcast::<NSString>().ok()?.to_string());
    path.is_absolute().then_some(path)
}

/// The legacy center, or `None` where macOS no longer provides it.
#[allow(deprecated)]
fn legacy_center() -> Option<Retained<NSUserNotificationCenter>> {
    // SAFETY: class method without arguments; a nil result is handled.
    unsafe { msg_send![NSUserNotificationCenter::class(), defaultUserNotificationCenter] }
}

fn path_from_response(response: &UNNotificationResponse) -> Option<PathBuf> {
    let info = response.notification().request().content().userInfo();
    let key = NSString::from_str(PATH_KEY);
    let key: &AnyObject = &key;
    let value = info.objectForKey(key)?;
    let path = value.downcast::<NSString>().ok()?.to_string();
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

fn running_in_app_bundle() -> bool {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
}

/// Sets up native notifications. `on_click` receives the path attached to a
/// clicked notification. Must be called once, on the main thread.
pub fn init(on_click: impl Fn(PathBuf) + Send + Sync + 'static) {
    if !running_in_app_bundle() {
        crate::log_info!("Not running from an app bundle; using osascript notifications");
        return;
    }
    if ON_CLICK.set(Box::new(on_click)).is_err() {
        return;
    }
    let legacy_available = match legacy_center() {
        Some(center) => {
            // SAFETY: plain allocation + init of an NSObject subclass without ivars.
            let delegate: Retained<LegacyDelegate> = unsafe { msg_send![LegacyDelegate::class(), new] };
            // SAFETY: the delegate is leaked below, so it outlives the center's
            // (unretained) reference to it.
            #[allow(deprecated)]
            unsafe {
                center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            }
            std::mem::forget(delegate);
            true
        }
        None => false,
    };
    let center = UNUserNotificationCenter::currentNotificationCenter();
    // SAFETY: plain allocation + init of an NSObject subclass without ivars.
    let delegate: Retained<NotificationDelegate> = unsafe { msg_send![NotificationDelegate::class(), new] };
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The notification center only keeps a weak reference; the delegate must
    // live as long as the process.
    std::mem::forget(delegate);

    STATE.store(STATE_PENDING, Ordering::SeqCst);
    let handler = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        // SAFETY: the framework passes either null or a valid NSError.
        if let Some(error) = unsafe { error.as_ref() } {
            let fallback = if legacy_available {
                STATE_LEGACY
            } else {
                STATE_UNAVAILABLE
            };
            crate::log_info!(
                "UserNotifications unavailable ({}); using {}",
                error.localizedDescription(),
                if legacy_available {
                    "NSUserNotificationCenter"
                } else {
                    "osascript"
                }
            );
            STATE.store(fallback, Ordering::SeqCst);
        } else if granted.as_bool() {
            STATE.store(STATE_GRANTED, Ordering::SeqCst);
        } else {
            crate::log_info!("Notifications are turned off for Remounty in System Settings");
            STATE.store(STATE_DENIED, Ordering::SeqCst);
        }
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &handler,
    );
}

/// Posts a notification; clicking it opens `open_path` (if given).
pub fn post(title: &str, body: &str, open_path: Option<PathBuf>) {
    match STATE.load(Ordering::SeqCst) {
        STATE_GRANTED | STATE_PENDING => post_native(title, body, open_path),
        STATE_DENIED => crate::log_info!("Notification (not shown, disabled by user): {title}: {body}"),
        STATE_LEGACY => {
            if !post_legacy(title, body, open_path) {
                post_fallback(title, body);
            }
        }
        _ => post_fallback(title, body),
    }
}

#[allow(deprecated)]
fn post_legacy(title: &str, body: &str, open_path: Option<PathBuf>) -> bool {
    let Some(center) = legacy_center() else {
        return false;
    };
    let notification = NSUserNotification::new();
    notification.setTitle(Some(&NSString::from_str(title)));
    notification.setInformativeText(Some(&NSString::from_str(body)));
    if let Some(path) = open_path {
        let key = NSString::from_str(PATH_KEY);
        let value = NSString::from_str(&path.to_string_lossy());
        let value: &AnyObject = &value;
        let dict: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(&[&*key], &[value]);
        // SAFETY: a dictionary of strings is a valid property list.
        unsafe { notification.setUserInfo(Some(&dict)) };
    }
    center.deliverNotification(&notification);
    true
}

fn post_native(title: &str, body: &str, open_path: Option<PathBuf>) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    if let Some(path) = open_path {
        let key = NSString::from_str(PATH_KEY);
        let value = NSString::from_str(&path.to_string_lossy());
        let dict: Retained<NSDictionary<NSString, NSString>> =
            NSDictionary::from_slices(&[&*key], &[&*value]);
        // SAFETY: an NSDictionary<NSString, NSString> is a valid property list
        // and a valid NSDictionary<AnyObject, AnyObject>.
        unsafe {
            let dict: Retained<NSDictionary> = Retained::cast_unchecked(dict);
            content.setUserInfo(&dict);
        }
    }
    let identifier = NSUUID::new().UUIDString();
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None);
    let title_for_log = title.to_string();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the framework passes either null or a valid NSError.
        if let Some(error) = unsafe { error.as_ref() } {
            crate::log_warn!(
                "Could not post notification “{title_for_log}”: {}",
                error.localizedDescription()
            );
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&handler));
}

fn post_fallback(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    let spawned = std::thread::Builder::new()
        .name("notification".into())
        .spawn(move || crate::ui::notify(&title, &body));
    if let Err(err) = spawned {
        crate::log_warn!("Could not post notification: {err}");
    }
}
