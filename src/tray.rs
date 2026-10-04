//! Menu bar stop button, and helpers that keep our recording chrome visible but out of captures.
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSCellImagePosition, NSFont, NSImage, NSStatusBar, NSStatusItem, NSView, NSWindowSharingType};
use objc2_foundation::NSString;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    mtm: MainThreadMarker,
}

impl StatusItem {
    pub fn new(stop: Arc<AtomicBool>) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let target: Retained<StopTarget> = {
            let this = StopTarget::alloc(mtm).set_ivars(stop);
            unsafe { msg_send![super(this), init] }
        };
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
        button.setToolTip(Some(&NSString::from_str("Stop recording")));
        let this = Self { item, _target: target, mtm };
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
