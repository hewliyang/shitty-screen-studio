use crate::capture::{self, RecState, RecordOptions, RecordingHandle, Source, Sources};
use crate::editor::Editor;
use crate::project::{self, Project};
use crate::theme::*;
use crate::demo;
use gpui::{
    AnyElement, Div, App, Bounds, Context, CursorStyle, DisplayId, Entity, FocusHandle, FontWeight, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Task, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img, point, prelude::*, px,
    rgb, size,
};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Display,
    Window,
    Area,
}

struct Recent {
    dir: PathBuf,
    title: String,
    detail: String,
}

fn load_recent() -> Vec<Recent> {
    project::list_recordings()
        .into_iter()
        .map(|dir| {
            let name = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let title = name
                .strip_prefix("recording-")
                .and_then(|s| s.parse::<u64>().ok())
                .map(fmt_date)
                .unwrap_or(name);
            let detail = Project::load(&dir)
                .map(|p| format!("{} · {}×{}", fmt_duration(p.rec.duration), p.rec.width, p.rec.height))
                .unwrap_or_default();
            Recent { dir, title, detail }
        })
        .collect()
}

enum Screen {
    Home,
    Recording,
    Busy(String),
    Editor(Entity<Editor>),
}

pub struct Studio {
    screen: Screen,
    recordings: Vec<Recent>,
    error: Option<String>,
    recorder: Option<Rc<RecordingHandle>>,
    bar: Option<WindowHandle<RecBar>>,
    tray: Option<crate::tray::StatusItem>,
    options: RecordOptions,
    tab: Tab,
    display: Option<u32>,
    window: Option<u32>,
    area: Option<Source>,
    sources: Option<Result<Sources, String>>,
    picker: Option<WindowHandle<AreaPicker>>,
    _sources_task: Option<Task<()>>,
    _task: Option<Task<()>>,
}

