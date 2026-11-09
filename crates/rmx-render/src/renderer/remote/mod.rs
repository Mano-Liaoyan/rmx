//! Remote desktop panes: a texture fed with `FrameUpdate`s and drawn in a [`Frame`].
//!
//! On a context with `GL_ARB_buffer_storage`, updates go to a worker thread that writes them to
//! persistently mapped pixel buffers, and the main thread only issues `glTexSubImage2D` from
//! them. Elsewhere, down to GLES 2, the main thread uploads the pixels itself. Only this module
//! makes GL calls, all from the thread owning the context.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::CStr;
use std::sync::Arc;
use std::{mem, ptr};

use log::{info, warn};
use rmx_display::FrameUpdate;

use crate::SizeInfo;
use crate::gl;
use crate::gl::types::*;
use crate::renderer::shader::{ShaderProgram, ShaderVersion};
use crate::renderer::{Error, Frame, GlExtensions, Renderer};

mod dirty;
mod layout;
mod stream;

pub use dirty::{DirtyRegion, MAX_RECTS, Rect};
pub use layout::{Filter, RemoteLayout, RemotePointer, ScaleMode};

use stream::{BYTES_PER_PIXEL, MAX_SPARES, Mapped, Notify, SLOTS, Stream};

static VERTEX_SHADER: &str = include_str!("../../../res/remote.v.glsl");
static FRAGMENT_SHADER: &str = include_str!("../../../res/remote.f.glsl");

/// How a texture receives its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadPath {
    /// Persistent buffers wherever the context has them, plain uploads elsewhere.
    Auto,
    /// Plain uploads even if the context could do better.
    Plain,
}

/// What [`RemoteTexture::upload`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadStatus {
    /// The texture changed, so the pane needs drawing.
    pub changed: bool,
    /// More is coming: call again next frame, even if nothing else asks for one.
    pub pending: bool,
}

/// The program drawing remote textures.
#[derive(Debug)]
pub(super) struct RemoteProgram {
    program: ShaderProgram,
    vao: GLuint,
    vbo: GLuint,
    source: GLint,
    tap: GLint,
    swap_red_blue: GLint,
}

impl RemoteProgram {
    pub(super) fn new(version: ShaderVersion) -> Result<Self, Error> {
        let program = ShaderProgram::new(version, None, VERTEX_SHADER, FRAGMENT_SHADER)?;
        let uniform = |name: &'static CStr| program.get_uniform_location(name);
        let source = uniform(c"source")?;
        let tap = uniform(c"tap")?;
        let swap_red_blue = uniform(c"swapRedBlue")?;
        let texture = uniform(c"tex")?;

        const CORNERS: [[f32; 2]; 4] = [[-1., -1.], [1., -1.], [-1., 1.], [1., 1.]];
        let (mut vao, mut vbo) = (0, 0);
        unsafe {
            gl::UseProgram(program.id());
            gl::Uniform1i(texture, 0);
            gl::UseProgram(0);

            let position = gl::GetAttribLocation(program.id(), c"aPos".as_ptr()) as GLuint;

            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                mem::size_of_val(&CORNERS) as isize,
                CORNERS.as_ptr().cast(),
                gl::STATIC_DRAW,
            );
            gl::VertexAttribPointer(position, 2, gl::FLOAT, gl::FALSE, 0, ptr::null());
            gl::EnableVertexAttribArray(position);
            gl::BindVertexArray(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
        }

        Ok(Self { program, vao, vbo, source, tap, swap_red_blue })
    }

    /// Draw `texture` as `layout` says. The caller keeps the scissor and restores the viewport.
    fn draw(&self, texture: &RemoteTexture, layout: &RemoteLayout) {
        let [width, height] = texture.size.map(|side| side.max(1) as f32);
        let [x, y, shown_width, shown_height] = layout.src.map(|value| value as f32);
        unsafe {
            gl::Viewport(layout.dst[0], layout.dst[1], layout.dst[2], layout.dst[3]);
            // Desktop pixels replace what is under them; the shader's alpha is always 1.
            gl::Disable(gl::BLEND);
            gl::UseProgram(self.program.id());
            gl::Uniform4f(
                self.source,
                x / width,
                y / height,
                (x + shown_width) / width,
                (y + shown_height) / height,
            );
            gl::Uniform2f(self.tap, layout.tap[0] / width, layout.tap[1] / height);
            gl::Uniform1f(self.swap_red_blue, if texture.swizzle { 1. } else { 0. });
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, texture.id);
            gl::BindVertexArray(self.vao);
            gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);
            gl::BindVertexArray(0);
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::UseProgram(0);
            gl::Enable(gl::BLEND);
        }
    }

    /// Have the driver finish preparing the program, which it only does when first drawing.
    pub(super) fn warm_up(&self) {
        unsafe {
            gl::Disable(gl::BLEND);
            gl::UseProgram(self.program.id());
            gl::Viewport(0, 0, 0, 0);
            gl::BindVertexArray(self.vao);
            gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
            gl::Enable(gl::BLEND);
        }
    }
}

