//! The task behind a [`DisplayHandle`](rmx_display::DisplayHandle).
//!
//! `vnc-rs` decodes on its own tasks, stalling when nobody takes its events, so this loop never
//! waits for the interface: it keeps taking events and lets [`Outbox`] absorb them. It also has
//! to be the only caller of the client: `VncClient::recv_event` holds the client's lock while
//! it waits, so a concurrent `input` would be stuck until the next server message. Polling both
//! from one `select!` drops that wait, and its lock, before input is sent.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use log::{debug, warn};
use rmx_display::{DisplayEvent, FrameUpdate, InputEvent};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, sleep_until};
use vnc::client::VncClient;
use vnc::{DesktopLayout, PixelFormat, Rect as VncRect, ResizeError, VncError, VncEvent, X11Event};

use crate::input;
use rmx_display::outbox::{InvalidRect, Outbox, Rect};

/// Asks for changes at most this often, once the server is producing them.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Asks again after this long without an answer. Servers normally hold a request until
/// something changes, but one that answers with an empty update produces no event here.
const IDLE_REFRESH: Duration = Duration::from_secs(1);

/// `vnc-rs` fails rather than truncates client cut text above this size.
const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

type ResizeFuture = Pin<Box<dyn Future<Output = Result<DesktopLayout, ResizeError>> + Send>>;

enum End {
    /// The user ended the session: the owed pixels no longer matter.
    Local,
    /// The session ended on its own.
    Remote(Option<String>),
    /// The interface is gone, so there is nobody to tell.
    InterfaceGone,
}

pub(crate) async fn run(
    client: VncClient,
    format: PixelFormat,
    input: Receiver<InputEvent>,
    events: Sender<DisplayEvent>,
) {
    let mut session = Session {
        client,
        format,
        events,
        outbox: Outbox::default(),
        held: 0,
        next_request: Instant::now() + IDLE_REFRESH,
        sized: false,
        resizing: None,
        queued_resize: None,
    };
    let end = session.pump(input).await;
    // `close` stops the network task; it must not wait for a slow interface.
    let _ = session.client.close().await;
    session.finish(end).await;
}

struct Session {
    client: VncClient,
    /// The format the server was asked to send pixels in.
    format: PixelFormat,
    events: Sender<DisplayEvent>,
    outbox: Outbox,
    /// Buttons currently down, which a wheel notch must keep down.
    held: u8,
    next_request: Instant,
    /// Whether the first size is known; `vnc-rs` itself requests the first frame.
    sized: bool,
    resizing: Option<ResizeFuture>,
    /// The newest size asked for while another resize is in flight.
    queued_resize: Option<(u16, u16)>,
}

impl Session {
    async fn pump(&mut self, mut input: Receiver<InputEvent>) -> End {
        // A permit borrows its sender, which must not be `self` in the arms below.
        let events = self.events.clone();
        loop {
            tokio::select! {
                biased;

                message = input.recv() => match message {
                    None | Some(InputEvent::Disconnect) => return End::Local,
                    Some(event) => {
                        if let Err(error) = self.handle_input(event).await {
                            return End::Remote(Some(error.to_string()));
                        }
                    },
                },

                permit = events.reserve(), if self.outbox.has_pending() => match permit {
                    Ok(permit) => {
                        if let Some(event) = self.outbox.next_event() {
                            permit.send(event);
                        }
                    },
                    Err(_) => return End::InterfaceGone,
                },

                result = poll_resize(&mut self.resizing), if self.resizing.is_some() => {
                    self.resizing = None;
                    self.resize_finished(result);
                },

                // While pixels are owed the interface is behind: asking for more would only
                // spend bandwidth and decoding on frames it cannot show yet.
                () = sleep_until(self.next_request), if !self.outbox.has_damage() => {
                    self.next_request = Instant::now() + IDLE_REFRESH;
                    if let Err(error) = self.client.input(X11Event::Refresh).await {
                        return End::Remote(Some(error.to_string()));
                    }
                },

                event = self.client.recv_event() => match event {
                    Ok(event) => {
                        if let Some(end) = self.handle_event(event).await {
                            return end;
                        }
                    },
                    Err(_) => return End::Remote(None),
                },
            }
        }
    }

