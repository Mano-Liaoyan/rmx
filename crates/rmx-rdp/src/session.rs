//! The RDP session: handshake on the calling thread, then decoding on it.

use std::net::SocketAddr;
use std::time::Duration;

use ironrdp_cliprdr::Cliprdr;
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_connector::connection_activation::{
    ConnectionActivationSequence, ConnectionActivationState,
};
use ironrdp_connector::{
    BitmapConfig, ClientConnector, Config, ConnectionOutcome, ConnectionResult, ConnectorResult,
    Credentials, DesktopSize, Sequence as _, ServerName,
};
use ironrdp_core::{MonotonicInstant, WriteBuf};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_pdu::Action;
use ironrdp_pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp_pdu::rdp::capability_sets::{
    MajorPlatformType, RailSupportLevel, client_codecs_capabilities,
};
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{
    ActiveStage, ActiveStageBuilder, ActiveStageOutput, GracefulDisconnectReason,
};
use ironrdp_tokio::bytes::BytesMut;
use ironrdp_tokio::{
    MovableTokioFramed, NetworkClient, connect_begin, connect_finalize, mark_as_upgraded,
};
use rmx_display::{DisplayEvent, DisplayHandle, InputEvent};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_rustls::client::TlsStream;

use crate::clipboard::{Action as ClipboardAction, ClipboardHandle};
use crate::input::InputMapper;
use crate::outbox::{Frame, Outbox, Rect};
use crate::{Quality, RdpConfig, RdpError, Security, tls};

/// Network frames waiting to be decoded. Beyond this, reading stops and the server is throttled.
const FRAME_QUEUE: usize = 8;
const INPUT_QUEUE: usize = 256;
/// How long the server gets to close the session after being asked.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// A write stalled this long means the peer is gone. It also bounds the outgoing queue.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs a whole session. `ready` reports the handshake's outcome.
pub(crate) async fn run<S>(
    stream: S,
    config: RdpConfig,
    mut ready: oneshot::Sender<Result<DisplayHandle, RdpError>>,
) where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let clipboard = ClipboardHandle::default();
    let event_capacity = config.event_capacity.max(1);
    let limit = config.connect_timeout;
    let established = tokio::select! {
        result = timeout(limit, handshake(stream, config, &clipboard)) => {
            result.unwrap_or(Err(RdpError::Timeout))
        }
        () = ready.closed() => return,
    };
    let (result, framed) = match established {
        Ok(established) => established,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        },
    };

    let (events_tx, events_rx) = mpsc::channel(event_capacity);
    let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
    if ready.send(Ok(DisplayHandle { events: events_rx, input: input_tx })).is_err() {
        return;
    }

    let (net_tx, net_rx) = mpsc::channel(FRAME_QUEUE);
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let (stream, leftover) = framed.into_inner();
    let (read_half, write_half) = tokio::io::split(stream);
    let reader = tokio::spawn(read_frames(
        MovableTokioFramed::new_with_leftover(read_half, leftover),
        net_tx.clone(),
    ));
    let writer = tokio::spawn(write_frames(write_half, write_rx, net_tx));

    let mut session = Session::new(result, write_tx, clipboard);
    let outcome = session.drive(net_rx, input_rx, &events_tx).await;
    session.finish(outcome, &events_tx).await;

    reader.abort();
    drop(session);
    let _ = writer.await;
}

async fn handshake<S>(
    stream: S,
    config: RdpConfig,
    clipboard: &ClipboardHandle,
) -> Result<(ConnectionResult, MovableTokioFramed<TlsStream<S>>), RdpError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin,
{
    let server_name = config.server_name.clone();
    let connector_config = connector_config(&config)?;

    let mut connector = ClientConnector::new(connector_config, SocketAddr::from(([0, 0, 0, 0], 0)))
        .with_static_channel(
            DrdynvcClient::new()
                .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new()))),
        )
        .with_static_channel(Cliprdr::<ironrdp_cliprdr::Client>::new(Box::new(
            clipboard.backend(),
        )));

    let mut framed = MovableTokioFramed::new(stream);
    let should_upgrade =
        connect_begin(&mut framed, &mut connector).await.map_err(handshake_error)?;

    let (stream, leftover) = framed.into_inner();
    let tls::Upgraded { stream, server_public_key } =
        tls::upgrade(stream, &server_name, &config.certificate).await?;
    let upgraded = mark_as_upgraded(should_upgrade, &mut connector);

    let mut framed = MovableTokioFramed::new_with_leftover(stream, leftover);
    let outcome = connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoNetwork,
        ServerName::new(server_name),
        server_public_key,
        None,
    )
    .await
    .map_err(handshake_error)?;

    match outcome {
        ConnectionOutcome::Connected(result) => Ok((result, framed)),
        ConnectionOutcome::Redirected(_) => Err(RdpError::Redirected),
    }
}

