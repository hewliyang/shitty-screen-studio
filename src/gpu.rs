//! Metal compositor: one fragment shader draws the whole frame, so export never leaves the GPU.
use crate::compositor::{HQ_BLUR, MAX_SHUTTER, Params, distance, draw_cursor, layout, map, sample_time, samples};
use crate::style::{Fill, fill, unit, wallpaper_file};
use anyhow::{Context as _, Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_av_foundation::{AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVMediaTypeVideo, AVURLAsset};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMTime, CMTimeRange};
use objc2_core_video::{
    CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferPool,
    CVPixelBufferGetWidth, kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLLibrary, MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSamplerAddressMode,
    MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerMipFilter, MTLSamplerState, MTLSize, MTLStorageMode,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;

const SHADER: &str = r#"#include <metal_stdlib>
using namespace metal;

struct U {
    float4 canvas;   // w, h, screen samples, cursor samples
    float4 view;     // zoom, tx, ty, ripple count
    float4 screen;   // x, y, w, h in canvas px
    float4 screen2;  // radius, -, shadow alpha, shadow sigma
    float4 misc;     // shadow offset, has camera, ring width, -
    float4 bg_from;
    float4 bg_to;
    float4 cam_rect; // x, y, side, radius in output px
    float4 cam_crop; // u0, v0, du, dv
    float4 cam_misc; // edge width, shadow sigma, shadow offset, shadow alpha
    float4 sprite;   // origin, extent in cursor units
    float4 wallpaper; // has image, uv scale x, y
    float4 keys;     // shortcut pill x, y, w, h in output px
    float4 keys2;    // radius, alpha, shadow sigma, visible
    float4 views[16];
    float4 cursors[24];
    float4 ripples[8];
};

struct VOut { float4 pos [[position]]; };

vertex VOut vs(uint id [[vertex_id]]) {
    float2 p = float2((id << 1) & 2, id & 2);
    VOut o;
    o.pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

static float sd_rrect(float2 p, float4 r, float rad) {
    float2 h = r.zw * 0.5;
    float2 q = abs(p - (r.xy + h)) - h + rad;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - rad;
}

static float2 erf2(float2 x) {
    float2 s = sign(x), a = abs(x);
    x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    x *= x;
    return s - s / (x * x);
}

static float gaussian(float x, float sigma) {
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (2.5066283 * sigma);
}

static float shadow_x(float x, float y, float sigma, float corner, float2 half_size) {
    float delta = min(half_size.y - corner - abs(y), 0.0);
    float curved = half_size.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    float2 integral = 0.5 + 0.5 * erf2((x + float2(-curved, curved)) * (0.70710678 / sigma));
    return integral.y - integral.x;
}

// Gaussian-blurred rounded box (Evan Wallace).
static float rrect_shadow(float2 lower, float2 upper, float2 p, float sigma, float corner) {
    float2 center = (lower + upper) * 0.5;
    float2 half_size = (upper - lower) * 0.5;
    p -= center;
    float low = p.y - half_size.y;
    float high = p.y + half_size.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);
    float step = (end - start) / 4.0;
    float y = start + step * 0.5;
    float value = 0.0;
    for (int i = 0; i < 4; i++) {
        value += shadow_x(p.x, p.y - y, sigma, corner, half_size) * gaussian(y, sigma) * step;
        y += step;
    }
    return value;
}

// Box-filters roughly one output pixel when the source is being shrunk.
static float4 sample_area(texture2d<float> tex, sampler s, float2 uv, float2 duv) {
    float2 texels = duv * float2(tex.get_width(), tex.get_height());
    if (max(texels.x, texels.y) <= 1.25) return tex.sample(s, uv);
    float2 o = duv * 0.25;
    return 0.25 * (tex.sample(s, uv + float2(-o.x, -o.y)) + tex.sample(s, uv + float2(o.x, -o.y)) +
                   tex.sample(s, uv + float2(-o.x, o.y)) + tex.sample(s, uv + float2(o.x, o.y)));
}

fragment float4 fs(VOut in [[stage_in]], constant U& u [[buffer(0)]],
                   texture2d<float> src [[texture(0)]], texture2d<float> cam [[texture(1)]],
                   texture2d<float> cursor [[texture(2)]], texture2d<float> wall [[texture(3)]],
                   texture2d<float> label [[texture(4)]],
                   sampler lin [[sampler(0)]], sampler mip [[sampler(1)]]) {
    float2 p = in.pos.xy;
    float z = u.view.x;
    float2 q = (p - u.view.yz) / z;
    float2 wh = u.canvas.xy;
    float3 col;
    if (u.wallpaper.x > 0.0) {
        float2 uv = 0.5 + (q / wh - 0.5) * u.wallpaper.yz;
        float2 duv = u.wallpaper.yz / (wh * z);
        col = wall.sample(mip, uv, gradient2d(float2(duv.x, 0.0), float2(0.0, duv.y))).rgb;
    } else {
        col = mix(u.bg_from.rgb, u.bg_to.rgb, clamp(dot(q, wh) / dot(wh, wh), 0.0, 1.0));
    }
    if (u.screen2.z > 0.0) {
        float2 lo = u.screen.xy + float2(0.0, u.misc.x);
        col *= 1.0 - u.screen2.z * rrect_shadow(lo, lo + u.screen.zw, q, u.screen2.w, u.screen2.x);
    }

    int n = int(u.canvas.z);
    float4 acc = 0.0;
    for (int i = 0; i < n; i++) {
        float4 v = u.views[i];
        float2 qi = (p - v.yz) / v.x;
        float cov = clamp(0.5 - sd_rrect(qi, u.screen, u.screen2.x) * v.x, 0.0, 1.0);
        if (cov <= 0.0) continue;
        float2 uv = (qi - u.screen.xy) / u.screen.zw;
        acc += float4(sample_area(src, lin, uv, 1.0 / (v.x * u.screen.zw)).rgb, 1.0) * cov;
    }
    acc /= float(max(n, 1));
    col = acc.rgb + col * (1.0 - acc.a);

    int rn = int(u.view.w);
    for (int i = 0; i < rn; i++) {
        float4 r = u.ripples[i];
        float d = length(q - r.xy) - r.z;
        col = mix(col, float3(1.0), clamp(0.5 - d * z, 0.0, 1.0) * 0.35 * r.w);
        col = mix(col, float3(1.0), clamp(0.5 - (abs(d) - u.misc.z * 0.5) * z, 0.0, 1.0) * 0.9 * r.w);
    }

    int cn = int(u.canvas.w);
    if (cn > 0) {
        float4 c = 0.0;
        for (int i = 0; i < cn; i++) {
            float4 k = u.cursors[i];
            c += cursor.sample(mip, ((p - k.xy) / k.z - u.sprite.xy) / u.sprite.zw);
        }
        c /= float(cn);
        col = c.rgb + col * (1.0 - c.a);
    }

    if (u.misc.y > 0.0) {
        float4 r = u.cam_rect;
        float2 lo = r.xy + float2(0.0, u.cam_misc.z);
        col *= 1.0 - u.cam_misc.w * rrect_shadow(lo, lo + r.z, p, u.cam_misc.y, r.w);
        float d = sd_rrect(p, float4(r.xy, r.z, r.z), r.w);
        float cov = clamp(0.5 - d, 0.0, 1.0);
        if (cov > 0.0) {
            float2 l = (p - r.xy) / r.z;
            float2 uv = u.cam_crop.xy + float2(1.0 - l.x, l.y) * u.cam_crop.zw;
            col = mix(col, sample_area(cam, lin, uv, u.cam_crop.zw / r.z).rgb, cov);
        }
        col = mix(col, float3(1.0), clamp(0.5 - (abs(d) - u.cam_misc.x * 0.5), 0.0, 1.0) * 0.18);
    }

    if (u.keys2.w > 0.0) {
        float4 r = u.keys;
        float a = u.keys2.y;
        float2 lo = r.xy + float2(0.0, u.keys2.z * 0.5);
        col *= 1.0 - 0.4 * a * rrect_shadow(lo, lo + r.zw, p, u.keys2.z, u.keys2.x);
        col = mix(col, float3(0.0), clamp(0.5 - sd_rrect(p, r, u.keys2.x), 0.0, 1.0) * 0.86 * a);
        float2 ts = float2(label.get_width(), label.get_height());
        float2 tl = floor(r.xy + (r.zw - ts) * 0.5 + 0.5);
        col = mix(col, float3(1.0), label.sample(lin, (p - tl) / ts).a * a);
    }
    return float4(col, 1.0);
}

// Full-range BT.601 NV12, which is what GPUI surfaces expect.
// GPUI's layer is shown as sRGB, so Display P3 frames are converted first.
float3 to_display(float3 c, constant uint& p3) {
    if (p3 == 0) return c;
    float3 lin = select(pow((c + 0.055) / 1.055, 2.4), c / 12.92, c <= 0.04045);
    const float3x3 m = float3x3(float3(1.2249, -0.0420, -0.0197), float3(-0.2247, 1.0419, -0.0786), float3(0.0, 0.0, 1.0979));
    lin = saturate(m * lin);
    return select(1.055 * pow(lin, 1.0 / 2.4) - 0.055, lin * 12.92, lin <= 0.0031308);
}

fragment float fs_y(VOut in [[stage_in]], texture2d<float> src [[texture(0)]], constant uint& p3 [[buffer(0)]]) {
    return dot(to_display(src.read(uint2(in.pos.xy)).rgb, p3), float3(0.299, 0.587, 0.114));
}

fragment float2 fs_uv(VOut in [[stage_in]], texture2d<float> src [[texture(0)]], constant uint& p3 [[buffer(0)]]) {
    uint2 p = uint2(in.pos.xy) * 2;
    float3 c = 0.25 * (to_display(src.read(p).rgb, p3) + to_display(src.read(p + uint2(1, 0)).rgb, p3)
        + to_display(src.read(p + uint2(0, 1)).rgb, p3) + to_display(src.read(p + uint2(1, 1)).rgb, p3));
    return float2(dot(c, float3(-0.168736, -0.331264, 0.5)) + 0.5, dot(c, float3(0.5, -0.418688, -0.081312)) + 0.5);
}
"#;

pub type Texture = Retained<ProtocolObject<dyn MTLTexture>>;
type CommandBuffer = Retained<ProtocolObject<dyn MTLCommandBuffer>>;

const MAX_VIEWS: usize = 16;
/// Longest blur streak, in pixels of a 1080p canvas. Fast pans get a shorter shutter instead.
const MAX_STREAK: f32 = 16.0;
const MAX_CURSORS: usize = 24;
const MAX_RIPPLES: usize = 8;
/// The cursor sprite covers this box in cursor units, at SPRITE_SCALE px per unit.
const SPRITE_ORIGIN: (f32, f32) = (-3.0, -3.0);
const SPRITE_EXTENT: f32 = 27.0;
const SPRITE_SCALE: f32 = 16.0;
/// Shortcut pill sizes, in pixels of a 1080p canvas.
const KEY_FONT: f32 = 36.0;
const KEY_HEIGHT: f32 = 72.0;
const KEY_PAD: f32 = 24.0;
const KEY_RADIUS: f32 = 18.0;
const KEY_MARGIN: f32 = 48.0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Uniforms {
    canvas: [f32; 4],
    view: [f32; 4],
    screen: [f32; 4],
    screen2: [f32; 4],
    misc: [f32; 4],
    bg_from: [f32; 4],
    bg_to: [f32; 4],
    cam_rect: [f32; 4],
    cam_crop: [f32; 4],
    cam_misc: [f32; 4],
    sprite: [f32; 4],
    wallpaper: [f32; 4],
    keys: [f32; 4],
    keys2: [f32; 4],
    views: [[f32; 4]; MAX_VIEWS],
    cursors: [[f32; 4]; MAX_CURSORS],
    ripples: [[f32; 4]; MAX_RIPPLES],
}

fn color(c: u32) -> [f32; 4] {
    [((c >> 16) & 255) as f32 / 255.0, ((c >> 8) & 255) as f32 / 255.0, (c & 255) as f32 / 255.0, 1.0]
}

/// `label` is the shortcut's alpha and the size of its text bitmap.
#[allow(clippy::too_many_arguments)]
fn uniforms(p: &Params, t: f64, w: u32, h: u32, cam: Option<(u32, u32)>, wall: Option<(u32, u32)>, label: Option<(f32, u32, u32)>) -> Uniforms {
    let st = p.style;
    let (wf, hf) = (w as f32, h as f32);
    let s = unit(w, h);
    let mut u = Uniforms::default();
    let lay = layout(w, h, p, t);
    let screen = lay.screen;
    let tf = lay.transform;
    let radius = st.radius * s;
    match (fill(st.background), wall) {
        (Fill::Wallpaper(_), Some((iw, ih))) => {
            let (canvas, image) = (wf / hf, iw as f32 / ih as f32);
            let scale = if image > canvas { [canvas / image, 1.0] } else { [1.0, image / canvas] };
            u.wallpaper = [1.0, scale[0], scale[1], 0.0];
        }
        (Fill::Gradient(bg), _) => {
            u.bg_from = color(bg.from);
            u.bg_to = color(bg.to);
        }
        (Fill::Wallpaper(_), None) => {
            u.bg_from = color(0x2b2d42);
            u.bg_to = color(0x1b1c2a);
        }
    }
    u.screen = [screen.left(), screen.top(), screen.width(), screen.height()];
    let blur_r = (36.0 * s / 4.0).round().max(0.5);
    u.screen2 = [radius.min(screen.width() / 2.0).min(screen.height() / 2.0), 0.0, st.shadow.clamp(0.0, 1.0), 4.0 * (blur_r * (blur_r + 1.0)).sqrt()];
    u.misc[0] = 18.0 * s;
    u.view = [tf.sx, tf.tx, tf.ty, 0.0];

    let shutter = st.motion_blur.clamp(0.0, 1.0) as f64 * MAX_SHUTTER;
    let tf_at = |t: f64| layout(w, h, p, t).transform;
    let corners = [(screen.left(), screen.top()), (screen.right(), screen.top()), (screen.left(), screen.bottom()), (screen.right(), screen.bottom())];
    let blur = (HQ_BLUR.0, MAX_VIEWS);
    let max_streak = MAX_STREAK * s;
    let capped = |shutter: f64, travel: f32| if travel > max_streak { (shutter * (max_streak / travel) as f64, max_streak) } else { (shutter, travel) };
    let (screen_shutter, n) = if shutter > 0.0 {
        let (a, b) = (tf_at(t - shutter / 2.0), tf_at(t + shutter / 2.0));
        let (sh, travel) = capped(shutter, corners.iter().map(|&c| distance(map(a, c), map(b, c))).fold(0.0, f32::max));
        (sh, samples(travel, blur))
    } else {
        (0.0, 1)
    };
    for i in 0..n {
        let v = if n == 1 { tf } else { tf_at(sample_time(t, screen_shutter, i, n)) };
        u.views[i] = [v.sx, v.tx, v.ty, 0.0];
    }
    u.canvas = [wf, hf, n as f32, 0.0];

    let to_canvas = |nx: f32, ny: f32| (screen.left() + nx * screen.width(), screen.top() + ny * screen.height());
    let px_per_pt = screen.width() / p.points_width.max(1.0);
    if st.click_ripple {
        let mut k = 0;
        for c in p.clicks {
            let age = (t - c.t) as f32;
            if !(0.0..0.5).contains(&age) || k == MAX_RIPPLES {
                continue;
            }
            let e = 1.0 - (1.0 - age / 0.5).powi(3);
            let (x, y) = to_canvas(c.x, c.y);
            u.ripples[k] = [x, y, (6.0 + 22.0 * e) * px_per_pt * st.cursor_size.max(0.5), 1.0 - e];
            k += 1;
        }
        u.view[3] = k as f32;
        u.misc[2] = 2.0 * px_per_pt;
    }

    let k = px_per_pt * st.cursor_size;
    let cursor_at = |t: f64| -> Option<[f32; 4]> {
        let (cx, cy) = p.motion.at(t).cursor;
        if !((-0.01..=1.01).contains(&cx) && (-0.01..=1.01).contains(&cy)) {
            return None;
        }
        let (x, y) = to_canvas(cx, cy);
        let v = tf_at(t);
        Some([v.sx * x + v.tx, v.sy * y + v.ty, v.sx * k, 0.0])
    };
    let (cursor_shutter, cn) = match (shutter > 0.0, cursor_at(t - shutter / 2.0), cursor_at(t + shutter / 2.0)) {
        (true, Some(a), Some(b)) => {
            let (sh, travel) = capped(shutter, distance((a[0], a[1]), (b[0], b[1])));
            (sh, samples(travel, (blur.0 / 2.0, MAX_CURSORS)))
        }
        _ => (0.0, 1),
    };
    let mut m = 0;
    for i in 0..cn {
        let ti = if cn == 1 { t } else { sample_time(t, cursor_shutter, i, cn) };
        if let Some(pose) = cursor_at(ti) {
            u.cursors[m] = pose;
            m += 1;
        }
    }
    u.canvas[3] = m as f32;
    u.sprite = [SPRITE_ORIGIN.0, SPRITE_ORIGIN.1, SPRITE_EXTENT, SPRITE_EXTENT];

    if let Some((cw, ch)) = cam.filter(|_| st.camera_visible) {
        let zoom = p.motion.at(t).camera.scale;
        let side = st.camera_size * hf * (1.0 - 0.3 * (zoom - 1.0).clamp(0.0, 1.0));
        if side >= 8.0 {
            let margin = 40.0 * s;
            let x = if st.camera_corner % 2 == 1 { wf - margin - side } else { margin };
            let y = if st.camera_corner >= 2 { hf - margin - side } else { margin };
            let radius = if st.camera_circle { side / 2.0 } else { side * 0.2 };
            let (cw, ch) = (cw as f32, ch as f32);
            let crop = cw.min(ch);
            u.cam_rect = [x, y, side, radius];
            u.cam_crop = [(cw - crop) / 2.0 / cw, (ch - crop) / 2.0 / ch, crop / cw, crop / ch];
            u.cam_misc = [1.5 * s.max(0.5), 8.0 * s, 6.0 * s, 0.35];
            u.misc[1] = 1.0;
        }
    }

    if let Some((alpha, tw, _)) = label {
        let ph = KEY_HEIGHT * s;
        let pw = (tw as f32 + 2.0 * KEY_PAD * s).max(ph);
        let margin = KEY_MARGIN * s;
        let x = match st.keys_position % 3 {
            0 => margin,
            1 => (wf - pw) / 2.0,
            _ => wf - margin - pw,
        };
        let rise = (1.0 - alpha) * 10.0 * s;
        let y = if st.keys_position >= 3 { hf - margin - ph + rise } else { margin - rise };
        u.keys = [x, y, pw, ph];
        u.keys2 = [KEY_RADIUS * s, alpha, 10.0 * s, 1.0];
    }
    u
}

fn key_font_size(w: u32, h: u32) -> u32 {
    (KEY_FONT * unit(w, h)).round().max(6.0) as u32
}

pub struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn objc2_metal::MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    linear: Retained<ProtocolObject<dyn MTLSamplerState>>,
    mip: Retained<ProtocolObject<dyn MTLSamplerState>>,
    sprite: Texture,
    cache: CFRetained<CVMetalTextureCache>,
    uploads: [Option<Texture>; 2],
    target: Option<Texture>,
    nv12: Option<Nv12>,
    wallpaper: Option<(usize, Option<Texture>)>,
    /// Text bitmap for the shortcut on screen, by label and font size.
    label: Option<(String, u32, Texture)>,
}

