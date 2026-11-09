//! Running egui only when something changed, and painting the result into a shared GL context.
//!
//! Once a frame, the app calls [`Chrome::update`], which is free when nothing changed, then draws
//! its panes, then calls [`Chrome::paint`], which replays the cached triangles. Terminal output
//! reaches neither egui nor the layout: only input, a running animation, or
//! [`Chrome::invalidate`] cause a pass.
//!
//! # GL state
//!
//! [`Chrome::paint`] is the only call that touches GL; `update` does not, so it may run with no
//! context current. `paint` draws into whatever framebuffer is bound on entry and returns with
//! these exactly as it found them:
//!
//! - current program, vertex array, `ARRAY_BUFFER`, `ELEMENT_ARRAY_BUFFER`
//! - `ACTIVE_TEXTURE`, and the `TEXTURE_2D` binding of unit 0
//! - `BLEND`, the four blend factors, the two blend equations
//! - `SCISSOR_TEST` and the scissor box, the viewport
//! - `CULL_FACE`, `DEPTH_TEST`, `STENCIL_TEST`, the color write mask
//! - `FRAMEBUFFER_SRGB` (desktop GL only)
//! - `UNPACK_ALIGNMENT`, `UNPACK_ROW_LENGTH`, `UNPACK_SKIP_ROWS`, `UNPACK_SKIP_PIXELS`, and the
//!   `PIXEL_UNPACK_BUFFER` binding (the last four not on GLES 2)
//!
//! While painting, `STENCIL_TEST` is off and, if textures are being uploaded, the unpack state
//! above is at its defaults. Left changed, on purpose: the contents of egui's own buffers and
//! textures. The depth and stencil write masks, `POLYGON_OFFSET_FILL`, `DITHER`, sampler objects
//! and the bound framebuffer are never touched. On a context with no vertex array objects (GLES 2
//! without `OES_vertex_array_object`), egui's vertex attributes 0 to 2 are left disabled.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{
    ClippedPrimitive, Context, CursorIcon, Event, PlatformOutput, Pos2, RawInput, Rect,
    TexturesDelta, Ui, ViewportId, pos2, vec2,
};
use egui_glow::glow::{self, HasContext};
use winit::event::{ElementState, WindowEvent};

use crate::fonts::{self, CjkFallback};
use crate::input;
use crate::theme::{self, ThemeMode};

/// How often to look for the CJK fallback while it is on its way.
const CJK_POLL: Duration = Duration::from_millis(50);
/// How long [`ChromeCore::prepare`] waits for the CJK fallback.
const CJK_PREPARE_WAIT: Duration = Duration::from_millis(60);
/// The texture size [`ChromeCore::prepare`] lays out for, before a GPU has been asked.
const PREPARE_TEXTURE_SIDE: usize = 16384;

/// A rectangle in physical pixels from the top-left of the framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// What egui asks of the platform.
#[derive(Clone, Debug, Default)]
pub struct ChromeOutput {
    pub cursor: CursorIcon,
    /// Text to put on the clipboard.
    pub copied: Option<String>,
    /// Where the IME's candidate box belongs, while a text field has focus, in points.
    pub ime_rect: Option<Rect>,
}

/// How long the last egui pass took.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    /// Layout: running the UI code.
    pub layout: Duration,
    /// Turning shapes into triangles.
    pub tessellate: Duration,
    /// Passes run since creation.
    pub passes: u64,
    /// Primitives and vertices in the cached frame.
    pub primitives: usize,
    pub vertices: usize,
}