fn handshake_error(error: ironrdp_connector::ConnectorError) -> RdpError {
    RdpError::Handshake(error.report().to_string())
}

/// CredSSP needs the network only to reach a Kerberos KDC, which this client does not configure.
struct NoNetwork;

impl NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _request: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!("no network client is available for Kerberos"))
    }
}

fn connector_config(config: &RdpConfig) -> Result<Config, RdpError> {
    let (enable_tls, enable_credssp) = match config.security {
        Security::Nla => (false, true),
        Security::Tls => (true, false),
        Security::NlaOrTls => (true, true),
    };
    let codec_settings: &[&str] = if config.remotefx { &[] } else { &["remotefx:off"] };
    let codecs = client_codecs_capabilities(codec_settings)
        .map_err(|error| RdpError::Handshake(format!("codec configuration: {error}")))?;
    let (color_depth, connection_type, performance_flags) = quality_settings(config.quality);
    Ok(Config {
        desktop_size: DesktopSize { width: config.width, height: config.height },
        monitor_layout: None,
        desktop_scale_factor: 0,
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: None,
        monitor_extended_layout: None,
        relative_mouse_input: false,
        enable_tls,
        enable_credssp,
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        client_build: 0,
        client_name: config.client_name.clone(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: config.keyboard_layout,
        connection_type,
        ime_file_name: String::new(),
        bitmap: Some(BitmapConfig { lossy_compression: true, color_depth, codecs }),
        dig_product_id: String::new(),
        client_dir: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        autologon: true,
        enable_audio_playback: false,
        enable_audio_capture: false,
        performance_flags,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        // There is no separate cursor plane on the interface, so the pointer is drawn into
        // the framebuffer here, keeping it visible on every server.
        enable_server_pointer: true,
        pointer_software_rendering: true,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
    })
}

/// The color depth, connection type and desktop effects a [`Quality`] asks for.
fn quality_settings(quality: Quality) -> (u32, ConnectionType, PerformanceFlags) {
    match quality {
        Quality::Best => (
            32,
            ConnectionType::Lan,
            PerformanceFlags::ENABLE_FONT_SMOOTHING | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION,
        ),
        Quality::Balanced => (32, ConnectionType::Lan, PerformanceFlags::default()),
        Quality::Fast => (
            16,
            ConnectionType::BroadbandLow,
            PerformanceFlags::DISABLE_WALLPAPER
                | PerformanceFlags::DISABLE_FULLWINDOWDRAG
                | PerformanceFlags::DISABLE_MENUANIMATIONS
                | PerformanceFlags::DISABLE_THEMING
                | PerformanceFlags::DISABLE_CURSOR_SHADOW,
        ),
    }
}

/// What the network tasks tell the decoder.
enum NetEvent {
    Frame { action: Action, frame: BytesMut, received_at: Option<MonotonicInstant> },
    Failed(String),
}

async fn read_frames<S>(mut framed: MovableTokioFramed<ReadHalf<S>>, tx: mpsc::Sender<NetEvent>)
where
    S: AsyncRead + Send + Unpin,
{
    loop {
        let event = match framed.read_pdu().await {
            Ok((action, frame)) => {
                NetEvent::Frame { action, frame, received_at: framed.last_read_at() }
            },
            Err(error) => NetEvent::Failed(format!("read failed: {error}")),
        };
        let failed = matches!(event, NetEvent::Failed(_));
        if tx.send(event).await.is_err() || failed {
            return;
        }
    }
}

async fn write_frames<S>(
    mut stream: WriteHalf<S>,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    tx: mpsc::Sender<NetEvent>,
) where
    S: AsyncWrite,
{
    while let Some(frame) = rx.recv().await {
        let written = timeout(WRITE_TIMEOUT, async {
            stream.write_all(&frame).await?;
            stream.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => {},
            Ok(Err(error)) => {
                let _ = tx.send(NetEvent::Failed(format!("write failed: {error}"))).await;
                return;
            },
            Err(_) => {
                let _ = tx.send(NetEvent::Failed("write timed out".to_owned())).await;
                return;
            },
        }
    }
    let _ = stream.shutdown().await;
}

/// How the event loop came to an end: `None` for a clean close, or the reason it failed.
type Outcome = Option<String>;

struct Session {
    stage: ActiveStage,
    image: DecodedImage,
    outbox: Outbox,
    mapper: InputMapper,
    write: mpsc::UnboundedSender<Vec<u8>>,
    clipboard: ClipboardHandle,
    last_remote_text: Option<String>,
    activation: ironrdp_connector::connection_activation::ConnectionActivationFactory,
    /// A running Deactivation-Reactivation Sequence, during which graphics are not decoded.
    reactivating: Option<ConnectionActivationSequence>,
    refresh_rect_support: bool,
    suppress_output_support: bool,
    /// A resize asked for before the display control channel was ready.
    pending_resize: Option<(u32, u32)>,
    /// Set once the server has been asked to close, until this time.
    disconnecting: Option<Instant>,
}

impl Session {
    fn new(
        result: ConnectionResult,
        write: mpsc::UnboundedSender<Vec<u8>>,
        clipboard: ClipboardHandle,
    ) -> Self {
        let ConnectionResult {
            io_channel_id,
            user_channel_id,
            message_channel_id,
            share_id,
            static_channels,
            desktop_size,
            enable_server_pointer,
            pointer_software_rendering,
            refresh_rect_support,
            suppress_output_support,
            activation_factory,
            compression_type,
            ..
        } = result;

        let stage = ActiveStageBuilder {
            static_channels,
            user_channel_id,
            io_channel_id,
            message_channel_id,
            share_id,
            compression_type,
            enable_server_pointer,
            pointer_software_rendering,
        }
        .build();

        let mut outbox = Outbox::default();
        outbox.resize(desktop_size.width.into(), desktop_size.height.into());

        Self {
            stage,
            image: DecodedImage::new(PixelFormat::BgrA32, desktop_size.width, desktop_size.height),
            outbox,
            mapper: InputMapper::new(),
            write,
            clipboard,
            last_remote_text: None,
            activation: activation_factory,
            reactivating: None,
            refresh_rect_support,
            suppress_output_support,
            pending_resize: None,
            disconnecting: None,
        }
    }

    /// The event loop: send what is owed when there is room, prefer input over graphics, and
    /// decode what the network delivers.
    async fn drive(
        &mut self,
        mut net: mpsc::Receiver<NetEvent>,
        mut input: mpsc::Receiver<InputEvent>,
        events: &mpsc::Sender<DisplayEvent>,
    ) -> Outcome {
        let mut input_open = true;
        loop {
            tokio::select! {
                biased;
                permit = events.reserve(), if self.outbox.has_pending() => {
                    let Ok(permit) = permit else { return None };
                    let frame = Frame { data: self.image.data(), stride: self.image.stride() };
                    if let Some(event) = self.outbox.next_event(frame) {
                        permit.send(event);
                    }
                }
                () = events.closed() => return None,
                () = sleep_until(self.disconnecting.unwrap_or_else(Instant::now)),
                    if self.disconnecting.is_some() =>
                {
                    return None;
                }
                event = input.recv(), if input_open && self.reactivating.is_none() => {
                    let event = event.unwrap_or(InputEvent::Disconnect);
                    input_open = !matches!(event, InputEvent::Disconnect);
                    if let Err(error) = self.on_input(event) {
                        return Some(error);
                    }
                }
                event = net.recv() => {
                    match event {
                        Some(NetEvent::Frame { action, frame, received_at }) => {
                            match self.on_frame(action, &frame, received_at) {
                                Ok(Some(outcome)) => return outcome,
                                Ok(None) => {}
                                Err(error) => return Some(error),
                            }
                        }
                        Some(NetEvent::Failed(error)) => {
                            return if self.disconnecting.is_some() { None } else { Some(error) };
                        }
                        None => return None,
                    }
                }
            }
        }
    }

    /// Delivers everything still owed, then says the session has ended.
    async fn finish(&mut self, outcome: Outcome, events: &mpsc::Sender<DisplayEvent>) {
        let frame = Frame { data: self.image.data(), stride: self.image.stride() };
        while let Some(event) = self.outbox.next_event(frame) {
            if events.send(event).await.is_err() {
                return;
            }
        }
        let _ = events.send(DisplayEvent::Closed(outcome)).await;
    }

    fn write_frame(&self, frame: Vec<u8>) {
        if !frame.is_empty() {
            let _ = self.write.send(frame);
        }
    }

    fn on_input(&mut self, event: InputEvent) -> Result<(), String> {
        match event {
            InputEvent::Disconnect => {
                let release = self.mapper.release_all();
                self.send_input(&release)?;
                self.apply(self.stage.graceful_shutdown().map_err(session_error)?)?;
                self.disconnecting = Some(Instant::now() + SHUTDOWN_GRACE);
            },
            InputEvent::Clipboard(text) => self.set_clipboard(text)?,
            InputEvent::Resize { width, height } => {
                self.pending_resize = Some(MonitorLayoutEntry::adjust_display_size(width, height));
                self.retry_resize()?;
            },
            event => {
                let events = self.mapper.translate(&event);
                self.send_input(&events)?;
            },
        }
        Ok(())
    }

    fn send_input(
        &mut self,
        events: &[ironrdp_pdu::input::fast_path::FastPathInputEvent],
    ) -> Result<(), String> {
        let outputs =
            self.stage.process_fastpath_input(&mut self.image, events).map_err(session_error)?;
        self.apply(outputs)?;
        Ok(())
    }

    fn set_clipboard(&mut self, text: String) -> Result<(), String> {
        // The interface echoing the text the remote just sent is not a new local copy.
        if self.last_remote_text.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.clipboard.set_local_text(text);
        self.advertise_clipboard()
    }

    fn advertise_clipboard(&mut self) -> Result<(), String> {
        let formats = self.clipboard.formats();
        let Some(channel) = self.stage.get_svc_processor_mut::<CliprdrClient>() else {
            return Ok(());
        };
        let messages = channel.initiate_copy(&formats).map_err(|e| e.to_string())?;
        let frame = self.stage.process_svc_processor_messages(messages).map_err(session_error)?;
        self.write_frame(frame);
        Ok(())
    }

    /// Carries out what the clipboard channel queued during the last frame.
    fn run_clipboard_actions(&mut self) -> Result<(), String> {
        for action in self.clipboard.take_actions() {
            let messages = {
                let Some(channel) = self.stage.get_svc_processor_mut::<CliprdrClient>() else {
                    return Ok(());
                };
                match action {
                    ClipboardAction::Advertise => {
                        self.advertise_clipboard()?;
                        continue;
                    },
                    ClipboardAction::Paste(format) => channel.initiate_paste(format),
                    ClipboardAction::Reply(response) => channel.submit_format_data(response),
                    ClipboardAction::Received(text) => {
                        self.last_remote_text = Some(text.clone());
                        self.outbox.clipboard(text);
                        continue;
                    },
                }
            }
            .map_err(|e| e.to_string())?;
            let frame =
                self.stage.process_svc_processor_messages(messages).map_err(session_error)?;
            self.write_frame(frame);
        }
        Ok(())
    }

    fn retry_resize(&mut self) -> Result<(), String> {
        let Some((width, height)) = self.pending_resize else {
            return Ok(());
        };
        if let Some(frame) = self.stage.encode_resize(width, height, None, None) {
            self.pending_resize = None;
            self.write_frame(frame.map_err(session_error)?);
        }
        Ok(())
    }

    /// Handles one network frame, returning the outcome if it ended the session.
    fn on_frame(
        &mut self,
        action: Action,
        frame: &[u8],
        received_at: Option<MonotonicInstant>,
    ) -> Result<Option<Outcome>, String> {
        if self.reactivating.is_some() {
            return self.reactivate_step(frame, received_at);
        }
        let outputs = self
            .stage
            .process(&mut self.image, action, frame, received_at)
            .map_err(session_error)?;
        if let Some(outcome) = self.apply(outputs)? {
            return Ok(Some(outcome));
        }
        if self.stage.take_bitmap_recovery_request() {
            let redraw = self
                .stage
                .request_full_redraw(
                    self.image.width(),
                    self.image.height(),
                    self.refresh_rect_support,
                    self.suppress_output_support,
                )
                .map_err(session_error)?;
            for frame in redraw {
                self.write_frame(frame);
            }
        }
        self.run_clipboard_actions()?;
        self.retry_resize()?;
        Ok(None)
    }

    /// Acts on the session's output, returning the outcome if the server ended the session.
    fn apply(&mut self, outputs: Vec<ActiveStageOutput>) -> Result<Option<Outcome>, String> {
        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => self.write_frame(frame),
                ActiveStageOutput::GraphicsUpdate(rect) => {
                    self.outbox.mark_dirty(Rect::from(&rect))
                },
                ActiveStageOutput::Terminate(reason) => {
                    return Ok(Some(self.termination_outcome(reason)));
                },
                ActiveStageOutput::DeactivateAll => {
                    let sequence = self.activation.create();
                    self.reactivating = Some(sequence);
                },
                ActiveStageOutput::ServerRedirection(_) => {
                    return Err(
                        "the server redirected the session, which is not supported".to_owned()
                    );
                },
                _ => {},
            }
        }
        Ok(None)
    }

    fn termination_outcome(&self, reason: GracefulDisconnectReason) -> Outcome {
        match reason {
            _ if self.disconnecting.is_some() => None,
            GracefulDisconnectReason::UserInitiated => None,
            reason => Some(reason.description()),
        }
    }

    /// Feeds a frame to the running Deactivation-Reactivation Sequence.
    fn reactivate_step(
        &mut self,
        frame: &[u8],
        received_at: Option<MonotonicInstant>,
    ) -> Result<Option<Outcome>, String> {
        let sequence = self.reactivating.as_mut().expect("checked by the caller");
        let mut buf = WriteBuf::new();
        match sequence.next_pdu_hint().map(|hint| hint.find_size(frame)) {
            Some(Ok(Some((true, _)))) => {
                let written = sequence
                    .step(frame, received_at, &mut buf)
                    .map_err(|e| e.report().to_string())?;
                if written.size().is_some() {
                    let _ = self.write.send(buf.filled().to_vec());
                }
            },
            // Graphics still in flight when the sequence began are stale.
            Some(_) | None => return Ok(None),
        }
        // Some steps produce their PDU with no input from the server.
        while !sequence.state().is_terminal() && sequence.next_pdu_hint().is_none() {
            buf.clear();
            let written = sequence.step_no_input(&mut buf).map_err(|e| e.report().to_string())?;
            if written.size().is_some() {
                let _ = self.write.send(buf.filled().to_vec());
            }
        }
        if !sequence.state().is_terminal() {
            return Ok(None);
        }
        let state = sequence.connection_activation_state();
        let (io, user) = (sequence.io_channel_id(), sequence.user_channel_id());
        self.reactivating = None;
        let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            static_channel_chunk_size,
            enable_server_pointer,
            pointer_software_rendering,
            refresh_rect_support,
            suppress_output_support,
            ..
        } = state
        else {
            return Err("the server redirected the session, which is not supported".to_owned());
        };
        if !self.stage.reactivate(
            io,
            user,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
            static_channel_chunk_size,
        ) {
            return Err("the server negotiated an invalid channel chunk size".to_owned());
        }
        self.refresh_rect_support = refresh_rect_support;
        self.suppress_output_support = suppress_output_support;
        self.image =
            DecodedImage::new(PixelFormat::BgrA32, desktop_size.width, desktop_size.height);
        self.outbox.resize(desktop_size.width.into(), desktop_size.height.into());
        Ok(None)
    }
}

