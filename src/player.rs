use crate::compositor::Params;
use crate::edit::Timeline;
use crate::gpu::{Frames, Gpu};
use crate::motion::Motion;
use crate::project::{Click, KeyPress, Project, Recording};
use crate::style::Style;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_av_foundation::AVPlayer;
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMTime;
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSString, NSURL};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Longest side of the preview canvas.
pub const PREVIEW_W: u32 = 1920;
pub const PREVIEW_FPS: u32 = 120;

/// Preview canvas for a style: the 1080p export shape, scaled to fit the preview budget.
pub fn preview_size(style: &Style, rec: &Recording) -> (u32, u32) {
    let (w, h) = crate::style::canvas_size(style.aspect, rec.width, rec.height, 1080);
    let k = (PREVIEW_W as f32 / w as f32).min(PREVIEW_W as f32 / h as f32).min(1.0);
    (((w as f32 * k / 2.0).round() as u32 * 2).max(2), ((h as f32 * k / 2.0).round() as u32 * 2).max(2))
}

pub struct Scene {
    pub style: Style,
    pub motion: Motion,
    pub clicks: Vec<Click>,
    pub keys: Vec<KeyPress>,
    pub timeline: Timeline,
}

/// An NV12 frame ready to hand to a GPUI surface.
#[derive(Clone)]
pub struct Surface(pub CFRetained<CVPixelBuffer>);

// Pixel buffers are reference-counted IOSurfaces; the player never touches one after publishing it.
unsafe impl Send for Surface {}

impl Surface {
    pub fn to_gpui(&self) -> core_video::pixel_buffer::CVPixelBuffer {
        use core_foundation::base::TCFType;
        let ptr = CFRetained::as_ptr(&self.0).as_ptr() as core_video::pixel_buffer::CVPixelBufferRef;
        unsafe { core_video::pixel_buffer::CVPixelBuffer::wrap_under_get_rule(ptr) }
    }
}

#[derive(Default)]
pub struct Shared {
    pub latest: Mutex<Option<Surface>>,
    pub frames: AtomicU64,
    pub position: Mutex<f64>,
    pub playing: AtomicBool,
}

enum Cmd {
    Play,
    Pause,
    Seek(f64),
    Update(Arc<Scene>),
}

pub struct Player {
    tx: Sender<Cmd>,
    pub shared: Arc<Shared>,
    audio: Option<Retained<AVPlayer>>,
    timeline: Timeline,
    audio_slice: Cell<Option<usize>>,
}

fn seconds(t: f64) -> CMTime {
    unsafe { CMTime::with_seconds(t, 600) }
}

impl Player {
    pub fn new(project: &Project, scene: Arc<Scene>) -> Self {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let (video, camera, rec) = (project.video(), project.camera(), project.rec.clone());
        let timeline = scene.timeline.clone();
        std::thread::Builder::new()
            .name("player".into())
            .spawn(move || run(video, camera, rec, scene, rx, thread_shared))
            .expect("spawn player thread");
        let audio = project.audio().and_then(|path| {
            let mtm = MainThreadMarker::new()?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            Some(unsafe { AVPlayer::playerWithURL(&url, mtm) })
        });
        Self { tx, shared, audio, timeline, audio_slice: Cell::new(None) }
    }

    pub fn duration(&self) -> f64 {
        self.timeline.duration()
    }

    fn sync_audio(&self, t: f64, play: bool) {
        let Some(audio) = &self.audio else { return };
        let i = self.timeline.index_at(t);
        let speed = self.timeline.slices.get(i).map_or(1.0, |s| s.speed);
        self.audio_slice.set(Some(i));
        unsafe {
            audio.seekToTime_toleranceBefore_toleranceAfter(seconds(self.timeline.to_source(t)), seconds(0.0), seconds(0.0));
            if play {
                audio.setRate(speed as f32);
            } else {
                audio.pause();
            }
        }
    }

    /// Re-aims the audio when playback crosses into another slice.
    pub fn tick(&self) {
        if self.audio.is_none() || !self.is_playing() {
            return;
        }
        let t = self.position();
        if self.audio_slice.get() != Some(self.timeline.index_at(t)) {
            self.sync_audio(t, true);
        }
    }

    pub fn play(&self) {
        let mut t = self.position();
        if t >= self.duration() - 0.05 {
            t = 0.0;
        }
        self.sync_audio(t, true);
        let _ = self.tx.send(Cmd::Play);
    }

    pub fn pause(&self) {
        if let Some(audio) = &self.audio {
            unsafe { audio.pause() };
        }
        let _ = self.tx.send(Cmd::Pause);
    }

