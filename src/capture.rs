use crate::camera::CameraHandle;
use crate::project::{CameraTrack, ColorSpace, CursorSample, Project, Recording};
use anyhow::{Context as _, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVMediaTypeAudio, AVMediaTypeVideo, AVVideoAllowFrameReorderingKey, AVVideoAverageBitRateKey,
    AVVideoCodecKey, AVVideoCodecTypeHEVC, AVVideoColorPrimariesKey, AVVideoColorPrimaries_P3_D65,
    AVVideoColorPropertiesKey, AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey,
    AVVideoHeightKey, AVVideoMaxKeyFrameIntervalDurationKey, AVVideoTransferFunctionKey,
    AVVideoTransferFunction_IEC_sRGB, AVVideoWidthKey, AVVideoYCbCrMatrixKey, AVVideoYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGEvent, CGEventSource, CGEventSourceStateID,
    CGMainDisplayID, CGMouseButton, kCGColorSpaceDisplayP3, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObjectProtocol, NSString, NSURL};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_FPS: u32 = 120;
const MAX_WIDTH: usize = 3840;
/// Idle screens send no frames; repeat the last one so seeking never lands far from a keyframe.
const HEARTBEAT: f64 = 0.25;
const BITRATE: i64 = 40_000_000;
const NV12: u32 = u32::from_be_bytes(*b"420v");
const AAC: i64 = u32::from_be_bytes(*b"aac ") as i64;
const SAMPLE_RATE: i64 = 48_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Display(u32),
    Window(u32),
    /// Rect in points, relative to the display's top-left corner.
    Area { display: u32, x: f64, y: f64, w: f64, h: f64 },
}

#[derive(Clone, Copy, Debug)]
pub struct RecordOptions {
    pub source: Source,
    pub system_audio: bool,
    pub mic: bool,
    pub camera: bool,
}

impl RecordOptions {
    pub fn default_display() -> u32 {
        CGMainDisplayID()
    }
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self { source: Source::Display(CGMainDisplayID()), system_audio: false, mic: false, camera: false }
    }
}

#[derive(Clone, Debug)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: f64,
    pub height: f64,
    pub main: bool,
}

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub id: u32,
    pub app: String,
    pub icon: Option<Arc<gpui::Image>>,
    pub title: String,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Sources {
    pub displays: Vec<DisplayInfo>,
    pub windows: Vec<WindowInfo>,
}

pub fn list_sources() -> Result<Sources> {
    let content = shareable_content()?;
    let main = CGMainDisplayID();
    let mut displays: Vec<DisplayInfo> = unsafe { content.displays() }
        .iter()
        .map(|d| unsafe {
            let f = d.frame();
            DisplayInfo { id: d.displayID(), width: f.size.width, height: f.size.height, main: d.displayID() == main }
        })
        .collect();
    displays.sort_by_key(|d| !d.main);
    let pid = std::process::id() as i32;
    let mut icons: std::collections::HashMap<i32, Option<Arc<gpui::Image>>> = Default::default();
    let mut windows: Vec<WindowInfo> = unsafe { content.windows() }
        .iter()
        .filter_map(|w| unsafe {
            let app = w.owningApplication()?;
            let f = w.frame();
            if w.windowLayer() != 0 || !w.isOnScreen() || app.processID() == pid || f.size.width < 120.0 || f.size.height < 80.0 {
                return None;
            }
            let title = w.title().map(|t| t.to_string()).unwrap_or_default();
            let icon = icons.entry(app.processID()).or_insert_with(|| app_icon(app.processID())).clone();
            Some(WindowInfo { id: w.windowID(), app: app.applicationName().to_string(), icon, title, width: f.size.width, height: f.size.height })
        })
        .collect();
    windows.sort_by(|a, b| a.app.to_lowercase().cmp(&b.app.to_lowercase()));
    Ok(Sources { displays, windows })
}

fn app_icon(pid: i32) -> Option<Arc<gpui::Image>> {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSRunningApplication};
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize};
    let icon = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?.icon()?;
    let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(64.0, 64.0));
    let cg = unsafe { icon.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg);
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    Some(Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png.to_vec())))
}