fn session_error(error: ironrdp_session::SessionError) -> String {
    error.report().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(quality: Quality) -> Config {
        let mut config = RdpConfig::new("host", "user", "secret", (800, 600), [0; 32]);
        config.quality = quality;
        connector_config(&config).unwrap()
    }

    #[test]
    fn balanced_keeps_the_library_defaults() {
        let config = config(Quality::default());
        assert_eq!(config.bitmap.unwrap().color_depth, 32);
        assert_eq!(config.performance_flags, PerformanceFlags::default());
        assert_eq!(config.connection_type, ConnectionType::Lan);
    }

    #[test]
    fn best_turns_the_desktop_effects_on() {
        let flags = config(Quality::Best).performance_flags;
        assert!(!flags.intersects(
            PerformanceFlags::DISABLE_WALLPAPER
                | PerformanceFlags::DISABLE_FULLWINDOWDRAG
                | PerformanceFlags::DISABLE_MENUANIMATIONS
                | PerformanceFlags::DISABLE_THEMING
        ));
        assert!(flags.contains(PerformanceFlags::ENABLE_DESKTOP_COMPOSITION));
    }

    #[test]
    fn fast_asks_for_fewer_colors_and_no_effects() {
        let config = config(Quality::Fast);
        assert_eq!(config.bitmap.unwrap().color_depth, 16);
        assert!(config.performance_flags.contains(PerformanceFlags::DISABLE_WALLPAPER));
        assert!(!config.performance_flags.contains(PerformanceFlags::ENABLE_FONT_SMOOTHING));
    }
}
