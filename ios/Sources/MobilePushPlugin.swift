import UIKit
import WebKit
import UserNotifications
import Tauri
import ObjectiveC

// MARK: - Event bridge

/// Bridges a platform event to the Rust side, which fans it out to the JS
/// listeners registered with `register_listener`.
///
/// `Plugin::trigger` cannot be used: it reaches listeners held by the plugin
/// object Tauri's own dispatch instantiated, and this plugin bypasses that
/// dispatch (see the FFI functions below). Rust owns the listener registry
/// instead, and this calls into it.
///
/// The symbol is `mobile_push_emit_event` in `src/ios.rs`, linked here rather
/// than imported because the Rust library is linked into the same binary.
@_silgen_name("mobile_push_emit_event")
private func mobilePushEmitEvent(_ event: UnsafePointer<CChar>, _ payload: UnsafePointer<CChar>)

/// Emits an event, encoding both strings as C strings for the Rust side.
private func emit(_ event: String, _ payload: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: payload),
          let json = String(data: data, encoding: .utf8)
    else {
        NSLog("[mobile-push] could not serialize the %@ payload", event)
        return
    }
    event.withCString { eventPtr in
        json.withCString { payloadPtr in
            mobilePushEmitEvent(eventPtr, payloadPtr)
        }
    }
}

// MARK: - Token Fetcher (thread-safe async token retrieval)

/// Fetches an APNs device token by registering for remote notifications
/// and blocking until the AppDelegate callback fires.
private class TokenFetcher {
    let semaphore = DispatchSemaphore(value: 0)
    var token: String?
    var error: String?

    func resolve(_ tokenString: String) {
        self.token = tokenString
        semaphore.signal()
    }

    func reject(_ errorMessage: String) {
        self.error = errorMessage
        semaphore.signal()
    }
}

/// The active token fetcher — set during getDeviceToken, read by AppDelegate callbacks.
/// Access is serialized: only one getToken call can be in-flight at a time (guarded by tokenLock).
private var activeTokenFetcher: TokenFetcher?
private let tokenLock = NSLock()

// MARK: - AppDelegate method injection

/// Whether we've already injected APNs methods into the AppDelegate class.
private var apnsDelegateSetUp = false

/// Configured foreground presentation options. Set via FFI at plugin init.
private var configuredForegroundPresentation: UNNotificationPresentationOptions =
    [.banner, .list, .sound, .badge]

/// UNUserNotificationCenter delegate for foreground notification handling.
/// `willPresent` fires only when the app is in the foreground; backgrounded
/// and locked states bypass this delegate entirely and iOS shows the system
/// banner natively.
private class PushNotificationHandler: NSObject, UNUserNotificationCenterDelegate {
    static let shared = PushNotificationHandler()

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        emit("notification-received", Self.eventPayload(for: notification))
        completionHandler(configuredForegroundPresentation)
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        emit("notification-tapped", Self.eventPayload(for: response.notification))
        completionHandler()
    }

    /// Builds the event body from a notification.
    ///
    /// The APNs payload's custom keys are the `data` map, matching what the
    /// Android side reports for the same push, so the JS handler reads one
    /// shape from both platforms. `aps.alert` is not part of it — the visible
    /// text is not something a handler navigates on.
    private static func eventPayload(for notification: UNNotification) -> [String: Any] {
        var data: [String: Any] = [:]
        for (key, value) in notification.request.content.userInfo {
            guard let key = key as? String, key != "aps" else { continue }
            if let string = value as? String {
                data[key] = string
            } else if let number = value as? NSNumber {
                data[key] = number
            } else if let nested = value as? [String: Any] {
                data[key] = nested
            }
        }

        var payload: [String: Any] = ["data": data]
        let content = notification.request.content
        if !content.title.isEmpty {
            payload["title"] = content.title
        }
        if !content.body.isEmpty {
            payload["body"] = content.body
        }
        return payload
    }
}

/// Installs the notification-center delegate, which is what hands this app
/// foreground notifications and notification taps.
///
/// Must run before the app finishes launching: iOS delivers a tap that
/// launched the app only to the delegate that is already set when launch
/// completes, and asks exactly once, so a late install drops that tap for
/// good. Safe to call repeatedly.
private func installNotificationCenterDelegate() {
    if let existing = UNUserNotificationCenter.current().delegate,
        existing is PushNotificationHandler {
        return
    }
    UNUserNotificationCenter.current().delegate = PushNotificationHandler.shared
    NSLog("[mobile-push] Set UNUserNotificationCenter.delegate")
}