impl Drop for RemoteProgram {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteBuffers(1, &self.vbo);
        }
    }
}

/// The picture of a remote desktop on the GPU.
///
/// Create, use and drop it on the thread owning the GL context, like the rest of the renderer.
pub struct RemoteTexture {
    id: GLuint,
    size: [u32; 2],
    /// GL's filter currently set on the texture.
    filter: Filter,
    /// Whether the texture holds defined pixels yet.
    drawable: bool,
    transport: Transport,
    /// Workers of earlier sizes, waited for without blocking until they are gone.
    retired: Vec<Persistent>,
    /// Pixels are uploaded as RGBA and swapped when drawn, as on GLES 2.
    swizzle: bool,
    can_persist: bool,
    notify: Notify,
}

impl std::fmt::Debug for RemoteTexture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTexture")
            .field("id", &self.id)
            .field("size", &self.size)
            .field("persistent", &self.is_persistent())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum Transport {
    Persistent(Persistent),
    Plain(Plain),
}

/// Staging buffers and the worker filling them.
#[derive(Debug)]
struct Persistent {
    stream: Stream,
    buffers: [GLuint; SLOTS],
    /// Passed by the GPU once it has read the buffer of the same index.
    fences: [GLsync; SLOTS],
}

impl Persistent {
    fn new(width: u32, height: u32, notify: Notify) -> Option<Self> {
        let len = width as isize * height as isize * BYTES_PER_PIXEL as isize;
        let storage = gl::MAP_WRITE_BIT
            | gl::MAP_PERSISTENT_BIT
            | gl::MAP_COHERENT_BIT
            | gl::CLIENT_STORAGE_BIT;
        let access = gl::MAP_WRITE_BIT | gl::MAP_PERSISTENT_BIT | gl::MAP_COHERENT_BIT;

        let mut buffers = [0; SLOTS];
        let mut mapped = [Mapped(ptr::null_mut()); SLOTS];
        unsafe {
            gl::GenBuffers(SLOTS as GLsizei, buffers.as_mut_ptr());
            for (buffer, mapped) in buffers.iter().zip(&mut mapped) {
                gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, *buffer);
                gl::BufferStorage(gl::PIXEL_UNPACK_BUFFER, len, ptr::null(), storage);
                *mapped =
                    Mapped(gl::MapBufferRange(gl::PIXEL_UNPACK_BUFFER, 0, len, access).cast());
            }
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, 0);

            if mapped.iter().any(|mapped| mapped.0.is_null()) {
                gl::DeleteBuffers(SLOTS as GLsizei, buffers.as_ptr());
                warn!("Could not map staging buffers for a remote desktop");
                return None;
            }
        }

        let stream = Stream::spawn(width, height, mapped, notify);
        Some(Self { stream, buffers, fences: [ptr::null(); SLOTS] })
    }

    /// Hand back the buffers the GPU has finished reading.
    fn poll_fences(&mut self) {
        for (slot, fence) in self.fences.iter_mut().enumerate() {
            if fence.is_null() {
                continue;
            }
            // The flush bit gets the fence to the GPU should no frame have done so since.
            let status = unsafe { gl::ClientWaitSync(*fence, gl::SYNC_FLUSH_COMMANDS_BIT, 0) };
            if status == gl::ALREADY_SIGNALED || status == gl::CONDITION_SATISFIED {
                unsafe { gl::DeleteSync(*fence) };
                *fence = ptr::null();
                self.stream.release(slot);
            }
        }
    }

    /// Upload the newest finished buffer, returning whether there was one.
    fn upload(&mut self, texture: GLuint, width: u32) -> bool {
        self.poll_fences();
        let Some((slot, region)) = self.stream.take_ready() else { return false };

        let stride = width as usize * BYTES_PER_PIXEL;
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, self.buffers[slot]);
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, width as GLint);
            for rect in region.rects() {
                let offset = rect.y as usize * stride + rect.x as usize * BYTES_PER_PIXEL;
                gl::TexSubImage2D(
                    gl::TEXTURE_2D,
                    0,
                    rect.x as GLint,
                    rect.y as GLint,
                    rect.width as GLsizei,
                    rect.height as GLsizei,
                    gl::BGRA,
                    gl::UNSIGNED_BYTE,
                    offset as *const _,
                );
            }
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
            // Left bound, it would turn every later texture upload into a read from this buffer.
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, 0);
            gl::BindTexture(gl::TEXTURE_2D, 0);
            self.fences[slot] = gl::FenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0);
        }
        true
    }

    /// Forget the GL objects. The worker must be gone first, as deleting unmaps its memory.
    fn delete(&mut self) {
        unsafe {
            for fence in self.fences.iter().filter(|fence| !fence.is_null()) {
                gl::DeleteSync(*fence);
            }
            gl::DeleteBuffers(SLOTS as GLsizei, self.buffers.as_ptr());
        }
    }
}

