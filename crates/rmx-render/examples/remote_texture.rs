//! Measure and check the remote desktop texture. Opens a window, so run it only as
//!
//! ```sh
//! scripts/headless.sh target/.../examples/remote_texture OUT.ppm [auto|plain|gles2]
//! ```
//!
//! It streams animated 3840x2160 updates and then small ones while timing the main thread, checks
//! with random overlapping rectangles that coalescing loses no pixel, and writes the window,
//! with a test card fitted in one pane and shown 1:1 in the other, to `OUT.ppm`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use glutin::context::{ContextApi, ContextAttributesBuilder, GlProfile, Version};
use glutin::display::GlDisplay;
use glutin::prelude::*;
use glutin::surface::SwapInterval;
use rmx_display::FrameUpdate;
use rmx_render::renderer::platform;
use rmx_render::{Frame, Renderer, RendererPreference, Rgb, ScaleMode, SizeInfo, UploadPath, gl};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

const REMOTE: [u32; 2] = [3840, 2160];
const WINDOW: [u32; 2] = [1600, 1000];
const SMALL: u32 = 64;
/// Pixel buffers the fake session keeps in circulation.
const POOL: usize = 4;
const FRAMES: usize = 150;
/// Frames left out of the numbers, for the driver's first-use costs.
const WARM_UP: usize = 20;
/// One frame at 60 Hz, which also keeps the GPU from being hogged.
const PACE: Duration = Duration::from_micros(16_667);

#[derive(Default)]
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Set on the fake session's thread, whose channels are not what is being measured.
    static UNCOUNTED: Cell<bool> = const { Cell::new(false) };
}

fn count_allocation() {
    if !UNCOUNTED.get() {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation();
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Auto,
    Plain,
    Gles2,
}

struct App {
    out: String,
    mode: Mode,
    result: Result<(), String>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.result = run(event_loop, &self.out, self.mode).map_err(|err| err.to_string());
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn main() -> Result<(), Box<dyn Error>> {
    // The script's private compositor: a safeguard against running this on the real desktop.
    if std::env::var("WAYLAND_DISPLAY").as_deref() != Ok("rmx-headless") {
        return Err("run this only through scripts/headless.sh".into());
    }

    let mut args = std::env::args().skip(1);
    let out = args.next().ok_or("usage: remote_texture OUT.ppm [auto|plain|gles2]")?;
    let mode = match args.next().as_deref() {
        None | Some("auto") => Mode::Auto,
        Some("plain") => Mode::Plain,
        Some("gles2") => Mode::Gles2,
        Some(other) => return Err(format!("unknown mode {other}").into()),
    };

    let mut app = App { out, mode, result: Ok(()) };
    EventLoop::new()?.run_app(&mut app)?;
    Ok(app.result?)
}

fn run(event_loop: &ActiveEventLoop, out: &str, mode: Mode) -> Result<(), Box<dyn Error>> {
    let attributes = Window::default_attributes()
        .with_title("remote_texture")
        .with_inner_size(PhysicalSize::new(WINDOW[0], WINDOW[1]));
    let window = event_loop.create_window(attributes)?;
    let raw_display = window.display_handle()?.as_raw();
    let raw_window = window.window_handle()?.as_raw();

    let display = platform::create_gl_display(raw_display, Some(raw_window), true)?;
    let config = platform::pick_gl_config(&display, Some(raw_window))?;
    let context = if mode == Mode::Gles2 {
        let attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(2, 0))))
            .with_profile(GlProfile::Core)
            .build(Some(raw_window));
        unsafe { display.create_context(&config, &attributes)? }
    } else {
        platform::create_gl_context(&display, &config, Some(raw_window), None)?
    };

    let size = window.inner_size();
    let surface = platform::create_gl_surface(&context, size, raw_window)?;
    let context = context.make_current(&surface)?;
    surface.set_swap_interval(&context, SwapInterval::DontWait)?;
    surface.resize(
        &context,
        NonZeroU32::new(size.width).unwrap(),
        NonZeroU32::new(size.height).unwrap(),
    );

    let preference = (mode == Mode::Gles2).then_some(RendererPreference::Gles2Pure);
    let mut renderer = Renderer::new(&context, preference)?;
    report_context();

    let path = if mode == Mode::Plain { UploadPath::Plain } else { UploadPath::Auto };
    let mut bench = Bench::new(&renderer, path, [size.width, size.height], mode)?;
    println!(
        "window {}x{}, texture {}x{}, {}",
        size.width,
        size.height,
        REMOTE[0],
        REMOTE[1],
        if bench.texture.is_persistent() { "persistent buffers" } else { "plain uploads" }
    );

    bench.scenario("full 3840x2160", &surface, &context, &mut renderer, Kind::Full);
    bench.scenario("small 64x64", &surface, &context, &mut renderer, Kind::Small);
    bench.check_lossless();
    bench.check_resize();
    bench.write_test_card(&surface, &context, &mut renderer, out)?;
    Ok(())
}