struct Nv12 {
    y: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    uv: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    pool: Option<(u32, u32, CFRetained<CVPixelBufferPool>)>,
}

/// Keeps a pixel buffer's Metal view alive until the GPU is done with it.
pub struct Wrapped {
    _cv: CFRetained<CVMetalTexture>,
    pub texture: Texture,
}

impl Gpu {
    pub fn new() -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .map_err(|e| anyhow!("shader: {}", e.localizedDescription()))?;
        let f = |name: &str| library.newFunctionWithName(&NSString::from_str(name)).context("missing shader function");
        let vs = f("vs")?;
        let pipe = |fs: &str, format: MTLPixelFormat| -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
            let desc = MTLRenderPipelineDescriptor::new();
            desc.setVertexFunction(Some(&vs));
            let func = f(fs)?;
            desc.setFragmentFunction(Some(&func));
            unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(format);
            device.newRenderPipelineStateWithDescriptor_error(&desc).map_err(|e| anyhow!("pipeline: {}", e.localizedDescription()))
        };
        let pipeline = pipe("fs", MTLPixelFormat::BGRA8Unorm)?;
        let nv12 = Nv12 { y: pipe("fs_y", MTLPixelFormat::R8Unorm)?, uv: pipe("fs_uv", MTLPixelFormat::RG8Unorm)?, pool: None };
        let queue = device.newCommandQueue().context("no command queue")?;
        let sampler = |mip: bool| {
            let d = MTLSamplerDescriptor::new();
            d.setMinFilter(MTLSamplerMinMagFilter::Linear);
            d.setMagFilter(MTLSamplerMinMagFilter::Linear);
            let mode = if mip { MTLSamplerAddressMode::ClampToZero } else { MTLSamplerAddressMode::ClampToEdge };
            d.setSAddressMode(mode);
            d.setTAddressMode(mode);
            if mip {
                d.setMipFilter(MTLSamplerMipFilter::Linear);
            }
            device.newSamplerStateWithDescriptor(&d).context("no sampler")
        };
        let (linear, mip) = (sampler(false)?, sampler(true)?);

        let mut cache: *mut CVMetalTextureCache = std::ptr::null_mut();
        let status = unsafe { CVMetalTextureCache::create(None, None, &device, None, NonNull::from(&mut cache)) };
        if status != kCVReturnSuccess || cache.is_null() {
            bail!("texture cache: {status}");
        }
        let cache = unsafe { CFRetained::from_raw(NonNull::new_unchecked(cache)) };
        let sprite = Self::make_sprite(&device, &queue)?;
        Ok(Self { device, queue, pipeline, linear, mip, sprite, cache, uploads: [None, None], target: None, nv12: Some(nv12), wallpaper: None, label: None })
    }

    fn make_sprite(device: &ProtocolObject<dyn MTLDevice>, queue: &ProtocolObject<dyn objc2_metal::MTLCommandQueue>) -> Result<Texture> {
        let size = (SPRITE_EXTENT * SPRITE_SCALE) as u32;
        let mut px = tiny_skia::Pixmap::new(size, size).context("sprite")?;
        let base = tiny_skia::Transform::from_scale(SPRITE_SCALE, SPRITE_SCALE).pre_translate(-SPRITE_ORIGIN.0, -SPRITE_ORIGIN.1);
        draw_cursor(&mut px, base);
        mipmapped(device, queue, px.data(), size, size)
    }

    /// Loads the style's wallpaper once and keeps it until the style picks another.
    pub fn prepare(&mut self, p: &Params) {
        let Fill::Wallpaper(i) = fill(p.style.background) else { return };
        if self.wallpaper.as_ref().is_some_and(|(cur, _)| *cur == i) {
            return;
        }
        let tex = wallpaper_file(i, false)
            .and_then(|path| image::open(path).ok())
            .map(|img| img.to_rgba8())
            .and_then(|img| mipmapped(&self.device, &self.queue, img.as_raw(), img.width(), img.height()).ok());
        self.wallpaper = Some((i, tex));
    }

    /// Draws the text for the shortcut shown at `t`, once per label and canvas size.
    pub fn prepare_label(&mut self, p: &Params, t: f64, w: u32, h: u32) {
        let Some(shown) = crate::keys::shown_at(p.keys, t).filter(|_| p.style.keys_visible) else { return };
        let size = key_font_size(w, h);
        if self.label.as_ref().is_some_and(|(l, s, _)| *l == shown.label && *s == size) {
            return;
        }
        self.label = crate::keys::rasterize(&shown.label, size as f32).and_then(|b| {
            let tex = self.texture(b.width, b.height, MTLPixelFormat::RGBA8Unorm, false).ok()?;
            replace(&tex, &b.rgba, b.width, b.height);
            Some((shown.label, size, tex))
        });
    }
}

