//! Global keyboard/mouse listener (replaces the Windows build's rdev/low-level hooks).
//!
//! Emits the same events the frontend listens for: `key-press` / `key-release` (key name),
//! `mouse-move` {x, y, memberId?} in physical screen px, `mouse-button` {button, action, memberId?}.

use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType, EventField,
};
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        #[cfg(debug_assertions)]
        if std::env::var("WUWA_FORCE_PERMISSION_DIALOG").is_ok() {
            crate::explain_input_monitoring(&app);
        }
        // A listen-only tap is created even without permission; macOS then silently drops every
        // event. So check the permission itself and ask for it, instead of trusting tap creation.
        if !unsafe { CGPreflightListenEventAccess() } {
            log::warn!("[input] Input Monitoring not granted; requesting it");
            unsafe { CGRequestListenEventAccess() };
            // macOS shows its own prompt only once per app; anyone who missed it gets stuck
            // (the app is not even listed in Settings), so explain and offer to open the pane.
            std::thread::sleep(Duration::from_millis(1500));
            if !unsafe { CGPreflightListenEventAccess() } {
                crate::explain_input_monitoring(&app);
            }
            while !unsafe { CGPreflightListenEventAccess() } {
                std::thread::sleep(Duration::from_secs(2));
            }
            log::info!("[input] Input Monitoring granted");
        }
        while !run_tap(&app) {
            log::warn!("[input] event tap unavailable; retrying");
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

/// Returns false when the tap could not be created (no permission yet).
fn run_tap(app: &AppHandle) -> bool {
    use CGEventType::*;
    let held_modifiers: Mutex<HashSet<i64>> = Mutex::new(HashSet::new());
    let pressed_on_pet = AtomicBool::new(false);
    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::TailAppendEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            KeyDown, KeyUp, FlagsChanged, MouseMoved, LeftMouseDragged, RightMouseDragged,
            OtherMouseDragged, LeftMouseDown, LeftMouseUp, RightMouseDown, RightMouseUp,
            OtherMouseDown, OtherMouseUp,
        ],
        |_proxy, ty, event| {
            match ty {
                KeyDown | KeyUp => {
                    if matches!(ty, KeyDown)
                        && event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0
                    {
                        return None;
                    }
                    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    let name = key_name(code);
                    let ev = if matches!(ty, KeyDown) { "key-press" } else { "key-release" };
                    let _ = app.emit(ev, name);
                }
                FlagsChanged => {
                    // Modifiers only arrive as flag changes; toggle per key code.
                    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    let mut held = held_modifiers.lock().unwrap();
                    let ev = if held.remove(&code) {
                        "key-release"
                    } else {
                        held.insert(code);
                        "key-press"
                    };
                    let _ = app.emit(ev, key_name(code));
                }
                MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
                    let p = event.location();
                    let (x, y, member, _) = crate::on_pointer(app, p.x, p.y);
                    let _ = app.emit("mouse-move", with_member(json!({ "x": x, "y": y }), member));
                }
                _ => {
                    let (button, action) = match ty {
                        LeftMouseDown => ("left", "press"),
                        LeftMouseUp => ("left", "release"),
                        RightMouseDown => ("right", "press"),
                        RightMouseUp => ("right", "release"),
                        OtherMouseDown => ("middle", "press"),
                        OtherMouseUp => ("middle", "release"),
                        _ => return None,
                    };
                    let p = event.location();
                    let (_, _, member, on_pet) = crate::on_pointer(app, p.x, p.y);
                    if action == "press" {
                        if on_pet {
                            crate::remember_focus_owner();
                        }
                        pressed_on_pet.store(on_pet, Ordering::Relaxed);
                    } else if pressed_on_pet.swap(false, Ordering::Relaxed) {
                        crate::yield_focus_after_pet_click(app);
                    }
                    let _ = app.emit(
                        "mouse-button",
                        with_member(json!({ "button": button, "action": action }), member),
                    );
                }
            }
            None
        },
    );
    let Ok(tap) = tap else { return false };
    let Ok(source) = tap.mach_port.create_runloop_source(0) else { return false };
    unsafe { CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes) };
    tap.enable();
    log::info!("[input] global event tap running");
    CFRunLoop::run_current();
    true
}

fn with_member(mut v: serde_json::Value, member: Option<serde_json::Value>) -> serde_json::Value {
    if let Some(m) = member {
        v["memberId"] = m;
    }
    v
}

/// macOS virtual key code -> rdev-style key name (F-keys must be "F1".."F12": the frontend's hotkeys).
fn key_name(code: i64) -> String {
    let s = match code {
        0 => "KeyA", 1 => "KeyS", 2 => "KeyD", 3 => "KeyF", 4 => "KeyH", 5 => "KeyG", 6 => "KeyZ",
        7 => "KeyX", 8 => "KeyC", 9 => "KeyV", 11 => "KeyB", 12 => "KeyQ", 13 => "KeyW",
        14 => "KeyE", 15 => "KeyR", 16 => "KeyY", 17 => "KeyT", 18 => "Num1", 19 => "Num2",
        20 => "Num3", 21 => "Num4", 22 => "Num6", 23 => "Num5", 24 => "Equal", 25 => "Num9",
        26 => "Num7", 27 => "Minus", 28 => "Num8", 29 => "Num0", 30 => "RightBracket",
        31 => "KeyO", 32 => "KeyU", 33 => "LeftBracket", 34 => "KeyI", 35 => "KeyP",
        36 => "Return", 37 => "KeyL", 38 => "KeyJ", 39 => "Quote", 40 => "KeyK",
        41 => "SemiColon", 42 => "BackSlash", 43 => "Comma", 44 => "Slash", 45 => "KeyN",
        46 => "KeyM", 47 => "Dot", 48 => "Tab", 49 => "Space", 50 => "BackQuote",
        51 => "Backspace", 53 => "Escape", 54 => "MetaRight", 55 => "MetaLeft",
        56 => "ShiftLeft", 57 => "CapsLock", 58 => "Alt", 59 => "ControlLeft",
        60 => "ShiftRight", 61 => "AltGr", 62 => "ControlRight", 63 => "Function",
        65 => "KpDelete", 67 => "KpMultiply", 69 => "KpPlus", 71 => "NumLock", 75 => "KpDivide",
        76 => "KpReturn", 78 => "KpMinus", 82 => "Kp0", 83 => "Kp1", 84 => "Kp2", 85 => "Kp3",
        86 => "Kp4", 87 => "Kp5", 88 => "Kp6", 89 => "Kp7", 91 => "Kp8", 92 => "Kp9",
        96 => "F5", 97 => "F6", 98 => "F7", 99 => "F3", 100 => "F8", 101 => "F9", 103 => "F11",
        109 => "F10", 111 => "F12", 114 => "Insert", 115 => "Home", 116 => "PageUp",
        117 => "Delete", 118 => "F4", 119 => "End", 120 => "F2", 121 => "PageDown", 122 => "F1",
        123 => "LeftArrow", 124 => "RightArrow", 125 => "DownArrow", 126 => "UpArrow",
        _ => return format!("Unknown({code})"),
    };
    s.to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hotkey_names_match_frontend() {
        // Frontend hotkeys compare against "F4" / "F5" / "F9".
        assert_eq!(super::key_name(118), "F4");
        assert_eq!(super::key_name(96), "F5");
        assert_eq!(super::key_name(101), "F9");
        assert_eq!(super::key_name(200), "Unknown(200)");
    }
}
