use crate::motion::Motion;
use crate::project::Click;
use crate::style::{Style, unit};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform};

pub struct Params<'a> {
    pub style: &'a Style,
    pub motion: &'a Motion,
    pub clicks: &'a [Click],
    pub src_w: u32,
    pub src_h: u32,
    pub points_width: f32,
}

/// Shutter time at 100% motion blur.
pub(crate) const MAX_SHUTTER: f64 = 1.0 / 30.0;
/// Canvas pixels of travel per blur sample, and the sample cap, for export and preview.
pub(crate) const HQ_BLUR: (f32, usize) = (3.0, 12);
/// Below this travel the frame is drawn sharp.
const MIN_BLUR_PX: f32 = 2.0;

pub(crate) fn map(tf: Transform, (x, y): (f32, f32)) -> (f32, f32) {
    (tf.sx * x + tf.kx * y + tf.tx, tf.ky * x + tf.sy * y + tf.ty)
}

pub(crate) fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

pub(crate) fn samples(travel: f32, (px_per_sample, max): (f32, usize)) -> usize {
    if travel < MIN_BLUR_PX {
        return 1;
    }
    ((travel / px_per_sample).ceil() as usize).clamp(2, max)
}

/// Sample times spread evenly over a shutter window centred on `t`.
pub(crate) fn sample_time(t: f64, shutter: f64, i: usize, n: usize) -> f64 {
    t + shutter * ((i as f64 + 0.5) / n as f64 - 0.5)
}

fn cursor_path() -> Path {
    let pts = [
        (0.0, 0.0),
        (0.0, 16.8),
        (4.0, 13.0),
        (6.9, 19.4),
        (9.7, 18.2),
        (6.9, 11.9),
        (12.2, 11.9),
    ];
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    for (x, y) in &pts[1..] {
        pb.line_to(*x, *y);
    }
    pb.close();
    pb.finish().expect("valid cursor path")
}

pub struct Layout {
    pub screen: Rect,
    pub transform: Transform,
}

pub fn layout(out_w: u32, out_h: u32, p: &Params, t: f64) -> Layout {
    let (w, h) = (out_w as f32, out_h as f32);
    let s = unit(out_w, out_h);
    let pad = p.style.padding * s;
    let (aw, ah) = ((w - 2.0 * pad).max(16.0), (h - 2.0 * pad).max(16.0));
    let aspect = p.src_w as f32 / p.src_h as f32;
    let (sw, sh) = if aw / ah > aspect { (ah * aspect, ah) } else { (aw, aw / aspect) };
    let screen = Rect::from_xywh((w - sw) / 2.0, (h - sh) / 2.0, sw, sh).unwrap();

    let cam = p.motion.at(t).camera;
    let z = cam.scale.max(1.0);
    let mut fx = screen.left() + cam.fx * sw;
    let mut fy = screen.top() + cam.fy * sh;
    let (hw, hh) = (w / (2.0 * z), h / (2.0 * z));
    fx = fx.clamp(hw, w - hw);
    fy = fy.clamp(hh, h - hh);
    let transform = Transform::from_row(z, 0.0, 0.0, z, w / 2.0 - z * fx, h / 2.0 - z * fy);
    Layout { screen, transform }
}

pub(crate) fn draw_cursor(out: &mut Pixmap, base: Transform) {
    let path = cursor_path();
    let mut shadow = Paint::default();
    shadow.anti_alias = true;
    shadow.set_color(Color::from_rgba(0.0, 0.0, 0.0, 0.28).unwrap());
    out.fill_path(&path, &shadow, FillRule::Winding, base.pre_translate(0.4, 1.2), None);

    let mut white = Paint::default();
    white.anti_alias = true;
    white.set_color(Color::WHITE);
    let stroke = Stroke { width: 2.4, line_join: LineJoin::Round, line_cap: LineCap::Round, ..Default::default() };
    out.stroke_path(&path, &white, &stroke, base, None);

    let mut black = Paint::default();
    black.anti_alias = true;
    black.set_color(Color::BLACK);
    out.fill_path(&path, &black, FillRule::Winding, base, None);
}

