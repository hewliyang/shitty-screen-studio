use gpui::{
    BoxShadow, Div, ElementId, FontFeatures, FontWeight, Hsla, SharedString, Stateful, div, point, prelude::*, px,
    rgb,
};
use std::sync::Arc;

// macOS dark appearance system colors.
pub const BG: u32 = 0x1e1e1e;
pub const STAGE: u32 = 0x161616;
pub const CHROME: u32 = 0x282828;
pub const GROUP: u32 = 0x2c2c2e;
pub const CONTROL: u32 = 0x4a4a4c;
pub const CONTROL_HOVER: u32 = 0x565658;
pub const TEXT: u32 = 0xe5e5e7;
pub const MUTED: u32 = 0x98989d;
pub const TERTIARY: u32 = 0x636366;
pub const ACCENT: u32 = 0x0a84ff;
pub const ACCENT_HOVER: u32 = 0x339bff;
pub const PURPLE: u32 = 0xbf5af2;
pub const RED: u32 = 0xff453a;
pub const RED_HOVER: u32 = 0xff6259;

pub const BODY: f32 = 13.;
pub const SMALL: f32 = 11.;

pub fn alpha(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

pub fn separator() -> Hsla {
    alpha(0xffffff, 0.09)
}

pub fn hairline_dark() -> Hsla {
    alpha(0x000000, 0.55)
}

pub fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

fn control_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow { color: alpha(0x000000, 0.35), offset: point(px(0.), px(0.5)), blur_radius: px(1.), spread_radius: px(0.), inset: false }]
}

pub fn small(text: impl Into<SharedString>) -> Div {
    div().text_size(px(SMALL)).text_color(rgb(MUTED)).child(text.into())
}

fn push_button_base(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap_1()
        .px(px(10.))
        .h(px(24.))
        .rounded(px(6.))
        .bg(rgb(CONTROL))
        .border_t_1()
        .border_color(alpha(0xffffff, 0.12))
        .shadow(control_shadow())
        .text_size(px(BODY))
        .text_color(rgb(TEXT))
        .cursor_default()
        .child(label.into())
}

/// Push button.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    push_button_base(id, label).hover(|s| s.bg(rgb(CONTROL_HOVER))).active(|s| s.bg(rgb(0x5e5e60)))
}

/// Default (accent) push button.
pub fn primary_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    push_button_base(id, label)
        .bg(rgb(ACCENT))
        .border_color(alpha(0xffffff, 0.2))
        .text_color(rgb(0xffffff))
        .font_weight(FontWeight::MEDIUM)
        .hover(|s| s.bg(rgb(ACCENT_HOVER)))
        .active(|s| s.bg(rgb(0x0070e0)))
}

/// Borderless toolbar button: no fill until hovered.
pub fn toolbar_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap_1()
        .px_2()
        .h(px(28.))
        .rounded(px(6.))
        .text_size(px(BODY))
        .text_color(rgb(MUTED))
        .cursor_default()
        .hover(|s| s.bg(alpha(0xffffff, 0.08)).text_color(rgb(TEXT)))
        .active(|s| s.bg(alpha(0xffffff, 0.14)))
        .child(label.into())
}

/// Bold group header, as in inspectors.
pub fn section_title(label: &'static str) -> Div {
    div().text_size(px(BODY)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(label)
}

/// Small grey header above an inset group, as in System Settings and sidebars.
pub fn group_title(label: &'static str) -> Div {
    div().px(px(10.)).text_size(px(SMALL)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(MUTED)).child(label)
}

/// Inset rounded group of rows.
pub fn group() -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .bg(rgb(GROUP))
        .border_1()
        .border_color(alpha(0xffffff, 0.06))
        .overflow_hidden()
}

/// One row in a [`group`]; rows after the first get a hairline on top.
pub fn group_row(first: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .min_h(px(36.))
        .px(px(12.))
        .py(px(6.))
        .text_size(px(BODY))
        .when(!first, |d| d.border_t_1().border_color(separator()))
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0).floor() as u32;
    let s = t - m as f64 * 60.0;
    format!("{m:02}:{s:04.1}")
}

pub fn fmt_duration(t: f64) -> String {
    let s = t.max(0.0).round() as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Localized "Oct 3, 2026 at 9:13 PM" for a Unix timestamp.
pub fn fmt_date(secs: u64) -> String {
    use objc2_foundation::{NSDate, NSDateFormatter, NSDateFormatterStyle};
    let formatter = NSDateFormatter::new();
    formatter.setDateStyle(NSDateFormatterStyle::MediumStyle);
    formatter.setTimeStyle(NSDateFormatterStyle::ShortStyle);
    formatter.setDoesRelativeDateFormatting(true);
    let date = NSDate::dateWithTimeIntervalSince1970(secs as f64);
    formatter.stringFromDate(&date).to_string()
}

/// One option in a segmented control.
pub fn segment(id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(10.))
        .h(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .text_size(px(BODY))
        .cursor_default()
        .when(on, |d| {
            d.bg(rgb(0x636366)).text_color(rgb(0xffffff)).shadow(control_shadow()).border_t_1().border_color(alpha(0xffffff, 0.12))
        })
        .when(!on, |d| d.text_color(rgb(TEXT)).hover(|s| s.bg(alpha(0xffffff, 0.05))))
        .child(label.into())
}

pub fn segments() -> Div {
    div().flex().p(px(2.)).gap(px(1.)).rounded(px(7.)).bg(alpha(0xffffff, 0.08))
}

/// macOS switch.
pub fn switch(on: bool) -> Div {
    div()
        .flex_none()
        .w(px(32.))
        .h(px(19.))
        .rounded_full()
        .p(px(1.5))
        .flex()
        .when(on, |d| d.bg(rgb(ACCENT)).justify_end())
        .when(!on, |d| d.bg(alpha(0xffffff, 0.16)))
        .child(
            div().size(px(16.)).rounded_full().bg(rgb(if on { 0xffffff } else { 0xdcdcdc })).shadow(control_shadow()),
        )
}

/// Checkmark column for single-choice lists.
pub fn check(on: bool) -> Div {
    div()
        .flex_none()
        .w(px(14.))
        .flex()
        .justify_end()
        .text_color(rgb(ACCENT))
        .font_weight(FontWeight::BOLD)
        .when(on, |d| d.child("✓"))
}