#[derive(Clone, Debug)]
pub enum RecState {
    Starting,
    Recording { since: Instant },
    Saving,
    Done(PathBuf),
    Failed(String),
}

pub struct RecordingHandle {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<RecState>>,
}

impl RecordingHandle {
    pub fn start(dir: PathBuf, options: RecordOptions) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(RecState::Starting));
        let (s, st) = (stop.clone(), state.clone());
        std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                if let Err(e) = record(&dir, options, &s, &st) {
                    let _ = std::fs::remove_dir_all(&dir);
                    *st.lock().unwrap() = RecState::Failed(format!("{e:#}"));
                }
            })
            .expect("spawn recorder");
        Self { stop, state }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn state(&self) -> RecState {
        self.state.lock().unwrap().clone()
    }
}

struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

pub fn host_now() -> f64 {
    unsafe { CMClock::host_time_clock().time().seconds() }
}

pub fn host_time(seconds: f64) -> CMTime {
    unsafe { CMTime::with_seconds(seconds, 1_000_000_000) }
}

pub fn dict(entries: &[(&NSString, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = entries.iter().map(|e| e.0).collect();
    let values: Vec<&AnyObject> = entries.iter().map(|e| e.1).collect();
    NSDictionary::from_slices(&keys, &values)
}

fn ns_error(e: Option<Retained<NSError>>) -> String {
    e.map(|e| e.localizedDescription().to_string()).unwrap_or_default()
}

/// Video settings shared by the screen and camera writers.
pub fn video_settings(width: usize, height: usize, codec: &NSString, bitrate: i64, fps: i64, p3: bool) -> Retained<NSDictionary<NSString, AnyObject>> {
    unsafe {
        let n = |v: i64| NSNumber::new_i64(v);
        let (bitrate, fps, keyint, reorder) = (n(bitrate), n(fps), NSNumber::new_f64(1.0), NSNumber::new_bool(false));
        let compression = dict(&[
            (AVVideoAverageBitRateKey.unwrap(), bitrate.as_ref()),
            (AVVideoExpectedSourceFrameRateKey.unwrap(), fps.as_ref()),
            (AVVideoMaxKeyFrameIntervalDurationKey.unwrap(), keyint.as_ref()),
            (AVVideoAllowFrameReorderingKey.unwrap(), reorder.as_ref()),
        ]);
        let (w, h) = (n(width as i64), n(height as i64));
        let mut entries: Vec<(&NSString, &AnyObject)> = vec![
            (AVVideoCodecKey.unwrap(), codec.as_ref()),
            (AVVideoWidthKey.unwrap(), w.as_ref()),
            (AVVideoHeightKey.unwrap(), h.as_ref()),
            (AVVideoCompressionPropertiesKey.unwrap(), compression.as_ref()),
        ];
        let color = dict(&[
            (AVVideoColorPrimariesKey.unwrap(), AVVideoColorPrimaries_P3_D65.unwrap().as_ref()),
            (AVVideoTransferFunctionKey.unwrap(), AVVideoTransferFunction_IEC_sRGB.unwrap().as_ref()),
            (AVVideoYCbCrMatrixKey.unwrap(), AVVideoYCbCrMatrix_ITU_R_709_2.unwrap().as_ref()),
        ]);
        if p3 {
            entries.push((AVVideoColorPropertiesKey.unwrap(), color.as_ref()));
        }
        dict(&entries)
    }
}

fn audio_settings(channels: i64) -> Retained<NSDictionary<NSString, AnyObject>> {
    let key = NSString::from_str;
    let (format, rate, chans, bitrate) =
        (NSNumber::new_i64(AAC), NSNumber::new_i64(SAMPLE_RATE), NSNumber::new_i64(channels), NSNumber::new_i64(64_000 * channels));
    let (k1, k2, k3, k4) = (key("AVFormatIDKey"), key("AVSampleRateKey"), key("AVNumberOfChannelsKey"), key("AVEncoderBitRateKey"));
    dict(&[(&k1, format.as_ref()), (&k2, rate.as_ref()), (&k3, chans.as_ref()), (&k4, bitrate.as_ref())])
}

pub fn new_writer(path: &Path) -> Result<Retained<AVAssetWriter>> {
    let _ = std::fs::remove_file(path);
    unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        AVAssetWriter::assetWriterWithURL_fileType_error(&url, AVFileTypeMPEG4.unwrap())
            .map_err(|e| anyhow!("create writer: {}", e.localizedDescription()))
    }
}

pub fn add_input(writer: &AVAssetWriter, media: &NSString, settings: &NSDictionary<NSString, AnyObject>) -> Result<Retained<AVAssetWriterInput>> {
    unsafe {
        if !writer.canApplyOutputSettings_forMediaType(Some(settings), media) {
            bail!("encoder rejected the {media} settings");
        }
        let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(media, Some(settings));
        input.setExpectsMediaDataInRealTime(true);
        if !writer.canAddInput(&input) {
            bail!("cannot add {media} input");
        }
        writer.addInput(&input);
        Ok(input)
    }
}

pub fn finish_writer(writer: &AVAssetWriter) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let done = RcBlock::new(move || {
        let _ = tx.send(());
    });
    unsafe { writer.finishWritingWithCompletionHandler(&done) };
    rx.recv_timeout(Duration::from_secs(30)).map_err(|_| anyhow!("timed out finishing the video"))?;
    if unsafe { writer.status() } != AVAssetWriterStatus::Completed {
        bail!("finish video: {}", ns_error(unsafe { writer.error() }));
    }
    Ok(())
}