/// Everything about a frame except GL: when to run egui, and the result of the last run.
pub struct ChromeCore {
    ctx: Context,
    start: Instant,
    raw: RawInput,
    dirty: bool,
    /// When egui next wants to run, if it does.
    deadline: Option<Instant>,
    size: [u32; 2],
    ppp: f32,
    primitives: Vec<ClippedPrimitive>,
    /// Texture work not yet carried out by a paint.
    textures: TexturesDelta,
    output: ChromeOutput,
    stats: FrameStats,
    /// Where panes are, in points: pointer motion there doesn't concern egui.
    pane_area: Option<Rect>,
    /// Parts of `pane_area` that belong to the interface after all, like pane footers.
    pane_holes: Vec<Rect>,
    /// Identifies the triangles of the cached frame, to tell whether a pass changed anything.
    frame_hash: u64,
    /// Whether the last pass produced triangles different from the ones before it.
    last_pass_changed: bool,
    /// The last pointer position passed to egui.
    pointer: Option<Pos2>,
    /// A floating surface is showing.
    float_open: bool,
    /// The pointer left the window with a button held, to be told once that is up.
    left_while_held: bool,
    mods: egui::Modifiers,
    /// Buttons whose press egui heard, so that it also hears the release.
    buttons_delivered: u8,
    /// Buttons whose press the interface kept from the app.
    buttons_consumed: u8,
    cjk: CjkFallback,
}

impl ChromeCore {
    /// A context with the fonts and `mode` applied.
    pub fn new(mode: ThemeMode, cjk: CjkFallback) -> Self {
        let ctx = Context::default();
        // Ctrl with + and - belong to the terminal, and egui's zoom would resize the shell.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        fonts::install(&ctx);
        theme::apply(&ctx, mode);
        Self {
            ctx,
            start: Instant::now(),
            raw: RawInput::default(),
            dirty: true,
            deadline: None,
            size: [0, 0],
            ppp: 1.,
            primitives: Vec::new(),
            textures: TexturesDelta::default(),
            output: ChromeOutput::default(),
            stats: FrameStats::default(),
            pane_area: None,
            pane_holes: Vec::new(),
            frame_hash: 0,
            last_pass_changed: false,
            pointer: None,
            float_open: false,
            left_while_held: false,
            mods: egui::Modifiers::NONE,
            buttons_delivered: 0,
            buttons_consumed: 0,
            cjk,
        }
    }

    /// The egui context, for widgets drawing outside the frame closure and for tests.
    pub fn ctx(&self) -> &Context {
        &self.ctx
    }

    /// Show `mode`.
    pub fn set_theme(&mut self, mode: ThemeMode) {
        theme::apply(&self.ctx, mode);
        self.dirty = true;
    }

    /// Say that something shown has changed, outside of input.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Tell the chrome where panes are, so pointer motion over them costs nothing.
    pub fn set_pane_area(&mut self, area: Option<Rect>) {
        self.pane_area = area;
    }

    /// Tell the chrome which parts of the pane area are its own, in points.
    pub fn set_pane_holes(&mut self, holes: impl IntoIterator<Item = Rect>) {
        self.pane_holes.clear();
        self.pane_holes.extend(holes);
    }

    /// Queue an egui event directly.
    pub fn push_event(&mut self, event: Event) {
        self.raw.events.push(event);
        self.dirty = true;
    }

    /// Queue pasted text.
    pub fn paste(&mut self, text: String) {
        self.push_event(Event::Paste(text));
    }

    /// Whether egui has something to do at `now`.
    pub fn needs_update(&self, now: Instant) -> bool {
        self.dirty || self.deadline.is_some_and(|deadline| deadline <= now)
    }

