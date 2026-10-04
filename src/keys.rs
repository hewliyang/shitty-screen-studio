//! Keyboard shortcuts: captured while recording, shown as a label in the frame.
//! Only shortcuts and special keys are kept, so normal typing is never recorded.
use crate::project::KeyPress;
use anyhow::{Context as _, Result, anyhow, bail};
use objc2_core_foundation::{CFAttributedString, CFDictionary, CFMachPort, CFRunLoop, CFString, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGEvent, CGEventField, CGEventFlags, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGImageAlphaInfo, CGImageByteOrderInfo, CGPreflightListenEventAccess,
    CGRequestListenEventAccess,
};
use objc2_core_text::{CTFont, CTFontUIFontType, CTLine, kCTFontAttributeName};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

/// Seconds a label stays after its last press.
const HOLD: f64 = 1.2;
const FADE_IN: f64 = 0.08;
const FADE_OUT: f64 = 0.25;

/// Name of a key on a US layout, and whether it counts as a shortcut without a modifier.
fn key_name(code: u16) -> Option<(&'static str, bool)> {
    let printable = match code {
        0 => "A", 1 => "S", 2 => "D", 3 => "F", 4 => "H", 5 => "G", 6 => "Z", 7 => "X", 8 => "C", 9 => "V",
        11 => "B", 12 => "Q", 13 => "W", 14 => "E", 15 => "R", 16 => "Y", 17 => "T", 18 => "1", 19 => "2",
        20 => "3", 21 => "4", 22 => "6", 23 => "5", 24 => "=", 25 => "9", 26 => "7", 27 => "-", 28 => "8",
        29 => "0", 30 => "]", 31 => "O", 32 => "U", 33 => "[", 34 => "I", 35 => "P", 37 => "L", 38 => "J",
        39 => "'", 40 => "K", 41 => ";", 42 => "\\", 43 => ",", 44 => "/", 45 => "N", 46 => "M", 47 => ".",
        49 => "Space", 50 => "`", 65 => ".", 67 => "*", 69 => "+", 75 => "/", 78 => "-", 81 => "=",
        82 => "0", 83 => "1", 84 => "2", 85 => "3", 86 => "4", 87 => "5", 88 => "6", 89 => "7", 91 => "8", 92 => "9",
        _ => "",
    };
    if !printable.is_empty() {
        return Some((printable, false));
    }
    let special = match code {
        36 => "⏎", 48 => "⇥", 51 => "⌫", 53 => "⎋", 76 => "⌤", 117 => "⌦", 115 => "↖", 119 => "↘",
        116 => "⇞", 121 => "⇟", 123 => "←", 124 => "→", 125 => "↓", 126 => "↑",
        122 => "F1", 120 => "F2", 99 => "F3", 118 => "F4", 96 => "F5", 97 => "F6", 98 => "F7", 100 => "F8",
        101 => "F9", 109 => "F10", 103 => "F11", 111 => "F12",
        _ => return None,
    };
    Some((special, true))
}

/// Label for a key press, or `None` when it is plain typing.
pub fn label(code: u16, flags: CGEventFlags) -> Option<String> {
    let (key, special) = key_name(code)?;
    let has = |m: CGEventFlags| flags.contains(m);
    let (ctrl, alt, shift, cmd) =
        (has(CGEventFlags::MaskControl), has(CGEventFlags::MaskAlternate), has(CGEventFlags::MaskShift), has(CGEventFlags::MaskCommand));
    if !(ctrl || alt || cmd || special) {
        return None;
    }
    let mods = [(ctrl, "⌃"), (alt, "⌥"), (shift, "⇧"), (cmd, "⌘")];
    let parts: Vec<&str> = mods.iter().filter(|(on, _)| *on).map(|(_, s)| *s).chain([key]).collect();
    Some(parts.join(" "))
}

#[derive(Debug, PartialEq)]
pub struct Shown {
    pub label: String,
    pub alpha: f32,
}

/// The label on screen at source time `t`. Repeats of one shortcut show a count.
pub fn shown_at(keys: &[KeyPress], t: f64) -> Option<Shown> {
    let last = keys.partition_point(|k| k.t <= t).checked_sub(1)?;
    let age = t - keys[last].t;
    if age >= HOLD {
        return None;
    }
    let mut first = last;
    while first > 0 && keys[first - 1].keys == keys[last].keys && keys[first].t - keys[first - 1].t < HOLD {
        first -= 1;
    }
    let replaces = first > 0 && keys[first].t - keys[first - 1].t < HOLD;
    let fade_in = if replaces { 1.0 } else { ((t - keys[first].t) / FADE_IN).min(1.0) };
    let fade_out = ((HOLD - age) / FADE_OUT).min(1.0);
    let n = last - first + 1;
    let label = if n > 1 { format!("{} ×{n}", keys[last].keys) } else { keys[last].keys.clone() };
    Some(Shown { label, alpha: fade_in.min(fade_out) as f32 })
}

/// Asks for Input Monitoring up front. macOS shows its prompt once; after that the user must use System Settings.
pub fn ensure_access() -> Result<()> {
    if CGPreflightListenEventAccess() || CGRequestListenEventAccess() {
        return Ok(());
    }
    bail!("keystroke access is off. Turn on Shitty Screen Studio in System Settings → Privacy & Security → Input Monitoring, then quit and reopen the app.")
}

type Presses = Mutex<Vec<KeyPress>>;