fn mipmapped(device: &ProtocolObject<dyn MTLDevice>, queue: &ProtocolObject<dyn objc2_metal::MTLCommandQueue>, rgba: &[u8], w: u32, h: u32) -> Result<Texture> {
    {
        let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA8Unorm, w as usize, h as usize, true) };
        desc.setUsage(MTLTextureUsage::ShaderRead);
        let tex = device.newTextureWithDescriptor(&desc).context("texture")?;
        replace(&tex, rgba, w, h);
        let cb = queue.commandBuffer().context("command buffer")?;
        let blit = cb.blitCommandEncoder().context("blit")?;
        blit.generateMipmapsForTexture(&tex);
        blit.endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        Ok(tex)
    }
}

impl Gpu {
    pub fn wrap(&self, buffer: &CVPixelBuffer) -> Result<Wrapped> {
        let (w, h) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer));
        self.wrap_plane(buffer, MTLPixelFormat::BGRA8Unorm, w, h, 0)
    }

    fn wrap_plane(&self, buffer: &CVPixelBuffer, format: MTLPixelFormat, w: usize, h: usize, plane: usize) -> Result<Wrapped> {
        let mut out: *mut CVMetalTexture = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCache::create_texture_from_image(None, &self.cache, buffer, None, format, w, h, plane, NonNull::from(&mut out))
        };
        if status != kCVReturnSuccess || out.is_null() {
            bail!("wrap pixel buffer: {status}");
        }
        let cv = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) };
        let texture = CVMetalTextureGetTexture(&cv).context("pixel buffer texture")?;
        Ok(Wrapped { _cv: cv, texture })
    }

    fn texture(&self, w: u32, h: u32, format: MTLPixelFormat, target: bool) -> Result<Texture> {
        let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, w as usize, h as usize, false) };
        desc.setUsage(if target { MTLTextureUsage::RenderTarget } else { MTLTextureUsage::ShaderRead });
        desc.setStorageMode(MTLStorageMode::Shared);
        self.device.newTextureWithDescriptor(&desc).context("texture")
    }

    fn upload(&mut self, slot: usize, rgba: &[u8], w: u32, h: u32) -> Result<Texture> {
        let fits = self.uploads[slot].as_ref().is_some_and(|t| t.width() == w as usize && t.height() == h as usize);
        if !fits {
            self.uploads[slot] = Some(self.texture(w, h, MTLPixelFormat::RGBA8Unorm, false)?);
        }
        let tex = self.uploads[slot].clone().unwrap();
        replace(&tex, rgba, w, h);
        Ok(tex)
    }

    /// Encodes one frame into `target` and commits it. Wait on the result before reading `target`.
    pub fn draw(&self, target: &ProtocolObject<dyn MTLTexture>, src: &ProtocolObject<dyn MTLTexture>, cam: Option<&ProtocolObject<dyn MTLTexture>>, t: f64, p: &Params) -> Result<CommandBuffer> {
        let (w, h) = (target.width() as u32, target.height() as u32);
        let wall = self.wallpaper.as_ref().and_then(|(_, t)| t.as_ref()).filter(|_| matches!(fill(p.style.background), Fill::Wallpaper(_)));
        let label = crate::keys::shown_at(p.keys, t).filter(|_| p.style.keys_visible).and_then(|shown| {
            let (l, size, tex) = self.label.as_ref()?;
            (*l == shown.label && *size == key_font_size(w, h)).then_some((shown.alpha, tex))
        });
        let u = uniforms(
            p,
            t,
            w,
            h,
            cam.map(|c| (c.width() as u32, c.height() as u32)),
            wall.map(|t| (t.width() as u32, t.height() as u32)),
            label.map(|(a, tex)| (a, tex.width() as u32, tex.height() as u32)),
        );
        let pass = MTLRenderPassDescriptor::new();
        let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        att.setTexture(Some(target));
        att.setLoadAction(MTLLoadAction::DontCare);
        att.setStoreAction(MTLStoreAction::Store);
        let cb = self.queue.commandBuffer().context("command buffer")?;
        let enc = cb.renderCommandEncoderWithDescriptor(&pass).context("render encoder")?;
        enc.setRenderPipelineState(&self.pipeline);
        unsafe {
            enc.setFragmentBytes_length_atIndex(NonNull::from(&u).cast::<c_void>(), size_of::<Uniforms>(), 0);
            enc.setFragmentTexture_atIndex(Some(src), 0);
            enc.setFragmentTexture_atIndex(Some(cam.unwrap_or(src)), 1);
            enc.setFragmentTexture_atIndex(Some(&self.sprite), 2);
            enc.setFragmentTexture_atIndex(Some(wall.map_or(&*self.sprite, |t| &**t)), 3);
            enc.setFragmentTexture_atIndex(Some(label.map_or(&*self.sprite, |(_, t)| &**t)), 4);
            enc.setFragmentSamplerState_atIndex(Some(&self.linear), 0);
            enc.setFragmentSamplerState_atIndex(Some(&self.mip), 1);
            enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }
        enc.endEncoding();
        cb.commit();
        Ok(cb)
    }

    /// Composes CPU-side RGBA frames and reads the result back as BGRA.
    #[allow(clippy::too_many_arguments)]
    pub fn render_bgra(&mut self, src: (&[u8], u32, u32), cam: Option<(&[u8], u32, u32)>, t: f64, p: &Params, w: u32, h: u32, out: &mut Vec<u8>) -> Result<()> {
        self.prepare(p);
        self.prepare_label(p, t, w, h);
        let src = self.upload(0, src.0, src.1, src.2)?;
        let cam = match cam {
            Some((b, cw, ch)) if b.len() == (cw * ch * 4) as usize => Some(self.upload(1, b, cw, ch)?),
            _ => None,
        };
        if !self.target.as_ref().is_some_and(|t| t.width() == w as usize && t.height() == h as usize) {
            self.target = Some(self.texture(w, h, MTLPixelFormat::BGRA8Unorm, true)?);
        }
        let target = self.target.clone().unwrap();
        let cb = self.draw(&target, &src, cam.as_deref(), t, p)?;
        cb.waitUntilCompleted();
        out.resize((w * h * 4) as usize, 0);
        unsafe {
            target.getBytes_bytesPerRow_fromRegion_mipmapLevel(NonNull::new(out.as_mut_ptr().cast()).unwrap(), (w * 4) as usize, region(w, h), 0);
        }
        Ok(())
    }
}

