use crate::capture::{add_input, finish_writer, host_time, new_writer, video_settings};
use anyhow::{Context as _, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAuthorizationStatus, AVCaptureConnection,
    AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset1280x720,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeAudio, AVMediaTypeVideo, AVVideoCodecTypeH264,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use objc2_foundation::NSObjectProtocol;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Track {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
}

/// The writer is created on the first frame, once the camera's frame size is known.
struct CamWriter {
    path: PathBuf,
    track: Option<Track>,
    start: Option<f64>,
    last: f64,
    failed: Option<String>,
}

unsafe impl Send for CamWriter {}

impl CamWriter {
    fn append(&mut self, sample: &CMSampleBuffer) {
        if self.failed.is_some() {
            return;
        }
        let Some(buffer) = (unsafe { sample.image_buffer() }) else { return };
        let host = unsafe { sample.presentation_time_stamp().seconds() };
        if self.track.is_none() {
            let (w, h) = (CVPixelBufferGetWidth(&buffer), CVPixelBufferGetHeight(&buffer));
            let made = new_writer(&self.path).and_then(|writer| {
                let settings = video_settings(w, h, unsafe { AVVideoCodecTypeH264.unwrap() }, 8_000_000, 30, false);
                let input = add_input(&writer, unsafe { AVMediaTypeVideo.unwrap() }, &settings)?;
                let adaptor = unsafe {
                    AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(&input, None)
                };
                unsafe {
                    if !writer.startWriting() {
                        bail!("camera writer did not start");
                    }
                    writer.startSessionAtSourceTime(host_time(host));
                }
                Ok(Track { writer, input, adaptor })
            });
            match made {
                Ok(track) => {
                    self.track = Some(track);
                    self.start = Some(host);
                }
                Err(e) => {
                    self.failed = Some(format!("{e:#}"));
                    return;
                }
            }
        }
        let track = self.track.as_ref().unwrap();
        if host <= self.last {
            return;
        }
        unsafe {
            if track.input.isReadyForMoreMediaData() && !track.adaptor.appendPixelBuffer_withPresentationTime(&buffer, host_time(host)) {
                self.failed = Some("camera frame rejected".into());
                return;
            }
        }
        self.last = host;
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Mutex<CamWriter>>]
    struct CameraSink;

    unsafe impl NSObjectProtocol for CameraSink {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraSink {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, sample: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            self.ivars().lock().unwrap().append(sample);
        }
    }
);

impl CameraSink {
    fn new(writer: Arc<Mutex<CamWriter>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(writer);
        unsafe { msg_send![super(this), init] }
    }
}

/// Asks for camera or microphone access up front, so the prompt never lands mid-recording.
pub fn ensure_access(mic: bool) -> Result<()> {
    let (media, name) = unsafe {
        if mic { (AVMediaTypeAudio.unwrap(), "Microphone") } else { (AVMediaTypeVideo.unwrap(), "Camera") }
    };
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) } {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::NotDetermined => {
            let (tx, rx) = mpsc::channel();
            let handler = RcBlock::new(move |granted: Bool| {
                let _ = tx.send(granted.as_bool());
            });
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler) };
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(true) => Ok(()),
                _ => bail!("{} access was not granted", name.to_lowercase()),
            }
        }
        _ => bail!("{} access is off. Turn it on in System Settings → Privacy & Security → {name}.", name.to_lowercase()),
    }
}

pub struct CameraHandle {
    session: Retained<AVCaptureSession>,
    writer: Arc<Mutex<CamWriter>>,
    _sink: Retained<CameraSink>,
    _output: Retained<AVCaptureVideoDataOutput>,
}

unsafe impl Send for CameraHandle {}

impl CameraHandle {
    /// Starts the default camera and waits for its first frame.
    pub fn start(path: &Path) -> Result<Self> {
        ensure_access(false)?;
        let writer = Arc::new(Mutex::new(CamWriter { path: path.to_path_buf(), track: None, start: None, last: f64::MIN, failed: None }));
        let sink = CameraSink::new(writer.clone());
        unsafe {
            let device = AVCaptureDevice::defaultDeviceWithMediaType(AVMediaTypeVideo.unwrap()).context("no camera found")?;
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device)
                .map_err(|e| anyhow!("open camera: {}", e.localizedDescription()))?;
            let session = AVCaptureSession::new();
            session.beginConfiguration();
            if session.canSetSessionPreset(AVCaptureSessionPreset1280x720) {
                session.setSessionPreset(AVCaptureSessionPreset1280x720);
            }
            if !session.canAddInput(&input) {
                bail!("cannot use the camera");
            }
            session.addInput(&input);
            let output = AVCaptureVideoDataOutput::new();
            output.setAlwaysDiscardsLateVideoFrames(true);
            let queue = DispatchQueue::new("shitty-screen-studio.camera", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*sink)), Some(&queue));
            if !session.canAddOutput(&output) {
                bail!("cannot read camera frames");
            }
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();

            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                {
                    let w = writer.lock().unwrap();
                    if let Some(e) = &w.failed {
                        bail!("camera: {e}");
                    }
                    if w.start.is_some() {
                        break;
                    }
                }
                if Instant::now() > deadline {
                    session.stopRunning();
                    bail!("the camera sent no frames");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(Self { session, writer, _sink: sink, _output: output })
        }
    }

    /// Stops the camera and returns the host time of its first frame.
    pub fn finish(self) -> Result<f64> {
        unsafe { self.session.stopRunning() };
        let mut w = self.writer.lock().unwrap();
        let start = w.start.context("the camera sent no frames")?;
        if let Some(e) = w.failed.take() {
            bail!("camera: {e}");
        }
        let track = w.track.take().context("camera writer missing")?;
        unsafe {
            track.input.markAsFinished();
            track.writer.endSessionAtSourceTime(host_time(w.last));
        }
        finish_writer(&track.writer)?;
        Ok(start)
    }
}
