use crate::capture::{dict, finish_writer, new_writer};
use crate::compositor::Params;
use crate::gpu::{Frames, Gpu, Wrapped, pixel_buffer_attributes};
use crate::motion::Motion;
use crate::edit::Timeline;
use crate::project::{ColorSpace, Project, Recording, ZoomSegment};
use crate::style::Style;
use anyhow::{Context as _, Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey,
    AVVideoCodecTypeH264, AVVideoCodecTypeHEVC, AVVideoColorPrimariesKey, AVVideoColorPrimaries_ITU_R_709_2,
    AVVideoColorPrimaries_P3_D65, AVVideoColorPropertiesKey, AVVideoCompressionPropertiesKey,
    AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey, AVVideoMaxKeyFrameIntervalDurationKey,
    AVVideoTransferFunctionKey, AVVideoTransferFunction_IEC_sRGB, AVVideoWidthKey, AVVideoYCbCrMatrixKey,
    AVVideoYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMTime;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferPool, kCVReturnSuccess};
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use std::collections::VecDeque;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

pub struct ExportSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Frames in flight on the GPU before the oldest is handed to the encoder.
const IN_FLIGHT: usize = 3;

fn encoder_settings(s: &ExportSettings, color: ColorSpace) -> Retained<NSDictionary<NSString, AnyObject>> {
    let pixels = (s.width * s.height) as f64;
    let hevc = pixels * s.fps as f64 > 3840.0 * 2160.0 * 60.0;
    let bitrate = (16e6 * pixels / (1920.0 * 1080.0) * (s.fps as f64 / 60.0).max(0.5)).min(120e6) as i64;
    unsafe {
        let n = |v: i64| NSNumber::new_i64(v);
        let (bitrate, fps, keyint) = (n(bitrate), n(s.fps as i64), NSNumber::new_f64(1.0));
        let compression = dict(&[
            (AVVideoAverageBitRateKey.unwrap(), bitrate.as_ref()),
            (AVVideoExpectedSourceFrameRateKey.unwrap(), fps.as_ref()),
            (AVVideoMaxKeyFrameIntervalDurationKey.unwrap(), keyint.as_ref()),
        ]);
        let primaries = match color {
            ColorSpace::Srgb => AVVideoColorPrimaries_ITU_R_709_2.unwrap(),
            ColorSpace::DisplayP3 => AVVideoColorPrimaries_P3_D65.unwrap(),
        };
        let colors = dict(&[
            (AVVideoColorPrimariesKey.unwrap(), primaries.as_ref()),
            (AVVideoTransferFunctionKey.unwrap(), AVVideoTransferFunction_IEC_sRGB.unwrap().as_ref()),
            (AVVideoYCbCrMatrixKey.unwrap(), AVVideoYCbCrMatrix_ITU_R_709_2.unwrap().as_ref()),
        ]);
        let codec = if hevc { AVVideoCodecTypeHEVC } else { AVVideoCodecTypeH264 }.unwrap();
        let (w, h) = (n(s.width as i64), n(s.height as i64));
        dict(&[
            (AVVideoCodecKey.unwrap(), codec.as_ref()),
            (AVVideoWidthKey.unwrap(), w.as_ref()),
            (AVVideoHeightKey.unwrap(), h.as_ref()),
            (AVVideoCompressionPropertiesKey.unwrap(), compression.as_ref()),
            (AVVideoColorPropertiesKey.unwrap(), colors.as_ref()),
        ])
    }
}

pub fn pool_buffer(pool: &CVPixelBufferPool) -> Result<CFRetained<CVPixelBuffer>> {
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut out)) };
    if status != kCVReturnSuccess || out.is_null() {
        bail!("pixel buffer pool: {status}");
    }
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