impl Studio {
    pub fn new(open: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            screen: Screen::Home,
            recordings: load_recent(),
            error: None,
            recorder: None,
            bar: None,
            tray: None,
            options: RecordOptions::default(),
            tab: Tab::Display,
            display: None,
            window: None,
            area: None,
            sources: None,
            picker: None,
            _sources_task: None,
            _task: None,
        };
        this.load_sources(cx);
        if let Some(dir) = open {
            this.open_project(dir, window, cx);
        }
        this
    }

    fn load_sources(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async { capture::list_sources().map_err(|e| format!("{e:#}")) });
        self._sources_task = Some(cx.spawn(async move |this, cx| {
            let sources = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(s) = &sources {
                    if this.window.is_some_and(|id| !s.windows.iter().any(|w| w.id == id)) {
                        this.window = None;
                    }
                }
                this.sources = Some(sources);
                cx.notify();
            });
        }));
    }

    fn source(&self) -> Option<Source> {
        let main = || {
            self.display.or_else(|| {
                let s = self.sources.as_ref()?.as_ref().ok()?;
                s.displays.first().map(|d| d.id)
            })
        };
        match self.tab {
            Tab::Display => Some(Source::Display(main().unwrap_or_else(|| RecordOptions::default_display()))),
            Tab::Window => self.window.map(Source::Window),
            Tab::Area => self.area,
        }
    }

    fn pick_area(&mut self, cx: &mut Context<Self>) {
        if let Some(old) = self.picker.take() {
            let _ = old.update(cx, |_, window, _| window.remove_window());
        }
        let display = self
            .display
            .map(|id| DisplayId::new(id as u64))
            .and_then(|id| cx.find_display(id))
            .or_else(|| cx.primary_display());
        let Some(display) = display else { return };
        let display_id = display.id();
        let studio = cx.entity().downgrade();
        self.picker = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: display.bounds().size })),
                    display_id: Some(display_id),
                    titlebar: None,
                    focus: true,
                    show: true,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    window_background: WindowBackgroundAppearance::Transparent,
                    ..Default::default()
                },
                |window, cx| {
                    let picker = cx.new(|cx| AreaPicker::new(u64::from(display_id) as u32, studio, cx));
                    let focus = picker.read(cx).focus.clone();
                    window.focus(&focus, cx);
                    picker
                },
            )
            .ok();
    }

    fn area_picked(&mut self, area: Option<Source>, cx: &mut Context<Self>) {
        if let Some(picker) = self.picker.take() {
            let _ = picker.update(cx, |_, window, _| window.remove_window());
        }
        if area.is_some() {
            self.area = area;
        }
        cx.notify();
    }

    pub fn go_home(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Home;
        self.recordings = load_recent();
        cx.notify();
    }

    fn open_project(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match Project::load(&dir) {
            Ok(project) => {
                let studio = cx.entity().downgrade();
                let editor = cx.new(|cx| Editor::new(project, studio, window, cx));
                self.screen = Screen::Editor(editor);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        cx.notify();
    }

    fn open_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = project::library_dir().join("demo");
        self.screen = Screen::Busy("Building demo recording…".into());
        cx.notify();
        let task = cx.background_executor().spawn(async move { demo::create(&dir).map(|p| p.dir) });
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(dir) => this.open_project(dir, window, cx),
                Err(e) => {
                    this.error = Some(format!("{e:#}"));
                    this.go_home(cx);
                }
            });
        }));
    }

    fn start_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.source().is_none() {
            return;
        }
        let dir = match project::new_recording_dir() {
            Ok(d) => d,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
                return;
            }
        };
        let Some(source) = self.source() else { return };
        self.options.source = source;
        let handle = Rc::new(RecordingHandle::start(dir, self.options));
        let bar_handle = handle.clone();
        self.recorder = Some(handle.clone());
        self.screen = Screen::Recording;
        self.error = None;

        let studio = cx.entity().downgrade();
        let bar_size = size(px(236.), px(46.));
        let origin = cx
            .primary_display()
            .map(|d| {
                let b = d.visible_bounds();
                point(b.center().x - bar_size.width / 2., b.bottom() - bar_size.height - px(24.))
            })
            .unwrap_or(point(px(200.), px(60.)));
        self.bar = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds { origin, size: bar_size })),
                    titlebar: None,
                    focus: false,
                    show: true,
                    kind: WindowKind::PopUp,
                    is_movable: true,
                    is_resizable: false,
                    is_minimizable: false,
                    window_background: WindowBackgroundAppearance::Transparent,
                    ..Default::default()
                },
                |window, cx| {
                    crate::tray::pin_panel(window);
                    cx.new(|cx| RecBar::new(bar_handle, studio, cx))
                },
            )
            .ok();
        self.tray = crate::tray::StatusItem::new(handle.stop_flag());
        window.minimize_window();

        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let keep = this.update_in(cx, |this, window, cx| this.poll_recording(window, cx));
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = &self.recorder {
            r.stop();
        }
        cx.notify();
    }

    fn close_bar(&mut self, cx: &mut App) {
        self.tray = None;
        if let Some(bar) = self.bar.take() {
            let _ = bar.update(cx, |_, window, _| window.remove_window());
        }
    }

    fn poll_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(rec) = self.recorder.clone() else { return false };
        match rec.state() {
            RecState::Done(dir) => {
                self.recorder = None;
                self.close_bar(cx);
                window.activate_window();
                self.open_project(dir, window, cx);
                false
            }
            RecState::Failed(e) => {
                self.recorder = None;
                self.close_bar(cx);
                window.activate_window();
                self.error = Some(e);
                self.go_home(cx);
                false
            }
            state => {
                if let Some(tray) = &self.tray {
                    tray.set_title(&match state {
                        RecState::Recording { since } => fmt_time(since.elapsed().as_secs_f64()),
                        RecState::Saving => "Saving…".into(),
                        _ => "Starting…".into(),
                    });
                }
                true
            }
        }
    }

    fn render_home(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .child(self.render_library(cx))
            .child(
                div()
                    .id("home")
                    .flex_1()
                    .h_full()
                    .bg(rgb(BG))
                    .overflow_y_scroll()
                    .flex()
                    .justify_center()
                    .child(self.render_new_recording(cx)),
            )
            .into_any_element()
    }

    fn render_library(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<_> = self
            .recordings
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let dir = r.dir.clone();
                div()
                    .id(("recent", i))
                    .flex()
                    .flex_col()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .cursor_default()
                    .hover(|s| s.bg(alpha(0xffffff, 0.07)))
                    .active(|s| s.bg(alpha(ACCENT, 0.6)))
                    .child(div().text_size(px(BODY)).text_color(rgb(TEXT)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(r.title.clone()))
                    .child(small(r.detail.clone()).font_features(tabular()))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_project(dir.clone(), window, cx)))
            })
            .collect();
        div()
            .w(px(250.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(alpha(0x202020, 0.78))
            .border_r_1()
            .border_color(hairline_dark())
            .child(div().h(px(52.)).flex_none())
            .child(group_title("Recordings").px(px(20.)).pb_1())
            .child(
                div()
                    .id("library")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .when(rows.is_empty(), |d| d.child(small("No recordings yet.").px(px(10.)).py_1()))
                    .children(rows),
            )
            .child(
                div()
                    .flex_none()
                    .p(px(10.))
                    .border_t_1()
                    .border_color(separator())
                    .child(toolbar_button("demo", "Open Demo Recording").on_click(cx.listener(|this, _, window, cx| this.open_demo(window, cx)))),
            )
            .into_any_element()
    }

    fn render_new_recording(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let can_record = self.source().is_some();
        div()
            .w(px(480.))
            .pt(px(64.))
            .pb(px(40.))
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_size(px(22.)).font_weight(FontWeight::BOLD).child("New Recording"))
                    .child(
                        div()
                            .text_size(px(BODY))
                            .text_color(rgb(MUTED))
                            .child("Smooth zooms, a pretty background and a big cursor are added for you."),
                    ),
            )
            .child(self.render_source(cx))
            .child(self.render_inputs(cx))
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .p_3()
                        .rounded(px(10.))
                        .bg(alpha(RED, 0.12))
                        .border_1()
                        .border_color(alpha(RED, 0.35))
                        .text_size(px(BODY))
                        .child(e),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .id("record")
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .h(px(36.))
                            .rounded(px(8.))
                            .bg(rgb(RED))
                            .border_t_1()
                            .border_color(alpha(0xffffff, 0.2))
                            .text_size(px(BODY))
                            .text_color(rgb(0xffffff))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_default()
                            .child(div().size(px(10.)).rounded_full().bg(rgb(0xffffff)))
                            .child("Start Recording")
                            .when(can_record, |d| d.hover(|s| s.bg(rgb(RED_HOVER))))
                            .when(!can_record, |d| d.opacity(0.4))
                            .on_click(cx.listener(|this, _, window, cx| this.start_recording(window, cx))),
                    )
                    .child(
                        small("Records at up to 120 fps. The app hides itself from the capture. Use the floating bar or the menu bar to stop.")
                            .text_color(rgb(TERTIARY))
                            .px(px(2.)),
                    ),
            )
            .into_any_element()
    }

    fn render_source(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = [(Tab::Display, "Display"), (Tab::Window, "Window"), (Tab::Area, "Area")].map(|(tab, label)| {
            segment(label, label, self.tab == tab).flex_1().on_click(cx.listener(move |this, _, _, cx| {
                this.tab = tab;
                if tab == Tab::Window {
                    this.load_sources(cx);
                }
                cx.notify();
            }))
        });
        let row = |id: gpui::ElementId, first: bool, selected: bool, content: Div| {
            group_row(first)
                .id(id)
                .gap_2()
                .cursor_default()
                .hover(|s| s.bg(alpha(0xffffff, 0.04)))
                .child(content.flex_1().min_w_0().flex().items_center().gap_2())
                .child(check(selected))
        };
        let note = |s: String| group().child(group_row(true).child(small(s))).into_any_element();
        let sources = match &self.sources {
            None => Err("Looking for screens and windows…".to_string()),
            Some(Err(e)) => Err(e.clone()),
            Some(Ok(s)) => Ok(s.clone()),
        };
        let body = match (self.tab, sources) {
            (_, Err(msg)) if self.tab != Tab::Area => note(msg),
            (Tab::Display, Ok(s)) => {
                let current = self.display.or(s.displays.first().map(|d| d.id));
                group()
                    .children(s.displays.iter().enumerate().map(|(i, d)| {
                        let id = d.id;
                        let content = div()
                            .child(div().flex_1().child(if d.main { "Main Display".to_string() } else { format!("Display {}", i + 1) }))
                            .child(small(format!("{:.0} × {:.0}", d.width, d.height)).font_features(tabular()));
                        row(("display", i).into(), i == 0, current == Some(id), content)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.display = Some(id);
                                cx.notify();
                            }))
                    }))
                    .into_any_element()
            }
            (Tab::Window, Ok(s)) if s.windows.is_empty() => note("No windows found.".into()),
            (Tab::Window, Ok(s)) => group()
                .child(
                    div().id("windows").max_h(px(260.)).overflow_y_scroll().children(s.windows.iter().enumerate().map(|(i, w)| {
                        let id = w.id;
                        let content = div()
                            .child(match w.icon.clone() {
                                Some(icon) => img(icon).flex_none().size(px(20.)).into_any_element(),
                                None => div().flex_none().size(px(20.)).rounded(px(5.)).bg(alpha(0xffffff, 0.1)).into_any_element(),
                            })
                            .child(div().flex_none().whitespace_nowrap().child(w.app.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(rgb(MUTED))
                                    .child(w.title.clone()),
                            );
                        row(("window", id as usize).into(), i == 0, self.window == Some(id), content)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.window = Some(id);
                                cx.notify();
                            }))
                    })),
                )
                .into_any_element(),
            (_, _) => {
                let label = match self.area {
                    Some(Source::Area { w, h, x, y, .. }) => format!("{w:.0} × {h:.0} at {x:.0}, {y:.0}"),
                    _ => "No area selected".into(),
                };
                group()
                    .child(
                        group_row(true)
                            .child(div().text_color(rgb(if self.area.is_some() { TEXT } else { MUTED })).font_features(tabular()).child(label))
                            .child(
                                button("pick-area", if self.area.is_some() { "Reselect…" } else { "Select Area…" })
                                    .on_click(cx.listener(|this, _, _, cx| this.pick_area(cx))),
                            ),
                    )
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(group_title("Source"))
            .child(segments().children(tabs))
            .child(body)
            .into_any_element()
    }

    fn render_inputs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, label: &'static str, first: bool, on: bool, f: fn(&mut RecordOptions)| {
            group_row(first)
                .id(id)
                .cursor_default()
                .child(label)
                .child(switch(on))
                .on_click(cx.listener(move |this, _, _, cx| {
                    f(&mut this.options);
                    cx.notify();
                }))
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(group_title("Audio & Camera"))
            .child(
                group()
                    .child(item("mic", "Microphone", true, self.options.mic, |o| o.mic = !o.mic))
                    .child(item("sys-audio", "System Audio", false, self.options.system_audio, |o| o.system_audio = !o.system_audio))
                    .child(item("camera", "Camera", false, self.options.camera, |o| o.camera = !o.camera)),
            )
            .into_any_element()
    }

    fn render_status(&self, msg: String, cx: &mut Context<Self>, stop: bool) -> AnyElement {
        div()
            .size_full()
            .bg(rgb(BG))
            .flex()
            .flex_col()
            .gap_4()
            .items_center()
            .justify_center()
            .child(div().text_size(px(15.)).text_color(rgb(MUTED)).child(msg))
            .when(stop, |d| {
                d.child(button("stop-main", "Stop Recording").on_click(cx.listener(|this, _, _, cx| this.stop_recording(cx))))
            })
            .into_any_element()
    }
}