    /// When egui next needs to run, for the event loop's timer; `None` when idle.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.dirty { Some(self.start) } else { self.deadline }
    }

    /// Whether something floats over the panes, like a menu or a dialog.
    pub fn float_open(&self) -> bool {
        self.float_open
    }

    /// Whether the keyboard belongs to the interface just now: a field or something floating.
    pub fn has_keyboard(&self) -> bool {
        self.float_open || self.ctx.egui_wants_keyboard_input()
    }

    /// Whether the pointer is doing something egui must hear about.
    fn pointer_concerns_egui(&self, pos: Pos2) -> bool {
        let inside = |p: Pos2| {
            self.pane_area.is_some_and(|area| area.contains(p))
                && !self.pane_holes.iter().any(|hole| hole.contains(p))
        };
        self.float_open
            || self.ctx.egui_is_using_pointer()
            || !inside(pos)
            || !self.pointer.is_some_and(inside)
    }

    /// Take a window event. Returns true if the interface used it, and the app should not.
    pub fn window_event(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let pos = input::point(position.x, position.y, self.ppp);
                let concerns = self.pointer_concerns_egui(pos);
                if concerns {
                    self.pointer = Some(pos);
                    self.push_event(Event::PointerMoved(pos));
                }
                concerns
            },
            WindowEvent::CursorLeft { .. } => {
                // A held button keeps the pointer: dragging out of the window is still a drag.
                if self.buttons_delivered != 0 {
                    self.left_while_held = true;
                    return false;
                }
                self.pointer = None;
                self.push_event(Event::PointerGone);
                false
            },
            WindowEvent::MouseInput { state, button, .. } => {
                let (Some(button), Some(pos)) = (input::button(*button), self.pointer_position())
                else {
                    return false;
                };
                let bit = 1 << button as u8;
                let pressed = *state == ElementState::Pressed;
                let consumed = if pressed {
                    // A press over a pane only matters to take the focus out of a text field.
                    let over_pane = !self.pointer_concerns_egui(pos);
                    if over_pane && !self.ctx.egui_wants_keyboard_input() {
                        return false;
                    }
                    self.buttons_delivered |= bit;
                    if !over_pane {
                        self.buttons_consumed |= bit;
                    }
                    !over_pane
                } else {
                    // A release goes wherever its press went, wherever the pointer is by now.
                    if self.buttons_delivered & bit == 0 {
                        return false;
                    }
                    self.buttons_delivered &= !bit;
                    let consumed = self.buttons_consumed & bit != 0;
                    self.buttons_consumed &= !bit;
                    if self.buttons_delivered == 0 && std::mem::take(&mut self.left_while_held) {
                        let modifiers = self.mods;
                        self.push_event(Event::PointerButton { pos, button, pressed, modifiers });
                        self.pointer = None;
                        self.push_event(Event::PointerGone);
                        return consumed;
                    }
                    consumed
                };
                let modifiers = self.mods;
                self.push_event(Event::PointerButton { pos, button, pressed, modifiers });
                consumed
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(pos) = self.pointer_position() else { return false };
                if !self.pointer_concerns_egui(pos) {
                    return false;
                }
                self.push_event(input::wheel(*delta, self.ppp, self.mods));
                true
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                self.mods = input::modifiers(modifiers.state());
                // Heard at the next pass; a modifier alone doesn't need one.
                if let Some(Event::ModifiersChanged(last)) = self.raw.events.last_mut() {
                    *last = self.mods;
                } else {
                    self.raw.events.push(Event::ModifiersChanged(self.mods));
                }
                false
            },
            WindowEvent::KeyboardInput { event, .. } => {
                if !self.ctx.egui_wants_keyboard_input() && !self.float_open {
                    return false;
                }
                let before = self.raw.events.len();
                input::key_events(event, self.mods, &mut self.raw.events);
                self.dirty |= self.raw.events.len() != before;
                true
            },
            WindowEvent::Ime(ime) => {
                if !self.ctx.egui_wants_keyboard_input() {
                    return false;
                }
                if let Some(event) = input::ime_event(ime) {
                    self.push_event(event);
                }
                true
            },
            WindowEvent::Focused(focused) => {
                self.raw.focused = *focused;
                self.push_event(Event::WindowFocused(*focused));
                false
            },
            _ => false,
        }
    }

    fn pointer_position(&self) -> Option<Pos2> {
        self.pointer
    }

    /// Whether the pointer is on the interface, whose cursor then applies, and not on a pane.
    pub fn pointer_on_interface(&self) -> bool {
        self.pointer.is_some_and(|pos| self.pointer_concerns_egui(pos))
    }

    /// A float was just closed outside egui: the pointer and keyboard are free at once.
    pub fn float_closed(&mut self) {
        self.float_open = false;
        self.dirty = true;
    }

    /// Release all buttons, after handing a drag to the compositor.
    pub fn cancel_pointer(&mut self) {
        self.buttons_delivered = 0;
        self.buttons_consumed = 0;
        if let Some(pos) = self.pointer {
            let modifiers = self.mods;
            for button in [egui::PointerButton::Primary, egui::PointerButton::Secondary] {
                self.push_event(Event::PointerButton { pos, button, pressed: false, modifiers });
            }
        }
    }

    /// Run egui if it has something to do. Returns whether it ran; see
    /// [`last_pass_changed`](Self::last_pass_changed) for whether that changed the cached frame.
    pub fn update(
        &mut self,
        now: Instant,
        size: [u32; 2],
        ppp: f32,
        mut ui: impl FnMut(&mut Ui),
    ) -> bool {
        if self.cjk.poll(&self.ctx) {
            self.dirty = true;
        }
        if size != self.size || ppp != self.ppp {
            self.size = size;
            self.ppp = ppp;
            self.dirty = true;
        }
        if !self.needs_update(now) {
            return false;
        }
        self.dirty = false;
        self.deadline = None;

        let begin = Instant::now();
        self.raw.screen_rect = Some(Rect::from_min_size(
            pos2(0., 0.),
            vec2(size[0] as f32 / ppp, size[1] as f32 / ppp),
        ));
        self.raw.time = Some(now.saturating_duration_since(self.start).as_secs_f64());
        self.raw.viewports.entry(self.raw.viewport_id).or_default().native_pixels_per_point =
            Some(ppp);
        let raw = self.raw.take();
        let output = self.ctx.run_ui(raw, &mut ui);

        let layout_done = Instant::now();
        self.primitives = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        let hash = frame_hash(&self.primitives);
        self.last_pass_changed = hash != self.frame_hash || !output.textures_delta.is_empty();
        self.frame_hash = hash;
        self.stats.tessellate = layout_done.elapsed();
        self.stats.layout = layout_done - begin;
        self.stats.passes += 1;
        self.stats.primitives = self.primitives.len();
        self.stats.vertices = self
            .primitives
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.len(),
                egui::epaint::Primitive::Callback(_) => 0,
            })
            .sum();
        self.textures.append(output.textures_delta);
        self.take_platform_output(output.platform_output);

        // Anything above the root layer is a float: motion over panes concerns it after all. A
        // tooltip only says something, and leaves the keyboard where it was.
        self.float_open = self.ctx.memory(|m| {
            m.areas().visible_layer_ids().iter().any(|layer| {
                layer.order != egui::Order::Background && layer.order != egui::Order::Tooltip
            })
        });

        let delay = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map_or(Duration::MAX, |viewport| viewport.repaint_delay);
        self.deadline = now.checked_add(delay);
        if self.cjk.pending() {
            // Nothing else would wake an idle window when the font arrives.
            let check = now + CJK_POLL;
            self.deadline = Some(self.deadline.map_or(check, |deadline| deadline.min(check)));
        }
        true
    }

    /// Whether the last pass changed what is drawn: a pass often changes nothing, when the
    /// pointer moves over something that doesn't respond.
    pub fn last_pass_changed(&self) -> bool {
        self.last_pass_changed
    }

    fn take_platform_output(&mut self, platform: PlatformOutput) {
        self.output.cursor = platform.cursor_icon;
        self.output.ime_rect = platform.ime.map(|ime| ime.cursor_rect);
        for command in platform.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                self.output.copied = Some(text);
            }
        }
    }

    /// Run `ui` once at `size` and `ppp`, on a throwaway frame, so that the first real frame finds
    /// its fonts loaded and its glyphs rasterized. Takes tens of milliseconds, and needs no GL.
    pub fn prepare(&mut self, size: [u32; 2], ppp: f32, ui: impl FnMut(&mut Ui)) {
        // What the GPU reports isn't known here, and a different value means a new atlas: this
        // is what desktop GPUs report.
        self.raw.max_texture_side = Some(PREPARE_TEXTURE_SIDE);
        // A fallback that arrives after the first frame rebuilds the font atlas on the main thread.
        let waited = Instant::now();
        while self.cjk.pending() && waited.elapsed() < CJK_PREPARE_WAIT {
            std::thread::sleep(CJK_POLL / 10);
            self.cjk.poll(&self.ctx);
        }
        self.update(self.start, size, ppp, ui);
    }

    /// What egui last asked of the platform. `copied` is taken, once.
    pub fn take_output(&mut self) -> ChromeOutput {
        let copied = self.output.copied.take();
        ChromeOutput { copied, ..self.output.clone() }
    }

    /// Timings and counts of the last pass.
    pub fn stats(&self) -> FrameStats {
        self.stats
    }

    /// The cached frame.
    pub fn primitives(&self) -> &[ClippedPrimitive] {
        &self.primitives
    }
}

