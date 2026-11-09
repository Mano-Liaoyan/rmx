//! The widgets. Each takes a plain view-model and returns what the user asked for.

pub mod context_menu;
pub mod controls;
pub mod files;
pub mod footer;
pub mod host_key;
pub mod new_connection;
pub mod overlay;
pub mod prompt;
pub mod quick_open;
pub mod settings;
pub mod snippets;
pub mod tabs;
pub mod title_bar;
pub mod tree;
pub mod vault;

pub use crate::float::float_frame;
pub use context_menu::{ContextMenu, MenuItem, context_menu};
pub use controls::{
    ButtonKind, InputOptions, button, button_full, icon_button, segmented, text_input,
};
pub use files::{
    CONFLICT_POLICIES, FileColumnView, FileKind, FileRow, FilesKey, FilesState, FilesView, TransferRow,
    TransferRowState, file_row_point, files_pane,
};
pub use footer::{PaneFooterView, PaneStatus, pane_footer};
pub use host_key::{HostKeyStatus, HostKeyView, host_key_body, host_key_dialog};
pub use new_connection::{
    AuthKind, ConnectionDraft, NewConnectionState, RdpSecurity, ScaleChoice, SerialPortChoice, new_connection_body,
    new_connection_dialog,
};
pub use overlay::{PaneOverlay, notice_rect, overlay_rect, pane_notice, pane_overlay};
pub use prompt::{PromptField, PromptState, prompt_dialog};
pub use quick_open::{QuickItem, QuickOpenState, quick_open, quick_open_body};
pub use settings::{SettingsDraft, SettingsState, SettingsView, SyncDraft, SyncKind, settings_dialog};
pub use snippets::{SnippetRow, SnippetsState, snippets_dialog};
pub use tabs::{TabStripState, TabView, tab_strip};
pub use title_bar::{TitleBarView, title_bar};
pub use tree::{
    NodeKind, SIDEBAR_STATUS_HEIGHT, SidebarStatus, TreeModel, TreeNode, TreeState, connection_tree,
    sidebar, sidebar_status,
};
pub use vault::{VaultSetupState, VaultSetupView, VaultState, VaultView, vault_setup, vault_unlock};
