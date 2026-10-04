use crate::project::{CursorSample, ZoomSegment};
use crate::style::Style;

pub const STEP: f64 = 1.0 / 120.0;

/// Fraction of the zoomed view the cursor can roam before the camera re-centres.
const DEAD_ZONE: f32 = 0.5;
/// Last-resort margin: the cursor never gets closer than this fraction of the half view to the edge.
const EDGE_MARGIN: f32 = 0.05;

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
}

impl Pan {
    fn new(pos: f32, omega: f32) -> Self {
        Self { goal: Spring::new(pos, omega), pos: Spring::new(pos, omega) }
    }

    fn step(&mut self, target: f32, dt: f32) {
        self.goal.step(target, dt);
        self.pos.step(self.goal.pos, dt);
    }

    /// Drags the pan just enough to keep the cursor inside the view.
    fn keep_visible(&mut self, cursor: f32, half: f32) {
        let reach = half * (1.0 - EDGE_MARGIN);
        let lo = (cursor - reach).max(half);
        let hi = (cursor + reach).min(1.0 - half);
        if lo <= hi && !(lo..=hi).contains(&self.pos.pos) {
            self.pos.pos = self.pos.pos.clamp(lo, hi);
            self.goal.pos = self.goal.pos.clamp(lo, hi);
        }
    }
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

fn zoom_at(zooms: &[ZoomSegment], t: f64) -> Option<f32> {
    zooms.iter().find(|z| t >= z.start && t < z.end).map(|z| z.scale)
}

impl Motion {
    pub fn build(samples: &[CursorSample], zooms: &[ZoomSegment], duration: f64, style: &Style) -> Self {
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

            let zoom = zoom_at(zooms, t).unwrap_or(1.0);
            if zoom > 1.0 {
                seg_size = 1.0 / zoom;
                let focus = follow(target, (rx, ry), raw_cursor(samples, t + tune.lookahead), zoom);
                if target.is_none() && size > 0.98 {
                    pan_x = Pan::new(focus.0, tune.pan_omega);
                    pan_y = Pan::new(focus.1, tune.pan_omega);
                }
                target = Some(focus);
                pan_x.step(focus.0, dt);
                pan_y.step(focus.1, dt);
                log_size.retarget(seg_size.ln(), tune.zoom_in);
                size = log_size.step(dt).exp();
                if size < 0.98 {
                    let half = size / 2.0;
                    pan_x.keep_visible(cx.pos, half);
                    pan_y.keep_visible(cy.pos, half);
                }
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
