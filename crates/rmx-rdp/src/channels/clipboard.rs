//! Plain text clipboard over the `cliprdr` static channel.
//!
//! The channel calls back into [`TextClipboard`] with no way to answer directly, so the
//! callbacks queue [`Action`]s for [`State`], which acts on them after each frame.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
};
use ironrdp_cliprdr::{Cliprdr, CliprdrClient};
use ironrdp_connector::ClientConnector;
use ironrdp_core::impl_as_any;

use super::Link;
use crate::session::session_error;

/// What the clipboard channel is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardOptions {
    /// Share the clipboard's text with the server.
    pub enabled: bool,
}

impl Default for ClipboardOptions {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The session's side of the channel, from before the handshake to the end.
#[derive(Debug, Default)]
pub(crate) struct State {
    handle: ClipboardHandle,
    /// The text the remote last sent, which the interface echoes back.
    last_remote_text: Option<String>,
}

impl State {
    pub fn register(&self, options: &ClipboardOptions, connector: &mut ClientConnector) {
        // Without the channel nothing crosses the clipboard either way.
        if options.enabled {
            connector.attach_static_channel(Cliprdr::<ironrdp_cliprdr::Client>::new(Box::new(
                self.handle.backend(),
            )));
        }
    }

    /// The local clipboard now holds `text`.
    pub fn set_local(&mut self, text: String, link: &mut Link<'_>) -> Result<(), String> {
        // The interface echoing the text the remote just sent is not a new local copy.
        if self.last_remote_text.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.handle.set_local_text(text);
        self.advertise(link)
    }

    fn advertise(&self, link: &mut Link<'_>) -> Result<(), String> {
        let formats = self.handle.formats();
        let Some(channel) = link.stage.get_svc_processor_mut::<CliprdrClient>() else {
            return Ok(());
        };
        let messages = channel.initiate_copy(&formats).map_err(|e| e.to_string())?;
        let frame = link.stage.process_svc_processor_messages(messages).map_err(session_error)?;
        link.send(frame);
        Ok(())
    }

    /// Carries out what the channel queued during the last frame.
    pub fn run_actions(&mut self, link: &mut Link<'_>) -> Result<(), String> {
        for action in self.handle.take_actions() {
            let messages = {
                let Some(channel) = link.stage.get_svc_processor_mut::<CliprdrClient>() else {
                    return Ok(());
                };
                match action {
                    Action::Advertise => {
                        self.advertise(link)?;
                        continue;
                    },
                    Action::Paste(format) => channel.initiate_paste(format),
                    Action::Reply(response) => channel.submit_format_data(response),
                    Action::Received(text) => {
                        self.last_remote_text = Some(text.clone());
                        link.outbox.clipboard(text);
                        continue;
                    },
                }
            }
            .map_err(|e| e.to_string())?;
            let frame =
                link.stage.process_svc_processor_messages(messages).map_err(session_error)?;
            link.send(frame);
        }
        Ok(())
    }
}

/// What the session must do on the channel's behalf.
#[derive(Debug)]
pub(crate) enum Action {
    /// Tell the server which formats the local clipboard holds.
    Advertise,
    /// Fetch the remote clipboard in this format.
    Paste(ClipboardFormatId),
    /// Answer the server's request for local clipboard data.
    Reply(OwnedFormatDataResponse),
    /// The remote clipboard holds this text.
    Received(String),
}

#[derive(Debug, Default)]
struct Shared {
    actions: VecDeque<Action>,
    local_text: Option<String>,
    requested: Option<ClipboardFormatId>,
}

/// The session's end of the clipboard.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClipboardHandle(Arc<Mutex<Shared>>);

impl ClipboardHandle {
    pub fn backend(&self) -> TextClipboard {
        TextClipboard { shared: self.clone() }
    }

    pub fn take_actions(&self) -> VecDeque<Action> {
        std::mem::take(&mut self.lock().actions)
    }

    pub fn set_local_text(&self, text: String) {
        self.lock().local_text = Some(text);
    }

    pub fn formats(&self) -> Vec<ClipboardFormat> {
        self.lock()
            .local_text
            .as_ref()
            .map(|_| vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)])
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.0.lock().expect("clipboard state is never poisoned")
    }
}

/// A [`CliprdrBackend`] handling text only.
#[derive(Debug)]
pub(crate) struct TextClipboard {
    shared: ClipboardHandle,
}

impl_as_any!(TextClipboard);

impl CliprdrBackend for TextClipboard {
    fn temporary_directory(&self) -> &str {
        ""
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        self.shared.lock().actions.push_back(Action::Advertise);
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let wanted = |id| available_formats.iter().any(|format| format.id == id);
        let format = [ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_TEXT]
            .into_iter()
            .find(|&id| wanted(id));
        if let Some(format) = format {
            let mut shared = self.shared.lock();
            shared.requested = Some(format);
            shared.actions.push_back(Action::Paste(format));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let mut shared = self.shared.lock();
        let response = match (&shared.local_text, request.format) {
            (Some(text), ClipboardFormatId::CF_UNICODETEXT) => {
                FormatDataResponse::new_unicode_string(&to_crlf(text))
            },
            (Some(text), ClipboardFormatId::CF_TEXT) => {
                FormatDataResponse::new_string(&to_crlf(text))
            },
            _ => FormatDataResponse::new_error(),
        };
        shared.actions.push_back(Action::Reply(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let mut shared = self.shared.lock();
        let format = shared.requested.take();
        if response.is_error() {
            return;
        }
        let text = match format {
            Some(ClipboardFormatId::CF_UNICODETEXT) => decode_utf16(response.data()),
            Some(ClipboardFormatId::CF_TEXT) => decode_ansi(response.data()),
            _ => return,
        };
        shared.actions.push_back(Action::Received(text.replace("\r\n", "\n")));
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// Decodes little-endian UTF-16 up to its terminator.
fn decode_utf16(data: &[u8]) -> String {
    let units = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0)
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

/// Decodes `CF_TEXT`, whose code page is unknown, so bytes that are not UTF-8 are replaced.
fn decode_ansi(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// Windows text uses CRLF. Existing CRLF pairs are left alone.
fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_stops_at_the_terminator() {
        let bytes: Vec<u8> = "hé\0xx".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_utf16(&bytes), "hé");
    }

    #[test]
    fn line_endings_convert_without_doubling() {
        assert_eq!(to_crlf("a\nb\r\nc"), "a\r\nb\r\nc");
    }

    #[test]
    fn data_requests_answer_with_the_local_text() {
        let handle = ClipboardHandle::default();
        handle.set_local_text("hi".to_owned());
        let mut backend = handle.backend();
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        let actions = handle.take_actions();
        assert!(matches!(actions.front(), Some(Action::Reply(r)) if !r.is_error()));
    }
}
