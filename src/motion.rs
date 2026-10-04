use crate::project::{CursorSample, ZoomSegment};
use crate::style::Style;

pub const STEP: f64 = 1.0 / 120.0;

/// Fraction of the zoomed view the cursor can roam before the camera re-centres.
const DEAD_ZONE: f32 = 0.5;
/// How much stiffer the pan gets while the cursor is at or past the view edge.
const EDGE_BOOST: f32 = 2.5;
/// Seconds the full view holds before the video ends.
const END_HOLD: f64 = 0.15;

/// Camera timing derived from one "camera speed" value in 0..1, where 0.5 is the default.
#[derive(Clone, Copy)]
struct Tuning {
    zoom_in: f32,
    zoom_out: f32,
    pan_omega: f32,
    /// Re-centring aims where the cursor will be, so one long move becomes one glide.
    lookahead: f64,
}

impl Tuning {
    fn new(speed: f32) -> Self {
        let s = speed.clamp(0.0, 1.0);
        let pick = |slow: f32, mid: f32, fast: f32| if s < 0.5 { slow + (mid - slow) * s * 2.0 } else { mid + (fast - mid) * (s - 0.5) * 2.0 };
        Self {
            zoom_in: pick(1.2, 0.8, 0.5),
            zoom_out: pick(1.5, 1.0, 0.6),
            pan_omega: pick(7.0, 11.0, 16.0),
            lookahead: pick(0.4, 0.3, 0.2) as f64,
        }
    }
}

/// Two chained springs: the pan starts with zero acceleration, so a re-centre never kicks.
#[derive(Clone, Copy)]
struct Pan {
    goal: Spring,
    pos: Spring,
    omega: f32,
}

impl Pan {
    fn new(pos: f32, omega: f32) -> Self {
        Self { goal: Spring::new(pos, omega), pos: Spring::new(pos, omega), omega }
    }

    fn step(&mut self, target: f32, dt: f32, boost: f32) {
        let omega = self.omega * boost;
        self.goal.omega = omega;
        self.pos.omega = omega;
        self.goal.step(target, dt);
        self.pos.step(self.goal.pos, dt);
    }
}

/// Stiffens the pan as the cursor nears the view edge, so fast moves are caught without a hard clamp.
/// Both axes share it so a diagonal pan stays straight.
fn edge_boost(cursor: (f32, f32), view: (f32, f32), half: f32) -> f32 {
    let reach = ((cursor.0 - view.0).abs()).max((cursor.1 - view.1).abs()) / half;
    let u = ((reach - 0.7) / 0.5).clamp(0.0, 1.0);
    1.0 + EDGE_BOOST * u * u * (3.0 - 2.0 * u)
}