fn report_context() {
    let string = |name| unsafe {
        let text = gl::GetString(name);
        std::ffi::CStr::from_ptr(text.cast()).to_string_lossy().into_owned()
    };
    println!("GL {} on {}", string(gl::VERSION), string(gl::RENDERER));
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Full,
    Small,
}

/// A session decoding on its own thread, recycling the buffers it gets back.
struct Producer {
    frames: Receiver<FrameUpdate>,
    spares: SyncSender<Vec<u8>>,
    thread: thread::JoinHandle<()>,
}

impl Producer {
    fn start(kind: Kind) -> Self {
        let (frame_tx, frames) = sync_channel(2);
        let (spares, spare_rx) = sync_channel::<Vec<u8>>(8);
        let thread = thread::spawn(move || {
            UNCOUNTED.set(true);
            let mut phase = 0u32;
            let (width, height) = match kind {
                Kind::Full => (REMOTE[0], REMOTE[1]),
                Kind::Small => (SMALL, SMALL),
            };
            let len = (width * height * 4) as usize;
            let mut pool: Vec<_> = (0..POOL).map(|_| vec![0; len]).collect();
            let mut rows = Vec::new();
            loop {
                let (x, y) = match kind {
                    Kind::Full => (0, 0),
                    Kind::Small => {
                        ((phase * 37) % (REMOTE[0] - SMALL), (phase * 23) % (REMOTE[1] - SMALL))
                    },
                };
                // A few buffers circulate, all allocated up front: once out, wait for one back.
                let mut pixels: Vec<u8> = match pool.pop() {
                    Some(pixels) => pixels,
                    None => match spare_rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(pixels) => pixels,
                        Err(RecvTimeoutError::Timeout) => Vec::new(),
                        Err(RecvTimeoutError::Disconnected) => return,
                    },
                };
                pixels.resize(len, 0);
                animate(&mut pixels, width, phase, &mut rows);
                let update = FrameUpdate { x, y, width, height, pixels };
                if frame_tx.send(update).is_err() {
                    return;
                }
                phase = phase.wrapping_add(1);
            }
        });
        Self { frames, spares, thread }
    }
}

/// Fill BGRA rows with a sweeping gradient, each row a copy of one of 256 prepared ones.
fn animate(pixels: &mut [u8], width: u32, phase: u32, rows: &mut Vec<u8>) {
    let stride = width as usize * 4;
    if rows.len() != stride * 256 {
        rows.clear();
        for shade in 0..=255u8 {
            for column in 0..width {
                rows.extend_from_slice(&[shade, column as u8, 255 - shade, 0]);
            }
        }
    }
    for (index, row) in pixels.chunks_exact_mut(stride).enumerate() {
        let shade = (index + phase as usize * 7) & 0xff;
        row.copy_from_slice(&rows[shade * stride..(shade + 1) * stride]);
    }
}

struct Bench {
    texture: rmx_render::RemoteTexture,
    window: [u32; 2],
    swizzled: bool,
}

