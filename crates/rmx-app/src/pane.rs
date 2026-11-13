//! What a pane is: its content, and the connection behind it.

use rmx_render::SizeInfo;
use rmx_terminal::{Terminal, Theme};
use rmx_ui::egui;
use rmx_ui::widgets::{
    PaneOverlay, PaneStatus, ScaleChoice, SessionLogging, notice_rect, overlay_rect,
};
use tokio::sync::oneshot;

use rmx_layout::PaneId;

use crate::connect::Goal;
use crate::files::FilesPane;
use crate::remote::Display;
use crate::session_log::SessionLog;

/// What fills a pane. Every kind of pane is one of these, and the layout, the workspace, focus and
/// the bar treat them all the same. Whatever the kind, setting up its connection is [`Link`]'s
/// business, so a new kind only adds a variant here and arms to the methods below.
pub enum PaneContent {
    /// Nothing to show yet, while the connection or process behind it is set up.
    Starting,
    Terminal(Box<Terminal>),
    /// A remote desktop, on a texture.
    Display(Box<Display>),
    /// Two file systems side by side, drawn entirely by the interface.
    Files(Box<FilesPane>),
}

impl PaneContent {
    pub fn terminal(&self) -> Option<&Terminal> {
        match self {
            Self::Terminal(terminal) => Some(terminal),
            Self::Starting | Self::Display(_) | Self::Files(_) => None,
        }
    }

    pub fn terminal_mut(&mut self) -> Option<&mut Terminal> {
        match self {
            Self::Terminal(terminal) => Some(terminal),
            Self::Starting | Self::Display(_) | Self::Files(_) => None,
        }
    }

    pub fn display(&self) -> Option<&Display> {
        match self {
            Self::Display(display) => Some(display),
            _ => None,
        }
    }

    pub fn display_mut(&mut self) -> Option<&mut Display> {
        match self {
            Self::Display(display) => Some(display),
            _ => None,
        }
    }

    pub fn files(&self) -> Option<&FilesPane> {
        match self {
            Self::Files(files) => Some(files),
            _ => None,
        }
    }

    pub fn files_mut(&mut self) -> Option<&mut FilesPane> {
        match self {
            Self::Files(files) => Some(files),
            _ => None,
        }
    }

    /// Whether the whole content belongs to the interface, pointer included.
    pub fn is_interface(&self) -> bool {
        matches!(self, Self::Files(_))
    }

    /// Fit the content to the pane.
    pub fn resize(&mut self, size: SizeInfo) {
        match self {
            Self::Terminal(terminal) => terminal.resize(size),
            Self::Starting | Self::Display(_) | Self::Files(_) => (),
        }
    }

    /// Tell the content whether it has the keyboard. Returns whether that changes what it shows.
    pub fn set_focused(&mut self, focused: bool) -> bool {
        match self {
            Self::Terminal(terminal) => terminal.set_focused(focused),
            Self::Display(display) => {
                if !focused {
                    display.release_all();
                }
                false
            },
            Self::Starting | Self::Files(_) => false,
        }
    }

    pub fn set_theme(&mut self, theme: Theme) {
        match self {
            Self::Terminal(terminal) => terminal.set_theme(theme),
            Self::Starting | Self::Display(_) | Self::Files(_) => (),
        }
    }

    /// What the bar's tooltip says about the content.
    pub fn size_fact(&self) -> Option<String> {
        match self {
            Self::Terminal(terminal) => {
                let size = terminal.size();
                Some(format!("{} x {}", size.columns_count(), size.lines_count()))
            },
            Self::Display(display) => {
                let scale = match display.scale {
                    ScaleChoice::Fit => "fit",
                    ScaleChoice::Native => "1:1, Alt+wheel pans",
                };
                display.desktop().map(|[width, height]| format!("{width} x {height}, {scale}"))
            },
            Self::Starting | Self::Files(_) => None,
        }
    }

    /// Whether the content shows anything to read.
    pub fn has_screen(&self) -> bool {
        !matches!(self, Self::Starting)
    }
}

/// How a pane's connection is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// Nothing to connect to: a local shell.
    Local,
    Connecting {
        stage: String,
    },
    Live,
    /// The session is over, for this reason.
    Closed {
        reason: String,
    },
    /// Connecting did not work, for this reason.
    Failed {
        reason: String,
    },
    /// Reopened from the last session, and connecting takes someone: this says what for. It
    /// stays this way until asked, so that many of them do not make for many dialogs at startup.
    Awaiting {
        why: String,
    },
}