/// Keeps the camera still while the cursor stays inside the dead zone of the current view.
fn follow(current: Option<(f32, f32)>, cursor: (f32, f32), ahead: (f32, f32), scale: f32) -> (f32, f32) {
    let half = 0.5 / scale;
    let clamp = |v: f32| v.clamp(half, 1.0 - half);
    let Some((x, y)) = current else { return (clamp(cursor.0), clamp(cursor.1)) };
    let slack = half * DEAD_ZONE;
    let inside = (cursor.0 - x).abs() <= slack && (cursor.1 - y).abs() <= slack;
    if inside { (clamp(x), clamp(y)) } else { (clamp(ahead.0), clamp(ahead.1)) }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Camera {
    pub scale: f32,
    /// Focus point in normalized screen coordinates.
    pub fx: f32,
    pub fy: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Frame {
    pub cursor: (f32, f32),
    pub camera: Camera,
}

/// Precomputed, deterministic motion so scrubbing and export agree frame for frame.
pub struct Motion {
    frames: Vec<Frame>,
}

#[derive(Clone, Copy)]
struct Spring {
    pos: f32,
    vel: f32,
    omega: f32,
}

impl Spring {
    fn new(pos: f32, omega: f32) -> Self {
        Self { pos, vel: 0.0, omega }
    }

    fn step(&mut self, target: f32, dt: f32) {
        let accel = self.omega * self.omega * (target - self.pos) - 2.0 * self.omega * self.vel;
        self.vel += accel * dt;
        self.pos += self.vel * dt;
    }
}

/// Time-based ease that starts and lands with zero acceleration. A retarget mid-flight
/// keeps the current velocity, so chained zooms never jerk.
struct Ease {
    from: f32,
    vel: f32,
    to: f32,
    elapsed: f32,
    dur: f32,
}

impl Ease {
    fn new(pos: f32) -> Self {
        Self { from: pos, vel: 0.0, to: pos, elapsed: 0.0, dur: 1.0 }
    }

    fn sample(&self) -> (f32, f32) {
        let u = (self.elapsed / self.dur).clamp(0.0, 1.0);
        let (u2, u3) = (u * u, u * u * u);
        let d = self.to - self.from;
        let v = self.vel * self.dur;
        let pos = self.from + d * u3 * (10.0 - 15.0 * u + 6.0 * u2) + v * (u - 6.0 * u3 + 8.0 * u3 * u - 3.0 * u3 * u2);
        let dpos = d * 30.0 * u2 * (1.0 - u) * (1.0 - u) + v * (1.0 - 18.0 * u2 + 32.0 * u3 - 15.0 * u3 * u);
        (pos, dpos / self.dur)
    }

    fn retarget(&mut self, to: f32, dur: f32) {
        if (to - self.to).abs() < 1e-6 {
            return;
        }
        let (pos, vel) = self.sample();
        *self = Self { from: pos, vel, to, elapsed: 0.0, dur };
    }

    fn step(&mut self, dt: f32) -> f32 {
        self.elapsed += dt;
        self.sample().0
    }
}

pub fn raw_cursor(samples: &[CursorSample], t: f64) -> (f32, f32) {
    match samples.len() {
        0 => (0.5, 0.5),
        _ => {
            let i = samples.partition_point(|s| s.t <= t);
            if i == 0 {
                return (samples[0].x, samples[0].y);
            }
            if i >= samples.len() {
                let s = samples[samples.len() - 1];
                return (s.x, s.y);
            }
            let (a, b) = (samples[i - 1], samples[i]);
            let k = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
            (a.x + (b.x - a.x) * k, a.y + (b.y - a.y) * k)
        }
    }
}

fn zoom_at(zooms: &[ZoomSegment], t: f64) -> Option<&ZoomSegment> {
    zooms.iter().find(|z| t >= z.start && t < z.end && z.scale > 1.0)
}

/// Where a zoom should land: the first click in the segment, else where the cursor rests once the zoom settles.
fn zoom_aim(samples: &[CursorSample], seg: &ZoomSegment, settle: f64) -> (f32, f32) {
    let from = samples.partition_point(|s| s.t < seg.start);
    let click = samples[from..].iter().take_while(|s| s.t < seg.end).enumerate().find(|&(i, s)| s.down && (i + from == 0 || !samples[i + from - 1].down));
    match click {
        Some((_, s)) => (s.x, s.y),
        None => raw_cursor(samples, settle.min(seg.end)),
    }
}

impl Motion {
    /// `end` is the source time where the video ends; the camera is fully zoomed out by then.
    pub fn build(samples: &[CursorSample], zooms: &[ZoomSegment], duration: f64, end: f64, style: &Style) -> Self {
        let tune = Tuning::new(style.camera_speed);
        let smoothing = style.cursor_smoothing;
        let n = (duration / STEP).ceil() as usize + 2;
        let dt = STEP as f32;
        let (x0, y0) = raw_cursor(samples, 0.0);
        let cursor_omega = 60.0 - 50.0 * smoothing.clamp(0.0, 1.0);
        let mut cx = Spring::new(x0, cursor_omega);
        let mut cy = Spring::new(y0, cursor_omega);

        // The camera eases ln(view size), so zoom in and zoom out feel equally paced to the eye.
        // The centre is derived from how far the size has travelled, so a zoom pushes straight
        // into its focus point instead of swinging.
        let mut log_size = Ease::new(0.0);
        let mut size = 1.0f32;
        let mut pan_x = Pan::new(0.5, tune.pan_omega);
        let mut pan_y = Pan::new(0.5, tune.pan_omega);
        let mut target: Option<(f32, f32)> = None;
        let mut seg_size = 0.5f32;
        let mut landing = 0.0f64;
        let release_by = end - tune.zoom_out as f64 - END_HOLD;

        let mut frames = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 * STEP;
            let (rx, ry) = raw_cursor(samples, t);
            if smoothing <= 0.01 {
                cx.pos = rx;
                cy.pos = ry;
            } else {
                cx.step(rx, dt);
                cy.step(ry, dt);
            }

            if let Some(seg) = zoom_at(zooms, t).filter(|_| t < release_by) {
                let zoom = seg.scale;
                seg_size = 1.0 / zoom;
                let half = 0.5 / zoom;
                let clamp = |v: f32| v.clamp(half, 1.0 - half);
                // Entering a zoom aims straight at the work area and holds there until the zoom lands,
                // so the camera pushes in along one line instead of zooming and then panning.
                if target.is_none() {
                    let aim = zoom_aim(samples, seg, t + tune.zoom_in as f64);
                    let aim = (clamp(aim.0), clamp(aim.1));
                    if size > 0.98 {
                        pan_x = Pan::new(aim.0, tune.pan_omega);
                        pan_y = Pan::new(aim.1, tune.pan_omega);
                    }
                    target = Some(aim);
                    landing = t + tune.zoom_in as f64;
                }
                let settling = t < landing;
                let focus = match target {
                    Some(held) if settling => held,
                    _ => follow(target, (rx, ry), raw_cursor(samples, t + tune.lookahead), zoom),
                };
                target = Some(focus);
                let boost = if settling { 1.0 } else { edge_boost((rx, ry), (pan_x.pos.pos, pan_y.pos.pos), half) };
                pan_x.step(focus.0, dt, boost);
                pan_y.step(focus.1, dt, boost);
                log_size.retarget(seg_size.ln(), tune.zoom_in);
                size = log_size.step(dt).exp();
            } else {
                target = None;
                log_size.retarget(0.0, tune.zoom_out);
                size = log_size.step(dt).exp();
            }

            let progress = ((1.0 - size) / (1.0 - seg_size).max(1e-3)).clamp(0.0, 1.0);
            frames.push(Frame {
                cursor: (cx.pos, cy.pos),
                camera: Camera {
                    scale: 1.0 / size.clamp(0.05, 1.0),
                    fx: 0.5 + (pan_x.pos.pos - 0.5) * progress,
                    fy: 0.5 + (pan_y.pos.pos - 0.5) * progress,
                },
            });
        }
        Self { frames }
    }

    pub fn at(&self, t: f64) -> Frame {
        let f = (t.max(0.0) / STEP).min((self.frames.len() - 1) as f64);
        let i = f.floor() as usize;
        let j = (i + 1).min(self.frames.len() - 1);
        let k = (f - i as f64) as f32;
        let (a, b) = (self.frames[i], self.frames[j]);
        let lerp = |p: f32, q: f32| p + (q - p) * k;
        Frame {
            cursor: (lerp(a.cursor.0, b.cursor.0), lerp(a.cursor.1, b.cursor.1)),
            camera: Camera {
                scale: lerp(a.camera.scale, b.camera.scale),
                fx: lerp(a.camera.fx, b.camera.fx),
                fy: lerp(a.camera.fy, b.camera.fy),
            },
        }
    }
}