/// Microseconds, sorted.
fn summary(mut micros: Vec<f64>) -> String {
    if micros.is_empty() {
        return "no samples".into();
    }
    micros.sort_by(f64::total_cmp);
    let at = |percentile: f64| {
        let index = ((micros.len() - 1) as f64 * percentile).round() as usize;
        micros[index]
    };
    let mean = micros.iter().sum::<f64>() / micros.len() as f64;
    format!(
        "mean {:.0} p50 {:.0} p99 {:.0} max {:.0} us (n={})",
        mean,
        at(0.5),
        at(0.99),
        micros[micros.len() - 1],
        micros.len()
    )
}

impl Bench {
    fn new(
        renderer: &Renderer,
        path: UploadPath,
        window: [u32; 2],
        mode: Mode,
    ) -> Result<Self, Box<dyn Error>> {
        let texture = renderer.create_remote_texture(REMOTE[0], REMOTE[1], path, || {})?;
        Ok(Self { texture, window, swizzled: mode == Mode::Gles2 })
    }

    fn panes(&self) -> [(Frame, SizeInfo); 2] {
        let [window_width, window_height] = self.window.map(|side| side as f32);
        let half = window_width / 2.;
        let size = SizeInfo::new(half, window_height, 10., 20., 0., 0., false);
        [0., half].map(|x| (Frame::new(&size, [x, 0., half, window_height], window_height), size))
    }

    fn draw(&mut self, renderer: &mut Renderer) {
        let [(fit_frame, fit_size), (native_frame, native_size)] = self.panes();
        let bars = Rgb::new(32, 32, 48);
        renderer.clear_rect(&fit_frame, bars, 1.);
        renderer.clear_rect(&native_frame, Rgb::new(48, 32, 32), 1.);
        renderer.draw_remote(&mut self.texture, ScaleMode::Fit, &fit_frame, &fit_size);
        let scroll = ScaleMode::Native { scroll: [u32::MAX, u32::MAX] };
        renderer.draw_remote(&mut self.texture, scroll, &native_frame, &native_size);
    }

    fn scenario<C, S>(
        &mut self,
        name: &str,
        surface: &S,
        context: &C,
        renderer: &mut Renderer,
        kind: Kind,
    ) where
        S: GlSurfaceLike<C>,
    {
        let producer = Producer::start(kind);
        let mut upload_micros = Vec::with_capacity(FRAMES);
        let mut empty_micros = Vec::with_capacity(FRAMES);
        let mut draw_micros = Vec::with_capacity(FRAMES);
        let mut swap_upload_micros = Vec::with_capacity(FRAMES);
        let mut swap_idle_micros = Vec::with_capacity(FRAMES);
        let mut first_upload = None;
        let mut updates = 0u64;
        let mut allocations_before = 0;

        let started = Instant::now();
        for frame in 0..FRAMES {
            if frame == WARM_UP {
                allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
            }
            let begin = Instant::now();
            let mut submitted = 0;
            while let Ok(update) = producer.frames.try_recv() {
                self.texture.submit(update);
                submitted += 1;
            }
            let status = self.texture.upload();
            let upload_us = begin.elapsed().as_secs_f64() * 1e6;
            updates += submitted;

            while let Some(spare) = self.texture.take_spare() {
                if producer.spares.try_send(spare).is_err() {
                    break;
                }
            }

            let draw_begin = Instant::now();
            self.draw(renderer);
            renderer.flush();
            let draw_us = draw_begin.elapsed().as_secs_f64() * 1e6;

            let swap_begin = Instant::now();
            surface.swap(context);
            let swap_us = swap_begin.elapsed().as_secs_f64() * 1e6;

            if frame >= WARM_UP {
                if status.changed {
                    upload_micros.push(upload_us);
                } else {
                    empty_micros.push(upload_us);
                }
                draw_micros.push(draw_us);
                if status.changed {
                    swap_upload_micros.push(swap_us);
                } else {
                    swap_idle_micros.push(swap_us);
                }
            } else if status.changed && first_upload.is_none() {
                first_upload = Some(upload_us);
            }

            let spent = begin.elapsed();
            if let Some(rest) = PACE.checked_sub(spent) {
                thread::sleep(rest);
            }
        }
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations_before;
        let elapsed = started.elapsed().as_secs_f64();
        let uploaded = upload_micros.len();

        drop(producer.frames);
        drop(producer.spares);
        let _ = producer.thread.join();

        println!("[{name}] {updates} updates over {elapsed:.1}s, {uploaded} frames uploaded");
        println!("[{name}] first upload {:.0} us", first_upload.unwrap_or(f64::NAN));
        println!("[{name}] submit+upload {}", summary(upload_micros));
        println!("[{name}] idle frame upload {}", summary(empty_micros));
        println!("[{name}] draw+flush {}", summary(draw_micros));
        println!("[{name}] swap after an upload {}", summary(swap_upload_micros));
        println!("[{name}] swap after no upload {}", summary(swap_idle_micros));
        println!("[{name}] allocations in steady state: {allocations}");
    }

