//! Desktop notifications for finished commands and for programs that post
//! their own (OSC 9 / OSC 777).
//!
//! Delivery on macOS: from an installed `.app` bundle, `UNUserNotificationCenter`
//! through the `objc` runtime — real notifications, with a click that
//! routes back to the pane. Outside a bundle (`cargo run`) the center has no
//! bundle to attach to and aborts the process, so `osascript` posts a
//! notification instead; those can't be clicked back into OmniPTY.
//!
//! Delivery on Linux: `notify-send` (libnotify), which talks to whatever
//! implements `org.freedesktop.Notifications` — mako, dunst, swaync, the
//! GNOME and KDE shells. Where the daemon supports actions, a click on the
//! notification routes back to the pane the same way.
//!
//! The decision of *whether* to notify is a pure function so it can be
//! tested without posting anything.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::schema::NotificationsConfig;

/// What just finished, as the threshold logic sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finished {
    pub duration: Duration,
    pub exit: Option<i32>,
    /// The pane had keyboard focus in an active window.
    pub pane_focused: bool,
}

/// Whether a finished command deserves a notification.
pub fn should_notify(config: &NotificationsConfig, finished: Finished) -> bool {
    if !config.enabled {
        return false;
    }
    if config.only_when_unfocused && finished.pane_focused {
        return false;
    }
    let failed = finished.exit.is_some_and(|e| e != 0);
    finished.duration >= config.min_duration.0 || (failed && config.on_failure_always)
}

/// `✓ cargo build — 2m14s` / `✗ npm test — exit 1 · 4.2s`.
pub fn command_summary(label: &str, exit: Option<i32>, duration: Duration) -> String {
    let time = crate::terminal::commands::format_duration(duration);
    match exit {
        Some(0) => format!("✓ {label} — {time}"),
        Some(code) => format!("✗ {label} — exit {code} · {time}"),
        None => format!("• {label} — {time}"),
    }
}

/// Routing key carried on a notification: which window-and-pane to focus
/// when it's clicked. Allocated by the app, process-unique.
pub type RouteKey = u64;

static ROUTE_TX: OnceLock<Mutex<Option<UnboundedSender<RouteKey>>>> = OnceLock::new();

/// Install the channel notification clicks are delivered on. Call once at
/// startup; the receiver is drained on the main thread.
pub fn install_click_channel() -> UnboundedReceiver<RouteKey> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let slot = ROUTE_TX.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some(tx);
    rx
}

fn route_clicked(key: RouteKey) {
    if let Some(slot) = ROUTE_TX.get()
        && let Some(tx) = slot.lock().unwrap().as_ref()
    {
        tx.unbounded_send(key).ok();
    }
}

/// Attach the delegate to the notification center. Apple requires this
/// before the app finishes launching: a delegate installed later isn't
/// consulted, and a notification arriving while OmniPTY is frontmost is
/// then filed in Notification Center without a banner. No-op outside a
/// bundle, where the center can't be used at all.
pub fn init() {
    #[cfg(target_os = "macos")]
    if crate::update::installed_bundle().is_some() {
        macos::init();
    }
}

/// Post a notification. `route` is attached so a click can find its pane.
pub fn post(title: &str, body: &str, route: Option<RouteKey>) {
    #[cfg(target_os = "macos")]
    if crate::update::installed_bundle().is_some() {
        macos::post(title, body, route);
    } else {
        post_via_osascript(title, body);
    }
    #[cfg(not(target_os = "macos"))]
    linux::post(title, body, route);
}

#[cfg(target_os = "macos")]
fn post_via_osascript(title: &str, body: &str) {
    // AppleScript string literals: escape backslash and double quote.
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        esc(body),
        esc(title)
    );
    let _ = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "macos"))]
mod linux {
    //! `notify-send` from libnotify. With a daemon that supports actions,
    //! `-A` attaches a default action and `-w` keeps notify-send alive
    //! until the notification closes, printing the action's key if it was
    //! clicked — that's the click routing. An older libnotify (before
    //! 0.7.9) rejects `-A`, so a failed spawn falls back to a plain send.

    use std::process::{Command, Stdio};

    use super::RouteKey;

    fn base_command(title: &str, body: &str) -> Command {
        let mut cmd = Command::new("notify-send");
        cmd.args([
            "--app-name=OmniPTY",
            "--icon=omnipty",
            // Lets daemons that key on the desktop file find the icon/name.
            "-h",
            "string:desktop-entry:omnipty",
            "--",
        ])
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
        cmd
    }

