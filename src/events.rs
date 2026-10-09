//! Fan-out from the native platforms to the JS listeners registered with
//! `register_listener`.
//!
//! Tauri's `Plugin::trigger` reaches listeners held by the plugin object that
//! Tauri instantiated through its plugin dispatch. This plugin does not use
//! that dispatch — the iOS commands go through direct `@_cdecl` FFI (see
//! `commands.rs`), and the Android commands run in a Kotlin plugin that Rust
//! forwards to — so `trigger` on either side has no listeners to reach.
//!
//! Instead the platforms call into [`emit`] (Android through the JNI symbol in
//! `commands.rs`, iOS through the `@_cdecl` function in
//! `ios/Sources/MobilePushPlugin.swift`), which sends the payload to every
//! `Channel` registered against that event name.
//!
//! A listener arrives as the `Channel` the JS side created for
//! `addPluginListener`: Tauri deserializes it from the `__CHANNEL__:<id>`
//! string and [`emit`] sends the platform's JSON payload through it, exactly as
//! `Plugin::trigger` would have.

use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::{Mutex, MutexGuard};
use serde_json::Value;
use tauri::ipc::Channel;

/// Listeners by event name.
///
/// A process-global rather than a field on the plugin struct because the
/// platforms reach [`emit`] from outside Rust's plugin state — the JNI symbol
/// and the `@_cdecl` function are plain functions.
static LISTENERS: LazyLock<Mutex<HashMap<String, Vec<Channel<Value>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Sticky payloads waiting for their first listener, by event name.
///
/// A notification tapped while the app is not running is delivered before the
/// webview has run any JavaScript, so there is nothing to receive it. The tap
/// is held here and replayed to the first listener that registers, which is
/// what makes a cold-start tap open the room the notification referred to.
static PENDING: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn registry() -> MutexGuard<'static, HashMap<String, Vec<Channel<Value>>>> {
    LISTENERS.lock()
}

/// Registers a listener for `event`, delivering a sticky payload if one is
/// waiting.
pub fn register(event: String, channel: Channel<Value>) {
    log::info!(
        "[mobile-push] listener registered for event `{event}` (channel {})",
        channel.id()
    );
    if let Some(pending) = PENDING.lock().remove(&event) {
        log::debug!("[mobile-push] replaying the pending `{event}` payload");
        if let Err(e) = channel.send(pending) {
            log::warn!("[mobile-push] could not replay `{event}`: {e}");
        }
    }
    registry().entry(event).or_default().push(channel);
}

/// Drops the listener for `event` with the given channel id.
///
/// `PluginListener::unregister` calls the `remove_listener` command with the
/// channel id it was given at registration.
pub fn remove(event: &str, channel_id: u32) {
    let mut map = registry();
    let Some(channels) = map.get_mut(event) else {
        return;
    };
    channels.retain(|c| c.id() != channel_id);
    if channels.is_empty() {
        map.remove(event);
    }
}

/// Drops every listener and pending payload.
///
/// Listeners outlive the webview that registered them, so they are cleared
/// when the app exits rather than left behind.
pub fn clear() {
    registry().clear();
    PENDING.lock().clear();
}