fn wait_ready(input: &AVAssetWriterInput, cancel: &AtomicBool) -> Result<()> {
    while !unsafe { input.isReadyForMoreMediaData() } {
        if cancel.load(Ordering::Relaxed) {
            bail!("export cancelled");
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    Ok(())
}

struct Pending {
    index: i64,
    buffer: CFRetained<CVPixelBuffer>,
    commands: Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLCommandBuffer>>,
    _keep: Vec<Wrapped>,
}

#[allow(clippy::too_many_arguments)]
pub fn export(
    project: &Project,
    rec: &Recording,
    style: &Style,
    zooms: &[ZoomSegment],
    timeline: &Timeline,
    out: &Path,
    settings: &ExportSettings,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<()> {
    use objc2_metal::MTLCommandBuffer as _;
    let motion = Motion::build(&rec.cursor, zooms, rec.duration, style);
    let clicks = rec.clicks();
    let params = Params { style, motion: &motion, clicks: &clicks, src_w: rec.width, src_h: rec.height, points_width: rec.points_width };
    let mut gpu = Gpu::new()?;
    gpu.prepare(&params);
    let mut screen = Seeker::new(project.video())?;
    let (mut camera, lead) = match (project.camera(), rec.camera) {
        (Some(path), Some(track)) if style.camera_visible => (Some(Seeker::new(path)?), track.lead.max(0.0)),
        _ => (None, 0.0),
    };

    let audio = project.audio();
    let video_out = if audio.is_some() { out.with_extension("video.mp4") } else { out.to_path_buf() };
    let writer = new_writer(&video_out)?;
    let input = unsafe {
        let s = encoder_settings(settings, rec.color);
        let media = AVMediaTypeVideo.unwrap();
        if !writer.canApplyOutputSettings_forMediaType(Some(&s), media) {
            bail!("the encoder does not support {}x{} at {} fps", settings.width, settings.height, settings.fps);
        }
        let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(media, Some(&s));
        input.setExpectsMediaDataInRealTime(false);
        writer.addInput(&input);
        input
    };
    let attrs = pixel_buffer_attributes(Some((settings.width, settings.height)));
    let adaptor = unsafe {
        AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(&input, Some(&attrs))
    };
    unsafe {
        writer.setShouldOptimizeForNetworkUse(true);
        if !writer.startWriting() {
            bail!("start writing: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
        }
        writer.startSessionAtSourceTime(CMTime::new(0, settings.fps as i32));
    }
    let pool = unsafe { adaptor.pixelBufferPool() }.context("no pixel buffer pool")?;

    let fps = settings.fps as f64;
    let total = ((timeline.duration() * fps).round() as i64).max(1);
    let half = 0.5 / fps;
    let mut pending: VecDeque<Pending> = VecDeque::new();
    let flush = |p: Pending| -> Result<()> {
        p.commands.waitUntilCompleted();
        wait_ready(&input, cancel)?;
        if !unsafe { adaptor.appendPixelBuffer_withPresentationTime(&p.buffer, CMTime::new(p.index, settings.fps as i32)) } {
            bail!("encode frame {}: {:?}", p.index, unsafe { writer.error() }.map(|e| e.localizedDescription().to_string()));
        }
        progress.store(((p.index + 1) * 1000 / total).min(1000) as u32, Ordering::Relaxed);
        Ok(())
    };
    for index in 0..total {
        if cancel.load(Ordering::Relaxed) {
            unsafe { writer.cancelWriting() };
            bail!("export cancelled");
        }
        let t = timeline.to_source(index as f64 / fps + half) - half;
        let src = gpu.wrap(screen.at(t + half)?.context("the recording has no frames")?)?;
        let cam = match camera.as_mut().map(|c| c.at(t + lead + half)).transpose()?.flatten() {
            Some(b) => Some(gpu.wrap(b)?),
            None => None,
        };
        let buffer = pool_buffer(&pool)?;
        let target = gpu.wrap(&buffer)?;
        let commands = gpu.draw(&target.texture, &src.texture, cam.as_ref().map(|c| &*c.texture), t, &params)?;
        let mut keep = vec![src, target];
        keep.extend(cam);
        pending.push_back(Pending { index, buffer, commands, _keep: keep });
        if pending.len() >= IN_FLIGHT {
            flush(pending.pop_front().unwrap())?;
        }
    }
    while let Some(p) = pending.pop_front() {
        flush(p)?;
    }
    if screen.frames.failed() {
        bail!("reading the recording failed");
    }
    unsafe { input.markAsFinished() };
    unsafe { writer.endSessionAtSourceTime(CMTime::new(total, settings.fps as i32)) };
    finish_writer(&writer)?;

    if let Some(audio) = audio {
        let status = crate::video::ffmpeg()
            .args(["-y", "-i"])
            .arg(&video_out)
            .arg("-i")
            .arg(&audio)
            .args(audio_args(timeline, rec.duration))
            .args(["-movflags", "+faststart"])
            .arg(out)
            .output()
            .map_err(|e| anyhow!("mux audio: {e}"))?;
        let _ = std::fs::remove_file(&video_out);
        if !status.status.success() {
            bail!("mux audio: {}", String::from_utf8_lossy(&status.stderr).trim());
        }
    }
    progress.store(1000, Ordering::Relaxed);
    Ok(())
}

/// Forward-only decoder that reopens instead of decoding through long cuts or going back.
struct Seeker {
    path: std::path::PathBuf,
    frames: Frames,
    at: f64,
}

impl Seeker {
    fn new(path: std::path::PathBuf) -> Result<Self> {
        Ok(Self { frames: Frames::open(&path)?, path, at: 0.0 })
    }

    fn at(&mut self, t: f64) -> Result<Option<&objc2_core_video::CVPixelBuffer>> {
        if t < self.at - 1e-4 || t > self.at + 1.5 {
            self.frames = Frames::open_at(&self.path, t.max(0.0))?;
        }
        self.at = t;
        Ok(self.frames.at(t))
    }
}

/// Copies the audio when nothing was cut; otherwise trims, retimes and joins it to match the slices.
fn audio_args(timeline: &Timeline, duration: f64) -> Vec<String> {
    if timeline.is_identity(duration) {
        return ["-map", "0:v", "-map", "1:a", "-c", "copy"].map(String::from).to_vec();
    }
    let mut graph = String::new();
    for (i, s) in timeline.slices.iter().enumerate() {
        graph += &format!("[1:a]atrim=start={:.6}:end={:.6},asetpts=PTS-STARTPTS", s.start, s.end);
        let mut k = s.speed;
        while k > 2.0 + 1e-9 {
            graph += ",atempo=2.0";
            k /= 2.0;
        }
        while k < 0.5 - 1e-9 {
            graph += ",atempo=0.5";
            k /= 0.5;
        }
        if (k - 1.0).abs() > 1e-9 {
            graph += &format!(",atempo={k:.6}");
        }
        graph += &format!("[a{i}];");
    }
    for i in 0..timeline.slices.len() {
        graph += &format!("[a{i}]");
    }
    graph += &format!("concat=n={}:v=0:a=1[a]", timeline.slices.len());
    ["-filter_complex", &graph, "-map", "0:v", "-map", "[a]", "-c:v", "copy", "-c:a", "aac", "-b:a", "192k"].map(String::from).to_vec()
}

pub struct ExportJob {
    pub progress: Arc<AtomicU32>,
    pub cancel: Arc<AtomicBool>,
    pub result: Arc<std::sync::Mutex<Option<Result<(), String>>>>,
    pub path: std::path::PathBuf,
}
