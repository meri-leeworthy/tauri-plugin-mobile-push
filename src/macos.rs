//! macOS implementation of the push plugin.
//!
//! macOS registers for remote notifications through AppKit, not UIKit: the
//! token arrives on `NSApplicationDelegate` and the notification callbacks come
//! from `UNUserNotificationCenterDelegate`. tao installs its own dynamic
//! delegate class (`TaoAppDelegateParent`) as `NSApplication.delegate` while the
//! event loop is created, so this module adds the APNs callbacks to whatever
//! class is there at runtime rather than declaring a delegate of its own.
//!
//! The listener registry in [`crate::events`] is platform-agnostic and is the
//! same fan-out iOS and Android use.

use std::sync::LazyLock;

use objc2::rc::Retained;
use objc2::runtime::{
    AnyClass, AnyObject, Imp, NSObjectProtocol, ProtocolObject, Sel,
};
use objc2::{
    define_class, msg_send, sel, AllocAnyThread, MainThreadMarker,
};
use objc2_app_kit::{NSApplication, NSApplicationDelegate};
use objc2_foundation::{NSData, NSError, NSString};
use objc2::runtime::Bool;
use objc2_user_notifications::{
    UNAuthorizationOptions, UNNotification, UNNotificationPresentationOptions,
    UNNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use parking_lot::{Condvar, Mutex};
use serde::de::DeserializeOwned;
use tauri::{plugin::PluginApi, AppHandle, Runtime};

use crate::events;
use crate::models::{PermissionResponse, TokenResponse};
use crate::Result;

/// Foreground presentation, as the bitmask `ForegroundPresentationOptions`
/// already produces for iOS. Read by both delegate callbacks.
static FOREGROUND_BITS: Mutex<u32> = Mutex::new(0);

/// Where the token callback deposits its result for the waiting command.
static PENDING_TOKEN: LazyLock<(Mutex<Option<std::result::Result<String, String>>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(None), Condvar::new()));

/// The notification-center delegate. The center holds its delegate weakly, so
/// this is the owning reference and lives for the process.
static CENTER_DELEGATE: LazyLock<Retained<CenterDelegate>> = LazyLock::new(|| {
    let delegate = CenterDelegate::alloc();
    // SAFETY: `alloc`/`init` is the two-step construction `define_class!`
    // classes use; `NSObject`'s `init` returns the same object.
    unsafe { msg_send![delegate, init] }
});

define_class!(
    #[unsafe(super(objc2::runtime::NSObject))]
    #[name = "RoomyPushCenterDelegate"]
    #[ivars = ()]
    struct CenterDelegate;

    unsafe impl NSObjectProtocol for CenterDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for CenterDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            notification: &UNNotification,
            completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            emit("notification-received", notification, false);
            let bits = *FOREGROUND_BITS.lock();
            completion_handler.call((UNNotificationPresentationOptions(bits as _),));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            // A tap can launch the app, so the listener that routes it may not
            // exist yet; `events` holds a sticky payload for the first one.
            emit("notification-tapped", &response.notification(), true);
            completion_handler.call(());
        }
    }
);

/// Serialises a notification into the `{ title?, body?, data }` shape the
/// JavaScript listeners read, matching what the iOS side emits for the same
/// push. The APNs payload's non-`aps` keys are the `data` map.
fn emit(event: &str, notification: &UNNotification, sticky: bool) {
    let content = notification.request().content();
    let mut data = serde_json::Map::new();
    for key in content.userInfo().allKeys().iter() {
        let Some(key) = key.downcast_ref::<NSString>() else {
            continue;
        };
        let name = key.to_string();
        if name == "aps" {
            continue;
        }
        // Values this appserver sends are strings; anything else is dropped
        // rather than guessed at.
        let value = content.userInfo().objectForKey(&*key as &AnyObject);
        if let Some(text) = value
            .as_ref()
            .and_then(|value| value.downcast_ref::<NSString>())
            .map(|value| value.to_string())
        {
            data.insert(name, serde_json::Value::String(text));
        }
    }

    let mut payload = serde_json::Map::new();
    payload.insert("data".into(), serde_json::Value::Object(data));
    if !content.title().to_string().is_empty() {
        payload.insert("title".into(), content.title().to_string().into());
    }
    if !content.body().to_string().is_empty() {
        payload.insert("body".into(), content.body().to_string().into());
    }
    events::emit(event, &serde_json::Value::Object(payload).to_string(), sticky);
}