/// Updates waiting for the main thread to upload them.
#[derive(Debug)]
struct Plain {
    pending: VecDeque<FrameUpdate>,
    spares: Vec<Vec<u8>>,
}

impl Plain {
    /// Updates held before the oldest is uploaded rather than kept.
    const CAPACITY: usize = 64;

    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(Self::CAPACITY),
            spares: Vec::with_capacity(MAX_SPARES),
        }
    }

    fn recycle(&mut self, pixels: Vec<u8>) {
        if self.spares.len() < MAX_SPARES {
            self.spares.push(pixels);
        }
    }
}

impl RemoteTexture {
    fn new(
        width: u32,
        height: u32,
        path: UploadPath,
        can_persist: bool,
        swizzle: bool,
        notify: Notify,
    ) -> Result<Self, Error> {
        check_size(width, height)?;

        let mut id = 0;
        unsafe { gl::GenTextures(1, &mut id) };

        let can_persist = can_persist && path == UploadPath::Auto;
        let mut texture = Self {
            id,
            size: [width, height],
            filter: Filter::Linear,
            drawable: false,
            transport: Transport::Plain(Plain::new()),
            retired: Vec::new(),
            swizzle,
            can_persist,
            notify,
        };

        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, id);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as GLint);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as GLint);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as GLint);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as GLint);
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }
        texture.allocate();
        Ok(texture)
    }

    /// Give the texture storage of the current size, and whatever feeds it.
    fn allocate(&mut self) {
        let [width, height] = self.size;
        let persistent =
            self.can_persist.then(|| Persistent::new(width, height, self.notify.clone())).flatten();

        // A persistent transport fills the texture with its first buffer; plain uploads start
        // from black, as nothing else would define the pixels no update has covered.
        let zeros = persistent.is_none().then(|| vec![0u8; pixel_bytes(width, height)]);
        let (internal, format) = self.formats();
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.id);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                internal as GLint,
                width as GLsizei,
                height as GLsizei,
                0,
                format,
                gl::UNSIGNED_BYTE,
                zeros.as_ref().map_or(ptr::null(), |zeros| zeros.as_ptr().cast()),
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }

        let (transport, drawable) = match persistent {
            Some(persistent) => (Transport::Persistent(persistent), false),
            None => (Transport::Plain(Plain::new()), true),
        };
        self.drawable = drawable;

        if let Transport::Persistent(old) = mem::replace(&mut self.transport, transport) {
            old.stream.stop();
            self.retired.push(old);
        }
    }

    /// `glTexImage2D` formats of the texture: its internal one and that of uploads.
    fn formats(&self) -> (GLenum, GLenum) {
        if self.swizzle { (gl::RGBA, gl::RGBA) } else { (gl::RGBA8, gl::BGRA) }
    }

    /// The remote desktop now has this size. All pixels are black until updates cover them, and
    /// updates still queued are dropped, as a session sends what follows for the new size.
    ///
    /// Allocates GPU memory, so it takes a while: fine for an event as rare as this one.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), Error> {
        check_size(width, height)?;
        if self.size != [width, height] {
            self.size = [width, height];
            self.allocate();
        }
        Ok(())
    }

    /// The remote desktop's size in pixels.
    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    /// Whether uploads go through the persistent buffers.
    pub fn is_persistent(&self) -> bool {
        matches!(self.transport, Transport::Persistent(_))
    }

    /// The GL texture name, for reading it back in tests.
    pub fn gl_id(&self) -> u32 {
        self.id
    }

    /// Hand over new pixels. Cheap: the pixels move and are copied elsewhere, or at [`Self::upload`].
    pub fn submit(&mut self, update: FrameUpdate) {
        match &mut self.transport {
            Transport::Persistent(persistent) => persistent.stream.submit(update),
            Transport::Plain(plain) => {
                // Only the newest of what backed up matters, as far as it covers the rest.
                let Some(clipped) = stream::clip_update(&update, self.size[0], self.size[1]) else {
                    plain.recycle(update.pixels);
                    return;
                };
                let mut index = 0;
                while index < plain.pending.len() {
                    let older = &plain.pending[index];
                    let rect =
                        Rect { x: older.x, y: older.y, width: older.width, height: older.height };
                    if clipped.rect.contains(&rect) {
                        if let Some(older) = plain.pending.remove(index) {
                            plain.recycle(older.pixels);
                        }
                    } else {
                        index += 1;
                    }
                }

                // Overdue: uploading the oldest keeps the queue bounded and nothing lost.
                if plain.pending.len() == Plain::CAPACITY {
                    self.upload_plain_front();
                }
                if let Transport::Plain(plain) = &mut self.transport {
                    plain.pending.push_back(update);
                }
            },
        }
    }

    /// Take a pixel buffer an update is done with, to fill for the next one without allocating.
    pub fn take_spare(&mut self) -> Option<Vec<u8>> {
        match &mut self.transport {
            Transport::Persistent(persistent) => persistent.stream.take_spare(),
            Transport::Plain(plain) => plain.spares.pop(),
        }
    }

    /// Apply new pixels to the texture. Call on every frame while anything is pending.
    pub fn upload(&mut self) -> UploadStatus {
        self.reap_retired();

        let [width, _] = self.size;
        match &mut self.transport {
            Transport::Persistent(persistent) => {
                let changed = persistent.upload(self.id, width);
                self.drawable |= changed;
                UploadStatus { changed, pending: persistent.stream.pending() }
            },
            Transport::Plain(plain) => {
                let changed = !plain.pending.is_empty();
                while matches!(&self.transport, Transport::Plain(plain) if !plain.pending.is_empty())
                {
                    self.upload_plain_front();
                }
                UploadStatus { changed, pending: false }
            },
        }
    }

    /// Upload the oldest update waiting on the plain path.
    fn upload_plain_front(&mut self) {
        let Transport::Plain(plain) = &mut self.transport else { return };
        let Some(update) = plain.pending.pop_front() else { return };
        let (_, format) = if self.swizzle { (gl::RGBA, gl::RGBA) } else { (gl::RGBA8, gl::BGRA) };
        if let Some(clipped) = stream::clip_update(&update, self.size[0], self.size[1]) {
            let rect = clipped.rect;
            unsafe {
                gl::BindTexture(gl::TEXTURE_2D, self.id);
                if rect.width == update.width {
                    tex_sub_image(rect, format, update.pixels.as_ptr());
                } else {
                    // Rows clipped on the right: no row length to tell GLES 2 about, so a row at
                    // a time.
                    for row in 0..rect.height {
                        let source = update.pixels[row as usize * clipped.stride..].as_ptr();
                        tex_sub_image(Rect { y: rect.y + row, height: 1, ..rect }, format, source);
                    }
                }
                gl::BindTexture(gl::TEXTURE_2D, 0);
            }
        }
        plain.recycle(update.pixels);
    }

    /// Delete the staging buffers of earlier sizes whose workers have exited.
    fn reap_retired(&mut self) {
        self.retired.retain_mut(|persistent| {
            let finished = persistent.stream.is_finished();
            if finished {
                persistent.stream.join();
                persistent.delete();
            }
            !finished
        });
    }

    fn set_filter(&mut self, filter: Filter) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        let value = match filter {
            Filter::Nearest => gl::NEAREST,
            Filter::Linear => gl::LINEAR,
        } as GLint;
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.id);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, value);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, value);
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }
    }
}

