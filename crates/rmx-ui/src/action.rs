//! What the interface asks the app to do.

use std::fmt;
use std::sync::Arc;

use egui::Pos2;
use zeroize::Zeroize;

use crate::icons;
use crate::widgets::new_connection::ConnectionDraft;
use crate::widgets::settings::{SettingsDraft, SyncDraft};

/// Text the user typed that must not be printed: wiped on drop, redacted in `Debug`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Sensitive(String);

impl Sensitive {
    /// Takes `text`.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text. Do not store or log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Hand the text over, leaving this empty.
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.0)
    }

    /// Whether nothing was typed.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Sensitive {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Sensitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// A tab, in the app's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

/// A node of the connection tree, in the app's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// A pane, in the app's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// One of the two columns of a file pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileSide {
    Local,
    Remote,
}

impl FileSide {
    /// Which of the pair this is, as an index.
    pub const fn index(self) -> usize {
        match self {
            FileSide::Local => 0,
            FileSide::Remote => 1,
        }
    }

    /// The column across from this one.
    pub const fn other(self) -> Self {
        match self {
            FileSide::Local => FileSide::Remote,
            FileSide::Remote => FileSide::Local,
        }
    }
}

/// What a file pane asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum FilesAction {
    /// Go into the directory or link `name` of the column.
    Open { side: FileSide, name: Arc<str> },
    Up { side: FileSide },
    /// Go to the directory the user typed.
    Goto { side: FileSide, path: String },
    Refresh { side: FileSide },
    /// Copy `names` of the column to the other one.
    Transfer { from: FileSide, names: Vec<Arc<str>> },
    Cancel(u64),
    Retry(u64),
    ClearFinished,
    /// Make the folder `name` in the column.
    NewFolder { side: FileSide, name: String },
    /// Call `from`, in the column, `to`.
    Rename { side: FileSide, from: Arc<str>, to: String },
    /// Remove `names` from the column.
    Delete { side: FileSide, names: Vec<Arc<str>> },
    /// Pick the policy at this index of [`CONFLICT_POLICIES`](crate::widgets::CONFLICT_POLICIES).
    Policy(usize),
    /// Compare the file `local` of the first column with the file `remote` of the second.
    Compare { local: Arc<str>, remote: Arc<str> },
    /// Write this over the open file, unless it has changed since it was read.
    Save { text: String },
    /// Write what was being saved over the open file, even though it has changed.
    SaveAnyway,
    /// Show how this differs from the open file as it was read.
    DiffEdits { text: String },
    /// Go back from what is shown instead of the columns.
    CloseBody,
    /// Color this, what the open file now says.
    Highlight { text: String },
}

/// The kinds of connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Ssh,
    Sftp,
    Ftp,
    Vnc,
    Rdp,
    Telnet,
    Serial,
}

impl Protocol {
    /// Every protocol, in the order the new-connection dialog offers them.
    pub const ALL: [Protocol; 7] = [
        Protocol::Ssh,
        Protocol::Sftp,
        Protocol::Ftp,
        Protocol::Vnc,
        Protocol::Rdp,
        Protocol::Telnet,
        Protocol::Serial,
    ];

    /// The display name.
    pub const fn label(self) -> &'static str {
        match self {
            Protocol::Ssh => "SSH",
            Protocol::Sftp => "SFTP",
            Protocol::Ftp => "FTP",
            Protocol::Vnc => "VNC",
            Protocol::Rdp => "RDP",
            Protocol::Telnet => "Telnet",
            Protocol::Serial => "Serial",
        }
    }

    /// The icon.
    pub const fn icon(self) -> char {
        match self {
            Protocol::Ssh => icons::SQUARE_TERMINAL,
            Protocol::Sftp => icons::FOLDER_SYNC,
            Protocol::Ftp => icons::HARD_DRIVE_UPLOAD,
            Protocol::Vnc => icons::MONITOR,
            Protocol::Rdp => icons::MONITOR_COG,
            Protocol::Telnet => icons::TERMINAL,
            Protocol::Serial => icons::CABLE,
        }
    }

    /// The usual port, or `None` for a local device.
    pub const fn default_port(self) -> Option<u16> {
        match self {
            Protocol::Ssh | Protocol::Sftp => Some(22),
            Protocol::Ftp => Some(21),
            Protocol::Vnc => Some(5900),
            Protocol::Rdp => Some(3389),
            Protocol::Telnet => Some(23),
            Protocol::Serial => None,
        }
    }
}