/// Injects APNs callback methods into the Tao-generated dynamic AppDelegate
/// class, so registration results reach the waiting `getDeviceToken` call.
///
/// Lazily: it needs `UIApplication.shared.delegate`, which Tao creates during
/// launch, and nothing needs it until a token is requested. Must be called on
/// the main thread.
private func setupApnsDelegateInternal() {
    installNotificationCenterDelegate()

    guard !apnsDelegateSetUp else { return }

    guard let delegate = UIApplication.shared.delegate else {
        NSLog("[mobile-push] setupApnsDelegate: no UIApplication.delegate found")
        return
    }

    let cls: AnyClass = type(of: delegate)
    NSLog("[mobile-push] Injecting APNs methods into %@", NSStringFromClass(cls))

    // application:didRegisterForRemoteNotificationsWithDeviceToken:
    let didRegisterSel = sel_registerName("application:didRegisterForRemoteNotificationsWithDeviceToken:")
    let didRegisterBlock: @convention(block) (AnyObject, UIApplication, NSData) -> Void = { _, _, tokenNSData in
        let tokenData = tokenNSData as Data
        let tokenString = tokenData.map { String(format: "%02.2hhx", $0) }.joined()
        NSLog("[mobile-push] APNs token received: %@...", String(tokenString.prefix(16)))
        activeTokenFetcher?.resolve(tokenString)
        MobilePushPlugin.instance?.handleToken(tokenData)
    }
    let didRegisterImp = imp_implementationWithBlock(didRegisterBlock as Any)
    if class_addMethod(cls, didRegisterSel, didRegisterImp, "v@:@@") {
        NSLog("[mobile-push] Added didRegisterForRemoteNotifications to AppDelegate")
    } else {
        NSLog("[mobile-push] didRegisterForRemoteNotifications already exists on AppDelegate")
    }

    // application:didFailToRegisterForRemoteNotificationsWithError:
    let didFailSel = sel_registerName("application:didFailToRegisterForRemoteNotificationsWithError:")
    let didFailBlock: @convention(block) (AnyObject, UIApplication, NSError) -> Void = { _, _, error in
        NSLog("[mobile-push] APNs registration failed: %@", error.localizedDescription)
        activeTokenFetcher?.reject(error.localizedDescription)
        MobilePushPlugin.instance?.handleTokenError(error as Error)
    }
    let didFailImp = imp_implementationWithBlock(didFailBlock as Any)
    if class_addMethod(cls, didFailSel, didFailImp, "v@:@@") {
        NSLog("[mobile-push] Added didFailToRegisterForRemoteNotifications to AppDelegate")
    } else {
        NSLog("[mobile-push] didFailToRegisterForRemoteNotifications already exists on AppDelegate")
    }

    apnsDelegateSetUp = true
}

// MARK: - Direct FFI functions (bypass PluginManager dispatch)

/// Configure what iOS shows when a notification arrives while the app is
/// foreground. `options` is the `UNNotificationPresentationOptions` bitmask
/// assembled on the Rust side from `ForegroundPresentationOptions`.
/// Called once from the Rust plugin setup after `register_ios_plugin`.
@_cdecl("mobile_push_set_foreground_presentation")
func setForegroundPresentation(_ options: UInt32) {
    configuredForegroundPresentation =
        UNNotificationPresentationOptions(rawValue: UInt(options))
    NSLog("[mobile-push] Foreground presentation set: %u", options)
}

/// Request notification permission. Blocks until the user responds (30s timeout).
/// Returns 1 if granted, 0 if denied or error.
@_cdecl("mobile_push_request_permission")
func requestPermissionDirect() -> Int32 {
    let sem = DispatchSemaphore(value: 0)
    var granted = false

    UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .badge, .sound]) { result, error in
        if let error = error {
            NSLog("[mobile-push] requestAuthorization error: %@", error.localizedDescription)
        }
        granted = result
        sem.signal()
    }

    let result = sem.wait(timeout: .now() + 30)
    if result == .timedOut {
        NSLog("[mobile-push] requestAuthorization timed out")
        return 0
    }

    NSLog("[mobile-push] Permission %@", granted ? "granted" : "denied")
    return granted ? 1 : 0
}