impl Render for Studio {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.screen {
            Screen::Editor(e) => e.clone().into_any_element(),
            Screen::Home => self.render_home(cx),
            Screen::Busy(msg) => self.render_status(msg.clone(), cx, false),
            Screen::Recording => self.render_status("Recording…".into(), cx, true),
        };
        let editor = matches!(self.screen, Screen::Editor(_));
        div()
            .size_full()
            .relative()
            .text_size(px(BODY))
            .text_color(rgb(TEXT))
            .child(body)
            .when(!editor, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(44.))
                        .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, _| {
                            if ev.click_count == 2 {
                                window.titlebar_double_click();
                            } else {
                                window.start_window_move();
                            }
                        }),
                )
            })
    }
}

pub struct RecBar {
    handle: Rc<RecordingHandle>,
    studio: WeakEntity<Studio>,
    _tick: Task<()>,
}

impl RecBar {
    fn new(handle: Rc<RecordingHandle>, studio: WeakEntity<Studio>, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self { handle, studio, _tick: tick }
    }
}

impl Render for RecBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.handle.state();
        let (label, can_stop) = match state {
            RecState::Starting => ("Starting…".to_string(), false),
            RecState::Recording { since } => (fmt_time(since.elapsed().as_secs_f64()), true),
            RecState::Saving => ("Saving…".to_string(), false),
            RecState::Done(_) => ("Done".to_string(), false),
            RecState::Failed(_) => ("Failed".to_string(), false),
        };
        let blink = matches!(state, RecState::Recording { since } if since.elapsed().as_millis() % 1000 < 600);
        let studio = self.studio.clone();
        div()
            .size_full()
            .flex()
            .items_center()
            .gap_3()
            .pl_4()
            .pr(px(6.))
            .rounded_full()
            .bg(alpha(0x1e1e1e, 0.92))
            .border_1()
            .border_color(alpha(0xffffff, 0.14))
            .text_size(px(BODY))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h_full()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(div().size(px(8.)).rounded_full().bg(if blink { rgb(RED).into() } else { alpha(RED, 0.3) }))
                    .child(div().text_size(px(15.)).font_weight(FontWeight::MEDIUM).font_features(tabular()).child(label)),
            )
            .child(
                div()
                    .id("stop")
                    .h(px(32.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_full()
                    .bg(if can_stop { rgb(RED) } else { rgb(CONTROL) })
                    .when(can_stop, |d| d.hover(|s| s.bg(rgb(RED_HOVER))))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0xffffff))
                    .cursor_default()
                    .child(div().size(px(9.)).rounded(px(2.)).bg(rgb(0xffffff)))
                    .child("Stop")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let _ = studio.update(cx, |s, cx| s.stop_recording(cx));
                    })),
            )
    }
}