/// Writes ScreenCaptureKit's IOSurface-backed frames straight into a hardware HEVC encoder,
/// with optional AAC tracks for system audio and the microphone. All timestamps are host time.
struct Writer {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    system: Option<Retained<AVAssetWriterInput>>,
    mic: Option<Retained<AVAssetWriterInput>>,
    start: Option<f64>,
    last: Option<(CFRetained<CVPixelBuffer>, f64)>,
    failed: Option<String>,
}

unsafe impl Send for Writer {}

type SharedWriter = Arc<Mutex<Writer>>;

impl Writer {
    fn new(path: &Path, width: usize, height: usize, system_audio: bool, mic: bool) -> Result<Self> {
        let writer = new_writer(path)?;
        let video = add_input(
            &writer,
            unsafe { AVMediaTypeVideo.unwrap() },
            &video_settings(width, height, unsafe { AVVideoCodecTypeHEVC.unwrap() }, BITRATE, MAX_FPS as i64, true),
        )?;
        let system = system_audio.then(|| add_input(&writer, unsafe { AVMediaTypeAudio.unwrap() }, &audio_settings(2))).transpose()?;
        let mic = mic.then(|| add_input(&writer, unsafe { AVMediaTypeAudio.unwrap() }, &audio_settings(1))).transpose()?;
        let adaptor = unsafe {
            AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(&video, None)
        };
        if !unsafe { writer.startWriting() } {
            bail!("start writing: {}", ns_error(unsafe { writer.error() }));
        }
        Ok(Self { writer, video, adaptor, system, mic, start: None, last: None, failed: None })
    }

    fn fail(&mut self, what: &str) {
        let e = ns_error(unsafe { self.writer.error() });
        self.failed = Some(format!("{what}: {e}"));
    }

    fn append_video(&mut self, buffer: CFRetained<CVPixelBuffer>, host: f64) {
        if self.failed.is_some() {
            return;
        }
        if self.start.is_none() {
            unsafe { self.writer.startSessionAtSourceTime(host_time(host)) };
            self.start = Some(host);
        }
        if self.last.as_ref().is_some_and(|(_, prev)| host <= *prev) {
            return;
        }
        unsafe {
            if !self.video.isReadyForMoreMediaData() {
                return;
            }
            if !self.adaptor.appendPixelBuffer_withPresentationTime(&buffer, host_time(host)) {
                return self.fail("append video");
            }
        }
        self.last = Some((buffer, host));
    }

    fn append_audio(&mut self, sample: &CMSampleBuffer, mic: bool) {
        let Some(start) = self.start else { return };
        if self.failed.is_some() || unsafe { sample.presentation_time_stamp().seconds() } < start {
            return;
        }
        let Some(input) = (if mic { &self.mic } else { &self.system }) else { return };
        unsafe {
            if input.isReadyForMoreMediaData() && !input.appendSampleBuffer(sample) {
                self.fail("append audio");
            }
        }
    }

