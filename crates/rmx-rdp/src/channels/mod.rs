//! The virtual channels a session opens beside the picture and input.
//!
//! Each channel is a module of its own, owning its options, how it registers on the connector,
//! and what it does while the session runs. Adding one takes a module, a field in [`Channels`],
//! and its lines in [`Active`]; the session itself is left alone.

mod clipboard;
mod display_control;
mod graphics_pipeline;

use ironrdp_connector::{ClientConnector, Config};
use ironrdp_dvc::DrdynvcClient;
use ironrdp_session::{ActiveStage, ActiveStageOutput};
use rmx_display::InputEvent;
use tokio::sync::mpsc;

pub use clipboard::ClipboardOptions;
pub use display_control::DisplayControlOptions;
pub use graphics_pipeline::GraphicsPipelineOptions;

use crate::RdpConfig;
use crate::outbox::Outbox;

/// What each channel is asked to do, one field per channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channels {
    pub clipboard: ClipboardOptions,
    pub display_control: DisplayControlOptions,
    pub graphics_pipeline: GraphicsPipelineOptions,
}

impl Channels {
    /// Puts what the channels need into the connection's own settings.
    pub(crate) fn configure(&self, config: &mut Config) {
        graphics_pipeline::configure(&self.graphics_pipeline, config);
    }
}

/// What a channel works with while the session runs.
pub(crate) struct Link<'a> {
    pub stage: &'a mut ActiveStage,
    pub outbox: &'a mut Outbox,
    pub write: &'a mpsc::UnboundedSender<Vec<u8>>,
}

impl Link<'_> {
    /// Queues a frame for the server.
    pub fn send(&self, frame: Vec<u8>) {
        if !frame.is_empty() {
            let _ = self.write.send(frame);
        }
    }
}

/// The session's side of every channel, made before the handshake so the channels' processors
/// can share state with it.
#[derive(Debug)]
pub(crate) struct Active {
    clipboard: clipboard::State,
    display_control: display_control::State,
}

impl Active {
    pub fn new(config: &RdpConfig) -> Self {
        let scale_factor =
            (config.scale_factor != 100).then(|| config.scale_factor.clamp(100, 500));
        Self {
            clipboard: clipboard::State::default(),
            display_control: display_control::State::new(scale_factor),
        }
    }

    /// Registers the channels `channels` turns on with `connector`.
    pub fn register(&self, channels: &Channels, connector: &mut ClientConnector) {
        let mut dynamic = DrdynvcClient::new();
        self.display_control.register(&channels.display_control, &mut dynamic);
        graphics_pipeline::register(&channels.graphics_pipeline, &mut dynamic);
        connector.attach_static_channel(dynamic);
        self.clipboard.register(&channels.clipboard, connector);
    }

    /// Handles the input meant for a channel, handing back what is not.
    pub fn on_input(
        &mut self,
        event: InputEvent,
        link: &mut Link<'_>,
    ) -> Result<Option<InputEvent>, String> {
        match event {
            InputEvent::Clipboard(text) => self.clipboard.set_local(text, link)?,
            InputEvent::Resize { width, height } => {
                self.display_control.resize(width, height, link)?;
            },
            event => return Ok(Some(event)),
        }
        Ok(None)
    }

    /// Handles an output of the session that the session does not act on itself. No channel
    /// needs one yet.
    pub fn on_output(&mut self, _output: ActiveStageOutput, _link: &mut Link<'_>) {}

    /// Does what the channels owe after each frame from the server.
    pub fn after_frame(&mut self, link: &mut Link<'_>) -> Result<(), String> {
        self.clipboard.run_actions(link)?;
        self.display_control.retry(link)
    }
}