/// Full-screen overlay for dragging out the area to record.
pub struct AreaPicker {
    display: u32,
    studio: WeakEntity<Studio>,
    start: Option<Point<Pixels>>,
    current: Option<Point<Pixels>>,
    focus: FocusHandle,
}

impl AreaPicker {
    fn new(display: u32, studio: WeakEntity<Studio>, cx: &mut Context<Self>) -> Self {
        Self { display, studio, start: None, current: None, focus: cx.focus_handle() }
    }

    fn rect(&self) -> Option<Bounds<Pixels>> {
        let (a, b) = (self.start?, self.current?);
        let origin = point(a.x.min(b.x), a.y.min(b.y));
        Some(Bounds { origin, size: size((a.x - b.x).abs(), (a.y - b.y).abs()) })
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        let area = self.rect().filter(|r| r.size.width >= px(40.) && r.size.height >= px(40.)).map(|r| Source::Area {
            display: self.display,
            x: f32::from(r.origin.x).round() as f64,
            y: f32::from(r.origin.y).round() as f64,
            w: f32::from(r.size.width).round() as f64,
            h: f32::from(r.size.height).round() as f64,
        });
        let studio = self.studio.clone();
        cx.defer(move |cx| {
            let _ = studio.update(cx, |s, cx| s.area_picked(area, cx));
        });
    }
}