    fn heartbeat(&mut self, host: f64) {
        let Some((buffer, t)) = self.last.as_ref() else { return };
        if host - t >= HEARTBEAT {
            let buffer = buffer.clone();
            self.append_video(buffer, host);
        }
    }

    /// Ends the file at host time `end`. Returns the duration.
    fn finish(&mut self, end: f64) -> Result<f64> {
        let start = self.start.context("screen capture produced no frames")?;
        if let Some((buffer, _)) = self.last.clone() {
            self.append_video(buffer, end);
        }
        if let Some(e) = self.failed.take() {
            bail!("video encoder: {e}");
        }
        let end = self.last.as_ref().map_or(end, |l| l.1).max(end);
        unsafe {
            self.video.markAsFinished();
            for input in self.system.iter().chain(self.mic.iter()) {
                input.markAsFinished();
            }
            self.writer.endSessionAtSourceTime(host_time(end));
        }
        finish_writer(&self.writer)?;
        Ok(end - start)
    }
}

fn frame_complete(sample: &CMSampleBuffer) -> bool {
    unsafe {
        let Some(attachments) = sample.sample_attachments_array(false) else { return false };
        let attachments: &NSArray<NSDictionary<NSString, AnyObject>> = &*(CFRetained::as_ptr(&attachments).as_ptr() as *const _);
        let Some(info) = attachments.firstObject() else { return false };
        let Some(status) = info.objectForKey(SCStreamFrameInfoStatus) else { return false };
        let Ok(status) = status.downcast::<NSNumber>() else { return false };
        status.integerValue() == SCFrameStatus::Complete.0
    }
}

struct OutputIvars {
    writer: SharedWriter,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl SCStreamOutput for FrameSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            let writer = &self.ivars().writer;
            match kind {
                SCStreamOutputType::Screen => {
                    if !frame_complete(sample) {
                        return;
                    }
                    let Some(buffer) = (unsafe { sample.image_buffer() }) else { return };
                    let host = unsafe { sample.presentation_time_stamp().seconds() };
                    writer.lock().unwrap().append_video(buffer, host);
                }
                SCStreamOutputType::Audio => writer.lock().unwrap().append_audio(sample, false),
                SCStreamOutputType::Microphone => writer.lock().unwrap().append_audio(sample, true),
                _ => {}
            }
        }
    }
);

impl FrameSink {
    fn new(writer: SharedWriter) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { writer });
        unsafe { msg_send![super(this), init] }
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel::<Result<SendBox<Retained<SCShareableContent>>, String>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = if let Some(error) = unsafe { error.as_ref() } {
            Err(error.localizedDescription().to_string())
        } else if let Some(content) = unsafe { Retained::retain(content) } {
            Ok(SendBox(content))
        } else {
            Err("no shareable content".into())
        };
        let _ = tx.send(result);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &handler,
        );
    }
    let result = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| anyhow!("timed out waiting for ScreenCaptureKit"))?;
    result
        .map(|b| b.0)
        .map_err(|e| anyhow!("{e}. Grant Screen Recording permission to this app (or your terminal) in System Settings → Privacy & Security."))
}

fn wait_completion(start: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let msg = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(msg);
    });
    start(&handler);
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => bail!("{e}"),
        Err(_) => bail!("timed out waiting for ScreenCaptureKit"),
    }
}

/// Samples carry host-clock seconds, normalised to `region` (global points).
fn track_cursor(stop: Arc<AtomicBool>, region: CGRect) -> Vec<CursorSample> {
    let (ox, oy) = (region.origin.x, region.origin.y);
    let (bw, bh) = (region.size.width.max(1.0), region.size.height.max(1.0));
    let mut samples = Vec::with_capacity(120 * 60);
    while !stop.load(Ordering::Relaxed) {
        let event = CGEvent::new(None);
        let loc = CGEvent::location(event.as_deref());
        let down = CGEventSource::button_state(CGEventSourceStateID::CombinedSessionState, CGMouseButton::Left);
        samples.push(CursorSample { t: host_now(), x: ((loc.x - ox) / bw) as f32, y: ((loc.y - oy) / bh) as f32, down });
        std::thread::sleep(Duration::from_millis(8));
    }
    samples
}