/// `application:didRegisterForRemoteNotificationsWithDeviceToken:`, added to the
/// application delegate's class at runtime.
extern "C" fn did_register(
    _this: &AnyObject,
    _sel: Sel,
    _app: &NSApplication,
    device_token: &NSData,
) {
    let hex: String = device_token
        .to_vec()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let (lock, condvar) = &*PENDING_TOKEN;
    *lock.lock() = Some(Ok(hex.clone()));
    condvar.notify_all();

    let payload = serde_json::json!({ "token": hex }).to_string();
    events::emit("token-received", &payload, false);
}

/// `application:didFailToRegisterForRemoteNotificationsWithError:`.
extern "C" fn did_fail(_this: &AnyObject, _sel: Sel, _app: &NSApplication, error: &NSError) {
    let message = error.localizedDescription().to_string();
    log::warn!("[mobile-push] APNs registration failed: {message}");
    let (lock, condvar) = &*PENDING_TOKEN;
    *lock.lock() = Some(Err(message));
    condvar.notify_all();
}

/// Adds the APNs callbacks to the class that is currently the application
/// delegate.
///
/// The class is tao's `TaoAppDelegateParent`, created at runtime, so it cannot
/// be named here. `class_addMethod` returns false when the class already
/// implements the selector; that is logged rather than ignored, because a second
/// implementation would silently lose.
fn install_delegate_methods(delegate_class: &AnyClass) {
    // SAFETY: both functions have the ABI the selectors describe, and the
    // encoding strings match their signatures.
    unsafe {
        let added = objc2::ffi::class_addMethod(
            delegate_class as *const AnyClass as *mut AnyClass,
            sel!(application:didRegisterForRemoteNotificationsWithDeviceToken:),
            std::mem::transmute::<
                extern "C" fn(&AnyObject, Sel, &NSApplication, &NSData),
                Imp,
            >(did_register),
            c"v@:@@".as_ptr(),
        );
        if !added.as_bool() {
            log::warn!("[mobile-push] the application delegate already handles a registered token");
        }

        let added = objc2::ffi::class_addMethod(
            delegate_class as *const AnyClass as *mut AnyClass,
            sel!(application:didFailToRegisterForRemoteNotificationsWithError:),
            std::mem::transmute::<
                extern "C" fn(&AnyObject, Sel, &NSApplication, &NSError),
                Imp,
            >(did_fail),
            c"v@:@@".as_ptr(),
        );
        if !added.as_bool() {
            log::warn!("[mobile-push] the application delegate already handles a failed token");
        }
    }
}