/// Get the APNs device token. Blocks until the token is received or timeout.
/// Writes the hex token string (null-terminated) to buffer.
/// Returns: >0 = token length, -1 = error, -2 = timeout.
@_cdecl("mobile_push_get_device_token")
func getDeviceTokenDirect(_ buffer: UnsafeMutablePointer<CChar>, _ bufferLen: Int32, _ timeoutSecs: Int32) -> Int32 {
    // Serialize concurrent calls
    tokenLock.lock()
    defer { tokenLock.unlock() }

    // Ensure APNs delegate is set up
    if !apnsDelegateSetUp {
        if Thread.isMainThread {
            setupApnsDelegateInternal()
        } else {
            DispatchQueue.main.sync {
                setupApnsDelegateInternal()
            }
        }
    }

    let fetcher = TokenFetcher()
    activeTokenFetcher = fetcher

    // Register for remote notifications (must be on main thread)
    DispatchQueue.main.async {
        NSLog("[mobile-push] Calling registerForRemoteNotifications...")
        UIApplication.shared.registerForRemoteNotifications()
    }

    // Wait for the token
    let timeout = max(Int(timeoutSecs), 5)
    let result = fetcher.semaphore.wait(timeout: .now() + .seconds(timeout))
    activeTokenFetcher = nil

    if result == .timedOut {
        NSLog("[mobile-push] Token request timed out after %ds", timeout)
        return -2
    }

    if let error = fetcher.error {
        NSLog("[mobile-push] Token error: %@", error)
        return -1
    }

    guard let token = fetcher.token else {
        NSLog("[mobile-push] Token is nil after semaphore signal")
        return -1
    }

    // Write token to buffer
    let bytes = Array(token.utf8)
    guard bytes.count < Int(bufferLen) else {
        NSLog("[mobile-push] Token too long for buffer: %d >= %d", bytes.count, bufferLen)
        return -1
    }
    for (i, b) in bytes.enumerated() {
        buffer[i] = CChar(bitPattern: b)
    }
    buffer[bytes.count] = 0

    NSLog("[mobile-push] Token (%d chars) written to buffer", bytes.count)
    return Int32(bytes.count)
}

// MARK: - Plugin class (kept for lifecycle + event callbacks)

@objc(MobilePushPlugin)
public class MobilePushPlugin: Plugin {
    public static var instance: MobilePushPlugin?

    override public func load(webview: WKWebView) {
        MobilePushPlugin.instance = self
        NSLog("[mobile-push] Plugin loaded (webview ready)")
    }

    // MARK: - Callbacks from AppDelegate injection

    public func handleToken(_ token: Data) {
        let tokenString = token.map { String(format: "%02.2hhx", $0) }.joined()
        NSLog("[mobile-push] handleToken: %@...", String(tokenString.prefix(16)))
        emit("token-received", ["token": tokenString])
    }

    public func handleTokenError(_ error: Error) {
        NSLog("[mobile-push] handleTokenError: %@", error.localizedDescription)
    }
}

// MARK: - Plugin entry point

@_cdecl("init_plugin_mobile_push")
func initPlugin() -> Plugin {
    // Set the notification-center delegate here rather than waiting for the
    // first `getDeviceToken`. The plugin is registered while the app is being
    // built, before `UIApplicationMain` runs, so this is the earliest hook
    // this plugin has — and installing the delegate only on demand is what
    // loses a tap that launched the app: iOS delivers that response once, to
    // whichever delegate is set when launch completes, and nothing in a
    // relaunch that already holds a stored device token ever calls
    // `getDeviceToken`.
    //
    // `setupApnsDelegateInternal` still installs it as well, so a command that
    // reaches a process where this early install did not stick behaves exactly
    // as before. The AppDelegate method injection stays lazy there:
    // `UIApplication.shared.delegate` may not exist yet at this point (Tao
    // creates the class during launch), and only a token request needs it.
    installNotificationCenterDelegate()
    return MobilePushPlugin()
}
