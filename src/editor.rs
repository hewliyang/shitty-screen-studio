use crate::app::Studio;
use crate::edit::{MIN_SLICE, SPEEDS, Timeline};
use crate::export::{ExportJob, ExportSettings, export};
use crate::motion::Motion;
use crate::player::{Player, Scene};
use crate::project::{Click, Project, ZoomSegment};
use crate::style::{Aspect, BACKGROUNDS, Style, WALLPAPERS, wallpaper_file};
use crate::theme::*;
use gpui::{
    AnyElement, Bounds, Context, FocusHandle, FontWeight, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, Task, WeakEntity, Window,
    canvas, div, img, linear_color_stop, linear_gradient, prelude::*, px, relative, rgb, surface,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Knob {
    Padding,
    Radius,
    Shadow,
    Zoom,
    CursorSize,
    Smoothing,
    MotionBlur,
    CameraSpeed,
    CameraSize,
}

impl Knob {
    fn label(self) -> &'static str {
        match self {
            Knob::Padding => "Padding",
            Knob::Radius => "Roundness",
            Knob::Shadow => "Shadow",
            Knob::Zoom => "Zoom level",
            Knob::CursorSize => "Cursor size",
            Knob::Smoothing => "Cursor smoothing",
            Knob::MotionBlur => "Motion blur",
            Knob::CameraSpeed => "Camera speed",
            Knob::CameraSize => "Size",
        }
    }

    fn range(self) -> (f32, f32) {
        match self {
            Knob::Padding => (0.0, 240.0),
            Knob::Radius => (0.0, 60.0),
            Knob::Shadow => (0.0, 1.0),
            Knob::Zoom => (1.1, 4.0),
            Knob::CursorSize => (0.5, 4.0),
            Knob::Smoothing | Knob::MotionBlur | Knob::CameraSpeed => (0.0, 1.0),
            Knob::CameraSize => (0.1, 0.5),
        }
    }

    fn format(self, v: f32) -> String {
        match self {
            Knob::Padding | Knob::Radius => format!("{v:.0}px"),
            Knob::Shadow | Knob::Smoothing | Knob::MotionBlur | Knob::CameraSize => format!("{:.0}%", v * 100.0),
            Knob::CameraSpeed => match v {
                v if v < 0.2 => "Slow".into(),
                v if v < 0.4 => "Relaxed".into(),
                v if v <= 0.6 => "Default".into(),
                v if v <= 0.8 => "Snappy".into(),
                _ => "Fast".into(),
            },
            Knob::Zoom | Knob::CursorSize => format!("{v:.1}×"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    Scrub,
    Knob(Knob),
    ZoomStart(usize),
    ZoomEnd(usize),
    ZoomMove { index: usize, grab: f64 },
    SliceEdge { index: usize, start: bool, x0: Pixels, v0: f64, sec_per_px: f64 },
}

const MIN_ZOOM_LEN: f64 = 0.4;

pub struct Editor {
    project: Project,
    style: Style,
    zooms: Vec<ZoomSegment>,
    clicks: Vec<Click>,
    timeline: Timeline,
    player: Player,
    seen_frames: u64,
    selected: Option<usize>,
    selected_slice: Option<usize>,
    history: Vec<(Vec<ZoomSegment>, Timeline)>,
    drag: Option<Drag>,
    export_4k: bool,
    export_fps: u32,
    export: Option<ExportJob>,
    export_msg: Option<(String, Option<PathBuf>)>,
    track_bounds: Rc<RefCell<Bounds<Pixels>>>,
    knob_bounds: Rc<RefCell<HashMap<Knob, Bounds<Pixels>>>>,
    focus: FocusHandle,
    studio: WeakEntity<Studio>,
    thumbs: Vec<Option<PathBuf>>,
    _thumbs: Task<()>,
    _poll: Task<()>,
}

impl Editor {
    pub fn new(project: Project, studio: WeakEntity<Studio>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let style = project.rec.style.unwrap_or_default();
        let zooms = project.rec.zooms.clone().unwrap_or_else(|| project.rec.auto_zooms(style.zoom));
        let clicks = project.rec.clicks();
        let timeline = project.rec.timeline();
        let scene = Arc::new(Scene {
            style,
            motion: Motion::build(&project.rec.cursor, &zooms, project.rec.duration, &style),
            clicks: clicks.clone(),
            timeline: timeline.clone(),
        });
        let player = Player::new(&project, scene);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        #[cfg(feature = "snapshot")]
        if let Some(t) = std::env::var("SSS_SEEK").ok().and_then(|s| s.parse().ok()) {
            player.seek(t);
        }

        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(8)).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        });

        let thumbs_task = cx.spawn(async move |this, cx| {
            let thumbs = cx
                .background_executor()
                .spawn(async { (0..WALLPAPERS.len()).map(|i| wallpaper_file(i, true)).collect::<Vec<_>>() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.thumbs = thumbs;
                cx.notify();
            });
            // Full-size copies are converted ahead so picking one feels instant.
            cx.background_executor()
                .spawn(async {
                    for i in 0..WALLPAPERS.len() {
                        wallpaper_file(i, false);
                    }
                })
                .await;
        });

        Self {
            project,
            style,
            zooms,
            clicks,
            timeline,
            player,
            seen_frames: 0,
            selected: None,
            selected_slice: None,
            history: Vec::new(),
            drag: None,
            export_4k: false,
            export_fps: 60,
            export: None,
            export_msg: None,
            track_bounds: Rc::default(),
            knob_bounds: Rc::default(),
            focus,
            studio,
            thumbs: Vec::new(),
            _thumbs: thumbs_task,
            _poll: poll,
        }
    }

    /// Length of the edited output.
    fn duration(&self) -> f64 {
        self.timeline.duration().max(0.001)
    }

    fn src(&self, t: f64) -> f64 {
        self.timeline.to_source(t)
    }

    fn out(&self, src: f64) -> f64 {
        self.timeline.to_output(src)
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        self.player.tick();
        let frames = self.player.shared.frames.load(Ordering::Relaxed);
        let mut changed = frames != self.seen_frames;
        self.seen_frames = frames;
        if let Some(job) = &self.export {
            let done = job.result.lock().unwrap().take();
            if let Some(result) = done {
                self.export_msg = Some(match result {
                    Ok(()) => ("Export finished".into(), job_path(job)),
                    Err(e) => (format!("Export failed: {e}"), None),
                });
                self.export = None;
            }
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn refresh(&mut self) {
        let scene = Scene {
            style: self.style,
            motion: Motion::build(&self.project.rec.cursor, &self.zooms, self.project.rec.duration, &self.style),
            clicks: self.clicks.clone(),
            timeline: self.timeline.clone(),
        };
        self.player.update(Arc::new(scene));
    }

    fn persist(&mut self) {
        self.project.rec.zooms = Some(self.zooms.clone());
        self.project.rec.style = Some(self.style);
        self.project.rec.timeline = Some(self.timeline.clone());
        let _ = self.project.save();
    }

    /// Saves the timeline edits so the next change can be undone.
    fn checkpoint(&mut self) {
        self.history.push((self.zooms.clone(), self.timeline.clone()));
        if self.history.len() > 100 {
            self.history.remove(0);
        }
    }

    fn undo(&mut self) {
        let Some((zooms, timeline)) = self.history.pop() else { return };
        self.zooms = zooms;
        self.timeline = timeline;
        self.selected = None;
        self.selected_slice = None;
        self.refresh();
        self.persist();
    }

    fn select_zoom(&mut self, i: Option<usize>) {
        self.selected = i;
        if i.is_some() {
            self.selected_slice = None;
        }
    }

    fn select_slice(&mut self, i: Option<usize>) {
        self.selected_slice = i;
        if i.is_some() {
            self.selected = None;
        }
    }

    fn split(&mut self) {
        let mut next = self.timeline.clone();
        if let Some(i) = next.split(self.player.position()) {
            self.checkpoint();
            self.timeline = next;
            self.select_slice(Some(i));
            self.refresh();
            self.persist();
        }
    }

    fn remove_slice(&mut self, i: usize) {
        let mut next = self.timeline.clone();
        if !next.remove(i) {
            return;
        }
        self.checkpoint();
        self.timeline = next;
        self.selected_slice = None;
        self.refresh();
        self.player.seek(self.timeline.bounds(i.min(self.timeline.slices.len() - 1)).0);
        self.persist();
    }

    fn set_speed(&mut self, i: usize, speed: f64) {
        if self.timeline.slices.get(i).is_none_or(|s| s.speed == speed) {
            return;
        }
        let src = self.src(self.player.position());
        self.checkpoint();
        self.timeline.slices[i].speed = speed;
        self.refresh();
        self.player.seek(self.out(src));
        self.persist();
    }

    fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.player.pause();
        } else {
            self.player.play();
        }
    }

    fn time_at(&self, x: Pixels) -> f64 {
        let b = *self.track_bounds.borrow();
        let w = f32::from(b.size.width).max(1.0);
        let frac = (f32::from(x - b.origin.x) / w).clamp(0.0, 1.0);
        frac as f64 * self.duration()
    }

    fn knob_value(&self, knob: Knob) -> f32 {
        match knob {
            Knob::Padding => self.style.padding,
            Knob::Radius => self.style.radius,
            Knob::Shadow => self.style.shadow,
            Knob::Zoom => self.selected.and_then(|i| self.zooms.get(i)).map(|z| z.scale).unwrap_or(self.style.zoom),
            Knob::CursorSize => self.style.cursor_size,
            Knob::Smoothing => self.style.cursor_smoothing,
            Knob::MotionBlur => self.style.motion_blur,
            Knob::CameraSpeed => self.style.camera_speed,
            Knob::CameraSize => self.style.camera_size,
        }
    }

    fn set_knob(&mut self, knob: Knob, x: Pixels) {
        let Some(b) = self.knob_bounds.borrow().get(&knob).copied() else { return };
        let frac = (f32::from(x - b.origin.x) / f32::from(b.size.width).max(1.0)).clamp(0.0, 1.0);
        let (lo, hi) = knob.range();
        let v = lo + (hi - lo) * frac;
        match knob {
            Knob::Padding => self.style.padding = v.round(),
            Knob::Radius => self.style.radius = v.round(),
            Knob::Shadow => self.style.shadow = v,
            Knob::Zoom => {
                let v = (v * 10.0).round() / 10.0;
                match self.selected.and_then(|i| self.zooms.get_mut(i)) {
                    Some(z) => z.scale = v,
                    None => {
                        self.style.zoom = v;
                        self.zooms.iter_mut().for_each(|z| z.scale = v);
                    }
                }
            }
            Knob::CursorSize => self.style.cursor_size = (v * 10.0).round() / 10.0,
            Knob::Smoothing => self.style.cursor_smoothing = v,
            Knob::MotionBlur => self.style.motion_blur = v,
            Knob::CameraSpeed => self.style.camera_speed = (v * 20.0).round() / 20.0,
            Knob::CameraSize => self.style.camera_size = (v * 100.0).round() / 100.0,
        }
        self.refresh();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_drag();
            cx.notify();
            return;
        }
        let dur = self.duration();
        match drag {
            Drag::Scrub => self.player.seek(self.time_at(ev.position.x)),
            Drag::Knob(k) => self.set_knob(k, ev.position.x),
            Drag::ZoomStart(i) => {
                let t = self.src(self.time_at(ev.position.x));
                if let Some(z) = self.zooms.get_mut(i) {
                    z.start = t.min(z.end - MIN_ZOOM_LEN).max(0.0);
                }
                self.refresh();
            }
            Drag::ZoomEnd(i) => {
                let t = self.src(self.time_at(ev.position.x));
                let src_dur = self.project.rec.duration;
                if let Some(z) = self.zooms.get_mut(i) {
                    z.end = t.max(z.start + MIN_ZOOM_LEN).min(src_dur);
                }
                self.refresh();
            }
            Drag::ZoomMove { index, grab } => {
                let t = self.time_at(ev.position.x);
                if let Some(z) = self.zooms.get(index).copied() {
                    let len = self.out(z.end) - self.out(z.start);
                    let start = (t - grab).clamp(0.0, (dur - len).max(0.0));
                    let (s, e) = (self.src(start), self.src(start + len));
                    if let Some(z) = self.zooms.get_mut(index) {
                        z.start = s;
                        z.end = e.max(s + MIN_ZOOM_LEN);
                    }
                }
                self.refresh();
            }
            Drag::SliceEdge { index, start, x0, v0, sec_per_px } => {
                let (lo, hi) = self.timeline.edge_limits(index, self.project.rec.duration);
                if let Some(s) = self.timeline.slices.get_mut(index) {
                    let v = v0 + f32::from(ev.position.x - x0) as f64 * sec_per_px * s.speed;
                    if start {
                        s.start = v.clamp(lo, s.end - MIN_SLICE);
                    } else {
                        s.end = v.clamp(s.start + MIN_SLICE, hi);
                    }
                }
                self.refresh();
            }
        }
        cx.notify();
    }

    fn end_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            if !matches!(drag, Drag::Scrub) {
                if let Some(sel) = self.selected {
                    let chosen = self.zooms.get(sel).copied();
                    self.zooms.sort_by(|a, b| a.start.total_cmp(&b.start));
                    self.selected = chosen.and_then(|c| self.zooms.iter().position(|z| *z == c));
                }
                if matches!(drag, Drag::SliceEdge { .. }) {
                    self.player.seek(self.player.position());
                }
                self.persist();
            }
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let fps = crate::player::PREVIEW_FPS as f64;
        let cmd = ev.keystroke.modifiers.platform;
        match ev.keystroke.key.as_str() {
            "z" if cmd => self.undo(),
            _ if cmd => return,
            "space" => self.toggle_play(),
            "s" => self.split(),
            "backspace" | "delete" => self.remove_selected(),
            "left" => self.player.seek((self.player.position() - 1.0 / fps).max(0.0)),
            "right" => self.player.seek((self.player.position() + 1.0 / fps).min(self.duration())),
            "escape" => {
                self.selected = None;
                self.selected_slice = None;
            }
            _ => return,
        }
        cx.notify();
    }

    fn add_zoom(&mut self) {
        let dur = self.duration();
        let from = self.player.position().min((dur - 2.0).max(0.0));
        let (start, end) = (self.src(from), self.src((from + 2.0).min(dur)));
        if end - start < MIN_ZOOM_LEN {
            return;
        }
        self.checkpoint();
        self.zooms.push(ZoomSegment { start, end, scale: self.style.zoom });
        self.zooms.sort_by(|a, b| a.start.total_cmp(&b.start));
        let i = self.zooms.iter().position(|z| z.start == start);
        self.select_zoom(i);
        self.refresh();
        self.persist();
    }

    fn remove_selected(&mut self) {
        if let Some(i) = self.selected.take() {
            if i < self.zooms.len() {
                self.checkpoint();
                self.zooms.remove(i);
                self.refresh();
                self.persist();
            }
        } else if let Some(i) = self.selected_slice {
            self.remove_slice(i);
        }
    }

    fn start_export(&mut self, cx: &mut Context<Self>) {
        if self.export.is_some() {
            return;
        }
        let dir = dirs::desktop_dir().or_else(dirs::home_dir).unwrap_or_default();
        let name = format!(
            "{}.mp4",
            self.project.dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or("recording".into())
        );
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.run_export(path);
                cx.notify();
            });
        })
        .detach();
    }

    fn run_export(&mut self, path: PathBuf) {
        let job = ExportJob {
            progress: Arc::new(AtomicU32::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            result: Arc::new(Mutex::new(None)),
            path: path.clone(),
        };
        let (progress, cancel, result) = (job.progress.clone(), job.cancel.clone(), job.result.clone());
        let (project, rec, style, zooms, timeline) =
            (self.project.clone(), self.project.rec.clone(), self.style, self.zooms.clone(), self.timeline.clone());
        let short = if self.export_4k { 2160 } else { 1080 };
        let (width, height) = crate::style::canvas_size(self.style.aspect, self.project.rec.width, self.project.rec.height, short);
        let settings = ExportSettings { width, height, fps: self.export_fps };
        std::thread::spawn(move || {
            let r = export(&project, &rec, &style, &zooms, &timeline, &path, &settings, &progress, &cancel);
            *result.lock().unwrap() = Some(r.map_err(|e| format!("{e:#}")));
        });
        self.export_msg = None;
        self.export = Some(job);
    }

    fn go_home(&mut self, cx: &mut Context<Self>) {
        self.player.pause();
        let studio = self.studio.clone();
        cx.defer(move |cx| {
            let _ = studio.update(cx, |s, cx| s.go_home(cx));
        });
    }

    fn render_topbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let rec = &self.project.rec;
        let name = self.project.dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let title = name.strip_prefix("recording-").and_then(|s| s.parse::<u64>().ok()).map(fmt_date).unwrap_or(name);
        let subtitle = format!("{} · {}×{} · {} fps", fmt_duration(self.timeline.duration()), rec.width, rec.height, rec.fps);
        let export_area: AnyElement = if let Some(job) = &self.export {
            let p = job.progress.load(Ordering::Relaxed) as f32 / 1000.0;
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(small(format!("Exporting… {:.0}%", p * 100.0)).font_features(tabular()))
                .child(
                    div()
                        .w(px(140.))
                        .h(px(5.))
                        .rounded_full()
                        .bg(alpha(0xffffff, 0.12))
                        .child(div().h_full().rounded_full().bg(rgb(ACCENT)).w(relative(p))),
                )
                .child(button("cancel-export", "Cancel").on_click(cx.listener(|this, _, _, _| {
                    if let Some(job) = &this.export {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                })))
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap_2()
                .when_some(self.export_msg.clone(), |d, (msg, path)| {
                    d.child(small(msg)).when_some(path, |d, path| {
                        d.child(toolbar_button("reveal", "Show in Finder").text_color(rgb(ACCENT)).on_click(move |_, _, cx| cx.reveal_path(&path)))
                    })
                })
                .child(
                    segments()
                        .child(segment("res-1080", "1080p", !self.export_4k).on_click(cx.listener(|this, _, _, cx| {
                            this.export_4k = false;
                            cx.notify();
                        })))
                        .child(segment("res-4k", "4K", self.export_4k).on_click(cx.listener(|this, _, _, cx| {
                            this.export_4k = true;
                            cx.notify();
                        }))),
                )
                .child(segments().children([30u32, 60, 120].map(|fps| {
                    let (id, label) = match fps {
                        30 => ("fps-30", "30"),
                        60 => ("fps-60", "60"),
                        _ => ("fps-120", "120"),
                    };
                    segment(id, label, self.export_fps == fps).on_click(cx.listener(move |this, _, _, cx| {
                        this.export_fps = fps;
                        cx.notify();
                    }))
                })))
                .child(small("fps").mr_2())
                .child(primary_button("export", "Export…").px(px(14.)).on_click(cx.listener(|this, _, _, cx| this.start_export(cx))))
                .into_any_element()
        };

        div()
            .h(px(52.))
            .flex_none()
            .flex()
            .items_center()
            .pl(px(84.))
            .pr(px(12.))
            .gap_2()
            .border_b_1()
            .border_color(hairline_dark())
            .bg(rgb(CHROME))
            .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, _| {
                if ev.click_count == 2 {
                    window.titlebar_double_click();
                } else {
                    window.start_window_move();
                }
            })
            .child(
                toolbar_button("home", "‹")
                    .w(px(28.))
                    .text_size(px(22.))
                    .pb(px(3.))
                    .on_click(cx.listener(|this, _, _, cx| this.go_home(cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::BOLD).text_size(px(BODY)).whitespace_nowrap().child(title))
                    .child(small(subtitle).font_features(tabular()).whitespace_nowrap()),
            )
            .child(div().flex_1())
            .child(export_area)
    }

    fn render_slider(&self, knob: Knob, cx: &mut Context<Self>) -> impl IntoElement {
        let (lo, hi) = knob.range();
        let v = self.knob_value(knob);
        let frac = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
        let bounds = self.knob_bounds.clone();
        let active = matches!(self.drag, Some(Drag::Knob(k)) if k == knob);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_baseline()
                    .text_size(px(BODY))
                    .child(knob.label())
                    .child(small(knob.format(v)).font_features(tabular())),
            )
            .child(
                div()
                    .id(("slider", knob as usize))
                    .relative()
                    .h(px(18.))
                    .cursor_default()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            this.drag = Some(Drag::Knob(knob));
                            this.set_knob(knob, ev.position.x);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(
                        canvas(
                            move |b, _, _| {
                                bounds.borrow_mut().insert(knob, b);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(px(7.))
                            .h(px(4.))
                            .rounded_full()
                            .bg(alpha(0xffffff, 0.14))
                            .child(div().h_full().rounded_full().bg(rgb(ACCENT)).w(relative(frac))),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(1.))
                            .left(relative(frac))
                            .ml(px(-8.))
                            .size(px(16.))
                            .rounded_full()
                            .bg(rgb(if active { 0xffffff } else { 0xdcdcdc }))
                            .border_1()
                            .border_color(alpha(0x000000, 0.25))
                            .shadow_sm(),
                    ),
            )
    }

    fn render_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Style) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(BODY))
            .cursor_default()
            .on_click(cx.listener(move |this, _, _, cx| {
                f(&mut this.style);
                this.refresh();
                this.persist();
                cx.notify();
            }))
            .child(label)
            .child(switch(on))
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let swatches = BACKGROUNDS.iter().enumerate().map(|(i, bg)| {
            let selected = self.style.background == i;
            div()
                .id(("bg", i))
                .w(px(56.))
                .h(px(38.))
                .rounded(px(7.))
                .cursor_default()
                .border_2()
                .border_color(if selected { rgb(ACCENT).into() } else { alpha(0xffffff, 0.08) })
                .bg(linear_gradient(135., linear_color_stop(rgb(bg.from), 0.), linear_color_stop(rgb(bg.to), 1.)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.style.background = i;
                    this.refresh();
                    this.persist();
                    cx.notify();
                }))
        });

        let set_bg = |i: usize| {
            cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                this.style.background = i;
                this.refresh();
                this.persist();
                cx.notify();
            })
        };
        let wallpapers: Vec<_> = (0..WALLPAPERS.len())
            .filter_map(|w| Some((w, self.thumbs.get(w).cloned().flatten()?)))
            .map(|(w, thumb)| {
                let i = BACKGROUNDS.len() + w;
                let selected = self.style.background == i;
                div()
                    .id(("wall", w))
                    .w(px(56.))
                    .h(px(38.))
                    .rounded(px(7.))
                    .overflow_hidden()
                    .cursor_default()
                    .border_2()
                    .border_color(if selected { rgb(ACCENT).into() } else { alpha(0xffffff, 0.08) })
                    .child(img(thumb).size_full().object_fit(ObjectFit::Cover))
                    .on_click(set_bg(i))
            })
            .collect();
        let aspects = Aspect::ALL.map(|a| {
            segment(a.label(), a.label(), self.style.aspect == a).flex_1().on_click(cx.listener(move |this, _, _, cx| {
                this.style.aspect = a;
                this.refresh();
                this.persist();
                cx.notify();
            }))
        });

        let zoom_hint = if self.selected.is_some() { "Applies to the selected zoom" } else { "Applies to all zooms" };

        let section = |title: &'static str| {
            div().flex().flex_col().gap_3().px_4().py(px(14.)).border_b_1().border_color(separator()).child(section_title(title))
        };

        let clip = self.selected_slice.and_then(|i| Some((i, *self.timeline.slices.get(i)?))).map(|(i, s)| {
            let speeds: Vec<_> = SPEEDS
                .iter()
                .enumerate()
                .map(|(k, &v)| {
                    segment(("speed", k), format!("{v}×"), s.speed == v).px_1().text_size(px(SMALL)).flex_1().on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.set_speed(i, v);
                            cx.notify();
                        },
                    ))
                })
                .collect();
            section("Clip")
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(small(format!("Clip {} of {}", i + 1, self.timeline.slices.len())))
                        .child(small(format!("{} → {}", fmt_time(s.end - s.start), fmt_time(s.len_out()))).font_features(tabular())),
                )
                .child(small("Speed"))
                .child(segments().children(speeds))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(button("split-clip", "Split at Playhead").flex_1().on_click(cx.listener(|this, _, _, cx| {
                            this.split();
                            cx.notify();
                        })))
                        .when(self.timeline.slices.len() > 1, |d| {
                            d.child(button("remove-clip", "Remove Clip").flex_1().on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_slice(i);
                                cx.notify();
                            })))
                        }),
                )
        });

        div()
            .id("sidebar")
            .w(px(288.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(hairline_dark())
            .bg(rgb(CHROME))
            .flex()
            .flex_col()
            .children(clip)
            .child(
                section("Background")
                    .when(!wallpapers.is_empty(), |d| d.child(small("macOS Wallpapers")))
                    .child(div().flex().flex_wrap().gap_2().children(wallpapers))
                    .child(small("Gradients"))
                    .child(div().flex().flex_wrap().gap_2().children(swatches)),
            )
            .child(
                section("Frame")
                    .child(segments().children(aspects))
                    .child(self.render_slider(Knob::Padding, cx))
                    .child(self.render_slider(Knob::Radius, cx))
                    .child(self.render_slider(Knob::Shadow, cx)),
            )
            .child(
                section("Zoom")
                    .child(self.render_slider(Knob::Zoom, cx))
                    .child(self.render_slider(Knob::CameraSpeed, cx))
                    .child(self.render_slider(Knob::MotionBlur, cx))
                    .child(small(zoom_hint))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button("auto-zoom", "Auto from Clicks").flex_1().on_click(cx.listener(|this, _, _, cx| {
                                this.checkpoint();
                                this.zooms = this.project.rec.auto_zooms(this.style.zoom);
                                this.selected = None;
                                this.refresh();
                                this.persist();
                                cx.notify();
                            })))
                            .child(button("clear-zoom", "Clear All").flex_1().on_click(cx.listener(|this, _, _, cx| {
                                this.checkpoint();
                                this.zooms.clear();
                                this.selected = None;
                                this.refresh();
                                this.persist();
                                cx.notify();
                            }))),
                    ),
            )
            .child(
                section("Cursor")
                    .child(self.render_slider(Knob::CursorSize, cx))
                    .child(self.render_slider(Knob::Smoothing, cx))
                    .child(self.render_toggle("ripple", "Click ripple", self.style.click_ripple, cx, |s| {
                        s.click_ripple = !s.click_ripple
                    })),
            )
            .when(self.project.rec.camera.is_some(), |d| {
                let corner = self.style.camera_corner;
                let corners: Vec<_> = ["Top Left", "Top Right", "Bottom Left", "Bottom Right"]
                    .into_iter()
                    .enumerate()
                    .map(|(i, label)| {
                        segment(("corner", i), label, corner as usize == i).px_1().text_size(px(SMALL)).flex_1().on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.style.camera_corner = i as u8;
                                this.refresh();
                                this.persist();
                                cx.notify();
                            },
                        ))
                    })
                    .collect();
                d.child(
                    section("Camera")
                        .child(self.render_toggle("cam-visible", "Show camera", self.style.camera_visible, cx, |s| {
                            s.camera_visible = !s.camera_visible
                        }))
                        .child(self.render_slider(Knob::CameraSize, cx))
                        .child(self.render_toggle("cam-circle", "Circle", self.style.camera_circle, cx, |s| {
                            s.camera_circle = !s.camera_circle
                        }))
                        .child(segments().children(corners)),
                )
            })
            .child(
                div()
                    .px_4()
                    .py(px(14.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(
                        [("Play / Pause", "Space"), ("Step frame", "← →"), ("Split clip", "S"), ("Delete selection", "⌫"), ("Undo", "⌘Z")]
                            .map(|(what, key)| div().flex().justify_between().child(small(what)).child(small(key).text_color(rgb(TERTIARY)))),
                    ),
            )
    }

    fn render_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let dur = self.duration();
        let pos = self.player.position().clamp(0.0, dur);
        let playing = self.player.is_playing();
        let frac = |t: f64| relative((t / dur).clamp(0.0, 1.0) as f32);
        let bounds = self.track_bounds.clone();

        let step = if dur <= 15.0 { 1.0 } else if dur <= 60.0 { 5.0 } else if dur <= 300.0 { 15.0 } else { 60.0 };
        let ticks = (0..=((dur / step) as usize)).filter(|i| (*i as f64 * step) < dur - step * 0.5).map(|i| {
            let t = i as f64 * step;
            div()
                .absolute()
                .top_0()
                .left(frac(t))
                .h_full()
                .border_l_1()
                .border_color(separator())
                .pl_1()
                .text_size(px(10.))
                .text_color(rgb(TERTIARY))
                .font_features(tabular())
                .child(fmt_duration(t))
        });

        let click_dots = self.clicks.iter().filter(|c| self.timeline.contains_source(c.t)).map(|c| {
            div()
                .absolute()
                .bottom(px(5.))
                .left(frac(self.out(c.t)))
                .ml(px(-3.))
                .size(px(6.))
                .rounded_full()
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(alpha(ACCENT, 0.8))
        });

        let selected = self.selected;
        let spans: Vec<_> = self.zooms.iter().map(|z| (self.out(z.start), self.out(z.end))).collect();
        let segments = self.zooms.iter().zip(spans).enumerate().filter(|(_, (_, (a, b)))| b - a > 1e-3).map(|(i, (z, (a, b)))| {
            let is_sel = selected == Some(i);
            let edge = |id: &'static str, start: bool| {
                div()
                    .id((id, i))
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .w(px(8.))
                    .when(start, |d| d.left_0())
                    .when(!start, |d| d.right_0())
                    .cursor_ew_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.checkpoint();
                            this.select_zoom(Some(i));
                            this.drag = Some(if start { Drag::ZoomStart(i) } else { Drag::ZoomEnd(i) });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
            };
            div()
                .id(("zoom", i))
                .absolute()
                .top_0()
                .bottom_0()
                .left(frac(a))
                .w(relative(((b - a) / dur) as f32))
                .rounded(px(6.))
                .bg(if is_sel { rgb(PURPLE).into() } else { alpha(PURPLE, 0.7) })
                .border_1()
                .border_color(if is_sel { rgb(0xffffff).into() } else { alpha(0xffffff, 0.15) })
                .when(is_sel, |d| d.border_2())
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(SMALL))
                .font_weight(FontWeight::SEMIBOLD)
                .whitespace_nowrap()
                .text_color(rgb(0xffffff))
                .cursor_grab()
                .child(format!("Zoom {:.1}×", z.scale))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        let t = this.time_at(ev.position.x);
                        let start = this.zooms.get(i).map(|z| this.out(z.start)).unwrap_or(0.0);
                        this.checkpoint();
                        this.select_zoom(Some(i));
                        this.drag = Some(Drag::ZoomMove { index: i, grab: t - start });
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(edge("zoom-start", true))
                .child(edge("zoom-end", false))
        });

        let n_slices = self.timeline.slices.len();
        let slices: Vec<_> = (0..n_slices)
            .map(|i| {
                let s = self.timeline.slices[i];
                let (a, b) = self.timeline.bounds(i);
                let is_sel = self.selected_slice == Some(i);
                let edge = |start: bool| {
                    div()
                        .id(("slice-edge", i * 2 + start as usize))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .w(px(8.))
                        .when(start, |d| d.left_0())
                        .when(!start, |d| d.right_0())
                        .cursor_ew_resize()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                let Some(s) = this.timeline.slices.get(i).copied() else { return };
                                let w = f32::from(this.track_bounds.borrow().size.width).max(1.0) as f64;
                                this.checkpoint();
                                this.select_slice(Some(i));
                                this.drag = Some(Drag::SliceEdge {
                                    index: i,
                                    start,
                                    x0: ev.position.x,
                                    v0: if start { s.start } else { s.end },
                                    sec_per_px: this.duration() / w,
                                });
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                };
                let speed = (s.speed != 1.0).then(|| div().text_color(rgb(0xffd60a)).child(format!("{}×", s.speed)));
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(frac(a))
                    .w(relative(((b - a) / dur) as f32))
                    .px(px(1.5))
                    .child(
                        div()
                            .id(("slice", i))
                            .relative()
                            .size_full()
                            .rounded(px(6.))
                            .overflow_hidden()
                            .bg(linear_gradient(
                                180.,
                                linear_color_stop(rgb(0x264a75), 0.),
                                linear_color_stop(rgb(0x1d3a5e), 1.),
                            ))
                            .border_1()
                            .border_color(if is_sel { rgb(0xffffff).into() } else { alpha(ACCENT, 0.5) })
                            .when(is_sel, |d| d.border_2())
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .whitespace_nowrap()
                            .text_size(px(SMALL))
                            .text_color(rgb(0xd6e8ff))
                            .font_features(tabular())
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(fmt_duration(b - a)))
                            .children(speed)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.select_slice(Some(i));
                                    this.drag = Some(Drag::Scrub);
                                    this.player.seek(this.time_at(ev.position.x));
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            )
                            .child(edge(true))
                            .child(edge(false)),
                    )
            })
            .collect();

        let controls = div()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_baseline()
                    .gap_1()
                    .font_features(tabular())
                    .child(div().text_size(px(15.)).font_weight(FontWeight::MEDIUM).child(fmt_time(pos)))
                    .child(small(format!("/ {}", fmt_time(dur)))),
            )
            .child(
                toolbar_button("play", if playing { "❚❚" } else { "▶" })
                    .w(px(36.))
                    .text_size(px(15.))
                    .text_color(rgb(TEXT))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_play();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(button("split", "✂ Split").on_click(cx.listener(|this, _, _, cx| {
                        this.split();
                        cx.notify();
                    })))
                    .when(self.selected.is_some(), |d| {
                        d.child(button("remove-zoom", "Delete Zoom").on_click(cx.listener(|this, _, _, cx| {
                            this.remove_selected();
                            cx.notify();
                        })))
                    })
                    .child(button("add-zoom", "Add Zoom").on_click(cx.listener(|this, _, _, cx| {
                        this.add_zoom();
                        cx.notify();
                    }))),
            );

        let track = div()
            .id("track")
            .relative()
            .flex()
            .flex_col()
            .gap_1p5()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.selected = None;
                    this.selected_slice = None;
                    this.drag = Some(Drag::Scrub);
                    this.player.seek(this.time_at(ev.position.x));
                    cx.notify();
                }),
            )
            .child(
                canvas(move |b, _, _| *bounds.borrow_mut() = b, |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(div().relative().h(px(16.)).children(ticks))
            .child(div().relative().h(px(40.)).mx(px(-1.5)).children(slices).children(click_dots))
            .child(
                div()
                    .relative()
                    .h(px(30.))
                    .rounded(px(6.))
                    .bg(alpha(0xffffff, 0.04))
                    .when(self.zooms.is_empty(), |d| {
                        d.flex()
                            .items_center()
                            .px_2()
                            .text_size(px(SMALL))
                            .text_color(rgb(TERTIARY))
                            .child("No zooms. Use Add Zoom or Auto from Clicks.")
                    })
                    .children(segments),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(frac(pos))
                    .ml(px(-1.))
                    .w(px(2.))
                    .bg(rgb(RED))
                    .child(div().absolute().top(px(2.)).ml(px(-4.)).w(px(10.)).h(px(12.)).rounded(px(3.)).bg(rgb(RED))),
            );

        div()
            .flex_none()
            .border_t_1()
            .border_color(hairline_dark())
            .bg(rgb(CHROME))
            .px_4()
            .pt_2()
            .pb_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(controls)
            .child(track)
    }
}

fn job_path(job: &ExportJob) -> Option<PathBuf> {
    Some(job.path.clone())
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.player.is_playing() || self.drag.is_some() {
            window.request_animation_frame();
        }

        let preview: AnyElement = match self.player.surface() {
            Some(frame) => surface(frame.to_gpui()).size_full().object_fit(ObjectFit::Contain).into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(MUTED))
                .child("Loading preview…")
                .into_any_element(),
        };

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.end_drag();
                    cx.notify();
                }),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(STAGE))
            .text_size(px(BODY))
            .text_color(rgb(TEXT))
            .child(self.render_topbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("preview")
                            .flex_1()
                            .min_w_0()
                            .p_6()
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus, cx);
                                this.toggle_play();
                                cx.notify();
                            }))
                            .child(preview),
                    )
                    .child(self.render_sidebar(cx)),
            )
            .child(self.render_timeline(cx))
    }
}