    pub fn seek(&self, t: f64) {
        let t = t.clamp(0.0, self.duration());
        *self.shared.position.lock().unwrap() = t;
        self.sync_audio(t, self.is_playing());
        let _ = self.tx.send(Cmd::Seek(t));
    }

    pub fn update(&mut self, scene: Arc<Scene>) {
        let changed = self.timeline != scene.timeline;
        self.timeline = scene.timeline.clone();
        let _ = self.tx.send(Cmd::Update(scene));
        if changed {
            let t = self.position().min(self.duration());
            *self.shared.position.lock().unwrap() = t;
            self.sync_audio(t, self.is_playing());
        }
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn position(&self) -> f64 {
        *self.shared.position.lock().unwrap()
    }

    pub fn surface(&self) -> Option<Surface> {
        self.shared.latest.lock().unwrap().clone()
    }
}

/// A decoder that only moves forward; it is reopened for backward or long jumps.
struct Track {
    path: PathBuf,
    frames: Option<Frames>,
    at: f64,
}

impl Track {
    fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), frames: None, at: f64::NEG_INFINITY }
    }

    fn frame(&mut self, t: f64) -> Option<&CVPixelBuffer> {
        if self.frames.is_none() || t < self.at - 1e-4 || t > self.at + 1.0 {
            self.frames = Frames::open_at(&self.path, t.max(0.0)).ok();
        }
        self.at = t;
        self.frames.as_mut()?.at(t)
    }
}

fn run(video: PathBuf, camera: Option<PathBuf>, rec: Recording, mut scene: Arc<Scene>, rx: Receiver<Cmd>, shared: Arc<Shared>) {
    let mut gpu = match Gpu::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("preview: {e:#}");
            return;
        }
    };
    let period = Duration::from_secs_f64(1.0 / PREVIEW_FPS as f64);
    let lead = rec.camera.map_or(0.0, |c| c.lead.max(0.0));
    let p3 = rec.color == crate::project::ColorSpace::DisplayP3;
    let mut screen = Track::new(&video);
    let mut cam = camera.as_deref().map(Track::new);
    let mut t = 0.0f64;
    let mut clock: Option<(Instant, f64)> = None;
    let mut first = true;

    loop {
        let mut cmds = Vec::new();
        if clock.is_none() && !first {
            match rx.recv() {
                Ok(c) => cmds.push(c),
                Err(_) => return,
            }
        }
        loop {
            match rx.try_recv() {
                Ok(c) => cmds.push(c),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let duration = |scene: &Scene| scene.timeline.duration();
        for cmd in cmds {
            match cmd {
                Cmd::Play if clock.is_none() => {
                    if t >= duration(&scene) - 1.0 / PREVIEW_FPS as f64 {
                        t = 0.0;
                    }
                    clock = Some((Instant::now(), t));
                    shared.playing.store(true, Ordering::Relaxed);
                }
                Cmd::Play => {}
                Cmd::Pause => {
                    clock = None;
                    shared.playing.store(false, Ordering::Relaxed);
                }
                Cmd::Seek(at) => {
                    t = at.clamp(0.0, duration(&scene));
                    if clock.is_some() {
                        clock = Some((Instant::now(), t));
                    }
                }
                Cmd::Update(s) => {
                    scene = s;
                    t = t.min(duration(&scene));
                }
            }
        }

        let tick = Instant::now();
        if let Some((since, from)) = clock {
            t = from + since.elapsed().as_secs_f64();
            if t >= duration(&scene) {
                t = duration(&scene);
                clock = None;
                shared.playing.store(false, Ordering::Relaxed);
            }
        }
        *shared.position.lock().unwrap() = t;

        let src_t = scene.timeline.to_source(t);
        let cam_frame = match (&mut cam, scene.style.camera_visible) {
            (Some(c), true) => c.frame(src_t + lead).map(|b| unsafe { CFRetained::retain(std::ptr::NonNull::from(b)) }),
            _ => None,
        };
        if let Some(frame) = screen.frame(src_t) {
            let params = Params {
                style: &scene.style,
                motion: &scene.motion,
                clicks: &scene.clicks,
                keys: &scene.keys,
                src_w: rec.width,
                src_h: rec.height,
                points_width: rec.points_width,
            };
            let (pw, ph) = preview_size(&scene.style, &rec);
            match gpu.present(frame, cam_frame.as_deref(), src_t, &params, p3, pw, ph) {
                Ok(buffer) => {
                    *shared.latest.lock().unwrap() = Some(Surface(buffer));
                    shared.frames.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => eprintln!("preview: {e:#}"),
            }
        }
        first = false;

        if clock.is_some() {
            if let Some(rest) = period.checked_sub(tick.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
}
