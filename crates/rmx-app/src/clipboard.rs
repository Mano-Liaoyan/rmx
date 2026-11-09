//! The system clipboards.

use copypasta::ClipboardProvider;
use copypasta::nop_clipboard::NopClipboardContext;
use copypasta::wayland_clipboard;
use copypasta::x11_clipboard::{Clipboard as ClipboardSelection, Primary, X11ClipboardContext};
use rmx_terminal::ClipboardType;
use winit::raw_window_handle::RawDisplayHandle;

pub struct Clipboard {
    clipboard: Box<dyn ClipboardProvider>,
    selection: Box<dyn ClipboardProvider>,
}

impl Clipboard {
    pub fn new(display: RawDisplayHandle) -> Self {
        if let RawDisplayHandle::Wayland(display) = display {
            // Safety: the display outlives the clipboards, which are dropped with the window.
            let (selection, clipboard) = unsafe {
                wayland_clipboard::create_clipboards_from_external(display.display.as_ptr())
            };
            return Self { clipboard: Box::new(clipboard), selection: Box::new(selection) };
        }

        match (
            X11ClipboardContext::<ClipboardSelection>::new(),
            X11ClipboardContext::<Primary>::new(),
        ) {
            (Ok(clipboard), Ok(selection)) => {
                Self { clipboard: Box::new(clipboard), selection: Box::new(selection) }
            },
            _ => Self {
                clipboard: Box::new(NopClipboardContext),
                selection: Box::new(NopClipboardContext),
            },
        }
    }

    fn provider(&mut self, ty: ClipboardType) -> &mut dyn ClipboardProvider {
        match ty {
            ClipboardType::Clipboard => self.clipboard.as_mut(),
            ClipboardType::Selection => self.selection.as_mut(),
        }
    }

    pub fn store(&mut self, ty: ClipboardType, text: String) {
        if let Err(err) = self.provider(ty).set_contents(text) {
            log::warn!("Could not write the clipboard: {err}");
        }
    }

    pub fn load(&mut self, ty: ClipboardType) -> String {
        self.provider(ty).get_contents().unwrap_or_default()
    }
}
