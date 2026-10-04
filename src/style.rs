use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Background {
    pub name: &'static str,
    pub from: u32,
    pub to: u32,
}

pub const BACKGROUNDS: &[Background] = &[
    Background { name: "Sunset", from: 0xff7e5f, to: 0xfeb47b },
    Background { name: "Ocean", from: 0x2193b0, to: 0x6dd5ed },
    Background { name: "Grape", from: 0x8e2de2, to: 0x4a00e0 },
    Background { name: "Mint", from: 0x11998e, to: 0x38ef7d },
    Background { name: "Candy", from: 0xf953c6, to: 0xb91d73 },
    Background { name: "Midnight", from: 0x0f2027, to: 0x2c5364 },
    Background { name: "Dusk", from: 0x355c7d, to: 0xc06c84 },
    Background { name: "Graphite", from: 0x232526, to: 0x414345 },
];

/// macOS wallpapers that ship with the system, by file name in `/System/Library/Desktop Pictures`.
pub const WALLPAPERS: &[&str] = &[
    "Sonoma", "iMac Blue", "iMac Purple", "iMac Pink", "iMac Orange", "iMac Yellow", "iMac Green", "iMac Silver",
    "Mac Blue", "Mac Purple", "Mac Pink", "Mac Yellow", "Radial Sky Blue",
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fill {
    Gradient(Background),
    Wallpaper(usize),
}

/// `Style::background` indexes gradients first, then wallpapers.
pub fn fill(background: usize) -> Fill {
    match background.checked_sub(BACKGROUNDS.len()) {
        Some(w) if w < WALLPAPERS.len() => Fill::Wallpaper(w),
        _ => Fill::Gradient(BACKGROUNDS[background % BACKGROUNDS.len()]),
    }
}

fn wallpaper_source(i: usize) -> PathBuf {
    PathBuf::from(format!("/System/Library/Desktop Pictures/{}.heic", WALLPAPERS[i]))
}

fn wallpaper_cache(i: usize, thumb: bool) -> PathBuf {
    let dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("shitty-screen-studio/wallpapers");
    dir.join(format!("{}{}.jpg", WALLPAPERS[i], if thumb { "-thumb" } else { "" }))
}

/// Converts the HEIC wallpaper to a cached JPEG the first time it is needed.
pub fn wallpaper_file(i: usize, thumb: bool) -> Option<PathBuf> {
    let out = wallpaper_cache(i, thumb);
    if out.exists() {
        return Some(out);
    }
    let src = wallpaper_source(i);
    if !src.exists() {
        return None;
    }
    std::fs::create_dir_all(out.parent()?).ok()?;
    let tmp = out.with_extension("tmp.jpg");
    let ok = std::process::Command::new("/usr/bin/sips")
        .args(["-Z", if thumb { "240" } else { "3200" }, "-s", "format", "jpeg", "-s", "formatOptions", "92"])
        .arg(&src)
        .arg("--out")
        .arg(&tmp)
        .output()
        .is_ok_and(|o| o.status.success());
    (ok && std::fs::rename(&tmp, &out).is_ok()).then_some(out)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Aspect {
    /// Same shape as the recording.
    #[default]
    Auto,
    Wide,
    Vertical,
    Square,
    Classic,
}

impl Aspect {
    pub const ALL: [Aspect; 5] = [Aspect::Auto, Aspect::Wide, Aspect::Vertical, Aspect::Square, Aspect::Classic];

    pub fn label(self) -> &'static str {
        match self {
            Aspect::Auto => "Auto",
            Aspect::Wide => "16:9",
            Aspect::Vertical => "9:16",
            Aspect::Square => "1:1",
            Aspect::Classic => "4:3",
        }
    }

    pub fn ratio(self, src_w: u32, src_h: u32) -> f32 {
        match self {
            Aspect::Auto => src_w as f32 / src_h.max(1) as f32,
            Aspect::Wide => 16.0 / 9.0,
            Aspect::Vertical => 9.0 / 16.0,
            Aspect::Square => 1.0,
            Aspect::Classic => 4.0 / 3.0,
        }
    }
}

/// Canvas size with the given short side, rounded to even numbers for the encoder.
pub fn canvas_size(aspect: Aspect, src_w: u32, src_h: u32, short: u32) -> (u32, u32) {
    let r = aspect.ratio(src_w, src_h);
    let even = |v: f32| ((v / 2.0).round() as u32 * 2).max(2);
    if r >= 1.0 { (even(short as f32 * r), short) } else { (short, even(short as f32 / r)) }
}

/// Style lengths are in pixels of a canvas whose short side is 1080.
pub fn unit(w: u32, h: u32) -> f32 {
    w.min(h) as f32 / 1080.0
}

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Style {
    pub background: usize,
    pub aspect: Aspect,
    pub padding: f32,
    pub radius: f32,
    pub shadow: f32,
    pub zoom: f32,
    pub auto_zoom: bool,
    pub cursor_size: f32,
    pub cursor_smoothing: f32,
    pub click_ripple: bool,
    pub motion_blur: f32,
    /// 0 slow, 0.5 default, 1 fast; drives zoom and pan timing together.
    pub camera_speed: f32,
    pub camera_visible: bool,
    /// Bubble side as a fraction of canvas height.
    pub camera_size: f32,
    /// 0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right.
    pub camera_corner: u8,
    pub camera_circle: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            background: BACKGROUNDS.len(),
            aspect: Aspect::Auto,
            padding: 90.0,
            radius: 14.0,
            shadow: 0.6,
            zoom: 2.0,
            auto_zoom: true,
            cursor_size: 1.6,
            cursor_smoothing: 0.6,
            click_ripple: true,
            motion_blur: 0.5,
            camera_speed: 0.5,
            camera_visible: true,
            camera_size: 0.24,
            camera_corner: 3,
            camera_circle: false,
        }
    }
}