struct Target {
    filter: Retained<SCContentFilter>,
    /// Captured region in global points, used to map the cursor.
    region: CGRect,
    source_rect: Option<CGRect>,
    width: usize,
    height: usize,
}

fn even_size(w: f64, h: f64) -> (usize, usize) {
    let (mut w, mut h) = (w.round() as usize, h.round() as usize);
    if w > MAX_WIDTH {
        h = h * MAX_WIDTH / w;
        w = MAX_WIDTH;
    }
    ((w & !1).max(2), (h & !1).max(2))
}

fn target(content: &SCShareableContent, source: Source) -> Result<Target> {
    unsafe {
        if let Source::Window(id) = source {
            let window = content.windows().iter().find(|w| w.windowID() == id).context("that window is gone")?;
            let filter = SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window);
            let rect = filter.contentRect();
            let scale = filter.pointPixelScale() as f64;
            let (width, height) = even_size(rect.size.width * scale, rect.size.height * scale);
            return Ok(Target { filter, region: window.frame(), source_rect: None, width, height });
        }
        let display_id = match source {
            Source::Display(id) | Source::Area { display: id, .. } => id,
            Source::Window(_) => unreachable!(),
        };
        let displays = content.displays();
        let display = displays
            .iter()
            .find(|d| d.displayID() == display_id)
            .or_else(|| displays.iter().next())
            .context("no display to capture")?;
        let display_id = display.displayID();
        let pid = std::process::id() as i32;
        let ours: Vec<_> = content.applications().iter().filter(|app| app.processID() == pid).collect();
        let filter = SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::from_retained_slice(&ours),
            &NSArray::<SCWindow>::new(),
        );
        let bounds = CGDisplayBounds(display_id);
        let mode = CGDisplayCopyDisplayMode(display_id);
        let pixel_w = CGDisplayMode::pixel_width(mode.as_deref()) as f64;
        let scale = if pixel_w > 0.0 { pixel_w / bounds.size.width } else { 2.0 };
        match source {
            Source::Area { x, y, w, h, .. } => {
                let local = CGRect::new(CGPoint::new(x, y), CGSize::new(w, h));
                let region = CGRect::new(CGPoint::new(bounds.origin.x + x, bounds.origin.y + y), CGSize::new(w, h));
                let (width, height) = even_size(w * scale, h * scale);
                Ok(Target { filter, region, source_rect: Some(local), width, height })
            }
            _ => {
                let (width, height) = even_size(bounds.size.width * scale, bounds.size.height * scale);
                Ok(Target { filter, region: bounds, source_rect: None, width, height })
            }
        }
    }
}

fn audio_streams(video: &Path) -> usize {
    let probe = std::process::Command::new(crate::video::ffprobe_path())
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "csv=p=0"])
        .arg(video)
        .output();
    probe.map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.trim().is_empty()).count()).unwrap_or(0)
}