/// A cheap fingerprint of the triangles of a frame.
fn frame_hash(primitives: &[ClippedPrimitive]) -> u64 {
    fn mix(hash: u64, word: u32) -> u64 {
        (hash.rotate_left(5) ^ u64::from(word)).wrapping_mul(0x517c_c1b7_2722_0a95)
    }
    let mut hash = 0u64;
    for primitive in primitives {
        let clip = primitive.clip_rect;
        for edge in [clip.min.x, clip.min.y, clip.max.x, clip.max.y] {
            hash = mix(hash, edge.to_bits());
        }
        let egui::epaint::Primitive::Mesh(mesh) = &primitive.primitive else { continue };
        hash = mix(hash, mesh.indices.len() as u32);
        for vertex in &mesh.vertices {
            hash = mix(hash, vertex.pos.x.to_bits());
            hash = mix(hash, vertex.pos.y.to_bits());
            hash = mix(hash, vertex.uv.x.to_bits());
            hash = mix(hash, vertex.color.to_array().iter().fold(0, |a, &b| a << 8 | u32::from(b)));
        }
    }
    hash
}

impl Drop for ChromeCore {
    fn drop(&mut self) {
        // egui panics on dropping texture work that was never done; at exit none needs doing.
        self.textures.clear();
    }
}