/// Delivers a platform event to every listener registered for `event`.
///
/// `payload` is a JSON document produced by the platform side. A payload that
/// does not parse is logged and dropped: a malformed notification must not
/// take down the thread that produced it. A listener whose channel fails to
/// send (its webview is gone) is dropped, so a reload does not accumulate dead
/// channels.
///
/// With `sticky`, a payload that arrives before any listener exists is kept
/// and replayed to the first one to register. That is for events which are
/// about a single past occurrence the user acted on — a notification tap —
/// where a late listener still needs it. Events that arrive continuously
/// (a received notification, a rotated token) are not sticky: replaying a stale
/// one on the next launch would act on a moment that has passed.
///
/// Only the mobile platforms produce events, so this is unused (and not
/// compiled) on the desktop targets.
#[cfg(any(mobile, target_os = "macos", test))]
pub fn emit(event: &str, payload: &str, sticky: bool) {
    let data: Value = match serde_json::from_str(payload) {
        Ok(data) => data,
        Err(e) => {
            log::error!("[mobile-push] dropping unparseable `{event}` payload: {e}");
            return;
        }
    };

    // Snapshot the channels and drop the lock before sending: `Channel::send`
    // evaluates JavaScript in the webview, which needs the main thread, so it
    // must never run while the lock is held.
    let channels: Vec<Channel<Value>> = match registry().get(event) {
        Some(channels) => channels.clone(),
        None => Vec::new(),
    };

    if channels.is_empty() {
        if sticky {
            log::debug!("[mobile-push] holding `{event}` until a listener registers");
            PENDING.lock().insert(event.to_string(), data);
        }
        return;
    }

    let mut delivered = 0usize;
    let mut dead = Vec::new();
    for channel in &channels {
        match channel.send(data.clone()) {
            Ok(()) => delivered += 1,
            Err(e) => {
                log::debug!(
                    "[mobile-push] dropping `{event}` listener {}: {e}",
                    channel.id()
                );
                dead.push(channel.id());
            }
        }
    }

    if !dead.is_empty() {
        let mut map = registry();
        if let Some(channels) = map.get_mut(event) {
            channels.retain(|c| !dead.contains(&c.id()));
            if channels.is_empty() {
                map.remove(event);
            }
        }
    }

    log::debug!("[mobile-push] `{event}` delivered to {delivered} listener(s)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The registry is process-global, so tests that share it must not run
    /// concurrently: one test's `clear` would otherwise wipe another's
    /// listeners. Each test holds this for its duration.
    fn serial() -> parking_lot::MutexGuard<'static, ()> {
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        let guard = LOCK.lock();
        clear();
        guard
    }

    /// A channel that records what it receives, plus the recording buffer.
    fn recorder() -> (Channel<Value>, Arc<Mutex<Vec<Value>>>) {
        let received: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink = received.clone();
        let channel = Channel::<Value>::new(move |body| {
            sink.lock().push(body.deserialize::<Value>().unwrap());
            Ok(())
        });
        (channel, received)
    }

    /// A channel whose send always fails, as one bound to a closed webview does.
    fn broken() -> Channel<Value> {
        Channel::<Value>::new(|_| Err(tauri::Error::WebviewNotFound))
    }

    #[test]
    fn delivers_only_to_listeners_of_that_event() {
        let _serial = serial();
        let (received, got) = recorder();
        let (tapped, tap_got) = recorder();
        register("notification-received".into(), received);
        register("notification-tapped".into(), tapped);

        emit("notification-received", r#"{"title":"hi"}"#, false);

        assert_eq!(*got.lock(), vec![serde_json::json!({"title": "hi"})]);
        assert!(tap_got.lock().is_empty());
    }

    #[test]
    fn delivers_to_every_listener_of_an_event() {
        let _serial = serial();
        let (first, first_got) = recorder();
        let (second, second_got) = recorder();
        register("token-received".into(), first);
        register("token-received".into(), second);

        emit("token-received", r#"{"token":"abc"}"#, false);

        assert_eq!(first_got.lock().len(), 1);
        assert_eq!(second_got.lock().len(), 1);
    }

    #[test]
    fn an_event_without_listeners_is_not_an_error() {
        let _serial = serial();
        emit("notification-tapped", r#"{"data":{}}"#, false);
    }

    #[test]
    fn malformed_payload_is_dropped_without_delivering() {
        let _serial = serial();
        let (channel, got) = recorder();
        let id = channel.id();
        register("notification-received".into(), channel);

        emit("notification-received", "not json", false);

        assert!(got.lock().is_empty());
        // Still registered: a bad payload says nothing about the listener.
        assert_eq!(registry().get("notification-received").unwrap().len(), 1);
        remove("notification-received", id);
    }

    #[test]
    fn remove_drops_only_that_channel() {
        let _serial = serial();
        let (first, first_got) = recorder();
        let (second, second_got) = recorder();
        let first_id = first.id();
        register("notification-received".into(), first);
        register("notification-received".into(), second);

        remove("notification-received", first_id);
        emit("notification-received", r#"{"n":1}"#, false);

        assert!(first_got.lock().is_empty());
        assert_eq!(second_got.lock().len(), 1);
    }

    #[test]
    fn a_listener_that_cannot_send_is_dropped() {
        let _serial = serial();
        register("notification-tapped".into(), broken());
        let (live, got) = recorder();
        register("notification-tapped".into(), live);

        emit("notification-tapped", r#"{"n":1}"#, false);
        assert_eq!(got.lock().len(), 1);
        // The dead channel is gone, the live one remains.
        assert_eq!(registry().get("notification-tapped").unwrap().len(), 1);

        emit("notification-tapped", r#"{"n":2}"#, false);
        assert_eq!(got.lock().len(), 2);
    }

    #[test]
    fn a_sticky_event_arriving_early_reaches_the_first_listener() {
        let _serial = serial();
        // The cold-start ordering: the platform reports the tap before any
        // JavaScript has run, so registration is what delivers it.
        emit("notification-tapped", r#"{"data":{"roomId":"r"}}"#, true);
        assert!(PENDING.lock().contains_key("notification-tapped"));

        let (channel, got) = recorder();
        register("notification-tapped".into(), channel);

        assert_eq!(
            *got.lock(),
            vec![serde_json::json!({"data": {"roomId": "r"}})]
        );
        // Delivered once, not held for every later listener.
        assert!(!PENDING.lock().contains_key("notification-tapped"));
    }

    #[test]
    fn a_non_sticky_event_arriving_early_is_not_replayed() {
        let _serial = serial();
        emit("token-received", r#"{"token":"abc"}"#, false);

        let (channel, got) = recorder();
        register("token-received".into(), channel);

        assert!(got.lock().is_empty());
    }
}