/// Mixes the audio tracks that actually got samples into one AAC file aligned to the first video frame.
fn mix_audio(video: &Path, out: &Path) -> Result<()> {
    let tracks = audio_streams(video);
    if tracks == 0 {
        bail!("no audio was captured");
    }
    let mut graph = String::new();
    for i in 0..tracks {
        graph += &format!("[0:a:{i}]aresample=async=1:first_pts=0[a{i}];");
    }
    for i in 0..tracks {
        graph += &format!("[a{i}]");
    }
    graph += &format!("amix=inputs={tracks}:normalize=0:duration=longest[a]");
    let output = crate::video::ffmpeg()
        .args(["-y", "-i"])
        .arg(video)
        .args(["-filter_complex", &graph, "-map", "[a]", "-c:a", "aac", "-b:a", "192k"])
        .arg(out)
        .output()?;
    if !output.status.success() {
        let _ = std::fs::remove_file(out);
        bail!("mixing audio failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn record(dir: &Path, options: RecordOptions, stop: &Arc<AtomicBool>, state: &Mutex<RecState>) -> Result<()> {
    if options.mic {
        crate::camera::ensure_access(true)?;
    }
    let content = shareable_content()?;
    let target = target(&content, options.source)?;
    let points_width = target.region.size.width as f32;

    let camera = if options.camera { Some(CameraHandle::start(&Project::camera_path(dir))?) } else { None };

    let writer: SharedWriter = Arc::new(Mutex::new(Writer::new(
        &Project::video_path(dir),
        target.width,
        target.height,
        options.system_audio,
        options.mic,
    )?));
    let sink = FrameSink::new(writer.clone());
    let stream = unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(target.width);
        config.setHeight(target.height);
        if let Some(rect) = target.source_rect {
            config.setSourceRect(rect);
        }
        config.setPixelFormat(NV12);
        config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        config.setColorSpaceName(kCGColorSpaceDisplayP3);
        config.setShowsCursor(false);
        config.setIgnoreShadowsSingleWindow(true);
        config.setMinimumFrameInterval(CMTime::new(1, MAX_FPS as i32));
        config.setQueueDepth(8);
        if options.system_audio {
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(SAMPLE_RATE as isize);
            config.setChannelCount(2);
        }
        if options.mic {
            config.setCaptureMicrophone(true);
        }
        let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &target.filter, &config, None);
        let queue = DispatchQueue::new("shitty-screen-studio.capture", None);
        let mut kinds = vec![SCStreamOutputType::Screen];
        if options.system_audio {
            kinds.push(SCStreamOutputType::Audio);
        }
        if options.mic {
            kinds.push(SCStreamOutputType::Microphone);
        }
        for kind in kinds {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*sink), kind, Some(&queue))
                .map_err(|e| anyhow!("add stream output: {}", e.localizedDescription()))?;
        }
        stream
    };

    let tracker = {
        let stop = stop.clone();
        let region = target.region;
        std::thread::spawn(move || track_cursor(stop, region))
    };
    let started = wait_completion(|h| unsafe { stream.startCaptureWithCompletionHandler(Some(h)) })
        .context("start screen capture")
        .and_then(|()| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while writer.lock().unwrap().start.is_none() {
                if Instant::now() > deadline {
                    bail!("screen capture produced no frames");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        });
    if let Err(e) = started {
        stop.store(true, Ordering::Relaxed);
        let _ = tracker.join();
        if let Some(camera) = camera {
            let _ = camera.finish();
        }
        return Err(e);
    }

    *state.lock().unwrap() = RecState::Recording { since: Instant::now() };
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
        let mut w = writer.lock().unwrap();
        w.heartbeat(host_now() - 0.03);
        if w.failed.is_some() {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);

    *state.lock().unwrap() = RecState::Saving;
    let end = host_now();
    let _ = wait_completion(|h| unsafe { stream.stopCaptureWithCompletionHandler(Some(h)) });
    let cursor = tracker.join().unwrap_or_default();
    let (duration, start) = {
        let mut w = writer.lock().unwrap();
        (w.finish(end)?, w.start.unwrap_or(end))
    };
    let camera = match camera {
        Some(camera) => {
            let cam_start = camera.finish()?;
            Some(CameraTrack { lead: start - cam_start })
        }
        None => None,
    };

    // Audio problems must not cost the user the video.
    if options.system_audio || options.mic {
        if let Err(e) = mix_audio(&Project::video_path(dir), &Project::audio_path(dir)) {
            eprintln!("audio: {e:#}");
        }
    }

    let mut cursor: Vec<CursorSample> = cursor
        .into_iter()
        .map(|s| CursorSample { t: s.t - start, ..s })
        .filter(|s| s.t <= duration)
        .collect();
    if let Some(first_in) = cursor.iter().position(|s| s.t >= 0.0) {
        cursor.drain(..first_in.saturating_sub(1));
    }
    if let Some(s) = cursor.first_mut() {
        s.t = s.t.max(0.0);
    }

    let project = Project {
        dir: dir.to_path_buf(),
        rec: Recording {
            width: target.width as u32,
            height: target.height as u32,
            fps: MAX_FPS,
            duration,
            points_width,
            cursor,
            zooms: None,
            style: None,
            color: ColorSpace::DisplayP3,
            camera,
            timeline: None,
        },
    };
    project.save()?;
    *state.lock().unwrap() = RecState::Done(dir.to_path_buf());
    Ok(())
}

