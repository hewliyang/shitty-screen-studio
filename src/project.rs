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

/// A shortcut pressed during recording, with its display label such as "⌘ ⇧ P".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct KeyPress {
    pub t: f64,
    pub keys: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<KeyPress>,
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
        let timeline = self.timeline();
        let mut out: Vec<ZoomSegment> = Vec::new();
        for c in self.clicks().into_iter().filter(|c| timeline.contains_source(c.t)) {
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

/// When a recording ends with a click on our stop button, the time the cursor left its last
/// resting spot to reach for it. Cutting there keeps the reach out of the video.
pub fn stop_reach(cursor: &[CursorSample], duration: f64) -> Option<f64> {
    const CLICK_TO_STOP: f64 = 0.4;
    const MAX_REACH: f64 = 3.0;
    const REST: f64 = 0.4;
    const STILL: f32 = 0.004;
    const AWAY: f32 = 0.03;
    /// Going back in time, the cursor getting this much closer to the button means the reach had not begun yet.
    const BACKTRACK: f32 = 0.02;
    const PAD: f64 = 0.1;

    let last_down = cursor.iter().rposition(|s| s.down)?;
    if cursor[last_down].t < duration - CLICK_TO_STOP {
        return None;
    }
    let press = cursor[..=last_down].iter().rposition(|s| !s.down).map_or(0, |i| i + 1);
    let (px, py, pt) = (cursor[press].x, cursor[press].y, cursor[press].t);
    let dist = |s: &CursorSample| (s.x - px).hypot(s.y - py);
    let mut farthest = (0.0f32, press);
    let mut release = None;
    let mut first_pause = None;
    let mut was_paused = false;
    for j in (0..press).rev() {
        let s = cursor[j];
        if pt - s.t > MAX_REACH {
            return None;
        }
        // The reach never spans an earlier click or drag: it begins at the first pause after it.
        if s.down {
            return reach_end(first_pause.or(release).unwrap_or(s.t) + PAD, pt);
        }
        release = Some(s.t);
        let paused = (cursor[j + 1].x - s.x).abs().max((cursor[j + 1].y - s.y).abs()) < STILL && dist(&s) >= AWAY;
        if paused && !was_paused {
            first_pause = Some(s.t);
        }
        was_paused = paused;
        let d = dist(&s);
        if d > farthest.0 {
            farthest = (d, j);
        } else if farthest.0 - d > BACKTRACK {
            return reach_end(cursor[farthest.1].t + PAD, pt);
        }
        if d < AWAY || s.t - cursor[0].t < REST {
            continue;
        }
        let still = cursor[..=j].iter().rev().take_while(|r| s.t - r.t <= REST).all(|r| (r.x - s.x).abs().max((r.y - s.y).abs()) < STILL);
        if still {
            return reach_end(s.t + PAD, pt);
        }
    }
    None
}

fn reach_end(end: f64, press: f64) -> Option<f64> {
    (end < press).then_some(end)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Rests at (0.3, 0.3), reaches for a button at (0.5, 1.1) from `move_at`, and clicks it near the end.
    fn session(move_at: f64, click_at: f64, duration: f64) -> Vec<CursorSample> {
        let mut out = Vec::new();
        let mut t = 0.0;
        while t <= duration {
            let k = ((t - move_at) / (click_at - 0.1 - move_at)).clamp(0.0, 1.0) as f32;
            let down = t >= click_at && t < click_at + 0.08;
            out.push(CursorSample { t, x: 0.3 + 0.2 * k, y: 0.3 + 0.8 * k, down });
            t += 0.008;
        }
        out
    }

    #[test]
    fn cuts_the_reach_for_the_stop_button() {
        let end = stop_reach(&session(4.0, 5.0, 5.1), 5.1).unwrap();
        assert!((4.0..4.2).contains(&end), "{end}");
    }

    #[test]
    fn skips_a_pause_halfway_through_the_reach() {
        let mut cursor = session(4.0, 5.0, 5.1);
        let mid = cursor.iter().position(|s| s.t >= 4.4).unwrap();
        let held = cursor[mid];
        cursor.iter_mut().filter(|s| (4.4..4.65).contains(&s.t)).for_each(|s| (s.x, s.y) = (held.x, held.y));
        let end = stop_reach(&cursor, 5.1).unwrap();
        assert!((4.0..4.2).contains(&end), "{end}");
    }

    #[test]
    fn cuts_where_the_cursor_turned_toward_the_button() {
        let mut cursor = session(4.0, 5.0, 5.1);
        for s in cursor.iter_mut().filter(|s| s.t < 4.0) {
            s.y = 0.3 + 0.2 * ((4.0 - s.t) / 4.0) as f32;
        }
        let end = stop_reach(&cursor, 5.1).unwrap();
        assert!((4.0..4.2).contains(&end), "{end}");
    }

    #[test]
    fn keeps_an_earlier_drag() {
        let mut cursor = session(4.0, 5.0, 5.1);
        cursor.iter_mut().filter(|s| (3.0..3.95).contains(&s.t)).for_each(|s| s.down = true);
        let end = stop_reach(&cursor, 5.1).unwrap();
        assert!((3.95..4.2).contains(&end), "{end}");
    }

    #[test]
    fn keeps_recordings_stopped_without_a_click() {
        let mut cursor = session(4.0, 5.0, 5.1);
        cursor.iter_mut().for_each(|s| s.down = false);
        assert_eq!(stop_reach(&cursor, 5.1), None);
    }

    #[test]
    fn keeps_a_real_click_long_before_the_stop() {
        assert_eq!(stop_reach(&session(4.0, 5.0, 6.0), 6.0), None);
    }

    #[test]
    fn keeps_a_reach_with_no_rest_before_it() {
        assert_eq!(stop_reach(&session(0.0, 5.0, 5.1), 5.1), None);
    }
}