/// What a pane connected to a saved connection knows about it.
pub struct Remote {
    /// The connection, as the tree numbers it.
    pub node: u64,
    /// Its name, shown until the session sets a title.
    pub name: String,
    /// `user@host:port`, or the name when that is not known.
    pub target: String,
    /// Which attempt at connecting is current: reports from earlier ones are stale.
    pub generation: u64,
    /// Dropping this ends the attempt or the session.
    pub cancel: Option<oneshot::Sender<()>>,
    /// Port forwards running.
    pub forwards: u32,
    /// What it connects for: the kind of pane is settled by the connection, or by this.
    pub goal: Goal,
    /// Reopened from the last session: anything it has to ask leaves it awaiting the user.
    pub quiet: bool,
    /// What of its terminal the connection says to log.
    pub log: SessionLogging,
}

/// A pane, whatever it shows.
pub struct Pane {
    pub id: PaneId,
    /// Which window it is in now, as [`crate::app`] numbers them.
    pub window: u64,
    pub content: PaneContent,
    pub dirty: bool,
    pub title: Option<String>,
    pub link: Link,
    pub remote: Option<Remote>,
    /// Something to tell the user, until dismissed.
    pub notice: Option<String>,
    /// The file its terminal is being logged to, carried over to the terminal of a reconnect.
    pub log: Option<SessionLog>,
}

impl Pane {
    /// A pane whose local shell is starting.
    pub fn local(id: PaneId) -> Self {
        Self {
            id,
            window: 0,
            content: PaneContent::Starting,
            dirty: true,
            title: None,
            link: Link::Local,
            remote: None,
            notice: None,
            log: None,
        }
    }

    /// A pane about to connect to the saved connection `node`.
    pub fn remote(id: PaneId, node: u64, name: String, goal: Goal) -> Self {
        let target = name.clone();
        Self {
            link: Link::Connecting { stage: "Starting".to_owned() },
            remote: Some(Remote {
                node,
                name,
                target,
                generation: 0,
                cancel: None,
                forwards: 0,
                goal,
                quiet: false,
                log: SessionLogging::Off,
            }),
            ..Self::local(id)
        }
    }

    /// What the bar's dot shows.
    pub fn status(&self) -> PaneStatus {
        match (&self.link, self.content.has_screen()) {
            (Link::Local, true) | (Link::Live, _) => PaneStatus::Connected,
            (Link::Local, false) | (Link::Connecting { .. }, _) => PaneStatus::Connecting,
            (Link::Closed { .. } | Link::Awaiting { .. }, _) => PaneStatus::Disconnected,
            (Link::Failed { .. }, _) => PaneStatus::Failed,
        }
    }

    /// What the bar calls the pane.
    pub fn label(&self) -> &str {
        self.title
            .as_deref()
            .or(self.remote.as_ref().map(|remote| remote.name.as_str()))
            .unwrap_or("local shell")
    }

    /// What to draw over the content about the connection, if anything.
    pub fn overlay(&self) -> Option<PaneOverlay<'_>> {
        let target = self.remote.as_ref().map_or("", |remote| remote.target.as_str());
        let screen = self.content.has_screen();
        match &self.link {
            Link::Connecting { stage } => Some(PaneOverlay::Connecting { target, stage, screen }),
            Link::Closed { reason } => Some(PaneOverlay::Closed { reason }),
            Link::Failed { reason } => Some(PaneOverlay::Failed { target, reason, screen }),
            Link::Awaiting { why } => Some(PaneOverlay::Awaiting { target, why, screen }),
            Link::Local | Link::Live => None,
        }
    }

    /// The parts of `area`, the pane's content in points, that the interface draws on.
    pub fn interface_rects(&self, area: egui::Rect) -> impl Iterator<Item = egui::Rect> {
        let overlay = self.overlay().map(|overlay| overlay_rect(area, &overlay));
        let notice = self.notice.as_ref().map(|_| notice_rect(area));
        let whole = self.content.is_interface().then_some(area);
        overlay.into_iter().chain(notice).chain(whole)
    }

    /// Stop the connection, if there is one.
    pub fn hang_up(&mut self) {
        if let Some(remote) = &mut self.remote {
            remote.cancel = None;
        }
        if let Some(display) = self.content.display_mut() {
            display.hang_up();
        }
    }
}
