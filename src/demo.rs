use crate::project::{CursorSample, Project, Recording};
use crate::video::ffmpeg;
use anyhow::{Result, bail};
use std::path::Path;

/// Builds a synthetic recording so the editor can be tried without screen capture permission.
pub fn create(dir: &Path) -> Result<Project> {
    std::fs::create_dir_all(dir)?;
    let (w, h, fps, duration) = (2880u32, 1800u32, 30u32, 14.0f64);
    let status = ffmpeg()
        .args(["-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={w}x{h}:rate={fps}:duration={duration}"))
        .args(["-vf", "scale=out_color_matrix=bt709:out_range=tv", "-c:v", "h264_videotoolbox", "-b:v", "20M", "-pix_fmt", "yuv420p"])
        .args(["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "iec61966-2-1", "-color_range", "tv"])
        .arg(Project::video_path(dir))
        .status()?;
    if !status.success() {
        bail!("ffmpeg could not create the demo video");
    }

    let waypoints: &[(f64, f32, f32, bool)] = &[
        (0.0, 0.50, 0.55, false),
        (1.5, 0.22, 0.30, false),
        (2.0, 0.22, 0.30, true),
        (2.15, 0.22, 0.30, false),
        (3.2, 0.30, 0.36, false),
        (3.4, 0.30, 0.36, true),
        (3.5, 0.30, 0.36, false),
        (7.5, 0.78, 0.70, false),
        (8.0, 0.78, 0.70, true),
        (8.1, 0.78, 0.70, false),
        (9.8, 0.70, 0.20, false),
        (11.0, 0.45, 0.85, false),
        (11.4, 0.45, 0.85, true),
        (11.5, 0.45, 0.85, false),
        (14.0, 0.55, 0.50, false),
    ];
    let mut cursor = Vec::new();
    let mut t = 0.0;
    while t <= duration {
        let i = waypoints.partition_point(|p| p.0 <= t).clamp(1, waypoints.len() - 1);
        let (a, b) = (waypoints[i - 1], waypoints[i]);
        let k = ((t - a.0) / (b.0 - a.0)).clamp(0.0, 1.0) as f32;
        let e = k * k * (3.0 - 2.0 * k);
        let wobble = (t as f32 * 7.0).sin() * 0.004;
        cursor.push(CursorSample {
            t,
            x: a.1 + (b.1 - a.1) * e + wobble,
            y: a.2 + (b.2 - a.2) * e - wobble,
            down: a.3,
        });
        t += 1.0 / 60.0;
    }
    let project = Project {
        dir: dir.to_path_buf(),
        rec: Recording { width: w, height: h, fps, duration, points_width: 1440.0, cursor, zooms: None, style: None, color: Default::default(), camera: None, timeline: None },
    };
    project.save()?;
    Ok(project)
}