    async fn handle_input(&mut self, event: InputEvent) -> Result<(), VncError> {
        match event {
            InputEvent::Key { key, pressed } => match input::key(key, pressed) {
                Some(message) => self.client.input(message.into()).await?,
                None => debug!("dropping key without keysym: evdev {}", key.evdev),
            },
            InputEvent::Pointer { x, y, buttons } => {
                self.held = buttons;
                self.client.input(input::pointer(x, y, buttons).into()).await?;
            },
            InputEvent::Wheel { x, y, vertical, horizontal } => {
                for message in input::wheel(x, y, self.held, vertical, horizontal) {
                    self.client.input(message.into()).await?;
                }
            },
            InputEvent::Clipboard(text) if text.len() > MAX_CLIPBOARD_BYTES => {
                warn!("not sending {} bytes of clipboard: over the VNC limit", text.len());
            },
            InputEvent::Clipboard(text) => self.client.input(X11Event::CopyText(text)).await?,
            InputEvent::Resize { width, height } => self.request_resize(width, height),
            // `pump` handles this before it gets here.
            InputEvent::Disconnect => {},
        }
        Ok(())
    }

    fn request_resize(&mut self, width: u32, height: u32) {
        let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) else {
            warn!("ignoring resize to {width}x{height}: too large for RFB");
            return;
        };
        if self.resizing.is_some() {
            self.queued_resize = Some((width, height));
            return;
        }
        let client = self.client.clone();
        // Kept and polled rather than dropped: `vnc-rs` treats a cancelled resize as having an
        // unknown outcome and refuses further ones.
        self.resizing = Some(Box::pin(async move { client.resize_desktop(width, height).await }));
    }

    fn resize_finished(&mut self, result: Result<DesktopLayout, ResizeError>) {
        if let Err(error) = result {
            warn!("desktop resize failed: {error}");
        }
        if let Some((width, height)) = self.queued_resize.take() {
            self.request_resize(u32::from(width), u32::from(height));
        }
    }

    async fn handle_event(&mut self, event: VncEvent) -> Option<End> {
        match event {
            VncEvent::SetResolution(screen) => {
                self.set_size(screen.width, screen.height).await.err()
            },
            // Same-size announcements keep their pixels, and `set_size` ignores those.
            VncEvent::DesktopUpdate(update) => match update.layout {
                Some(layout) => self.set_size(layout.width, layout.height).await.err(),
                None => {
                    debug!("desktop update without a layout: {:?}", update.status);
                    self.request_now();
                    None
                },
            },
            VncEvent::RawImage(rect, pixels) => {
                self.request_soon();
                self.handle_image(rect, pixels)
            },
            VncEvent::Copy(dst, src) => {
                self.request_soon();
                let dst = to_rect(dst);
                match self.outbox.copy(dst, to_rect(src)) {
                    Ok(()) => {
                        self.outbox.add_damage(dst);
                        None
                    },
                    Err(InvalidRect) => Some(End::Remote(Some("server sent a bad copy".into()))),
                }
            },
            // Only sent after the JPEG quality pseudo-encodings, which `vnc-rs` cannot advertise.
            VncEvent::JpegImage(..) => {
                Some(End::Remote(Some("server sent JPEG data without being offered it".into())))
            },
            VncEvent::Text(text) => {
                self.outbox.set_clipboard(text);
                None
            },
            VncEvent::Bell => {
                self.outbox.ring_bell();
                None
            },
            VncEvent::Error(reason) => Some(End::Remote(Some(reason))),
            // Neither is requested: the pixel format is ours and the cursor stays in the picture.
            other => {
                debug!("ignoring event: {other:?}");
                None
            },
        }
    }

    fn handle_image(&mut self, rect: VncRect, pixels: Vec<u8>) -> Option<End> {
        let pixels = to_bgra(&self.format, pixels);
        let (x, y) = (u32::from(rect.x), u32::from(rect.y));
        let (width, height) = (u32::from(rect.width), u32::from(rect.height));
        let update = FrameUpdate { x, y, width, height, pixels };
        if self.outbox.paint(&update) == Err(InvalidRect) {
            return Some(End::Remote(Some("server sent a bad rectangle".into())));
        }

        // With nothing older waiting, these pixels can go as they are, without another copy.
        if self.outbox.is_idle() {
            return match self.events.try_send(DisplayEvent::Update(update)) {
                Ok(()) => None,
                Err(TrySendError::Closed(_)) => Some(End::InterfaceGone),
                Err(TrySendError::Full(_)) => {
                    self.outbox.add_damage(Rect { x, y, width, height });
                    None
                },
            };
        }
        self.outbox.add_damage(Rect { x, y, width, height });
        None
    }

    async fn set_size(&mut self, width: u16, height: u16) -> Result<(), End> {
        let changed = self.outbox.resize(u32::from(width), u32::from(height));
        let first = !std::mem::replace(&mut self.sized, true);
        if first {
            return Ok(());
        }
        if changed {
            self.client
                .input(X11Event::FullRefresh)
                .await
                .map_err(|error| End::Remote(Some(error.to_string())))?;
        } else {
            self.request_now();
        }
        Ok(())
    }

    /// An update that only carried a layout used up the request it answered, and no pixels will
    /// come until there is another. TigerVNC does this to every full request, whose picture then
    /// waits for this one. Incremental, as a full request would be answered with a layout again.
    fn request_now(&mut self) {
        self.next_request = Instant::now();
    }

    /// The server is producing changes: ask for the next ones after one frame.
    fn request_soon(&mut self) {
        self.next_request = self.next_request.min(Instant::now() + FRAME_INTERVAL);
    }

    /// Delivers what is left, then `Closed`. Waits for the interface; dropping its receiver
    /// releases this.
    async fn finish(mut self, end: End) {
        let reason = match end {
            End::InterfaceGone => return,
            End::Local => {
                self.outbox.discard_damage();
                None
            },
            End::Remote(reason) => reason,
        };
        self.outbox.close(reason);
        while let Some(event) = self.outbox.next_event() {
            if self.events.send(event).await.is_err() {
                return;
            }
        }
    }
}

