//! The widgets. Each takes a plain view-model and returns what the user asked for.

pub mod context_menu;
pub mod controls;
pub mod empty;
pub mod files;
pub mod host_key;
pub mod import_preview;
pub mod new_connection;
pub mod overlay;
pub mod pane_bar;
pub mod panels;
pub mod prompt;
pub mod quick_open;
pub mod rdp_advanced;
pub mod settings;
pub mod snippets;
pub mod tabs;
mod text_view;
pub mod title_bar;
pub mod tree;
pub mod vault;

pub use crate::float::float_frame;
pub use context_menu::{ContextMenu, MenuItem, context_menu};
pub use controls::{
    ButtonKind, InputOptions, button, button_full, checkbox, icon_button, segmented, text_input,
};
pub use empty::empty_state;
pub use files::{
    CONFLICT_POLICIES, FileColumnView, FileKind, FileRow, FilesKey, FilesState, FilesView,
    TransferRow, TransferRowState, file_row_point, files_edges, files_fold_point, files_pane,
};
pub use host_key::{HostKeyStatus, HostKeyView, host_key_body, host_key_dialog};
pub use import_preview::{ImportPreviewState, ImportProblem, ImportRow, import_preview_dialog};
pub use new_connection::{
    AuthKind, ConnectionDraft, InheritedLogin, NewConnectionState, RdpQuality, RdpSecurity,
    ScaleChoice, SerialPortChoice, SessionLogging, VncColors, is_rustdesk_address,
    new_connection_body, new_connection_dialog,
};
pub use overlay::{PaneOverlay, notice_rect, overlay_rect, pane_notice, pane_overlay};
pub use pane_bar::{PaneBarView, PaneStatus, pane_bar};
pub use panels::{Handle, Panels, Reveal, paint_pill, resize_handle};
pub use prompt::{PromptField, PromptState, prompt_dialog};
pub use quick_open::{QuickItem, QuickOpenState, quick_open, quick_open_body};
pub use rdp_advanced::{
    RDP_SCALES, RdpAdvanced, RdpColorDepth, RdpCompression, RdpConnection, RdpEffects, RdpPicture,
    parse_keyboard_layout,
};
pub use settings::{
    SettingsDraft, SettingsState, SettingsView, SyncDraft, SyncKind, settings_dialog,
};
pub use snippets::{SnippetRow, SnippetsState, snippets_dialog};
pub use tabs::{TabStripState, TabView, tab_strip};
pub use text_view::{
    DiffLine, DiffRow, DiffView, FilesBody, Highlight, HighlightLine, TextView, TokenRun,
};
pub use title_bar::{TitleBarView, title_bar};
pub use tree::{
    NodeKind, SIDEBAR_STATUS_HEIGHT, SessionDot, SidebarStatus, TreeModel, TreeNode, TreeState,
    connection_tree, sidebar, sidebar_status,
};
pub use vault::{
    VaultSetupState, VaultSetupView, VaultState, VaultView, vault_setup, vault_unlock,
};
