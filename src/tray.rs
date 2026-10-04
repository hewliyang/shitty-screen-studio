//! Menu bar stop button, stop shortcut, and helpers that keep our recording chrome visible but out of captures.
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSCellImagePosition, NSFont, NSImage, NSStatusBar, NSStatusItem, NSView, NSWindowSharingType};
use objc2_foundation::NSString;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// ⇧⌘2 stops a recording without moving the cursor.
pub const STOP_SHORTCUT: &str = "⇧⌘2";
const STOP_KEY_CODE: u16 = 19;

/// The stop shortcut as the keystroke overlay would label it, so it can be left out of the video.
pub fn is_stop_shortcut(label: &str) -> bool {
    use objc2_core_graphics::CGEventFlags;
    crate::keys::label(STOP_KEY_CODE, CGEventFlags::MaskShift | CGEventFlags::MaskCommand).is_some_and(|l| l == label)
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Arc<AtomicBool>]
    struct StopTarget;

    impl StopTarget {
        #[unsafe(method(stop:))]
        fn stop(&self, _sender: Option<&AnyObject>) {
            self.ivars().store(true, Ordering::Relaxed);
        }
    }
);

pub struct StatusItem {
    item: Retained<NSStatusItem>,
    _target: Retained<StopTarget>,
    _hotkey: Option<StopHotKey>,
    mtm: MainThreadMarker,
}

impl StatusItem {
    pub fn new(stop: Arc<AtomicBool>) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let target: Retained<StopTarget> = {
            let this = StopTarget::alloc(mtm).set_ivars(stop);
            unsafe { msg_send![super(this), init] }
        };
        let hotkey = StopHotKey::new(target.ivars().clone());
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
        let button = item.button(mtm)?;
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("stop.circle.fill"),
            Some(&NSString::from_str("Stop recording")),
        );
        if let Some(image) = &image {
            image.setTemplate(true);
        }
        button.setImage(image.as_deref());
        button.setImagePosition(NSCellImagePosition::ImageLeading);
        button.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(13.0, 0.0)));
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(stop:)));
        }
        button.setToolTip(Some(&NSString::from_str(&format!("Stop recording ({STOP_SHORTCUT})"))));
        let this = Self { item, _target: target, _hotkey: hotkey, mtm };
        this.set_title("Starting…");
        Some(this)
    }

    pub fn set_title(&self, title: &str) {
        if let Some(button) = self.item.button(self.mtm) {
            button.setTitle(&NSString::from_str(&format!(" {title}")));
        }
    }
}

impl Drop for StatusItem {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

#[repr(C)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

type EventHandler = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(target: *mut c_void, handler: EventHandler, count: usize, types: *const EventTypeSpec, data: *mut c_void, out: *mut *mut c_void) -> i32;
    fn RemoveEventHandler(handler: *mut c_void) -> i32;
    fn RegisterEventHotKey(code: u32, modifiers: u32, id: EventHotKeyId, target: *mut c_void, options: u32, out: *mut *mut c_void) -> i32;
    fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
}

unsafe extern "C" fn on_hotkey(_: *mut c_void, _: *mut c_void, data: *mut c_void) -> i32 {
    let stop = unsafe { &*(data as *const AtomicBool) };
    stop.store(true, Ordering::Relaxed);
    0
}

/// A Carbon hot key: it works from any app and needs no Input Monitoring access.
struct StopHotKey {
    hotkey: *mut c_void,
    handler: *mut c_void,
    _stop: Arc<AtomicBool>,
}

impl StopHotKey {
    fn new(stop: Arc<AtomicBool>) -> Option<Self> {
        const KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
        const HOT_KEY_PRESSED: u32 = 5;
        const CMD: u32 = 1 << 8;
        const SHIFT: u32 = 1 << 9;
        let spec = EventTypeSpec { class: KEYBOARD, kind: HOT_KEY_PRESSED };
        let id = EventHotKeyId { signature: u32::from_be_bytes(*b"SSSt"), id: 1 };
        let (mut handler, mut hotkey) = (std::ptr::null_mut(), std::ptr::null_mut());
        unsafe {
            let target = GetApplicationEventTarget();
            let data = Arc::as_ptr(&stop) as *mut c_void;
            if InstallEventHandler(target, on_hotkey, 1, &spec, data, &mut handler) != 0 {
                return None;
            }
            if RegisterEventHotKey(STOP_KEY_CODE as u32, CMD | SHIFT, id, target, 0, &mut hotkey) != 0 {
                RemoveEventHandler(handler);
                return None;
            }
        }
        Some(Self { hotkey, handler, _stop: stop })
    }
}

impl Drop for StopHotKey {
    fn drop(&mut self) {
        unsafe {
            UnregisterEventHotKey(self.hotkey);
            RemoveEventHandler(self.handler);
        }
    }
}

/// Keeps a GPUI panel on screen while other apps are active, and out of screenshots and captures.
pub fn pin_panel(window: &impl HasWindowHandle) {
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
    if let Some(win) = view.window() {
        win.setHidesOnDeactivate(false);
        win.setSharingType(NSWindowSharingType::None);
    }
}