/// Installs both delegates and registers with APNs. Must run on the main thread.
fn start_on_main_thread(foreground_bits: u32) {
    *FOREGROUND_BITS.lock() = foreground_bits;

    let Some(mtm) = MainThreadMarker::new() else {
        log::error!("[mobile-push] APNs registration must run on the main thread");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    match app.delegate() {
        Some(delegate) => {
            // The delegate is an `NSApplicationDelegate`; the methods are added
            // to its class, which is what the runtime dispatches against.
            let object: &AnyObject = unsafe {
                &*(Retained::as_ptr(&delegate) as *const dyn NSApplicationDelegate
                    as *const AnyObject)
            };
            install_delegate_methods(object.class());
        }
        None => log::error!("[mobile-push] no application delegate to install APNs callbacks on"),
    }

    let center = UNUserNotificationCenter::currentNotificationCenter();
    let delegate: &CenterDelegate = &CENTER_DELEGATE;
    let delegate: &ProtocolObject<dyn UNUserNotificationCenterDelegate> =
        ProtocolObject::from_ref(delegate);
    center.setDelegate(Some(delegate));

    app.registerForRemoteNotifications();
}

/// Runs `f` on the main thread and waits for it, since both AppKit and
/// UserNotifications require it and the commands run on a runtime worker.
fn on_main_thread<R: Runtime, F: FnOnce() + Send + 'static>(
    app: &AppHandle<R>,
    f: F,
) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        f();
        let _ = tx.send(());
    })
    .map_err(|error| crate::Error::Response(error.to_string()))?;
    let _ = rx.recv();
    Ok(())
}

/// Asks the user for notification permission and blocks until they answer.
pub fn request_permission<R: Runtime>(app: &AppHandle<R>) -> Result<PermissionResponse> {
    on_main_thread(app, || {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let (tx, rx) = std::sync::mpsc::channel();
        let handler = block2::RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            let _ = tx.send(granted.as_bool());
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert
                | UNAuthorizationOptions::Badge
                | UNAuthorizationOptions::Sound,
            &handler,
        );
        let granted = rx.recv().unwrap_or(false);
        let (lock, condvar) = &*PENDING_PERMISSION;
        *lock.lock() = Some(granted);
        condvar.notify_all();
    })?;

    // The handler runs on an arbitrary queue; wait for its answer.
    let (lock, condvar) = &*PENDING_PERMISSION;
    let mut guard = lock.lock();
    while guard.is_none() {
        condvar
            .wait_for(&mut guard, std::time::Duration::from_secs(30))
            .timed_out()
            .then(|| log::warn!("[mobile-push] the permission prompt timed out"));
        if guard.is_none() {
            break;
        }
    }
    Ok(PermissionResponse {
        granted: guard.take().unwrap_or(false),
    })
}

static PENDING_PERMISSION: LazyLock<(Mutex<Option<bool>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(None), Condvar::new()));

/// Registers with APNs and waits for the device token.
pub fn get_token<R: Runtime>(app: &AppHandle<R>) -> Result<TokenResponse> {
    {
        let (lock, _) = &*PENDING_TOKEN;
        *lock.lock() = None;
    }
    on_main_thread(app, || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        NSApplication::sharedApplication(mtm).registerForRemoteNotifications();
    })?;

    let (lock, condvar) = &*PENDING_TOKEN;
    let mut guard = lock.lock();
    while guard.is_none() {
        if condvar
            .wait_for(&mut guard, std::time::Duration::from_secs(15))
            .timed_out()
        {
            return Err(crate::Error::Response(
                "Timed out waiting for the APNs device token".into(),
            ));
        }
    }
    match guard.take() {
        Some(Ok(token)) => Ok(TokenResponse { token }),
        Some(Err(message)) => Err(crate::Error::Response(message)),
        None => Err(crate::Error::Response(
            "The APNs token callback produced no token".into(),
        )),
    }
}

/// Plugin handle on macOS.
pub struct MobilePush<R: Runtime>(AppHandle<R>);

pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<MobilePush<R>> {
    Ok(MobilePush(app.clone()))
}

/// Registers with APNs and installs the delegates, once the plugin's
/// configuration is known.
pub fn setup<R: Runtime>(app: &AppHandle<R>, foreground_bits: u32) {
    if let Err(error) = on_main_thread(app, move || start_on_main_thread(foreground_bits)) {
        log::error!("[mobile-push] could not start on macOS: {error}");
    }
}

/// Kept so the module compiles the same way on every target it is used from.
pub fn handle<R: Runtime>(push: &MobilePush<R>) -> &AppHandle<R> {
    &push.0
}