unsafe extern "C-unwind" fn on_key(_: CGEventTapProxy, kind: CGEventType, event: NonNull<CGEvent>, info: *mut c_void) -> *mut CGEvent {
    if kind == CGEventType::KeyDown {
        let ev = Some(unsafe { event.as_ref() });
        let repeat = CGEvent::integer_value_field(ev, CGEventField::KeyboardEventAutorepeat) != 0;
        let code = CGEvent::integer_value_field(ev, CGEventField::KeyboardEventKeycode) as u16;
        if let Some(keys) = label(code, CGEvent::flags(ev)).filter(|_| !repeat) {
            let presses = unsafe { &*(info as *const Presses) };
            presses.lock().unwrap().push(KeyPress { t: crate::capture::host_now(), keys });
        }
    }
    event.as_ptr()
}

/// Listens for shortcuts on its own run loop until `stop` is set. Times are host-clock seconds.
pub fn start(stop: Arc<AtomicBool>) -> Result<JoinHandle<Vec<KeyPress>>> {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let presses: Box<Presses> = Box::default();
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                1 << CGEventType::KeyDown.0,
                Some(on_key),
                &*presses as *const Presses as *mut c_void,
            )
        };
        let mode = unsafe { kCFRunLoopDefaultMode };
        let setup = (|| {
            let tap = tap.context("could not listen for keystrokes; check Input Monitoring in System Settings")?;
            let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0).context("keystroke run loop source")?;
            CFRunLoop::current().context("run loop")?.add_source(Some(&source), mode);
            CGEvent::tap_enable(&tap, true);
            Ok::<_, anyhow::Error>(tap)
        })();
        let tap = match setup {
            Ok(tap) => {
                let _ = tx.send(Ok(()));
                tap
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                return Vec::new();
            }
        };
        while !stop.load(Ordering::Relaxed) {
            CFRunLoop::run_in_mode(mode, 0.05, false);
            if !CGEvent::tap_is_enabled(&tap) {
                CGEvent::tap_enable(&tap, true);
            }
        }
        CGEvent::tap_enable(&tap, false);
        tap.invalidate();
        presses.into_inner().unwrap_or_default()
    });
    rx.recv().map_err(|_| anyhow!("keystroke listener stopped"))??;
    Ok(handle)
}

/// A label drawn white on clear by Core Text. Only alpha matters; the shader adds the colour.
pub struct Bitmap {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Lays the label out so the cap height sits at the vertical centre of the bitmap.
pub fn rasterize(label: &str, size: f32) -> Option<Bitmap> {
    unsafe {
        let font = CTFont::new_ui_font_for_language(CTFontUIFontType::EmphasizedSystem, size as f64, None)?;
        let attrs = CFDictionary::<CFString, CTFont>::from_slices(&[kCTFontAttributeName], &[&font]);
        let text = CFAttributedString::new(None, Some(&CFString::from_str(label)), Some(attrs.as_opaque()))?;
        let line = CTLine::with_attributed_string(&text);
        let advance = line.typographic_bounds(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        let (ascent, descent, cap) = (font.ascent(), font.descent(), font.cap_height());
        let pad = 2.0;
        let half = (ascent - cap / 2.0).max(descent + cap / 2.0);
        let width = (advance.ceil() + 2.0 * pad) as u32;
        let height = (2.0 * (half + pad)).ceil() as u32;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        let space = CGColorSpace::new_device_rgb()?;
        let ctx = CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            width as usize,
            height as usize,
            8,
            width as usize * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
        )?;
        CGContext::set_rgb_fill_color(Some(&ctx), 1.0, 1.0, 1.0, 1.0);
        CGContext::set_text_position(Some(&ctx), pad, height as f64 / 2.0 - cap / 2.0);
        line.draw(&ctx);
        drop(ctx);
        Some(Bitmap { rgba, width, height })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(t: f64, keys: &str) -> KeyPress {
        KeyPress { t, keys: keys.into() }
    }

    #[test]
    fn plain_typing_is_dropped_and_shortcuts_are_kept() {
        assert_eq!(label(0, CGEventFlags::empty()), None);
        assert_eq!(label(0, CGEventFlags::MaskShift), None);
        assert_eq!(label(49, CGEventFlags::empty()), None);
        assert_eq!(label(8, CGEventFlags::MaskCommand).as_deref(), Some("⌘ C"));
        assert_eq!(label(35, CGEventFlags::MaskCommand | CGEventFlags::MaskShift).as_deref(), Some("⇧ ⌘ P"));
        assert_eq!(label(51, CGEventFlags::empty()).as_deref(), Some("⌫"));
        assert_eq!(label(123, CGEventFlags::MaskSecondaryFn).as_deref(), Some("←"));
    }

    #[test]
    fn labels_fade_count_repeats_and_replace_each_other() {
        let keys = [press(1.0, "⌘ Z"), press(1.5, "⌘ Z"), press(2.0, "⌘ C"), press(5.0, "⌫")];
        assert_eq!(shown_at(&keys, 0.5), None);
        assert!(shown_at(&keys, 1.02).unwrap().alpha < 1.0);
        assert_eq!(shown_at(&keys, 1.6).unwrap(), Shown { label: "⌘ Z ×2".into(), alpha: 1.0 });
        assert_eq!(shown_at(&keys, 2.01).unwrap(), Shown { label: "⌘ C".into(), alpha: 1.0 });
        assert!(shown_at(&keys, 3.1).unwrap().alpha < 1.0);
        assert_eq!(shown_at(&keys, 3.3), None);
    }
}
