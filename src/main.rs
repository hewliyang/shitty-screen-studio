mod app;
mod camera;
mod capture;
mod compositor;
mod demo;
mod edit;
mod editor;
mod export;
mod gpu;
mod tray;
mod motion;
mod player;
mod project;
mod style;
mod theme;
mod video;

use gpui::{
    App, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, actions,
    point, prelude::*, px, size,
};
use std::path::PathBuf;

actions!(shitty_screen_studio, [Quit]);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--render") {
        if let Err(e) = cli::render(&args[2..]) {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--sources") {
        match capture::list_sources() {
            Ok(s) => {
                for d in s.displays {
                    println!("display {} {}x{}{}", d.id, d.width, d.height, if d.main { " main" } else { "" });
                }
                for w in s.windows {
                    println!("window {} {}x{} {} — {}", w.id, w.width, w.height, w.app, w.title);
                }
            }
            Err(e) => eprintln!("{e:#}"),
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--record") {
        let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5.0);
        if let Err(e) = cli::record(secs, &args[3..]) {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    let open = args.get(1).map(PathBuf::from);

    gpui_platform::application().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus([Menu::new("Shitty Screen Studio").items([MenuItem::action("Quit", Quit)])]);

        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let main_window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Shitty Screen Studio".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(18.), px(19.))),
                }),
                window_background: gpui::WindowBackgroundAppearance::Blurred,
                window_min_size: Some(size(px(960.), px(640.))),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| app::Studio::new(open, window, cx)),
        )
        .expect("open main window");
        #[cfg(feature = "snapshot")]
        snapshot::schedule(main_window.into(), cx);
        #[cfg(not(feature = "snapshot"))]
        let _ = main_window;
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(feature = "snapshot")]
mod snapshot {
    use gpui::{AnyWindowHandle, App};
    use std::time::Duration;

    /// `SSS_SNAPSHOT=out.png` writes the main window to a PNG after a short delay, then quits.
    pub fn schedule(window: AnyWindowHandle, cx: &mut App) {
        let Ok(path) = std::env::var("SSS_SNAPSHOT") else { return };
        let delay: u64 = std::env::var("SSS_SNAPSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(4000);
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(delay)).await;
            let _ = cx.update(|cx| {
                let _ = window.update(cx, |_, window, _| {
                    match window.render_to_image() {
                        Ok(img) => {
                            let _ = img.save(&path);
                        }
                        Err(e) => eprintln!("snapshot failed: {e:#}"),
                    }
                });
                cx.quit();
            });
        })
        .detach();
    }
}