/// [`ChromeCore`] and the painter that replays its frames.
pub struct Chrome {
    core: ChromeCore,
    gl: Arc<glow::Context>,
    painter: egui_glow::Painter,
    caps: GlCaps,
    /// Each primitive's own clip, while `damage` has narrowed the live ones.
    clips: Vec<Rect>,
}

impl std::ops::Deref for Chrome {
    type Target = ChromeCore;

    fn deref(&self) -> &ChromeCore {
        &self.core
    }
}

impl std::ops::DerefMut for Chrome {
    fn deref_mut(&mut self) -> &mut ChromeCore {
        &mut self.core
    }
}

impl Chrome {
    /// Build on `gl`, which must be current. Locates the CJK fallback in the background.
    pub fn new(gl: Arc<glow::Context>, mode: ThemeMode) -> Result<Self, String> {
        Self::with_core(ChromeCore::new(mode, CjkFallback::locate()), gl)
    }

    /// Build on the context of `loader`, which must be current, around `core`, which may have
    /// been made and prepared on another thread: only this call touches GL.
    pub fn from_loader(
        core: ChromeCore,
        loader: impl FnMut(&std::ffi::CStr) -> *const std::ffi::c_void,
    ) -> Result<Self, String> {
        // SAFETY: the context is current, as the caller promises, and `loader` resolves its
        // functions.
        let gl = unsafe { glow::Context::from_loader_function_cstr(loader) };
        Self::with_core(core, Arc::new(gl))
    }

    fn with_core(mut core: ChromeCore, gl: Arc<glow::Context>) -> Result<Self, String> {
        let painter = egui_glow::Painter::new(gl.clone(), "", None, false)
            .map_err(|error| error.to_string())?;
        let caps = GlCaps::probe(&gl);
        // No atlas gets anywhere near this, and larger values would mean rebuilding what
        // `prepare` built.
        core.raw.max_texture_side = Some(painter.max_texture_side().min(PREPARE_TEXTURE_SIDE));
        Ok(Self { core, gl, painter, caps, clips: Vec::new() })
    }