impl Drop for RemoteTexture {
    fn drop(&mut self) {
        // Waiting for a worker would cost the main thread up to a whole frame's copy, more than a
        // frame's budget at 4K: the staging buffers go when their workers have left.
        unsafe { gl::DeleteTextures(1, &self.id) };
        if let Transport::Persistent(persistent) = &self.transport {
            persistent.stream.stop();
        }
        let transport = mem::replace(&mut self.transport, Transport::Plain(Plain::new()));
        let retired = mem::take(&mut self.retired);
        CLOSING.with_borrow_mut(|closing| {
            if let Transport::Persistent(persistent) = transport {
                closing.push(persistent);
            }
            closing.extend(retired);
        });
    }
}

thread_local! {
    /// Staging buffers of dropped textures whose workers may still be writing. Kept on the thread
    /// that owns the context, as they are deleted with GL.
    static CLOSING: RefCell<Vec<Persistent>> = const { RefCell::new(Vec::new()) };
}

/// Delete the staging buffers of dropped textures whose workers have left. Call on the thread
/// that owns the context, once in a while.
pub fn reap_closed() {
    CLOSING.with_borrow_mut(|closing| {
        closing.retain_mut(|persistent| {
            let finished = persistent.stream.is_finished();
            if finished {
                persistent.stream.join();
                persistent.delete();
            }
            !finished
        });
    });
}

