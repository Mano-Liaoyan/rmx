//! The window, its GL context, and drawing panes into it.

use std::cell::Cell;
use std::error::Error;
use std::num::NonZeroU32;
use std::thread::{self, JoinHandle};

use alacritty_terminal::index::{Column, Point};
use crossfont::{Metrics, Rasterize, Rasterizer};
use glutin::config::GetGlConfig;
use glutin::context::PossiblyCurrentContext;
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin::surface::{Rect as DamageRect, Surface, SwapInterval, WindowSurface};
use rmx_layout::Rect;
use rmx_layout::region::{DamageHistory, Region};
use rmx_render::font::Font;
use rmx_render::probe::{self, Phase};
use rmx_render::renderer::platform;
use rmx_render::renderer::rects::{RenderLines, RenderRect};
use rmx_render::{
    Frame, GlyphCache, RasterizedGlyphs, RemoteLayout, RemoteTexture, Renderer, Rgb, ScaleMode,
    SizeInfo, UploadPath,
};
use rmx_terminal::{IntoRects, Terminal, Theme};
use rmx_ui::{Chrome, PixelRect, ThemeMode};
use unicode_width::UnicodeWidthChar;
use winit::dpi::PhysicalSize;
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use crate::mark;
use crate::pane::Pane;
use crate::shell::Shell;

/// Padding around a terminal, in logical pixels.
const PADDING: f32 = 8.;
/// Thickness of beam and underline cursors, as a fraction of the cell's width.
const CURSOR_THICKNESS: f32 = 0.15;

/// The gutter behind the panes: the shell's own background, as behind the interface's panels.
fn gutter(mode: ThemeMode) -> Rgb {
    let color = mode.palette().background;
    Rgb::new(color.r(), color.g(), color.b())
}

/// The terminal colors of a theme.
pub fn terminal_theme(mode: ThemeMode) -> Theme {
    match mode {
        ThemeMode::Dark => Theme::dark(),
        ThemeMode::Light => Theme::light(),
    }
}

/// A pane to be drawn. Holds no borrows, so that the list of them can be kept between frames.
pub struct PaneDraw {
    /// Which terminal shows in it, as an index of the panes given to [`Gfx::draw`]. None draws
    /// it blank, while its terminal is starting.
    pub terminal: Option<usize>,
    /// Which remote desktop shows in it, likewise.
    pub display: Option<usize>,
    /// Where it is drawn this frame, which may differ from its terminal's size as it moves.
    pub rect: Rect,
    /// Where it sits at rest, whose size a remote desktop is laid out for.
    pub rest: Rect,
    /// Whether it shows the input method's unfinished text.
    pub focused: bool,
    /// Whether its content changed since it was last drawn.
    pub dirty: bool,
    /// Set by drawing when its terminal could not be read, which leaves it to be tried again.
    pub stale: bool,
}

/// Text being composed in the input method, shown at the cursor of the focused terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    /// Where its cursor is, in bytes, if it shows one.
    pub cursor: Option<usize>,
}

impl Preedit {
    /// The characters to show in `columns` columns, the last ones when there are too many; the
    /// columns they take; and those before the cursor.
    fn fit(&self, columns: usize) -> (&str, usize, usize) {
        let width = |c: char| c.width().unwrap_or(0);
        let total: usize = self.text.chars().map(width).sum();

        let mut shown = self.text.as_str();
        let mut taken = total;
        while taken > columns {
            let Some(first) = shown.chars().next() else { break };
            taken -= width(first);
            shown = &shown[first.len_utf8()..];
        }

        let at = self.cursor.map_or(self.text.len(), |at| at.min(self.text.len()));
        let cut = self.text.len() - shown.len();
        let before: usize = self.text[cut..at.max(cut)].chars().map(width).sum();
        (shown, taken, before)
    }
}

/// Time spent in each phase of a frame, in microseconds.
#[cfg(feature = "bench")]
pub type Phases = [u32; 3];