    pub fn post(title: &str, body: &str, route: Option<RouteKey>) {
        let title = title.to_string();
        let body = body.to_string();
        // Blocks for as long as the notification is showing: off the main
        // thread. One thread per notification, gone when it's dismissed.
        std::thread::Builder::new()
            .name("omnipty-notify".into())
            .spawn(move || {
                let mut with_action = base_command(&title, &body);
                with_action.args(["-A", "default=Open", "-w"]);
                match with_action.stdout(Stdio::piped()).output() {
                    Ok(out) if out.status.success() => {
                        let clicked = String::from_utf8_lossy(&out.stdout);
                        if clicked.trim() == "default"
                            && let Some(route) = route
                        {
                            super::route_clicked(route);
                        }
                    }
                    // Unknown option (old libnotify), or no daemon: try the
                    // plain form once so the message at least shows.
                    _ => {
                        let _ = base_command(&title, &body).stdout(Stdio::null()).status();
                    }
                }
            })
            .ok();
    }
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
mod macos {
    //! UNUserNotificationCenter over the raw objc runtime. Everything here
    //! runs on the main thread (AppKit delivers delegate callbacks there),
    //! and the delegate is installed once per process.

    use std::ffi::CStr;
    use std::os::raw::c_void;
    use std::sync::Once;

    use block::{Block, ConcreteBlock};
    use objc::declare::ClassDecl;
    use objc::runtime::{BOOL, Class, Object, Protocol, Sel};
    use objc::{class, msg_send, sel, sel_impl};

    use super::RouteKey;

    type Id = *mut Object;

    const ROUTE_USERINFO_KEY: &str = "omnipty.route";

    fn nsstring(s: &str) -> Id {
        let bytes = s.as_bytes();
        unsafe {
            let cls = class!(NSString);
            let obj: Id = msg_send![cls, alloc];
            // UTF-8 without a NUL: length-delimited init.
            msg_send![obj, initWithBytes: bytes.as_ptr() length: bytes.len() encoding: 4u64]
        }
    }

