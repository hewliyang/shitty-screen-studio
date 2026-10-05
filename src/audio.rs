//! Audio mixing and editing on AVFoundation, so the app needs no external tools.
use crate::capture::{dict, finish_writer, new_writer_as, ns_error};
use crate::edit::Timeline;
use anyhow::{Context as _, Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAsset, AVAssetReader, AVAssetReaderAudioMixOutput, AVAssetReaderOutput, AVAssetReaderStatus,
    AVAssetTrack, AVAssetWriterInput, AVFileTypeAppleM4A, AVMediaTypeAudio, AVMutableComposition, AVURLAsset,
};
use objc2_core_media::{CMTime, CMTimeRange};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use std::path::Path;
use std::time::Duration;

const AAC: i64 = u32::from_be_bytes(*b"aac ") as i64;
const LPCM: i64 = u32::from_be_bytes(*b"lpcm") as i64;
const SAMPLE_RATE: i64 = 48_000;
const TIMESCALE: i32 = 48_000;

pub fn aac_settings(channels: i64, bitrate: i64) -> Retained<NSDictionary<NSString, AnyObject>> {
    let key = NSString::from_str;
    let (format, rate, chans, bitrate) = (NSNumber::new_i64(AAC), NSNumber::new_i64(SAMPLE_RATE), NSNumber::new_i64(channels), NSNumber::new_i64(bitrate));
    let (k1, k2, k3, k4) = (key("AVFormatIDKey"), key("AVSampleRateKey"), key("AVNumberOfChannelsKey"), key("AVEncoderBitRateKey"));
    dict(&[(&k1, format.as_ref()), (&k2, rate.as_ref()), (&k3, chans.as_ref()), (&k4, bitrate.as_ref())])
}

fn pcm_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    let key = NSString::from_str;
    let n = NSNumber::new_i64;
    let (format, rate, chans, bits) = (n(LPCM), n(SAMPLE_RATE), n(2), n(32));
    let (yes, no) = (NSNumber::new_bool(true), NSNumber::new_bool(false));
    let keys = [
        key("AVFormatIDKey"),
        key("AVSampleRateKey"),
        key("AVNumberOfChannelsKey"),
        key("AVLinearPCMBitDepthKey"),
        key("AVLinearPCMIsFloatKey"),
        key("AVLinearPCMIsNonInterleaved"),
        key("AVLinearPCMIsBigEndianKey"),
    ];
    let values: [&AnyObject; 7] = [format.as_ref(), rate.as_ref(), chans.as_ref(), bits.as_ref(), yes.as_ref(), no.as_ref(), no.as_ref()];
    let entries: Vec<(&NSString, &AnyObject)> = keys.iter().map(|k| &**k).zip(values).collect();
    dict(&entries)
}

fn time(seconds: f64) -> CMTime {
    unsafe { CMTime::with_seconds(seconds, TIMESCALE) }
}

fn url_asset(path: &Path) -> Retained<AVURLAsset> {
    unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        AVURLAsset::URLAssetWithURL_options(&url, None)
    }
}

#[allow(deprecated)]
fn audio_tracks(asset: &AVAsset) -> Vec<Retained<AVAssetTrack>> {
    unsafe { asset.tracksWithMediaType(AVMediaTypeAudio.unwrap()) }.iter().filter(|t| unsafe { t.totalSampleDataLength() } > 0).collect()
}

fn start_reader(asset: &AVAsset, output: &AVAssetReaderOutput) -> Result<Retained<AVAssetReader>> {
    unsafe {
        let reader = AVAssetReader::assetReaderWithAsset_error(asset).map_err(|e| anyhow!("audio reader: {}", e.localizedDescription()))?;
        if !reader.canAddOutput(output) {
            bail!("the audio reader does not take the output");
        }
        reader.addOutput(output);
        if !reader.startReading() {
            bail!("read audio: {}", ns_error(reader.error()));
        }
        Ok(reader)
    }
}

/// A started reader, the output to pull PCM from, and the writer settings to encode it with.
pub struct Source {
    pub reader: Retained<AVAssetReader>,
    pub output: Retained<AVAssetReaderOutput>,
    pub settings: Retained<NSDictionary<NSString, AnyObject>>,
}

/// The recorded audio cut and retimed to the slices, with the pitch kept.
/// Reading through a composition also applies the AAC priming trim, which a plain track passthrough loses.
pub fn edited(audio: &Path, timeline: &Timeline) -> Result<Source> {
    let asset = url_asset(audio);
    let track = audio_tracks(&asset).into_iter().next().context("no audio track")?;
    unsafe {
        let range = track.timeRange();
        let track_end = range.start.seconds() + range.duration.seconds();
        let composition = AVMutableComposition::composition();
        let lane = composition
            .addMutableTrackWithMediaType_preferredTrackID(AVMediaTypeAudio.unwrap(), 0)
            .context("add audio track")?;
        let mut at = 0.0;
        for s in &timeline.slices {
            let end = s.end.min(track_end);
            if end - s.start <= 0.0 {
                break;
            }
            let len = time(end - s.start);
            let src = CMTimeRange { start: time(s.start), duration: len };
            lane.insertTimeRange_ofTrack_atTime_error(src, &track, time(at)).map_err(|e| anyhow!("edit audio: {}", e.localizedDescription()))?;
            let out = (end - s.start) / s.speed;
            if s.speed != 1.0 {
                lane.scaleTimeRange_toDuration(CMTimeRange { start: time(at), duration: len }, time(out));
            }
            at += out;
        }
        let lane = Retained::into_super(Retained::into_super(lane));
        let output = AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(&NSArray::from_retained_slice(&[lane]), Some(&pcm_settings()));
        let output = Retained::into_super(output);
        let reader = start_reader(&composition, &output)?;
        Ok(Source { reader, output, settings: aac_settings(2, 192_000) })
    }
}

/// Mixes the audio tracks of the recording that got samples into one AAC file.
pub fn mix(video: &Path, out: &Path) -> Result<()> {
    let result = write_mix(video, out);
    if result.is_err() {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn write_mix(video: &Path, out: &Path) -> Result<()> {
    let asset = url_asset(video);
    let tracks = audio_tracks(&asset);
    if tracks.is_empty() {
        bail!("no audio was captured");
    }
    unsafe {
        let output = AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(&NSArray::from_retained_slice(&tracks), Some(&pcm_settings()));
        let reader = start_reader(&asset, &output)?;
        let writer = new_writer_as(out, AVFileTypeAppleM4A.unwrap())?;
        let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(AVMediaTypeAudio.unwrap(), Some(&aac_settings(2, 192_000)));
        input.setExpectsMediaDataInRealTime(false);
        if !writer.canAddInput(&input) {
            bail!("the audio writer does not take the input");
        }
        writer.addInput(&input);
        if !writer.startWriting() {
            bail!("start writing audio: {}", ns_error(writer.error()));
        }
        writer.startSessionAtSourceTime(CMTime::new(0, TIMESCALE));
        loop {
            if !input.isReadyForMoreMediaData() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let Some(sample) = output.copyNextSampleBuffer() else { break };
            if !input.appendSampleBuffer(&sample) {
                bail!("encode audio: {}", ns_error(writer.error()));
            }
        }
        if reader.status() == AVAssetReaderStatus::Failed {
            writer.cancelWriting();
            bail!("read audio: {}", ns_error(reader.error()));
        }
        input.markAsFinished();
        finish_writer(&writer)
    }
}