mod cli {
    use crate::compositor::Params;
    use crate::gpu::Gpu;
    use crate::export::{ExportSettings, export};
    use crate::motion::Motion;
    use crate::project::Project;
    use crate::video::{FrameReader, fit_even};
    use anyhow::{Context as _, Result};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    /// `--record <secs> [--mic] [--system-audio] [--camera] [--window <id>] [--area <display>,<x>,<y>,<w>,<h>]`
    pub fn record(secs: f64, flags: &[String]) -> Result<()> {
        use crate::capture::{RecState, RecordOptions, RecordingHandle, Source};
        let mut options = RecordOptions::default();
        let mut it = flags.iter();
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--mic" => options.mic = true,
                "--system-audio" => options.system_audio = true,
                "--camera" => options.camera = true,
                "--window" => options.source = Source::Window(it.next().context("window id")?.parse()?),
                "--area" => {
                    let v: Vec<f64> = it.next().context("area")?.split(',').map(|s| s.parse()).collect::<Result<_, _>>()?;
                    anyhow::ensure!(v.len() == 5, "area is display,x,y,w,h");
                    options.source = Source::Area { display: v[0] as u32, x: v[1], y: v[2], w: v[3], h: v[4] };
                }
                other => anyhow::bail!("unknown flag {other}"),
            }
        }
        let handle = RecordingHandle::start(crate::project::new_recording_dir()?, options);
        let mut stop_at = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            match handle.state() {
                RecState::Recording { since } if stop_at.is_none() => {
                    stop_at = Some(since + std::time::Duration::from_secs_f64(secs));
                }
                RecState::Done(dir) => {
                    println!("{}", dir.display());
                    return Ok(());
                }
                RecState::Failed(e) => anyhow::bail!(e),
                _ => {}
            }
            if stop_at.is_some_and(|t| std::time::Instant::now() >= t) {
                handle.stop();
            }
        }
    }

    /// `--render <dir> <t> <out.png>` renders one frame, `--render <dir> export <out.mp4>` exports.
    pub fn render(args: &[String]) -> Result<()> {
        let dir = Path::new(args.first().context("missing dir")?);
        let what = args.get(1).context("missing time")?;
        let out = Path::new(args.get(2).context("missing output")?);
        let project = if dir.join("recording.json").exists() {
            Project::load(dir)?
        } else {
            crate::demo::create(dir)?
        };
        let rec = &project.rec;
        let style = rec.style.unwrap_or_default();
        let zooms = rec.zooms.clone().unwrap_or_else(|| rec.auto_zooms(style.zoom));
        if what == "export" {
            let started = std::time::Instant::now();
            export(
                &project,
                rec,
                &style,
                &zooms,
                &rec.timeline(),
                out,
                &{
                    let (width, height) = crate::style::canvas_size(style.aspect, rec.width, rec.height, 1080);
                    ExportSettings { width, height, fps: 60 }
                },
                &AtomicU32::new(0),
                &AtomicBool::new(false),
            )?;
            println!("exported in {:.1}s", started.elapsed().as_secs_f64());
            return Ok(());
        }
        if what == "motion" {
            let motion = Motion::build(&rec.cursor, &zooms, rec.duration, &style);
            let mut t = 0.0;
            while t <= rec.duration {
                let f = motion.at(t);
                println!("{t:.2} scale {:.3} focus {:.3},{:.3} cursor {:.3},{:.3}", f.camera.scale, f.camera.fx, f.camera.fy, f.cursor.0, f.cursor.1);
                t += 0.1;
            }
            return Ok(());
        }
        if what == "bench" {
            let motion = Motion::build(&rec.cursor, &zooms, rec.duration, &style);
            let clicks = rec.clicks();
            let params = Params { style: &style, motion: &motion, clicks: &clicks, src_w: rec.width, src_h: rec.height, points_width: rec.points_width };
            let mut composer = Gpu::new()?;
            let mut frames = crate::gpu::Frames::open(&project.video())?;
            let (pw, ph) = crate::player::preview_size(&style, rec);
            let (mut n, mut decode, mut compose) = (0, 0.0, 0.0);
            loop {
                let t = n as f64 / crate::player::PREVIEW_FPS as f64;
                if t > rec.duration { break; }
                let a = std::time::Instant::now();
                let Some(frame) = frames.at(t) else { break };
                let b = std::time::Instant::now();
                composer.present(frame, None, t, &params, true, pw, ph)?;
                decode += (b - a).as_secs_f64();
                compose += b.elapsed().as_secs_f64();
                n += 1;
            }
            println!("{n} frames: decode {:.1}ms/f, compose {:.1}ms/f", decode * 1000.0 / n as f64, compose * 1000.0 / n as f64);
            return Ok(());
        }
        let t: f64 = what.parse()?;
        let motion = Motion::build(&rec.cursor, &zooms, rec.duration, &style);
        let clicks = rec.clicks();
        let (sw, sh) = fit_even(rec.width, rec.height, 3840);
        let mut reader = FrameReader::open(&project.video(), t, 60, sw, sh)?;
        let mut buf = Vec::new();
        reader.read_into(&mut buf)?;

        let params = Params {
            style: &style,
            motion: &motion,
            clicks: &clicks,
            src_w: rec.width,
            src_h: rec.height,
            points_width: rec.points_width,
        };
        let started = std::time::Instant::now();
        let cam = match (project.camera(), rec.camera) {
            (Some(path), Some(track)) => {
                let mut r = FrameReader::open(&path, t + track.lead, 60, 1280, 720)?;
                let mut b = Vec::new();
                r.read_into(&mut b)?.then_some(b)
            }
            _ => None,
        };
        let mut bgra = Vec::new();
        let (ow, oh) = crate::style::canvas_size(style.aspect, rec.width, rec.height, 1080);
        Gpu::new()?.render_bgra((&buf, sw, sh), cam.as_deref().map(|b| (b, 1280, 720)), t, &params, ow, oh, &mut bgra)?;
        println!("composed in {:.1}ms", started.elapsed().as_secs_f64() * 1000.0);
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        image::save_buffer(out, &bgra, ow, oh, image::ExtendedColorType::Rgba8)?;
        Ok(())
    }
}