    /// Replay the cached frame into the bound framebuffer, on top of what is there, applying any
    /// texture changes from the last update. GL state comes back as it was; see
    /// [`self`](crate::chrome).
    ///
    /// `damage` limits drawing to that rectangle, for partial redraws; the app's scissor is
    /// ignored otherwise, as every mesh sets its own.
    pub fn paint(&mut self, damage: Option<PixelRect>) {
        let [width, height] = self.core.size;
        if width == 0 || height == 0 {
            return;
        }
        let gl = &*self.gl;
        let saved = GlState::capture(gl, self.caps);
        let uploading = !self.core.textures.set.is_empty();
        saved.isolate(gl, self.caps, uploading);

        let mut textures = std::mem::take(&mut self.core.textures);
        for (id, deltas) in &textures.set {
            for delta in deltas {
                self.painter.set_texture(*id, delta);
            }
        }

        let ppp = self.core.ppp;
        let primitives = &mut self.core.primitives;
        // Room for every clip is made on a full frame, so that a partial one never allocates.
        if damage.is_none() {
            self.clips.reserve(primitives.len().saturating_sub(self.clips.len()));
        }
        if let Some(damage) = damage {
            let clip = Rect::from_min_size(
                pos2(damage.x as f32 / ppp, damage.y as f32 / ppp),
                vec2(damage.width as f32 / ppp, damage.height as f32 / ppp),
            );
            self.clips.clear();
            self.clips.extend(primitives.iter().map(|p| p.clip_rect));
            for primitive in primitives.iter_mut() {
                primitive.clip_rect = primitive.clip_rect.intersect(clip);
            }
        }
        if damage.is_some() {
            // Empty clips would still upload their vertices, the bulk of a replay's cost.
            for run in primitives.split(|primitive| !primitive.clip_rect.is_positive()) {
                self.painter.paint_primitives([width, height], ppp, run);
            }
        } else {
            self.painter.paint_primitives([width, height], ppp, primitives);
        }
        if damage.is_some() {
            for (primitive, clip) in primitives.iter_mut().zip(&self.clips) {
                primitive.clip_rect = *clip;
            }
        }

        for id in &textures.free {
            self.painter.free_texture(*id);
        }
        textures.clear();
        saved.restore(gl, self.caps);
    }

    /// Release the GL objects; `gl` must be current.
    pub fn destroy(&mut self) {
        self.painter.destroy();
    }

    /// The GL context.
    pub fn gl(&self) -> &Arc<glow::Context> {
        &self.gl
    }
}

#[derive(Clone, Copy)]
struct GlCaps {
    /// Desktop GL, where `FRAMEBUFFER_SRGB` exists.
    desktop: bool,
    vao: bool,
    /// GLES 3 or desktop: the pixel-unpack state beyond the alignment exists.
    unpack: bool,
}

impl GlCaps {
    fn probe(gl: &glow::Context) -> Self {
        let version = gl.version();
        let modern = version.major >= 3;
        let vao =
            modern || gl.supported_extensions().iter().any(|e| e.contains("vertex_array_object"));
        Self { desktop: !version.is_embedded, vao, unpack: modern }
    }
}

#[derive(Default)]
struct GlState {
    program: i32,
    vao: i32,
    array_buffer: i32,
    element_buffer: i32,
    active_texture: i32,
    texture0: i32,
    blend: bool,
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    blend_eq_rgb: i32,
    blend_eq_alpha: i32,
    scissor_test: bool,
    scissor: [i32; 4],
    viewport: [i32; 4],
    cull: bool,
    depth: bool,
    stencil: bool,
    color_mask: [bool; 4],
    srgb: bool,
    unpack_alignment: i32,
    unpack_row_length: i32,
    unpack_skip_rows: i32,
    unpack_skip_pixels: i32,
    unpack_buffer: i32,
}

fn nz(id: i32) -> Option<std::num::NonZeroU32> {
    std::num::NonZeroU32::new(id as u32)
}

