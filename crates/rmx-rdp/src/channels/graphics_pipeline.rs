//! The graphics pipeline, the dynamic channel that carries the picture in place of the older
//! bitmap updates.

use ironrdp_connector::Config;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};

/// What the graphics pipeline is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphicsPipelineOptions {
    /// Offer the pipeline, which always carries 32-bit color. Without it the server sends the
    /// older bitmap updates.
    pub enabled: bool,
}

impl Default for GraphicsPipelineOptions {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Tells the server, in the connection's capabilities, whether the pipeline is offered.
pub(crate) fn configure(options: &GraphicsPipelineOptions, config: &mut Config) {
    // The graphics pipeline always carries 32-bit color, so a server whose listener caps the
    // older bitmap updates at 16-bit (the Windows Server default) still sends full color.
    config.support_dyn_vc_gfx_protocol = options.enabled;
}

pub(crate) fn register(options: &GraphicsPipelineOptions, dynamic: &mut DrdynvcClient) {
    if options.enabled {
        dynamic.attach_dynamic_channel(GraphicsPipelineClient::new(Box::new(Pipeline), None));
    }
}

/// The pipeline's handler. [`ActiveStage`](ironrdp_session::ActiveStage) composites what the
/// pipeline decodes into the session's image, so nothing here needs to act. Without an H.264
/// decoder the pipeline offers the server only its other codecs: RemoteFX Progressive,
/// ClearCodec and planar.
struct Pipeline;

impl GraphicsPipelineHandler for Pipeline {}