impl Gpu {
    /// Composes a frame from decoded pixel buffers into a fresh NV12 buffer that GPUI can show as a surface.
    pub fn present(&mut self, src: &CVPixelBuffer, cam: Option<&CVPixelBuffer>, t: f64, p: &Params, p3: bool, w: u32, h: u32) -> Result<CFRetained<CVPixelBuffer>> {
        self.prepare(p);
        self.prepare_label(p, t, w, h);
        let src = self.wrap(src)?;
        let cam = cam.map(|c| self.wrap(c)).transpose()?;
        if !self.target.as_ref().is_some_and(|t| t.width() == w as usize && t.height() == h as usize) {
            let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::BGRA8Unorm, w as usize, h as usize, false) };
            desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            desc.setStorageMode(MTLStorageMode::Private);
            self.target = Some(self.device.newTextureWithDescriptor(&desc).context("texture")?);
        }
        let target = self.target.clone().unwrap();
        let mut nv12 = self.nv12.take().context("nv12 pipelines")?;
        let result = (|| {
            if !nv12.pool.as_ref().is_some_and(|(pw, ph, _)| (*pw, *ph) == (w, h)) {
                nv12.pool = Some((w, h, nv12_pool(w, h)?));
            }
            let buffer = crate::export::pool_buffer(&nv12.pool.as_ref().unwrap().2)?;
            let y = self.wrap_plane(&buffer, MTLPixelFormat::R8Unorm, w as usize, h as usize, 0)?;
            let uv = self.wrap_plane(&buffer, MTLPixelFormat::RG8Unorm, w as usize / 2, h as usize / 2, 1)?;
            self.draw(&target, &src.texture, cam.as_ref().map(|c| &*c.texture), t, p)?;
            let cb = self.queue.commandBuffer().context("command buffer")?;
            for (plane, pipeline) in [(&y, &nv12.y), (&uv, &nv12.uv)] {
                let pass = MTLRenderPassDescriptor::new();
                let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
                att.setTexture(Some(&plane.texture));
                att.setLoadAction(MTLLoadAction::DontCare);
                att.setStoreAction(MTLStoreAction::Store);
                let enc = cb.renderCommandEncoderWithDescriptor(&pass).context("render encoder")?;
                enc.setRenderPipelineState(pipeline);
                unsafe {
                    enc.setFragmentTexture_atIndex(Some(&target), 0);
                    let flag = p3 as u32;
                    enc.setFragmentBytes_length_atIndex(NonNull::from(&flag).cast(), 4, 0);
                    enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
                }
                enc.endEncoding();
            }
            cb.commit();
            cb.waitUntilCompleted();
            drop((y, uv, src, cam));
            Ok(buffer)
        })();
        self.nv12 = Some(nv12);
        result
    }
}