impl GlState {
    fn capture(gl: &glow::Context, caps: GlCaps) -> Self {
        // SAFETY: only queries, of enums valid for the capabilities probed from this context.
        unsafe {
            let get = |name: u32| gl.get_parameter_i32(name);
            let on = |name: u32| gl.is_enabled(name);
            let active_texture = get(glow::ACTIVE_TEXTURE);
            gl.active_texture(glow::TEXTURE0);
            let texture0 = get(glow::TEXTURE_BINDING_2D);

            let mut scissor = [0; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor);
            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let mut mask = [0; 4];
            gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut mask);

            let mut state = Self {
                program: get(glow::CURRENT_PROGRAM),
                array_buffer: get(glow::ARRAY_BUFFER_BINDING),
                element_buffer: get(glow::ELEMENT_ARRAY_BUFFER_BINDING),
                active_texture,
                texture0,
                blend: on(glow::BLEND),
                blend_src_rgb: get(glow::BLEND_SRC_RGB),
                blend_dst_rgb: get(glow::BLEND_DST_RGB),
                blend_src_alpha: get(glow::BLEND_SRC_ALPHA),
                blend_dst_alpha: get(glow::BLEND_DST_ALPHA),
                blend_eq_rgb: get(glow::BLEND_EQUATION_RGB),
                blend_eq_alpha: get(glow::BLEND_EQUATION_ALPHA),
                scissor_test: on(glow::SCISSOR_TEST),
                scissor,
                viewport,
                cull: on(glow::CULL_FACE),
                depth: on(glow::DEPTH_TEST),
                stencil: on(glow::STENCIL_TEST),
                color_mask: mask.map(|c| c != 0),
                unpack_alignment: get(glow::UNPACK_ALIGNMENT),
                ..Self::default()
            };
            if caps.vao {
                state.vao = get(glow::VERTEX_ARRAY_BINDING);
            }
            if caps.desktop {
                state.srgb = on(glow::FRAMEBUFFER_SRGB);
            }
            if caps.unpack {
                state.unpack_row_length = get(glow::UNPACK_ROW_LENGTH);
                state.unpack_skip_rows = get(glow::UNPACK_SKIP_ROWS);
                state.unpack_skip_pixels = get(glow::UNPACK_SKIP_PIXELS);
                state.unpack_buffer = get(glow::PIXEL_UNPACK_BUFFER_BINDING);
            }
            state
        }
    }

    /// Clear what egui's painter assumes to be at its defaults.
    fn isolate(&self, gl: &glow::Context, caps: GlCaps, uploading: bool) {
        // SAFETY: plain state changes with valid enums.
        unsafe {
            if self.stencil {
                gl.disable(glow::STENCIL_TEST);
            }
            if uploading && caps.unpack {
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
                gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
                gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            }
        }
    }

    fn restore(&self, gl: &glow::Context, caps: GlCaps) {
        // SAFETY: restoring names and values that this context itself returned in `capture`.
        unsafe {
            let set = |name: u32, enabled: bool| {
                if enabled { gl.enable(name) } else { gl.disable(name) }
            };
            gl.use_program(nz(self.program).map(glow::NativeProgram));
            if caps.vao {
                gl.bind_vertex_array(nz(self.vao).map(glow::NativeVertexArray));
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, nz(self.array_buffer).map(glow::NativeBuffer));
            // Part of the vertex array's own state, so only after it.
            gl.bind_buffer(
                glow::ELEMENT_ARRAY_BUFFER,
                nz(self.element_buffer).map(glow::NativeBuffer),
            );

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, nz(self.texture0).map(glow::NativeTexture));
            gl.active_texture(self.active_texture as u32);

            set(glow::BLEND, self.blend);
            gl.blend_func_separate(
                self.blend_src_rgb as u32,
                self.blend_dst_rgb as u32,
                self.blend_src_alpha as u32,
                self.blend_dst_alpha as u32,
            );
            gl.blend_equation_separate(self.blend_eq_rgb as u32, self.blend_eq_alpha as u32);

            set(glow::SCISSOR_TEST, self.scissor_test);
            gl.scissor(self.scissor[0], self.scissor[1], self.scissor[2], self.scissor[3]);
            gl.viewport(self.viewport[0], self.viewport[1], self.viewport[2], self.viewport[3]);
            set(glow::CULL_FACE, self.cull);
            set(glow::DEPTH_TEST, self.depth);
            set(glow::STENCIL_TEST, self.stencil);
            let [r, g, b, a] = self.color_mask;
            gl.color_mask(r, g, b, a);
            if caps.desktop {
                set(glow::FRAMEBUFFER_SRGB, self.srgb);
            }
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, self.unpack_alignment);
            if caps.unpack {
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, self.unpack_row_length);
                gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, self.unpack_skip_rows);
                gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, self.unpack_skip_pixels);
                gl.bind_buffer(
                    glow::PIXEL_UNPACK_BUFFER,
                    nz(self.unpack_buffer).map(glow::NativeBuffer),
                );
            }
        }
    }
}