/// What a frame drew, to check that damage tracking works.
#[cfg(feature = "bench")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    /// Whether only part of the window was redrawn.
    pub partial: bool,
    /// Panes on screen.
    pub shown: u32,
    /// Panes redrawn.
    pub drawn: u32,
    /// Panes that could not be read, and were drawn as they last were.
    pub stale: u32,
    /// Age of the back buffer: 0 when its content is unknown.
    pub age: u32,
}

/// A frame to write to a file.
#[cfg(feature = "bench")]
pub struct Shot {
    pub path: String,
    /// Force a full redraw. Without it the file shows what partial redraws left in the buffer.
    pub full: bool,
}

thread_local! {
    /// The [`Gfx::key`] of the window whose context is current on the main thread.
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

/// Declared so that GL objects drop before the context, and the context before the window.
pub struct Gfx {
    /// Says which window's context is current, without asking the driver.
    key: u64,
    renderer: Renderer,
    glyph_cache: GlyphCache,
    metrics: Metrics,
    cell: (f32, f32),
    theme: Theme,
    gutter: Rgb,
    history: DamageHistory,
    /// When frames reach the screen. Before the window, whose surface it watches.
    #[cfg(feature = "bench")]
    pub presentation: Option<crate::bench::present::Presentation>,
    /// Draw next to nothing, to see what the compositor does with that.
    #[cfg(feature = "bench")]
    pub control: Option<crate::bench::Control>,
    #[cfg(feature = "bench")]
    control_lit: bool,
    // Kept between frames so that drawing one does not allocate.
    lines: RenderLines,
    rects: Vec<RenderRect>,
    damage: Vec<DamageRect>,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    pub window: Window,
    #[cfg(feature = "bench")]
    pub phases: Phases,
    #[cfg(feature = "bench")]
    pub stats: FrameStats,
    /// Allocations `pre_present_notify` made in this frame.
    #[cfg(feature = "bench")]
    pub winit_allocations: u64,
    /// The next frame is to be written out, for testing without a desktop.
    #[cfg(feature = "bench")]
    pub shot: Option<Shot>,
}

impl Drop for Gfx {
    fn drop(&mut self) {
        if CURRENT.get() == self.key {
            CURRENT.set(0);
        }
    }
}

/// The whole pixels covering `rect`.
fn pixel_rect(rect: Rect) -> PixelRect {
    let (x, y) = (rect.x.floor() as i32, rect.y.floor() as i32);
    PixelRect {
        x,
        y,
        width: rect.right().ceil() as i32 - x,
        height: rect.bottom().ceil() as i32 - y,
    }
}

/// The font at the size it has on a screen of `scale`.
fn scaled_font(scale: f64) -> Font {
    let font = Font::default();
    let size = font.size().scale(scale as f32);
    font.with_size(size)
}

/// Fonts found and their common glyphs rasterized, which takes neither GL nor the window.
struct Glyphs {
    cache: GlyphCache,
    common: RasterizedGlyphs,
    scale: f64,
}

fn find_fonts(scale: f64) -> Result<Glyphs, String> {
    let rasterizer = Rasterizer::new().map_err(|err| err.to_string())?;
    let mut cache =
        GlyphCache::new(rasterizer, &scaled_font(scale)).map_err(|err| err.to_string())?;
    let common = cache.rasterize_common();
    mark("fonts");
    Ok(Glyphs { cache, common, scale })
}

/// Fonts being found on a thread of their own, alongside setting up GL.
#[cfg(not(windows))]
struct FontLoad(JoinHandle<Result<Glyphs, String>>);

#[cfg(not(windows))]
impl FontLoad {
    fn start(scale: f64) -> Self {
        Self(thread::spawn(move || find_fonts(scale)))
    }

    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    fn wait(self) -> Result<Glyphs, String> {
        self.0.join().map_err(|_| String::from("the thread finding fonts panicked"))?
    }
}

/// DirectWrite's fonts stay on the thread that found them, so on Windows that is the thread that
/// asks for them, when it does.
#[cfg(windows)]
struct FontLoad(f64);

#[cfg(windows)]
impl FontLoad {
    fn start(scale: f64) -> Self {
        Self(scale)
    }

    fn is_finished(&self) -> bool {
        true
    }

    fn wait(self) -> Result<Glyphs, String> {
        find_fonts(self.0)
    }
}

/// Fonts being found, and the interface's being laid out, on threads while the window and GL are
/// set up.
pub struct Early {
    fonts: FontLoad,
    chrome: JoinHandle<(rmx_ui::ChromeCore, Shell)>,
}

impl Early {
    /// `size` is the window's expected size, in logical pixels.
    pub fn start(scale: f64, size: (f64, f64), mode: ThemeMode) -> Self {
        let chrome = thread::spawn(move || {
            let cjk = rmx_ui::fonts::CjkFallback::locate();
            let mut core = rmx_ui::ChromeCore::new(mode, cjk);
            let mut shell = Shell::default();
            let size = [(size.0 * scale) as u32, (size.1 * scale) as u32];
            core.prepare(size, scale as f32, |ui| shell.rehearse(ui));
            mark("chrome");
            (core, shell)
        });
        Self { fonts: FontLoad::start(scale), chrome }
    }

    /// Whether making the window can begin without waiting.
    pub fn finished(&self) -> bool {
        self.fonts.is_finished() && self.chrome.is_finished()
    }
}

/// Load the GPU driver on a thread, while the compositor is being waited for.
pub fn preload_driver() {
    #[cfg(not(any(target_os = "macos", windows)))]
    thread::spawn(|| {
        platform::preload_driver();
        mark("driver_loaded");
    });
}

impl Gfx {
    pub fn new(
        window: Window,
        early: Early,
        mode: ThemeMode,
        key: u64,
        share: Option<&Gfx>,
    ) -> Result<(Self, Chrome, Shell), Box<dyn Error>> {
        let Early { fonts, chrome } = early;
        let raw_display = window.display_handle()?.as_raw();
        let raw_window = window.window_handle()?.as_raw();

        // Later windows join the first one's context group, so that what a pane has on the GPU
        // goes with it when it changes windows.
        let context = match share {
            Some(share) => platform::create_gl_context(
                &share.context.display(),
                &share.context.config(),
                Some(raw_window),
                Some(&share.context),
            )?,
            None => {
                let display = platform::create_gl_display(raw_display, Some(raw_window), true)?;
                mark("egl_display");
                let config = platform::pick_gl_config(&display, Some(raw_window))?;
                platform::create_gl_context(&display, &config, Some(raw_window), None)?
            },
        };
        mark("egl_context");

        let size = window.inner_size();
        let surface = platform::create_gl_surface(&context, size, raw_window)?;
        let context = context.make_current(&surface)?;
        CURRENT.set(key);
        mark("egl_current");

        // Frames are paced by `Pacer`; a blocking swap would only add latency.
        surface.set_swap_interval(&context, SwapInterval::DontWait)?;

        platform::touch_back_buffer(&context);
        let mut renderer = Renderer::new(&context, None)?;
        mark("shaders");

        let Glyphs { cache: mut glyph_cache, common, scale } = fonts.wait()?;
        renderer.with_loader(|mut api| glyph_cache.load_rasterized(common, &mut api));
        // The font was picked before the window knew its screen, usually rightly.
        if scale != window.scale_factor() {
            glyph_cache.update_font_size(&scaled_font(window.scale_factor()))?;
            renderer.with_loader(|mut api| glyph_cache.reset_glyph_cache(&mut api));
        }
        mark("atlas");

        let metrics = glyph_cache.font_metrics();
        let cell = (
            metrics.average_advance.floor().max(1.) as f32,
            metrics.line_height.floor().max(1.) as f32,
        );

        let gfx = Self {
            key,
            renderer,
            glyph_cache,
            metrics,
            cell,
            theme: terminal_theme(mode),
            gutter: gutter(mode),
            history: DamageHistory::default(),
            #[cfg(feature = "bench")]
            presentation: (key == 1)
                .then(|| crate::bench::present::Presentation::attach(&window))
                .flatten(),
            #[cfg(feature = "bench")]
            control: crate::bench::control(),
            #[cfg(feature = "bench")]
            control_lit: false,
            lines: RenderLines::new(),
            rects: Vec::new(),
            damage: Vec::new(),
            surface,
            context,
            window,
            #[cfg(feature = "bench")]
            phases: [0; 3],
            #[cfg(feature = "bench")]
            stats: FrameStats::default(),
            #[cfg(feature = "bench")]
            winit_allocations: 0,
            #[cfg(feature = "bench")]
            shot: None,
        };

        let (core, shell) =
            chrome.join().map_err(|_| "the thread preparing the interface panicked")?;
        let chrome = gfx.new_chrome(core)?;
        mark("chrome_painter");
        Ok((gfx, chrome, shell))
    }

    /// Make GL calls go to this window, as they must before any are made outside a frame.
    pub fn make_current(&self) {
        if CURRENT.get() != self.key {
            match self.context.make_current(&self.surface) {
                Ok(()) => CURRENT.set(self.key),
                Err(err) => log::error!("Could not switch to a window's GL context: {err}"),
            }
        }
    }

    /// Finish what starting up left for after the first frame.
    pub fn finish_startup(&mut self) {
        let (width, height) = self.window_size();
        let window = SizeInfo::new(width, height, self.cell.0, self.cell.1, 0., 0., false);
        self.renderer.set_frame(None);
        self.renderer.resize(&window);
        self.renderer.finish_shaders(&window, &self.metrics, &mut self.glyph_cache);
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Draw terminals and the gutter in `mode` from the next frame on.
    pub fn set_theme(&mut self, mode: ThemeMode) {
        self.theme = terminal_theme(mode);
        self.gutter = gutter(mode);
    }

    pub fn scale(&self) -> f32 {
        self.window.scale_factor() as f32
    }

    /// Build the interface's painter. Only ever called on the main thread, as all EGL calls are.
    pub fn new_chrome(&self, core: rmx_ui::ChromeCore) -> Result<Chrome, String> {
        Chrome::from_loader(core, |name| self.context.display().get_proc_address(name))
    }

    /// Where the terminal of `rect` is drawing the cell at `(line, column)`, in window pixels.
    pub fn cell_rect(&self, rect: Rect, line: usize, column: usize) -> (i32, i32, u32, u32) {
        let (cell_w, cell_h) = self.cell;
        let padding = PADDING * self.scale();
        let x = rect.x + padding + column as f32 * cell_w;
        let y = rect.y + padding + line as f32 * cell_h;
        (x as i32, y as i32, cell_w as u32, cell_h as u32)
    }

    /// Window size in pixels, never empty.
    pub fn window_size(&self) -> (f32, f32) {
        let PhysicalSize { width, height } = self.window.inner_size();
        (width.max(1) as f32, height.max(1) as f32)
    }

    /// The pixels of a cell.
    pub fn cell(&self) -> (f32, f32) {
        self.cell
    }

    /// The terminal size filling `rect`.
    pub fn size_for(&self, rect: Rect) -> SizeInfo {
        let padding = PADDING * self.scale();
        SizeInfo::new(rect.w, rect.h, self.cell.0, self.cell.1, padding, padding, false)
    }

    /// Make the GL surface fit the window.
    pub fn resize(&mut self) {
        let PhysicalSize { width, height } = self.window.inner_size();
        if let (Some(width), Some(height)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            self.surface.resize(&self.context, width, height);
        }
        self.history.reset();
    }

    /// Draw `panes` and present them. Unless `full`, only what is dirty is redrawn.
    ///
    /// Returns false, having presented nothing, when there was nothing to redraw.
    pub fn draw(
        &mut self,
        panes: &mut [PaneDraw],
        contents: &mut [Pane],
        preedit: Option<&Preedit>,
        full: bool,
        chrome: &mut Chrome,
    ) -> bool {
        #[cfg(feature = "bench")]
        let start = std::time::Instant::now();
        #[cfg(feature = "bench")]
        {
            self.phases = [0; 3];
        }

        // A frame that writes itself out may need every pixel.
        #[cfg(feature = "bench")]
        let full = full || self.shot.as_ref().is_some_and(|shot| shot.full);

        let mut changed = Region::default();
        if !full {
            panes.iter().filter(|pane| pane.dirty).for_each(|pane| changed.push(pane.rect));
            // Presenting an empty damage list would damage the whole window.
            if changed.is_empty() {
                return false;
            }
        }
        #[cfg(feature = "bench")]
        if let Some(presentation) = &mut self.presentation {
            presentation.begin_frame();
        }
        let age = {
            let _probe = probe::enter(Phase::BufferAge);
            self.surface.buffer_age() as usize
        };
        let needed = self.history.advance((!full).then_some(changed), age);

        let (width, height) = self.window_size();
        let window = SizeInfo::new(width, height, self.cell.0, self.cell.1, 0., 0., false);

        if needed.is_none() {
            let _probe = probe::enter(Phase::Clear);
            self.renderer.set_frame(None);
            self.renderer.resize(&window);
            self.renderer.clear(self.gutter, 1.);
        }

        let visible = |pane: &PaneDraw| needed.is_none_or(|needed| needed.intersects(&pane.rect));

        rmx_render::renderer::remote::reap_closed();
        self.glyph_cache.begin_frame();

        let (mut drawn, mut stale) = (0u32, 0u32);
        for pane in panes.iter_mut() {
            if !visible(pane) {
                continue;
            }
            drawn += 1;
            if let Some(index) = pane.display {
                let _probe = probe::enter(Phase::Display);
                self.draw_display(pane, &mut contents[index], height);
                continue;
            }
            let Some(index) = pane.terminal else {
                self.draw_blank(pane, height);
                continue;
            };

            let terminal: &Terminal =
                contents[index].content.terminal().expect("only panes with a terminal are listed");
            if pane.dirty {
                #[cfg(feature = "bench")]
                let start = std::time::Instant::now();
                let probe = probe::enter(Phase::Snapshot);

                pane.stale = !terminal.refresh();
                #[allow(
                    clippy::drop_non_drop,
                    reason = "the probe only has a destructor when measuring"
                )]
                drop(probe);
                stale += u32::from(pane.stale);

                #[cfg(feature = "bench")]
                {
                    self.phases[0] += start.elapsed().as_micros() as u32;
                }
            }
            let preedit = preedit.filter(|_| pane.focused);
            self.draw_pane(pane, terminal, preedit, height);
        }

        #[cfg(feature = "bench")]
        {
            self.stats = FrameStats {
                partial: needed.is_some(),
                shown: panes.len() as u32,
                drawn,
                stale,
                age: age as u32,
            };
        }
        #[cfg(not(feature = "bench"))]
        let _ = (drawn, stale);

        self.renderer.set_frame(None);
        self.renderer.resize(&window);

        // The chrome goes on top of what was drawn and nowhere else: over what is left of an
        // older frame it would blend its translucent parts a second time.
        let probe = probe::enter(Phase::ChromePaint);
        match needed {
            None => chrome.paint(None),
            Some(_) => {
                panes.iter().filter(|pane| visible(pane)).for_each(|pane| {
                    chrome.paint(Some(pixel_rect(pane.rect)));
                });
            },
        }
        #[allow(clippy::drop_non_drop, reason = "the probe only has a destructor when measuring")]
        drop(probe);
        // Everything else is on its way to the GPU already. The interface goes too, so that
        // swapping has less left to wait for.
        self.renderer.flush();

        #[cfg(feature = "bench")]
        let drawn = std::time::Instant::now();
        #[cfg(feature = "bench")]
        if let Some(shot) = self.shot.take() {
            self.write_frame(&shot.path);
        }

        #[cfg(feature = "bench")]
        let allocations_before = crate::bench::allocations();
        {
            let _probe = probe::enter(Phase::PrePresent);
            self.window.pre_present_notify();
        }
        #[cfg(feature = "bench")]
        if let Some(presentation) = &mut self.presentation {
            presentation.before_swap();
        }
        #[cfg(feature = "bench")]
        {
            self.winit_allocations = crate::bench::allocations() - allocations_before;
        }
        // The compositor only has to know what differs from the last frame it was given, however
        // much of this older buffer had to be brought up to date.
        {
            let _probe = probe::enter(Phase::Swap);
            self.present((needed.is_some() && !full).then_some(changed), height);
        }
        #[cfg(feature = "bench")]
        if let Some(presentation) = &mut self.presentation {
            presentation.after_swap();
        }

        #[cfg(feature = "bench")]
        {
            let total = (drawn - start).as_micros() as u32;
            self.phases[1] = total.saturating_sub(self.phases[0]);
            self.phases[2] = drawn.elapsed().as_micros() as u32;
        }
        true
    }

    /// The whole of a control frame: clear the window, change a little of it, and present.
    #[cfg(feature = "bench")]
    pub fn draw_control(&mut self, control: crate::bench::Control) {
        use rmx_render::gl;

        const SQUARE: (i32, i32, i32, i32) = (8, 8, 16, 16);
        if let Some(presentation) = &mut self.presentation {
            presentation.begin_frame();
        }
        self.phases = [0; 3];
        self.stats = FrameStats { partial: true, shown: 0, drawn: 0, stale: 0, age: 0 };
        let (width, height) = self.window_size();
        let window = SizeInfo::new(width, height, self.cell.0, self.cell.1, 0., 0., false);
        self.renderer.set_frame(None);
        self.renderer.resize(&window);
        self.renderer.clear(self.gutter, 1.);
        self.control_lit = !self.control_lit;
        let level = if self.control_lit { 1. } else { 0.25 };
        unsafe {
            gl::Enable(gl::SCISSOR_TEST);
            gl::Scissor(SQUARE.0, SQUARE.1, SQUARE.2, SQUARE.3);
            gl::ClearColor(level, level, level, 1.);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            gl::Disable(gl::SCISSOR_TEST);
        }

        let allocations_before = crate::bench::allocations();
        self.window.pre_present_notify();
        if let Some(presentation) = &mut self.presentation {
            presentation.before_swap();
        }
        self.winit_allocations = crate::bench::allocations() - allocations_before;
        let result = match control {
            crate::bench::Control::FullDamage => self.surface.swap_buffers(&self.context),
            crate::bench::Control::TinyDamage => {
                self.damage.clear();
                self.damage.push(DamageRect::new(SQUARE.0, SQUARE.1, SQUARE.2, SQUARE.3));
                self.swap_with_damage()
            },
        };
        if let Err(err) = result {
            log::error!("Could not present: {err}");
        }
        if let Some(presentation) = &mut self.presentation {
            presentation.after_swap();
        }
    }

    /// A texture for a remote desktop of `width` by `height`.
    pub fn create_remote_texture(
        &self,
        width: u32,
        height: u32,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<RemoteTexture, rmx_render::renderer::Error> {
        #[cfg(feature = "bench")]
        crate::bench::present::note("texture", || format!("remote {width}x{height}"));
        self.renderer.create_remote_texture(width, height, UploadPath::Auto, notify)
    }

    /// Start the GPU on what has been asked of it so far.
    pub fn flush(&self) {
        self.renderer.flush();
    }

    /// The size a remote desktop in a pane at `rest` is laid out for, with no padding.
    fn remote_size(&self, rest: Rect) -> SizeInfo {
        SizeInfo::new(rest.w, rest.h, self.cell.0, self.cell.1, 0., 0., false)
    }

    /// Where a desktop of `remote` pixels lands in `rect`, for a pane at `rest`.
    pub fn remote_layout(
        &self,
        mode: ScaleMode,
        remote: [u32; 2],
        rect: Rect,
        rest: Rect,
    ) -> RemoteLayout {
        let (_, height) = self.window_size();
        let size = self.remote_size(rest);
        let frame = Frame::new(&size, [rect.x, rect.y, rect.w, rect.h], height);
        RemoteLayout::new(mode, remote, &frame, &size)
    }

    /// Draw the texture of a remote desktop, or black bars while it has none.
    fn draw_display(&mut self, pane: &PaneDraw, content: &mut Pane, window_height: f32) {
        let Some(display) = content.content.display_mut() else {
            self.draw_blank(pane, window_height);
            return;
        };
        let size = self.remote_size(pane.rest);
        let rect = pane.rect;
        let frame = Frame::new(&size, [rect.x, rect.y, rect.w, rect.h], window_height);
        self.renderer.set_frame(Some(frame));
        self.renderer.resize(&size);
        self.renderer.clear_rect(&frame, Rgb::new(0, 0, 0), 1.);
        let mode = display.mode();
        if let Some(texture) = &mut display.texture {
            self.renderer.draw_remote(texture, mode, &frame, &size);
        }
        self.renderer.flush();
    }

    fn draw_blank(&mut self, pane: &PaneDraw, window_height: f32) {
        let size = self.size_for(pane.rect);
        let rect = pane.rect;
        let frame = Frame::new(&size, [rect.x, rect.y, rect.w, rect.h], window_height);
        self.renderer.clear_rect(&frame, self.theme.background(), 1.);
    }

    fn draw_pane(
        &mut self,
        pane: &mut PaneDraw,
        terminal: &Terminal,
        preedit: Option<&Preedit>,
        window_height: f32,
    ) {
        // The size the snapshot was laid out for, stretched over the pane: while the terminal is
        // being resized that is not the size it is about to have.
        let snapshot = terminal.last_snapshot();
        let (size, cursor) = (snapshot.size, snapshot.cursor);

        let rect = pane.rect;
        let frame = Frame::new(&size, [rect.x, rect.y, rect.w, rect.h], window_height);
        self.renderer.set_frame(Some(frame));
        self.renderer.resize(&size);
        {
            let _probe = probe::enter(Phase::Clear);
            self.renderer.clear_rect(&frame, self.theme.background(), 1.);
        }

        self.lines.clear();
        let lines = &mut self.lines;
        let cells = snapshot.cells.iter().inspect(|cell| lines.update(cell));
        {
            let _probe = probe::enter(Phase::Cells);
            self.renderer.draw_cells(&size, &mut self.glyph_cache, cells);
        }
        // Glyphs left blank for now mean another frame, with the same snapshot, to fill them in.
        pane.stale |= self.glyph_cache.take_deferred();

        self.rects.clear();
        self.lines.push_rects(&mut self.rects, &self.metrics, &size);
        match preedit {
            Some(preedit) => self.draw_preedit(pane, &size, cursor.point(), preedit),
            None => self.rects.extend(cursor.rects(&size, CURSOR_THICKNESS)),
        }
        {
            let _probe = probe::enter(Phase::Rects);
            self.renderer.draw_rects(&size, &self.metrics, &self.rects);
        }
        let _probe = probe::enter(Phase::Flush);
        // Swapping waits for the GPU to get through the frame. It may as well start now.
        self.renderer.flush();
    }

    /// Draw the unfinished text of the input method over the terminal at its cursor, underlined
    /// and with a bar where the input method's cursor is, in the place of the terminal's.
    fn draw_preedit(
        &mut self,
        pane: &mut PaneDraw,
        size: &SizeInfo,
        cursor: Point<usize>,
        preedit: &Preedit,
    ) {
        let columns = size.columns_count();
        let (text, width, before) = preedit.fit(columns);
        let start = cursor.column.0.min(columns.saturating_sub(width));

        let (fg, bg) = (self.theme.foreground(), self.theme.background());
        let at = Point::new(cursor.line, Column(start));
        // A wide character is followed by a cell the renderer skips.
        let chars = text.chars().flat_map(|c| {
            let spacer = (c.width() == Some(2)).then_some(' ');
            std::iter::once(c).chain(spacer)
        });
        self.renderer.draw_string(at, fg, bg, chars, size, &mut self.glyph_cache);
        pane.stale |= self.glyph_cache.take_deferred();

        let (cell_w, cell_h) = (size.cell_width(), size.cell_height());
        let left = size.padding_x() + start as f32 * cell_w;
        let top = size.padding_y() + cursor.line as f32 * cell_h;
        let thickness = (cell_h * CURSOR_THICKNESS / 2.).round().max(1.);
        self.rects.extend([
            RenderRect::new(
                left,
                top + cell_h - thickness,
                width as f32 * cell_w,
                thickness,
                fg,
                1.,
            ),
            RenderRect::new(left + before as f32 * cell_w, top, thickness, cell_h, fg, 1.),
        ]);
    }

    /// Show the back buffer, in which only `changed` differs from the last frame, if known.
    fn present(&mut self, changed: Option<Region>, window_height: f32) {
        let result = match changed {
            Some(changed) => {
                self.damage.clear();
                self.damage.extend(changed.rects().iter().map(|rect| {
                    // Whole pixels covering the rect, counted from the bottom as EGL does.
                    let (left, right) = (rect.x.floor(), rect.right().ceil());
                    let (bottom, top) =
                        ((window_height - rect.bottom()).floor(), (window_height - rect.y).ceil());
                    DamageRect::new(
                        left as i32,
                        bottom as i32,
                        (right - left) as i32,
                        (top - bottom) as i32,
                    )
                }));
                self.swap_with_damage()
            },
            None => self.surface.swap_buffers(&self.context),
        };
        if let Err(err) = result {
            log::error!("Could not present: {err}");
        }
    }

    /// Swap, telling the compositor what changed where the platform has a way to. Elsewhere the
    /// compositor just takes the whole buffer, which is up to date either way.
    fn swap_with_damage(&self) -> glutin::error::Result<()> {
        #[cfg(not(target_os = "macos"))]
        if let (Surface::Egl(surface), PossiblyCurrentContext::Egl(context)) =
            (&self.surface, &self.context)
        {
            return surface.swap_buffers_with_damage(context, &self.damage);
        }
        self.surface.swap_buffers(&self.context)
    }

    /// The name of the GPU, or software rasterizer, drawing this.
    #[cfg(feature = "bench")]
    pub fn renderer_name() -> String {
        use rmx_render::gl;

        let name = unsafe { gl::GetString(gl::RENDERER) };
        if name.is_null() {
            return String::new();
        }
        unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_string_lossy().into_owned()
    }

    /// Write what was just drawn to `path`, as a PPM image.
    #[cfg(feature = "bench")]
    fn write_frame(&self, path: &str) {
        use rmx_render::gl;

        let (width, height) = self.window_size();
        let (width, height) = (width as usize, height as usize);
        let mut pixels = vec![0u8; width * height * 3];
        unsafe {
            gl::PixelStorei(gl::PACK_ALIGNMENT, 1);
            gl::ReadPixels(
                0,
                0,
                width as i32,
                height as i32,
                gl::RGB,
                gl::UNSIGNED_BYTE,
                pixels.as_mut_ptr().cast(),
            );
        }

        let mut image = format!("P6\n{width} {height}\n255\n").into_bytes();
        for row in pixels.chunks_exact(width * 3).rev() {
            image.extend_from_slice(row);
        }
        if let Err(err) = std::fs::write(path, image) {
            eprintln!("rmx: could not write {path}: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preedit(text: &str, cursor: Option<usize>) -> Preedit {
        Preedit { text: text.into(), cursor }
    }

    #[test]
    fn preedit_fits_counting_wide_characters() {
        assert_eq!(preedit("ni", None).fit(10), ("ni", 2, 2));
        assert_eq!(preedit("你好", Some(3)).fit(10), ("你好", 4, 2));
    }

    #[test]
    fn preedit_too_long_for_the_line_keeps_its_end() {
        assert_eq!(preedit("abcd", None).fit(2), ("cd", 2, 2));
        assert_eq!(preedit("你好", None).fit(3), ("好", 2, 2));
        // A cursor before what is shown sits at its start.
        assert_eq!(preedit("abcd", Some(0)).fit(2).2, 0);
    }
}