const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");

fn nv12_pool(w: u32, h: u32) -> Result<CFRetained<CVPixelBufferPool>> {
    let key = NSString::from_str;
    let (k_fmt, k_metal, k_io, k_w, k_h) = (key("PixelFormatType"), key("MetalCompatibility"), key("IOSurfaceProperties"), key("Width"), key("Height"));
    let (fmt, metal, io) = (NSNumber::new_u32(NV12_FULL), NSNumber::new_bool(true), NSDictionary::<NSString, AnyObject>::new());
    let (nw, nh) = (NSNumber::new_u32(w), NSNumber::new_u32(h));
    let attrs = crate::capture::dict(&[
        (&k_fmt, fmt.as_ref()),
        (&k_metal, metal.as_ref()),
        (&k_io, io.as_ref()),
        (&k_w, nw.as_ref()),
        (&k_h, nh.as_ref()),
    ]);
    let mut out: *mut CVPixelBufferPool = std::ptr::null_mut();
    let attrs: &objc2_core_foundation::CFDictionary = unsafe { &*(Retained::as_ptr(&attrs) as *const objc2_core_foundation::CFDictionary) };
    let status = unsafe { CVPixelBufferPool::create(None, None, Some(attrs), NonNull::from(&mut out)) };
    if status != kCVReturnSuccess || out.is_null() {
        bail!("nv12 pool: {status}");
    }
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

fn region(w: u32, h: u32) -> MTLRegion {
    MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w as usize, height: h as usize, depth: 1 } }
}