    /// Bury the texture in random overlapping updates, uploading rarely, and compare every pixel.
    fn check_lossless(&mut self) {
        let [width, height] = REMOTE;
        let stride = width as usize * 4;
        let mut model = vec![0u8; stride * height as usize];

        // A known starting point: what earlier scenarios left is not what this one checks.
        self.texture.submit(FrameUpdate { x: 0, y: 0, width, height, pixels: model.clone() });

        let mut seed = 0x51ed_270b_u32;
        let mut next = move |bound: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % bound
        };

        for index in 0..400u32 {
            // Most small, some reaching across the screen, as dirty rectangles tend to be.
            let span = if next(4) == 0 { 1400 } else { 200 };
            let (x, y) = (next(width - 1), next(height - 1));
            let rect_width = (next(span) + 1).min(width - x);
            let rect_height = (next(span) + 1).min(height - y);
            let pixel = [index as u8, (index >> 3) as u8, next(255) as u8, 0xff];

            let mut pixels = Vec::with_capacity((rect_width * rect_height * 4) as usize);
            for _ in 0..rect_width * rect_height {
                pixels.extend_from_slice(&pixel);
            }
            for row in y..y + rect_height {
                let start = row as usize * stride + x as usize * 4;
                model[start..start + rect_width as usize * 4]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .for_each(|out| *out = pixel);
            }
            self.texture.submit(FrameUpdate {
                x,
                y,
                width: rect_width,
                height: rect_height,
                pixels,
            });

            // A main thread that is too busy to take most frames.
            if next(8) == 0 {
                self.texture.upload();
            }
            if next(16) == 0 {
                thread::sleep(Duration::from_millis(1));
            }
        }

        self.settle();