/// Waits for the resize in `slot`, which the caller guarantees is there.
/// Pixels in `format` as opaque BGRA8.
fn to_bgra(format: &PixelFormat, mut pixels: Vec<u8>) -> Vec<u8> {
    if format.bits_per_pixel == 32 {
        // Full color is already BGRA, with the server's padding byte where alpha belongs.
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = u8::MAX;
        }
        return pixels;
    }
    let channel = |value: u32, max: u16, shift: u8| {
        let max = u32::from(max);
        let level = (value >> shift) & max;
        // Rounded, so that the largest level is 255 and none is 0 but the smallest.
        ((level * 255 + max / 2) / max) as u8
    };
    let convert = |value: u32| {
        [
            channel(value, format.blue_max, format.blue_shift),
            channel(value, format.green_max, format.green_shift),
            channel(value, format.red_max, format.red_shift),
            u8::MAX,
        ]
    };
    match format.bits_per_pixel {
        16 => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|pixel| convert(u32::from(u16::from_le_bytes(*pixel))))
            .collect(),
        _ => pixels.iter().flat_map(|&pixel| convert(u32::from(pixel))).collect(),
    }
}

async fn poll_resize(slot: &mut Option<ResizeFuture>) -> Result<DesktopLayout, ResizeError> {
    match slot {
        Some(future) => future.await,
        None => std::future::pending().await,
    }
}