fn replace(tex: &ProtocolObject<dyn MTLTexture>, data: &[u8], w: u32, h: u32) {
    unsafe { tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region(w, h), 0, NonNull::new(data.as_ptr() as *mut c_void).unwrap(), (w * 4) as usize) };
}

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

pub fn pixel_buffer_attributes(w: Option<(u32, u32)>) -> Retained<NSDictionary<NSString, AnyObject>> {
    let key = NSString::from_str;
    let (k_fmt, k_metal, k_io, k_w, k_h) = (key("PixelFormatType"), key("MetalCompatibility"), key("IOSurfaceProperties"), key("Width"), key("Height"));
    let fmt = NSNumber::new_u32(BGRA);
    let metal = NSNumber::new_bool(true);
    let io = NSDictionary::<NSString, AnyObject>::new();
    let mut entries: Vec<(&NSString, &AnyObject)> = vec![(&k_fmt, fmt.as_ref()), (&k_metal, metal.as_ref()), (&k_io, io.as_ref())];
    let (nw, nh) = w.map(|(w, h)| (NSNumber::new_u32(w), NSNumber::new_u32(h))).unzip();
    if let (Some(nw), Some(nh)) = (&nw, &nh) {
        entries.push((&k_w, nw.as_ref()));
        entries.push((&k_h, nh.as_ref()));
    }
    crate::capture::dict(&entries)
}

