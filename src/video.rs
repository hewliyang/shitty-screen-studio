use anyhow::{Context as _, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

fn tool(name: &str) -> String {
    ["/opt/homebrew/bin", "/usr/local/bin"]
        .iter()
        .map(|dir| format!("{dir}/{name}"))
        .find(|p| Path::new(p).exists())
        .unwrap_or_else(|| name.to_string())
}

pub fn ffmpeg() -> Command {
    let mut cmd = Command::new(tool("ffmpeg"));
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    cmd
}

pub fn fit_even(src_w: u32, src_h: u32, max_w: u32) -> (u32, u32) {
    if src_w <= max_w {
        return (src_w & !1, src_h & !1);
    }
    let h = (src_h as f64 * max_w as f64 / src_w as f64 / 2.0).round() as u32 * 2;
    (max_w & !1, h.max(2))
}

/// Streams decoded RGBA frames at a constant rate, even from variable-rate recordings.
pub struct FrameReader {
    child: Child,
    stdout: ChildStdout,
    pub width: u32,
    pub height: u32,
}

impl FrameReader {
    pub fn open(path: &Path, start: f64, fps: u32, width: u32, height: u32) -> Result<Self> {
        let mut child = ffmpeg()
            .args(["-hwaccel", "videotoolbox"])
            .args(["-ss", &format!("{:.4}", start.max(0.0))])
            .arg("-i")
            .arg(path)
            .args(["-copyts"])
            .args([
                "-vf",
                &format!("fps={fps}:start_time={:.4},scale={width}:{height}:flags=bilinear", start.max(0.0)),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn ffmpeg; install it with `brew install ffmpeg`")?;
        let stdout = child.stdout.take().unwrap();
        Ok(Self { child, stdout, width, height })
    }

    pub fn frame_len(&self) -> usize {
        (self.width * self.height * 4) as usize
    }

    /// Returns false at end of stream.
    pub fn read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool> {
        buf.resize(self.frame_len(), 0);
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.stdout.read(&mut buf[filled..])?;
            if n == 0 {
                return Ok(false);
            }
            filled += n;
        }
        Ok(true)
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