fn pixel_bytes(width: u32, height: u32) -> usize {
    width as usize * height as usize * BYTES_PER_PIXEL
}

fn check_size(width: u32, height: u32) -> Result<(), Error> {
    let mut max = 0;
    unsafe { gl::GetIntegerv(gl::MAX_TEXTURE_SIZE, &mut max) };
    if width == 0 || height == 0 || width > max as u32 || height > max as u32 {
        return Err(format!("Remote desktop of {width}x{height} exceeds textures of {max}").into());
    }
    Ok(())
}

unsafe fn tex_sub_image(rect: Rect, format: GLenum, pixels: *const u8) {
    unsafe {
        gl::TexSubImage2D(
            gl::TEXTURE_2D,
            0,
            rect.x as GLint,
            rect.y as GLint,
            rect.width as GLsizei,
            rect.height as GLsizei,
            format,
            gl::UNSIGNED_BYTE,
            pixels.cast(),
        );
    }
}

/// Whether this context has persistently mapped buffers and fences.
fn supports_persistent_buffers(is_gles: bool, gl_version: &str) -> bool {
    if is_gles || !gl::BufferStorage::is_loaded() || !gl::FenceSync::is_loaded() {
        return false;
    }
    let version = gl_version.split_whitespace().next().unwrap_or_default();
    let mut numbers = version.split('.').flat_map(str::parse::<u32>);
    let at_least_4_4 = (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0)) >= (4, 4);
    at_least_4_4 || GlExtensions::contains("GL_ARB_buffer_storage")
}

impl Renderer {
    /// Create a texture for a remote desktop of `width` by `height`.
    ///
    /// `notify` is called from another thread whenever an upload is ready; pass one that wakes
    /// the event loop, and call [`RemoteTexture::upload`] on the frame it leads to.
    pub fn create_remote_texture(
        &self,
        width: u32,
        height: u32,
        path: UploadPath,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<RemoteTexture, Error> {
        let can_persist =
            supports_persistent_buffers(self.remote_caps.is_gles, &self.remote_caps.gl_version);
        let texture = RemoteTexture::new(
            width,
            height,
            path,
            can_persist,
            self.remote_caps.is_gles,
            Arc::new(notify),
        )?;
        info!(
            "Remote desktop texture of {width}x{height}, {} uploads",
            if texture.is_persistent() { "persistent buffer" } else { "plain" }
        );
        Ok(texture)
    }

    /// Draw a remote desktop in `frame`, whose `size` is the pane's. Only the picture is painted:
    /// fill the frame with [`Self::clear_rect`] first for the bars around it.
    pub fn draw_remote(
        &mut self,
        texture: &mut RemoteTexture,
        mode: ScaleMode,
        frame: &Frame,
        size: &SizeInfo,
    ) {
        if !texture.drawable {
            return;
        }
        let layout = RemoteLayout::new(mode, texture.size, frame, size);
        texture.set_filter(layout.filter);

        let Some(program) = self.remote_program() else { return };
        Self::scissor(Some(*frame));
        program.draw(texture, &layout);
        Self::scissor(self.tabbed.frame());
        self.set_viewport(size);
    }
}