impl Render for AreaPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let full = window.viewport_size();
        let shade = alpha(0x000000, 0.45);
        let rect = self.rect();
        let mut root = div()
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .cursor(CursorStyle::Crosshair)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.start = None;
                    this.finish(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                this.start = Some(ev.position);
                this.current = Some(ev.position);
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                this.start = None;
                this.finish(cx);
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.start.is_some() && ev.pressed_button == Some(MouseButton::Left) {
                    this.current = Some(ev.position);
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                this.current = Some(ev.position);
                this.finish(cx);
            }));
        match rect {
            None => {
                root = root.bg(shade).child(
                    div().size_full().flex().items_center().justify_center().child(
                        div()
                            .px_4()
                            .py_2()
                            .rounded_lg()
                            .bg(alpha(0x1e1e1e, 0.92))
                            .text_color(rgb(TEXT))
                            .text_size(px(BODY))
                            .child("Drag to select the area to record · Esc or right-click to cancel"),
                    ),
                );
            }
            Some(r) => {
                let (l, t) = (r.origin.x, r.origin.y);
                let (rr, b) = (r.origin.x + r.size.width, r.origin.y + r.size.height);
                let band = |x: Pixels, y: Pixels, w: Pixels, h: Pixels| div().absolute().left(x).top(y).w(w).h(h).bg(shade);
                root = root
                    .child(band(px(0.), px(0.), full.width, t))
                    .child(band(px(0.), b, full.width, full.height - b))
                    .child(band(px(0.), t, l, r.size.height))
                    .child(band(rr, t, full.width - rr, r.size.height))
                    .child(div().absolute().left(l).top(t).w(r.size.width).h(r.size.height).border_2().border_color(rgb(ACCENT)))
                    .child(
                        div()
                            .absolute()
                            .left(l)
                            .top(b + px(8.))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(alpha(0x1e1e1e, 0.92))
                            .text_color(rgb(TEXT))
                            .text_size(px(SMALL))
                            .font_features(tabular())
                            .child(format!("{:.0} × {:.0}", f32::from(r.size.width), f32::from(r.size.height))),
                    );
            }
        }
        root
    }
}
