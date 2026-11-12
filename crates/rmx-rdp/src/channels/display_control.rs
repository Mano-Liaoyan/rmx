//! Display control, the dynamic channel that resizes the remote desktop to fit the pane.

use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp_dvc::DrdynvcClient;

use super::Link;
use crate::session::session_error;

/// What the display control channel is asked to do. The channel is always offered, and has no
/// options yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayControlOptions {}

/// The session's side of the channel.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// A resize asked for before the channel was ready.
    pending: Option<(u32, u32)>,
    /// The interface scale the desktop keeps across resizes, if not the server's own.
    scale_factor: Option<u32>,
}

impl State {
    pub fn new(scale_factor: Option<u32>) -> Self {
        Self { pending: None, scale_factor }
    }

    pub fn register(&self, _options: &DisplayControlOptions, dynamic: &mut DrdynvcClient) {
        dynamic.attach_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    }

    /// Asks the server for a desktop of this size, once the channel is ready.
    pub fn resize(&mut self, width: u32, height: u32, link: &mut Link<'_>) -> Result<(), String> {
        self.pending = Some(MonitorLayoutEntry::adjust_display_size(width, height));
        self.retry(link)
    }

    /// Sends a resize that had to wait, if the channel is now ready.
    pub fn retry(&mut self, link: &mut Link<'_>) -> Result<(), String> {
        let Some((width, height)) = self.pending else {
            return Ok(());
        };
        if let Some(frame) = link.stage.encode_resize(width, height, self.scale_factor, None) {
            self.pending = None;
            link.send(frame.map_err(session_error)?);
        }
        Ok(())
    }
}