        let read = self.read_texture(REMOTE);
        let mismatches = read
            .as_chunks::<4>()
            .0
            .iter()
            .zip(model.as_chunks::<4>().0)
            .filter(|&(read, &[b, g, r, a])| {
                *read != if self.swizzled { [b, g, r, a] } else { [r, g, b, a] }
            })
            .count();
        println!("[lossless] 400 overlapping updates, {mismatches} wrong pixels of 8294400");
        assert_eq!(mismatches, 0, "coalescing lost pixels");
    }

    /// Resize down and back: a worker retires, its buffers go, and the new size gets the pixels.
    fn check_resize(&mut self) {
        let [width, height] = [1280, 720];
        self.texture.resize(width, height).expect("resizing");
        let pixels = [40, 80, 120, 255].repeat((width * height) as usize);
        self.texture.submit(FrameUpdate { x: 0, y: 0, width, height, pixels });
        self.settle();

        let read = self.read_texture([width, height]);
        let want = if self.swizzled { [40, 80, 120, 255] } else { [120, 80, 40, 255] };
        let wrong = read.as_chunks::<4>().0.iter().filter(|read| **read != want).count();
        println!("[resize] 1280x720, {wrong} wrong pixels");
        assert_eq!(wrong, 0, "the resized texture has the wrong pixels");

        self.texture.resize(REMOTE[0], REMOTE[1]).expect("resizing back");
    }

    /// Upload until nothing is left.
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.texture.upload().pending {
            assert!(Instant::now() < deadline, "uploads never finished");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Every pixel of the texture, as GL_RGBA bytes, by attaching it to a framebuffer.
    fn read_texture(&self, [width, height]: [u32; 2]) -> Vec<u8> {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        unsafe {
            let mut framebuffer = 0;
            gl::GenFramebuffers(1, &mut framebuffer);
            gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                self.texture.gl_id(),
                0,
            );
            assert_eq!(gl::CheckFramebufferStatus(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
            gl::ReadPixels(
                0,
                0,
                width as i32,
                height as i32,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                pixels.as_mut_ptr().cast(),
            );
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::DeleteFramebuffers(1, &framebuffer);
        }
        pixels
    }

    fn write_test_card<C, S>(
        &mut self,
        surface: &S,
        context: &C,
        renderer: &mut Renderer,
        out: &str,
    ) -> Result<(), Box<dyn Error>>
    where
        S: GlSurfaceLike<C>,
    {
        let [width, height] = REMOTE;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&test_card(x, y));
            }
        }
        self.texture.submit(FrameUpdate { x: 0, y: 0, width, height, pixels });

        self.settle();
        self.draw(renderer);
        renderer.finish();

        let [window_width, window_height] = self.window;
        // RGBA, the one format GLES 2 is sure to read back.
        let stride = window_width as usize * 4;
        let mut rows = vec![0u8; stride * window_height as usize];
        unsafe {
            gl::ReadPixels(
                0,
                0,
                window_width as i32,
                window_height as i32,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                rows.as_mut_ptr().cast(),
            );
        }
        surface.swap(context);

        let mut file = File::create(out)?;
        write!(file, "P6\n{window_width} {window_height}\n255\n")?;
        // OpenGL rows run bottom to top.
        for row in rows.chunks_exact(stride).rev() {
            for pixel in row.as_chunks::<4>().0 {
                file.write_all(&pixel[..3])?;
            }
        }
        println!("wrote {out}");
        Ok(())
    }
}

/// Saturated quadrants told apart by colour, each with an F that would show any flip. The fourth
/// byte is zero on purpose: sessions leave it unset, and the picture has to be opaque anyway.
fn test_card(x: u32, y: u32) -> [u8; 4] {
    let (half_width, half_height) = (REMOTE[0] / 2, REMOTE[1] / 2);
    let (right, bottom) = (x >= half_width, y >= half_height);
    let (cell_x, cell_y) = (x % half_width, y % half_height);

    // An F of 8x10 blocks, in the middle of the quadrant.
    let block = 96;
    let (origin_x, origin_y) = (half_width / 2 - 2 * block, half_height / 2 - 5 * block);
    let in_f = cell_x >= origin_x && cell_y >= origin_y && {
        let (column, row) = ((cell_x - origin_x) / block, (cell_y - origin_y) / block);
        (row < 10 && column < 1) || (row < 2 && column < 5) || ((4..6).contains(&row) && column < 3)
    };

    let [r, g, b] = match (right, bottom) {
        _ if in_f => [255, 255, 255],
        (false, false) => [230, 20, 20],
        (true, false) => [20, 200, 20],
        (false, true) => [20, 40, 230],
        (true, true) => [230, 200, 20],
    };
    [b, g, r, 0]
}

/// Swapping, whatever the surface and context types are.
trait GlSurfaceLike<C> {
    fn swap(&self, context: &C);
}

impl GlSurfaceLike<glutin::context::PossiblyCurrentContext>
    for glutin::surface::Surface<glutin::surface::WindowSurface>
{
    fn swap(&self, context: &glutin::context::PossiblyCurrentContext) {
        self.swap_buffers(context).expect("swapping buffers");
    }
}