    fn rust_string(ns: Id) -> Option<String> {
        if ns.is_null() {
            return None;
        }
        unsafe {
            let ptr: *const std::os::raw::c_char = msg_send![ns, UTF8String];
            if ptr.is_null() {
                return None;
            }
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }

    static DELEGATE_ONCE: Once = Once::new();

    /// Build the delegate class and hand an instance to the center. The
    /// center keeps only a weak reference, so the instance is leaked on
    /// purpose to stay alive for the process.
    fn install_delegate(center: Id) {
        DELEGATE_ONCE.call_once(|| unsafe {
            let superclass = class!(NSObject);
            let Some(mut decl) = ClassDecl::new("OmniPTYNotificationDelegate", superclass) else {
                return;
            };

            // Show banners even while OmniPTY is frontmost — a command that
            // finished in a background pane is still news.
            // Completion handlers arrive as blocks; objc's `Encode` covers raw
            // pointers, so they're typed as such and cast before calling.
            extern "C" fn will_present(
                _this: &Object,
                _sel: Sel,
                _center: Id,
                _notification: Id,
                handler: *mut c_void,
            ) {
                if std::env::var_os("OMNIPTY_DEBUG_NOTIFY").is_some() {
                    eprintln!("omnipty: willPresentNotification");
                }
                // UNNotificationPresentationOptionBanner (1<<4) | Sound (1<<1) | List (1<<3)
                unsafe {
                    let handler = &*(handler as *mut Block<(u64,), ()>);
                    handler.call(((1 << 4) | (1 << 1) | (1 << 3),));
                }
            }

            extern "C" fn did_receive(
                _this: &Object,
                _sel: Sel,
                _center: Id,
                response: Id,
                handler: *mut c_void,
            ) {
                unsafe {
                    let notification: Id = msg_send![response, notification];
                    let request: Id = msg_send![notification, request];
                    let content: Id = msg_send![request, content];
                    let info: Id = msg_send![content, userInfo];
                    if !info.is_null() {
                        let key = nsstring(ROUTE_USERINFO_KEY);
                        let value: Id = msg_send![info, objectForKey: key];
                        if let Some(text) = rust_string(value)
                            && let Ok(route) = text.parse::<RouteKey>()
                        {
                            super::route_clicked(route);
                        }
                    }
                    let handler = &*(handler as *mut Block<(), ()>);
                    handler.call(());
                }
            }

            // Declare conformance too: the center may check the protocol,
            // not just `respondsToSelector:`.
            if let Some(protocol) = Protocol::get("UNUserNotificationCenterDelegate") {
                decl.add_protocol(protocol);
            }
            decl.add_method(
                sel!(userNotificationCenter:willPresentNotification:withCompletionHandler:),
                will_present as extern "C" fn(&Object, Sel, Id, Id, *mut c_void),
            );
            decl.add_method(
                sel!(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:),
                did_receive as extern "C" fn(&Object, Sel, Id, Id, *mut c_void),
            );
            let cls: &Class = decl.register();
            let delegate: Id = msg_send![cls, new];
            let _: () = msg_send![center, setDelegate: delegate];
        });
    }

    pub fn init() {
        unsafe {
            let center_cls = class!(UNUserNotificationCenter);
            let center: Id = msg_send![center_cls, currentNotificationCenter];
            if !center.is_null() {
                install_delegate(center);
            }
        }
    }

    pub fn post(title: &str, body: &str, route: Option<RouteKey>) {
        unsafe {
            let center_cls = class!(UNUserNotificationCenter);
            let center: Id = msg_send![center_cls, currentNotificationCenter];
            if center.is_null() {
                super::post_via_osascript(title, body);
                return;
            }
            install_delegate(center);
            if std::env::var_os("OMNIPTY_DEBUG_NOTIFY").is_some() {
                let delegate: Id = msg_send![center, delegate];
                eprintln!(
                    "omnipty: posting notification; delegate set = {}",
                    !delegate.is_null()
                );
            }

            // UNAuthorizationOptionBadge (1<<0) | Sound (1<<1) | Alert (1<<2).
            // The first call shows the system permission prompt; later ones
            // are no-ops. The completion block must exist even if ignored.
            let on_auth = ConcreteBlock::new(move |_granted: BOOL, _error: Id| {}).copy();
            let _: () = msg_send![center, requestAuthorizationWithOptions: 7u64 completionHandler: &*on_auth];

            let content: Id = msg_send![class!(UNMutableNotificationContent), new];
            let _: () = msg_send![content, setTitle: nsstring(title)];
            let _: () = msg_send![content, setBody: nsstring(body)];
            let sound: Id = msg_send![class!(UNNotificationSound), defaultSound];
            let _: () = msg_send![content, setSound: sound];
            if let Some(route) = route {
                let dict: Id = msg_send![class!(NSMutableDictionary), new];
                let _: () = msg_send![dict, setObject: nsstring(&route.to_string()) forKey: nsstring(ROUTE_USERINFO_KEY)];
                let _: () = msg_send![content, setUserInfo: dict];
            }

            let ident = nsstring(&format!("omnipty-{}", route.unwrap_or(0)));
            let request: Id = msg_send![class!(UNNotificationRequest), requestWithIdentifier: ident content: content trigger: std::ptr::null::<Object>()];
            let on_added = ConcreteBlock::new(move |_error: Id| {}).copy();
            let _: () = msg_send![center, addNotificationRequest: request withCompletionHandler: &*on_added];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DurationText;

    fn cfg() -> NotificationsConfig {
        NotificationsConfig::default()
    }

    fn finished(secs: u64, exit: i32, focused: bool) -> Finished {
        Finished {
            duration: Duration::from_secs(secs),
            exit: Some(exit),
            pane_focused: focused,
        }
    }

    #[test]
    fn long_commands_notify_only_when_unfocused() {
        assert!(should_notify(&cfg(), finished(45, 0, false)));
        assert!(!should_notify(&cfg(), finished(45, 0, true)));
        assert!(
            !should_notify(&cfg(), finished(5, 0, false)),
            "under the threshold"
        );
        assert!(
            should_notify(&cfg(), finished(30, 0, false)),
            "threshold is inclusive"
        );
    }

    #[test]
    fn failures_respect_on_failure_always() {
        assert!(
            !should_notify(&cfg(), finished(2, 1, false)),
            "short failure, flag off"
        );
        let mut c = cfg();
        c.on_failure_always = true;
        assert!(should_notify(&c, finished(2, 1, false)));
        assert!(
            !should_notify(&c, finished(2, 0, false)),
            "short success still quiet"
        );
        assert!(
            !should_notify(&c, finished(2, 1, true)),
            "focused pane still quiet"
        );
    }

    #[test]
    fn disabled_and_focus_override() {
        let mut c = cfg();
        c.enabled = false;
        assert!(!should_notify(&c, finished(600, 1, false)));
        let mut c = cfg();
        c.only_when_unfocused = false;
        c.min_duration = DurationText(Duration::from_secs(1));
        assert!(should_notify(&c, finished(2, 0, true)));
    }

    #[test]
    fn summaries_read_like_the_plan() {
        assert_eq!(
            command_summary("cargo build", Some(0), Duration::from_secs(134)),
            "✓ cargo build — 2m14s"
        );
        assert_eq!(
            command_summary("npm test", Some(1), Duration::from_millis(4200)),
            "✗ npm test — exit 1 · 4.2s"
        );
        assert_eq!(
            command_summary("vim", None, Duration::from_secs(61)),
            "• vim — 1m01s"
        );
    }
}