/// Hardware-decoded BGRA frames, sampled at arbitrary times. Times start at the first frame.
pub struct Frames {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
    origin: Option<f64>,
    cur: Option<(CFRetained<CVPixelBuffer>, f64)>,
    next: Option<(CFRetained<CVPixelBuffer>, f64)>,
    done: bool,
}

impl Frames {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_at(path, 0.0)
    }

    /// Starts decoding at `start` seconds; frame times still count from the first frame of the file.
    #[allow(deprecated)]
    pub fn open_at(path: &Path, start: f64) -> Result<Self> {
        unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
            let track = asset.tracksWithMediaType(AVMediaTypeVideo.unwrap()).firstObject().context("no video track")?;
            let settings = pixel_buffer_attributes(None);
            let output = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&track, Some(&settings));
            output.setAlwaysCopiesSampleData(false);
            let reader = AVAssetReader::assetReaderWithAsset_error(&asset).map_err(|e| anyhow!("reader: {}", e.localizedDescription()))?;
            if !reader.canAddOutput(&output) {
                bail!("cannot read {}", path.display());
            }
            reader.addOutput(&output);
            let range = track.timeRange();
            let origin = range.start.seconds();
            if start > 0.0 {
                let from = CMTime::with_seconds(origin + start, 600);
                let rest = (range.duration.seconds() - start).max(0.0) + 1.0;
                reader.setTimeRange(CMTimeRange { start: from, duration: CMTime::with_seconds(rest, 600) });
            }
            if !reader.startReading() {
                bail!("start reading {}", path.display());
            }
            Ok(Self { reader, output, origin: Some(origin), cur: None, next: None, done: false })
        }
    }

    fn pull(&mut self) -> Option<(CFRetained<CVPixelBuffer>, f64)> {
        loop {
            let sample = unsafe { self.output.copyNextSampleBuffer() }?;
            let Some(image) = (unsafe { sample.image_buffer() }) else { continue };
            let pts = unsafe { sample.presentation_time_stamp().seconds() };
            let origin = *self.origin.get_or_insert(pts);
            return Some((image, pts - origin));
        }
    }

    /// Latest frame shown at or before `t`. Past the end it holds the last frame.
    pub fn at(&mut self, t: f64) -> Option<&CVPixelBuffer> {
        loop {
            if self.next.is_none() && !self.done {
                self.next = self.pull();
                self.done = self.next.is_none();
            }
            match &self.next {
                Some((_, pts)) if *pts <= t || self.cur.is_none() => self.cur = self.next.take(),
                _ => break,
            }
        }
        self.cur.as_ref().map(|c| &*c.0)
    }

    pub fn failed(&self) -> bool {
        unsafe { self.reader.status() == AVAssetReaderStatus::Failed }
    }
}