fn to_rect(rect: VncRect) -> Rect {
    Rect {
        x: u32::from(rect.x),
        y: u32::from(rect.y),
        width: u32::from(rect.width),
        height: u32::from(rect.height),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::marker::PhantomData;
    use std::task::{Context, Poll};

    use rmx_display::{DisplayHandle, Key};
    use tokio::io::{
        AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf, ReadHalf,
        WriteHalf, duplex, split,
    };
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    use crate::{Config, ConnectError, connect};

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    /// A stream that is `Send` but not `Sync`, as a tunnel channel may be.
    struct NotSync {
        inner: DuplexStream,
        _not_sync: PhantomData<Cell<()>>,
    }

    impl AsyncRead for NotSync {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for NotSync {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// What the client sent to the fake server.
    #[derive(Debug, PartialEq, Eq)]
    enum Received {
        Encodings(Vec<i32>),
        Refresh { incremental: bool },
        Key { keysym: u32, down: bool },
        Pointer { x: u16, y: u16, mask: u8 },
        CutText(Vec<u8>),
    }

    /// The server's side of a session, after the handshake.
    struct FakeServer {
        writer: WriteHalf<DuplexStream>,
        received: mpsc::UnboundedReceiver<Received>,
        reader: JoinHandle<io::Result<()>>,
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            // The reader holds the other half of the stream; the client sees EOF without it.
            self.reader.abort();
        }
    }

    impl FakeServer {
        async fn write(&mut self, bytes: &[u8]) {
            self.writer.write_all(bytes).await.unwrap();
        }

        async fn raw(&mut self, x: u16, y: u16, w: u16, h: u16, pixel: [u8; 4]) {
            self.raw_pixels(x, y, w, h, &pixel).await;
        }

        /// A Raw rectangle filled with `pixel`, in whatever format the client asked for.
        async fn raw_pixels(&mut self, x: u16, y: u16, w: u16, h: u16, pixel: &[u8]) {
            let mut message = vec![0, 0, 0, 1];
            for field in [x, y, w, h] {
                message.extend_from_slice(&field.to_be_bytes());
            }
            message.extend_from_slice(&0i32.to_be_bytes());
            for _ in 0..u32::from(w) * u32::from(h) {
                message.extend_from_slice(pixel);
            }
            self.write(&message).await;
        }

        async fn copy(&mut self, dst: (u16, u16), src: (u16, u16), w: u16, h: u16) {
            let mut message = vec![0, 0, 0, 1];
            for field in [dst.0, dst.1, w, h] {
                message.extend_from_slice(&field.to_be_bytes());
            }
            message.extend_from_slice(&1i32.to_be_bytes());
            for field in [src.0, src.1] {
                message.extend_from_slice(&field.to_be_bytes());
            }
            self.write(&message).await;
        }

        async fn desktop_size(&mut self, w: u16, h: u16) {
            let mut message = vec![0, 0, 0, 1, 0, 0, 0, 0];
            message.extend_from_slice(&w.to_be_bytes());
            message.extend_from_slice(&h.to_be_bytes());
            message.extend_from_slice(&(-223i32).to_be_bytes());
            self.write(&message).await;
        }

        /// An ExtendedDesktopSize update, which carries no pixels.
        async fn layout(&mut self, reason: u16, status: u16, w: u16, h: u16) {
            let mut message = vec![0, 0, 0, 1];
            for field in [reason, status, w, h] {
                message.extend_from_slice(&field.to_be_bytes());
            }
            message.extend_from_slice(&(-308i32).to_be_bytes());
            message.extend_from_slice(&[1, 0, 0, 0]);
            message.extend_from_slice(&1u32.to_be_bytes());
            for field in [0, 0, w, h] {
                message.extend_from_slice(&field.to_be_bytes());
            }
            message.extend_from_slice(&0u32.to_be_bytes());
            self.write(&message).await;
        }

        async fn cut_text(&mut self, text: &str) {
            let mut message = vec![3, 0, 0, 0];
            message.extend_from_slice(&(text.len() as u32).to_be_bytes());
            message.extend_from_slice(text.as_bytes());
            self.write(&message).await;
        }

        async fn next_matching(&mut self, wanted: impl Fn(&Received) -> bool) -> Received {
            timeout(WAIT, async {
                loop {
                    let message = self.received.recv().await.expect("client hung up");
                    if wanted(&message) {
                        return message;
                    }
                }
            })
            .await
            .expect("timed out waiting for a client message")
        }
    }

    async fn read_client_messages(
        mut reader: ReadHalf<DuplexStream>,
        sink: mpsc::UnboundedSender<Received>,
    ) -> io::Result<()> {
        loop {
            let message = match reader.read_u8().await? {
                0 => {
                    let mut body = [0; 19];
                    reader.read_exact(&mut body).await?;
                    continue;
                },
                2 => {
                    reader.read_u8().await?;
                    let count = reader.read_u16().await?;
                    let mut encodings = Vec::new();
                    for _ in 0..count {
                        encodings.push(reader.read_i32().await?);
                    }
                    Received::Encodings(encodings)
                },
                3 => {
                    let incremental = reader.read_u8().await? != 0;
                    let mut rect = [0; 8];
                    reader.read_exact(&mut rect).await?;
                    Received::Refresh { incremental }
                },
                4 => {
                    let down = reader.read_u8().await? != 0;
                    reader.read_u16().await?;
                    Received::Key { keysym: reader.read_u32().await?, down }
                },
                5 => {
                    let mask = reader.read_u8().await?;
                    let x = reader.read_u16().await?;
                    Received::Pointer { x, y: reader.read_u16().await?, mask }
                },
                6 => {
                    let mut padding = [0; 3];
                    reader.read_exact(&mut padding).await?;
                    let mut text = vec![0; reader.read_u32().await? as usize];
                    reader.read_exact(&mut text).await?;
                    Received::CutText(text)
                },
                other => panic!("unexpected client message {other}"),
            };
            let _ = sink.send(message);
        }
    }

    /// Answers the RFB 3.8 handshake with no security for a `width` by `height` desktop.
    async fn handshake(mut stream: DuplexStream, width: u16, height: u16) -> FakeServer {
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).await.unwrap();
        stream.write_all(&[1, 1]).await.unwrap();
        stream.read_u8().await.unwrap();
        stream.write_u32(0).await.unwrap();
        stream.read_u8().await.unwrap();

        stream.write_u16(width).await.unwrap();
        stream.write_u16(height).await.unwrap();
        let format = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
        stream.write_all(&format).await.unwrap();
        stream.write_u32(4).await.unwrap();
        stream.write_all(b"fake").await.unwrap();

        let (reader, writer) = split(stream);
        let (sink, received) = mpsc::unbounded_channel();
        let reader = tokio::spawn(read_client_messages(reader, sink));
        FakeServer { writer, received, reader }
    }

    async fn start(width: u16, height: u16, config: Config) -> (FakeServer, DisplayHandle) {
        let (near, far) = duplex(1 << 20);
        let stream = NotSync { inner: near, _not_sync: PhantomData };
        let (server, handle) = tokio::join!(handshake(far, width, height), connect(stream, config));
        (server, handle.unwrap())
    }

    async fn next_event(handle: &mut DisplayHandle) -> DisplayEvent {
        timeout(WAIT, handle.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("the session ended without Closed")
    }

    /// A desktop as the interface rebuilds it from events.
    struct Screen {
        width: u32,
        pixels: Vec<u8>,
    }

    impl Screen {
        fn new(width: u32, height: u32) -> Self {
            Self { width, pixels: vec![0; (width * height * 4) as usize] }
        }

        fn apply(&mut self, update: &FrameUpdate) {
            let row_len = update.width as usize * 4;
            for (row, source) in update.pixels.chunks_exact(row_len).enumerate() {
                let y = update.y as usize + row;
                let start = (y * self.width as usize + update.x as usize) * 4;
                self.pixels[start..start + row_len].copy_from_slice(source);
            }
        }

        fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
            let start = ((y * self.width + x) * 4) as usize;
            self.pixels[start..start + 4].try_into().unwrap()
        }
    }

    #[tokio::test]
    async fn a_stalled_interface_gets_the_latest_picture_after_coalescing() {
        let config = Config { event_capacity: 1, ..Config::default() };
        let (mut server, mut handle) = start(4, 4, config).await;
        const FRAMES: u8 = 40;

        // Nothing reads events while the frames arrive, and the decoder must keep going.
        for frame in 1..=FRAMES {
            server.raw(0, 0, 4, 4, [frame, 2, 3, 0]).await;
        }
        sleep(Duration::from_millis(300)).await;
        drop(server);

        let mut screen = Screen::new(0, 0);
        let mut updates = 0;
        loop {
            match next_event(&mut handle).await {
                DisplayEvent::Resized { width, height } => screen = Screen::new(width, height),
                DisplayEvent::Update(update) => {
                    updates += 1;
                    screen.apply(&update);
                },
                DisplayEvent::Closed(None) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }

        assert!(updates < u32::from(FRAMES), "{updates} updates were not coalesced");
        for (x, y) in [(0, 0), (3, 3)] {
            assert_eq!(screen.pixel(x, y), [FRAMES, 2, 3, 255]);
        }
    }

    #[tokio::test]
    async fn events_reach_the_interface_in_bgra_with_copies_resolved() {
        let (mut server, mut handle) = start(4, 2, Config::default()).await;
        assert_eq!(next_event(&mut handle).await, DisplayEvent::Resized { width: 4, height: 2 });

        server.raw(0, 0, 2, 2, [10, 20, 30, 0]).await;
        let DisplayEvent::Update(first) = next_event(&mut handle).await else {
            panic!("expected the raw update");
        };
        assert_eq!((first.x, first.y, first.width, first.height), (0, 0, 2, 2));
        assert_eq!(first.pixels, [10, 20, 30, 255].repeat(4));

        server.copy((2, 0), (0, 0), 2, 2).await;
        let DisplayEvent::Update(copied) = next_event(&mut handle).await else {
            panic!("expected the copy as an update");
        };
        assert_eq!((copied.x, copied.y, copied.width, copied.height), (2, 0, 2, 2));
        assert_eq!(copied.pixels, [10, 20, 30, 255].repeat(4));

        server.cut_text("from server").await;
        assert_eq!(next_event(&mut handle).await, DisplayEvent::Clipboard("from server".into()));
        server.write(&[2]).await;
        assert_eq!(next_event(&mut handle).await, DisplayEvent::Bell);
    }

    /// The first update of a 1x1 desktop whose one pixel the server sends as `pixel`.
    async fn one_pixel_at(color_depth: crate::ColorDepth, pixel: &[u8]) -> Vec<u8> {
        let config = Config { color_depth, ..Config::default() };
        let (mut server, mut handle) = start(1, 1, config).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Resized { .. }));
        server.raw_pixels(0, 0, 1, 1, pixel).await;
        let DisplayEvent::Update(update) = next_event(&mut handle).await else {
            panic!("expected the raw update");
        };
        update.pixels
    }

    #[tokio::test]
    async fn sixteen_bit_pixels_reach_the_interface_in_bgra() {
        use crate::ColorDepth::Bits16;
        // 5-6-5 little-endian: full red, half green, no blue.
        let pixel = (31u16 << 11 | 32 << 5).to_le_bytes();
        assert_eq!(one_pixel_at(Bits16, &pixel).await, [0, 130, 255, 255]);
        assert_eq!(one_pixel_at(Bits16, &u16::MAX.to_le_bytes()).await, [255; 4]);
    }

    #[tokio::test]
    async fn eight_bit_pixels_reach_the_interface_in_bgra() {
        use crate::ColorDepth::Bits8;
        // 3-3-2: no red, full green, full blue.
        assert_eq!(one_pixel_at(Bits8, &[0b0001_1111]).await, [255, 255, 0, 255]);
        assert_eq!(one_pixel_at(Bits8, &[0]).await, [0, 0, 0, 255]);
    }

    #[tokio::test]
    async fn fewer_colors_leave_tight_out() {
        let config = Config { color_depth: crate::ColorDepth::Bits16, ..Config::default() };
        let (mut server, _handle) = start(2, 2, config).await;

        let message = server.next_matching(|m| matches!(m, Received::Encodings(_))).await;

        let Received::Encodings(encodings) = message else { unreachable!() };
        assert!(!encodings.contains(&7), "offered Tight at 16 bits: {encodings:?}");
        assert!(encodings.contains(&16), "ZRLE missing from {encodings:?}");
    }

    #[tokio::test]
    async fn a_desktop_resize_asks_for_a_full_frame() {
        let (mut server, mut handle) = start(4, 2, Config::default()).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Resized { .. }));
        // The request `vnc-rs` makes for the first frame.
        let first = |m: &Received| matches!(m, Received::Refresh { incremental: false });
        server.next_matching(first).await;

        server.desktop_size(6, 3).await;

        assert_eq!(next_event(&mut handle).await, DisplayEvent::Resized { width: 6, height: 3 });
        server.next_matching(first).await;
    }

    /// An update with no pixels must not leave the session waiting for the idle refresh.
    async fn asks_again_right_after(reason: u16, status: u16) {
        let (mut server, mut handle) = start(4, 2, Config::default()).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Resized { .. }));
        let first = |m: &Received| matches!(m, Received::Refresh { incremental: false });
        server.next_matching(first).await;

        server.layout(reason, status, 4, 2).await;

        let incremental = |m: &Received| matches!(m, Received::Refresh { incremental: true });
        timeout(IDLE_REFRESH / 2, server.next_matching(incremental))
            .await
            .expect("the next request waited for the idle refresh");
    }

    #[tokio::test]
    async fn a_same_size_layout_is_followed_by_a_request_at_once() {
        asks_again_right_after(0, 0).await;
    }

    #[tokio::test]
    async fn a_refused_resize_is_followed_by_a_request_at_once() {
        asks_again_right_after(1, 1).await;
    }

    #[tokio::test]
    async fn encodings_leave_the_cursor_in_the_picture() {
        let (mut server, _handle) = start(2, 2, Config::default()).await;

        let message = server.next_matching(|m| matches!(m, Received::Encodings(_))).await;

        let Received::Encodings(encodings) = message else { unreachable!() };
        for wanted in [1, 16, 7, 15, 0, -223, -308, -224] {
            assert!(encodings.contains(&wanted), "{wanted} missing from {encodings:?}");
        }
        assert!(!encodings.contains(&-239), "asked for the cursor pseudo-encoding");
    }

    #[tokio::test]
    async fn input_reaches_the_server_as_rfb_messages() {
        let (mut server, handle) = start(100, 100, Config::default()).await;
        let is_input = |m: &Received| {
            matches!(m, Received::Key { .. } | Received::Pointer { .. } | Received::CutText(_))
        };
        let events = [
            InputEvent::Key { key: Key { evdev: 30, keysym: Some(0x61) }, pressed: true },
            InputEvent::Pointer { x: 7, y: 9, buttons: rmx_display::buttons::RIGHT },
            InputEvent::Wheel { x: 7, y: 9, vertical: 1, horizontal: 0 },
            InputEvent::Clipboard("hello".into()),
        ];
        for event in events {
            handle.input.send(event).await.unwrap();
        }

        let expected = [
            Received::Key { keysym: 0x61, down: true },
            Received::Pointer { x: 7, y: 9, mask: 4 },
            Received::Pointer { x: 7, y: 9, mask: 4 | 1 << 3 },
            Received::Pointer { x: 7, y: 9, mask: 4 },
            Received::CutText(b"hello".to_vec()),
        ];
        for want in expected {
            assert_eq!(server.next_matching(is_input).await, want);
        }
    }

    #[tokio::test]
    async fn input_is_sent_while_the_server_is_silent() {
        // Regression guard: `recv_event` holds the client's lock while it waits.
        let (mut server, mut handle) = start(10, 10, Config::default()).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Resized { .. }));

        let key = Key { evdev: 30, keysym: Some(0x62) };
        handle.input.send(InputEvent::Key { key, pressed: true }).await.unwrap();

        let received = server.next_matching(|m| matches!(m, Received::Key { .. })).await;
        assert_eq!(received, Received::Key { keysym: 0x62, down: true });
    }

    #[tokio::test]
    async fn refreshes_follow_updates() {
        let (mut server, mut handle) = start(2, 2, Config::default()).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Resized { .. }));

        server.raw(0, 0, 2, 2, [1, 1, 1, 1]).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Update(_)));

        let incremental = |m: &Received| matches!(m, Received::Refresh { incremental: true });
        server.next_matching(incremental).await;
    }

    #[tokio::test]
    async fn disconnect_ends_with_closed_and_no_reason() {
        let (_server, mut handle) = start(2, 2, Config::default()).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Resized { .. }));

        handle.input.send(InputEvent::Disconnect).await.unwrap();

        assert_eq!(next_event(&mut handle).await, DisplayEvent::Closed(None));
        assert!(handle.events.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_garbled_stream_closes_with_a_reason() {
        let (mut server, mut handle) = start(2, 2, Config::default()).await;
        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Resized { .. }));

        server.write(&[200]).await;

        assert!(matches!(next_event(&mut handle).await, DisplayEvent::Closed(Some(_))));
    }

    #[tokio::test]
    async fn a_password_is_required_when_the_server_asks() {
        let (near, mut far) = duplex(4096);
        let server = async move {
            far.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0; 12];
            far.read_exact(&mut version).await.unwrap();
            far.write_all(&[1, 2]).await.unwrap();
            far.read_u8().await.unwrap();
            far
        };

        let (_far, result) = tokio::join!(server, connect(near, Config::default()));

        assert_eq!(result.unwrap_err(), ConnectError::PasswordRequired);
    }

    #[test]
    fn config_debug_hides_the_password() {
        let config = Config { password: Some("hunter2".into()), ..Config::default() };
        assert!(!format!("{config:?}").contains("hunter2"));
    }
}
