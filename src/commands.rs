//! The plugin's commands.
//!
//! Every command here is registered on every target, and each one dispatches
//! per platform. That registration is deliberate: Tauri runs a plugin's
//! `extend_api` handlers *before* native plugin dispatch, so a command that is
//! not registered here falls through to the native side, and on iOS that
//! fall-through is what hangs (see `mobile.rs`). Registering all of them keeps
//! the JS-facing command names stable and lets each one choose how to reach the
//! platform.

use tauri::ipc::Channel;
use tauri::{command, AppHandle, Runtime};

use crate::models::*;
use crate::Result;

#[cfg(target_os = "ios")]
extern "C" {
    /// Request notification permission. Blocks until the user responds.
    /// Returns 1 if granted, 0 if denied.
    fn mobile_push_request_permission() -> i32;

    /// Get the APNs device token. Blocks until the token arrives or the
    /// timeout elapses. Writes the hex token to the buffer. Returns the token
    /// length, -1 on error, -2 on timeout.
    fn mobile_push_get_device_token(buffer: *mut i8, buffer_len: i32, timeout_secs: i32) -> i32;
}

/// Forwards a command to the platform plugin (Kotlin) and deserializes its
/// response.
///
/// Used on Android only: the crate registers these commands on every target,
/// and without this the `#[cfg]` arms below would answer with a stub and the
/// Kotlin implementation — which is the one that can actually talk to Firebase
/// and the notification manager — would never run.
///
/// `command` is the name the Kotlin plugin registered, which is not always the
/// name on the JS side: the permission request is the framework's alias-driven
/// `requestPermissions`, plural.
#[cfg(target_os = "android")]
async fn forward<R: Runtime, T>(app: &AppHandle<R>, command: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    use tauri::Manager;

    let push = app.state::<crate::mobile::MobilePush<R>>();
    let response = push
        .handle()
        .run_mobile_plugin_async::<serde_json::Value>(command, ())
        .await?;
    serde_json::from_value(response).map_err(|e| crate::Error::Response(e.to_string()))
}

#[command]
pub(crate) async fn request_permission<R: Runtime>(
    app: AppHandle<R>,
) -> Result<PermissionResponse> {
    #[cfg(target_os = "android")]
    {
        forward::<R, PermissionResponse>(&app, "requestPermissions").await
    }

    #[cfg(target_os = "ios")]
    {
        let _ = &app;
        // The Swift function blocks on a semaphore, so it must not run on a
        // runtime worker thread.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(unsafe { mobile_push_request_permission() } == 1);
        });
        Ok(PermissionResponse {
            granted: rx.recv().unwrap_or(false),
        })
    }

    #[cfg(target_os = "macos")]
    {
        crate::macos::request_permission(&app)
    }

    #[cfg(all(desktop, not(target_os = "macos")))]
    {
        let _ = &app;
        Ok(PermissionResponse { granted: false })
    }
}

#[command]
pub(crate) async fn get_token<R: Runtime>(app: AppHandle<R>) -> Result<TokenResponse> {
    #[cfg(target_os = "android")]
    {
        forward::<R, TokenResponse>(&app, "getToken").await
    }

    #[cfg(target_os = "ios")]
    {
        let _ = &app;
        // 64 hex characters; the buffer is sized well past that.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0i8; 256];
            let result = unsafe { mobile_push_get_device_token(buffer.as_mut_ptr(), 256, 15) };
            if result > 0 {
                let len = result as usize;
                let bytes: Vec<u8> = buffer[..len].iter().map(|&b| b as u8).collect();
                match String::from_utf8(bytes) {
                    Ok(token) => tx.send(Ok(TokenResponse { token })),
                    Err(e) => tx.send(Err(format!("APNs device token is not UTF-8: {e}"))),
                }
            } else if result == -2 {
                tx.send(Err("Timed out waiting for the APNs device token".to_string()))
            } else {
                tx.send(Err("Failed to get the APNs device token".to_string()))
            }
        });

        match rx.recv() {
            Ok(Ok(token)) => Ok(token),
            Ok(Err(e)) => Err(crate::Error::Response(e)),
            Err(_) => Err(crate::Error::Response(
                "The APNs token thread panicked".to_string(),
            )),
        }
    }

    #[cfg(target_os = "macos")]
    {
        crate::macos::get_token(&app)
    }

    #[cfg(all(desktop, not(target_os = "macos")))]
    {
        let _ = &app;
        Err(crate::Error::Unsupported)
    }
}

/// Registers a JS listener for a platform event.
///
/// `addPluginListener` creates the `Channel` and passes it here, and the
/// platforms deliver through [`crate::events::emit`] using the same event
/// names: `notification-received`, `notification-tapped`, `token-received`.
#[command]
pub(crate) async fn register_listener<R: Runtime>(
    _app: AppHandle<R>,
    event: String,
    handler: Channel<serde_json::Value>,
) -> Result<()> {
    crate::events::register(event, handler);
    Ok(())
}

/// Drops a listener registered with [`register_listener`].
#[command]
pub(crate) async fn remove_listener<R: Runtime>(
    _app: AppHandle<R>,
    event: String,
    channel_id: u32,
) -> Result<()> {
    crate::events::remove(&event, channel_id);
    Ok(())
}

/// JNI entry point for `MobilePushPlugin.emitEvent` in Kotlin.
///
/// Kotlin declares `emitEvent` as an instance method, so the JNI symbol takes
/// the receiver as its second argument. Kotlin calls this from the FCM callback
/// thread with the event name and a JSON payload, and it fans out to the JS
/// listeners. Nothing in Rust calls it: the symbol is exported because the
/// crate defines a `#[no_mangle]` symbol and Rust libraries are linked whole
/// into the app's `cdylib` (verified — a `#[no_mangle]` symbol defined in a
/// dependency rlib is present in the final `.so`, including under LTO).
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_app_tauri_mobilepush_MobilePushPlugin_emitEvent<'local>(
    mut env: jni::JNIEnv<'local>,
    _this: jni::objects::JObject<'local>,
    event: jni::objects::JString<'local>,
    payload: jni::objects::JString<'local>,
) {
    let mut read = |s: &jni::objects::JString<'local>| -> Option<String> {
        env.get_string(s)
            .ok()
            .map(|s| s.to_string_lossy().into_owned())
    };
    let (event, payload) = (read(&event), read(&payload));
    let (Some(event), Some(payload)) = (event, payload) else {
        return;
    };
    log::debug!("[mobile-push] {event} from the platform");
    // A tap can arrive before any JavaScript has run (cold start), so it is
    // held for the first listener; a received notification cannot.
    let sticky = event == "notification-tapped";
    crate::events::emit(&event, &payload, sticky);
}