/// Something the interface asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    WindowMinimize,
    WindowToggleMaximize,
    WindowClose,
    /// Start an interactive window move. After handing it to the compositor call
    /// [`Chrome::cancel_pointer`](crate::Chrome::cancel_pointer): egui never sees the release.
    WindowDrag,
    ToggleSidebar,
    TabActivate(TabId),
    TabClose(TabId),
    TabNew,
    /// Tab `id` was dropped so that it now belongs at index `to`.
    TabReorder {
        id: TabId,
        to: usize,
    },
    TabContextMenu {
        id: TabId,
        pos: Pos2,
    },
    /// Edit the name of tab `id` where it is shown.
    TabStartRename(TabId),
    /// Call tab `id` this, or again what its focused pane is called.
    TabRename {
        id: TabId,
        name: Option<String>,
    },
    /// Make each pane of tab `id` a tab of its own.
    TabUngroup(TabId),
    /// Tab `id` was dragged off the strip: follow it until the button is up.
    TabDragOut(TabId),
    /// Give tab `id` a window of its own.
    TabDetach(TabId),
    /// Move tab `id` to this other window.
    TabToWindow(TabId, u64),
    TreeSelect(NodeId),
    TreeOpen(NodeId),
    /// Connect to this node in a new pane beside the focused one, stacked when `down`.
    TreeOpenSplit {
        id: NodeId,
        down: bool,
    },
    /// Browse the files of this node.
    TreeOpenFiles(NodeId),
    /// Show a session to this node.
    TreeShow(NodeId),
    TreeReconnect(NodeId),
    TreeDisconnect(NodeId),
    TreeFolderToggled {
        id: NodeId,
        expanded: bool,
    },
    TreeContextMenu {
        id: NodeId,
        pos: Pos2,
    },
    /// Put this text on the clipboard.
    CopyText(String),
    /// Connect to what was typed, as `user@host:port`.
    QuickConnect(String),
    /// The sidebar is being dragged to this width, in points.
    SidebarResize(f32),
    /// A panel was let go at a new size: remember it.
    PanelsChanged,
    /// Show the new-connection dialog.
    NewConnection,
    ConnectionCancel,
    ConnectionSubmit(Box<ConnectionDraft>),
    QuickOpenPick(u64),
    QuickOpenDismiss,
    /// Unlock with this password and optional key file, off the UI thread.
    VaultUnlock {
        password: Sensitive,
        key_file: String,
    },
    /// Create the first vault, off the UI thread.
    VaultCreate {
        password: Sensitive,
        key_file: String,
        /// The user has read that without a password nothing is encrypted, and agreed.
        plain_ok: bool,
    },
    /// Make a working copy of a vault that another device has synced.
    VaultJoin {
        password: Sensitive,
        key_file: String,
        sync: Box<SyncDraft>,
        /// As for [`VaultCreate`](Self::VaultCreate).
        plain_ok: bool,
    },
    /// Lock now.
    VaultLock,
    /// Show the vaults there are to switch to.
    VaultMenu(Pos2),
    /// Close this vault and go to the one at this path.
    VaultSwitch(String),
    /// Ask for the path of a vault: an existing one, or else one to make.
    VaultAskPath {
        create: bool,
    },
    /// Ask where this vault's file should go.
    VaultAskMove,
    /// Ask for the current master password and a new one.
    VaultAskPassword,
    HostKeyReject,
    HostKeyAcceptOnce,
    HostKeyAcceptAlways,
    /// Connect to, or edit, or copy, or forget a node of the tree.
    TreeEdit(NodeId),
    TreeDuplicate(NodeId),
    TreeDelete(NodeId),
    /// Add a folder, inside this one if any.
    TreeNewFolder(Option<NodeId>),
    TreeRenameFolder(NodeId),
    /// The fields of the prompt dialog as answered.
    PromptSubmit(Vec<Sensitive>),
    PromptCancel,
    /// Give up on connecting this pane.
    PaneCancel(PaneId),
    /// End this pane's session, keeping the pane.
    PaneDisconnect(PaneId),
    PaneReconnect(PaneId),
    PaneClose(PaneId),
    /// Stop showing that output was lost.
    PaneDismissNotice(PaneId),
    OpenSettings,
    /// Switch between the light and the dark theme.
    ToggleTheme,
    SettingsSave(Box<SettingsDraft>),
    SettingsCancel,
    SyncNow,
    /// Save a snippet, as new when it has no number yet.
    SnippetSave {
        id: Option<u64>,
        name: String,
        body: String,
    },
    SnippetDelete(u64),
    /// Find out which serial ports this machine has, for the connection form.
    ListSerialPorts,
    /// Send a saved snippet to the focused terminal.
    SnippetSend(u64),
    /// Read the user's `~/.ssh/config` into the vault.
    ImportSshConfig,
    PaneSplitRight(PaneId),
    PaneSplitDown(PaneId),
    /// The user took hold of a pane by its bar: follow the primary button until it is up.
    PaneDragStart(PaneId),
    PaneZoom(PaneId),
    /// Open the same session again, in a new tab.
    PaneDuplicate(PaneId),
    /// Make this pane a tab of its own.
    PaneToTab(PaneId),
    /// Give this pane a window of its own.
    PaneToWindow(PaneId),
    PaneCopy(PaneId),
    PanePaste(PaneId),
    PaneMenu {
        id: PaneId,
        pos: Pos2,
    },
    /// A click in a pane that keeps its pointer for itself.
    PaneFocus(PaneId),
    /// Send Ctrl+Alt+Del to this pane's remote desktop.
    PaneCtrlAltDel(PaneId),
    /// Switch this pane between fitting its desktop and showing it 1:1.
    PaneToggleScale(PaneId),
    /// Browse the files of this SSH pane at its working directory.
    PaneOpenFiles(PaneId),
    Files(PaneId, FilesAction),
    /// The menu of a column of a file pane, of the row `name` or else its empty space.
    FilesMenu {
        pane: PaneId,
        side: FileSide,
        name: Option<Arc<str>>,
        pos: Pos2,
    },
    /// The menu of what to do about a file already at the destination.
    FilesPolicyMenu {
        pane: PaneId,
        pos: Pos2,
    },
    /// Ask what to call a new folder.
    FilesAskNewFolder(PaneId, FileSide),
    /// Ask what to call a file instead.
    FilesAskRename(PaneId, FileSide, Arc<str>),
    /// Ask whether to remove these.
    FilesAskDelete(PaneId, FileSide, Vec<Arc<str>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::settings::SyncDraft;

    #[test]
    fn typed_secrets_do_not_appear_in_debug_output() {
        let action = UiAction::VaultUnlock { password: Sensitive::new("hunter2"), key_file: String::new() };
        assert!(!format!("{action:?}").contains("hunter2"));

        let mut draft = ConnectionDraft::new(Protocol::Ssh);
        draft.password = "hunter2".to_owned();
        assert!(!format!("{:?}", UiAction::ConnectionSubmit(Box::new(draft))).contains("hunter2"));

        let sync = SyncDraft { secret: "hunter2".to_owned(), ..SyncDraft::default() };
        assert!(!format!("{sync:?}").contains("hunter2"));
    }

    #[test]
    fn taking_a_secret_leaves_it_empty() {
        let mut secret = Sensitive::new("text");
        assert_eq!(secret.take(), "text");
        assert!(secret.is_empty());
    }
}
