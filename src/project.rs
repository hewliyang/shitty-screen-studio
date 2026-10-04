use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct CursorSample {
    pub t: f64,
    pub x: f32,
    pub y: f32,
    pub down: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Click {
    pub t: f64,
    pub x: f32,
    pub y: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ZoomSegment {
    pub start: f64,
    pub end: f64,
    pub scale: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ColorSpace {
    #[default]
    Srgb,
    DisplayP3,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct CameraTrack {
    /// Seconds the camera file runs ahead of the screen recording.
    pub lead: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Recording {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub duration: f64,
    /// Display width in points; cursor art is sized relative to this.
    pub points_width: f32,
    pub cursor: Vec<CursorSample>,
    #[serde(default)]
    pub zooms: Option<Vec<ZoomSegment>>,
    #[serde(default)]
    pub style: Option<crate::style::Style>,
    #[serde(default)]
    pub color: ColorSpace,
    #[serde(default)]
    pub camera: Option<CameraTrack>,
    #[serde(default)]
    pub timeline: Option<crate::edit::Timeline>,
}

impl Recording {
    pub fn timeline(&self) -> crate::edit::Timeline {
        self.timeline.clone().unwrap_or_else(|| crate::edit::Timeline::full(self.duration))
    }

    pub fn clicks(&self) -> Vec<Click> {
        let mut clicks = Vec::new();
        let mut was_down = false;
        for s in &self.cursor {
            if s.down && !was_down {
                clicks.push(Click { t: s.t, x: s.x, y: s.y });
            }
            was_down = s.down;
        }
        clicks
    }

    pub fn auto_zooms(&self, scale: f32) -> Vec<ZoomSegment> {
        const LEAD: f64 = 0.4;
        const HOLD: f64 = 2.2;
        const MERGE_GAP: f64 = 0.8;
        let mut out: Vec<ZoomSegment> = Vec::new();
        for c in self.clicks() {
            let start = (c.t - LEAD).max(0.0);
            let end = (c.t + HOLD).min(self.duration);
            match out.last_mut() {
                Some(last) if start - last.end < MERGE_GAP => last.end = last.end.max(end),
                _ => out.push(ZoomSegment { start, end, scale }),
            }
        }
        out
    }
}

#[derive(Clone)]
pub struct Project {
    pub dir: PathBuf,
    pub rec: Recording,
}

impl Project {
    pub fn video_path(dir: &Path) -> PathBuf {
        dir.join("screen.mp4")
    }

    pub fn camera_path(dir: &Path) -> PathBuf {
        dir.join("camera.mp4")
    }

    pub fn audio_path(dir: &Path) -> PathBuf {
        dir.join("audio.m4a")
    }

    pub fn meta_path(dir: &Path) -> PathBuf {
        dir.join("recording.json")
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let raw = std::fs::read(Self::meta_path(dir))
            .with_context(|| format!("read {}", Self::meta_path(dir).display()))?;
        let rec: Recording = serde_json::from_slice(&raw)?;
        Ok(Self { dir: dir.to_path_buf(), rec })
    }

    pub fn save(&self) -> Result<()> {
        std::fs::write(Self::meta_path(&self.dir), serde_json::to_vec_pretty(&self.rec)?)?;
        Ok(())
    }

    pub fn video(&self) -> PathBuf {
        Self::video_path(&self.dir)
    }

    pub fn camera(&self) -> Option<PathBuf> {
        self.rec.camera.map(|_| Self::camera_path(&self.dir)).filter(|p| p.exists())
    }

    pub fn audio(&self) -> Option<PathBuf> {
        Some(Self::audio_path(&self.dir)).filter(|p| p.exists())
    }
}

pub fn library_dir() -> PathBuf {
    let base = dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Shitty Screen Studio")
}

pub fn new_recording_dir() -> Result<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let dir = library_dir().join(format!("recording-{secs}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn list_recordings() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(library_dir()) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| Project::meta_path(p).exists() && Project::video_path(p).exists())
        .collect();
    dirs.sort();
    dirs.reverse();
    dirs
}
