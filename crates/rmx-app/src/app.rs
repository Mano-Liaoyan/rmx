//! The application: routing window and terminal events to panes.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rmx_layout::dnd::{Source, Target};
use rmx_layout::motion::Motion;
use rmx_layout::session::{self, SavedTab, SavedWindow};
use rmx_layout::split::{Axis, Divider};
use rmx_layout::workspace::{Closed, Direction, Tab, Workspace};
use rmx_layout::{PaneId, Rect};
use rmx_render::probe::{self, Phase};
use rmx_ssh::SecretString;
use rmx_terminal::{
    ClipboardType, Output, SessionEvent, Sink, TermEvent, Terminal, Wheel, encode_key,
};
use rmx_ui::theme::{PANE_BAR_HEIGHT, STRIP_HEIGHT};
use rmx_ui::widgets::{ConnectionDraft, FilesKey, MenuItem, NodeKind, PromptField, PromptState};
use rmx_ui::{
    Chrome, FilesAction, NodeId, Protocol, Sensitive, TabId, ThemeMode, UiAction, egui, icons,
    snippet,
};
use rmx_vault::Secret;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::window::{
    CursorIcon, CustomCursor, ResizeDirection, Window, WindowAttributes, WindowId,
};

use crate::background::{Boot, Layout, SavedPane, SavedSession};
use crate::clipboard::Clipboard;
use crate::connect::{
    self, ConnectRequest, Goal, HostKeyAnswer, LinkEvent, Origin, Prompt, PromptKind, Ready,
};
use crate::files::{self, FilesEvent, FilesPane};
use crate::gfx::{Early, Gfx, PaneDraw, Preedit};
use crate::pacer::Pacer;
use crate::pane::{Link, Pane, PaneContent};
use crate::remote::{self, ClipboardOutcome, Display, Incoming, WheelDelta};
use crate::shell::{self, Asking, Common, Gate, PaneDrop, Picked, Shell, ShellView};
use crate::store::{self, Request, SyncState, VaultEvent, VaultHandle};

/// The refresh interval assumed for a monitor that does not report one.
const DEFAULT_REFRESH: Duration = Duration::from_micros(16_667);
/// How often the vault's thread hears that the user is still there, at most.
const TOUCH_EVERY: Duration = Duration::from_secs(5);
/// How long ending the application waits for the window layout to be saved.
const SAVE_ON_EXIT: Duration = Duration::from_millis(300);
/// How long after the windows last changed the session goes in the state file.
const SESSION_SAVE_AFTER: Duration = Duration::from_secs(1);
/// How long a tab let go outside its window waits for another window to say the pointer came down
/// on it, before it gets a window of its own.
const CLAIM_WAIT: Duration = Duration::from_millis(120);
/// How often to look whether what a new window is made of is ready.
const PREPARE_POLL: Duration = Duration::from_millis(4);
/// Gap between panes and around them, in logical pixels.
const SPACING: f32 = 4.;
/// Thinnest band around a divider that takes the pointer, in logical pixels.
const DIVIDER_BAND: f32 = 8.;
/// Distance from the middle within which a dragged divider snaps to it, in logical pixels.
const DIVIDER_SNAP: f32 = 6.;
/// Longest time between the presses of a double click on a divider.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
/// Smallest pane, in cells.
const MIN_CELLS: (f32, f32) = (rmx_layout::split::MIN_COLUMNS, rmx_layout::split::MIN_LINES);
/// How wide the band at the window's edges that resizes it is, in logical pixels.
const RESIZE_BAND: f32 = 5.;
/// Smallest window, in logical pixels.
const MIN_WINDOW: (f64, f64) = (480., 320.);
/// Bytes in a mebibyte, for saying how much output was lost.
const MIB: u64 = 1024 * 1024;
/// What is said when a clipboard cannot be carried by VNC.
const VNC_CLIPBOARD_NOTICE: &str = "VNC can only carry ASCII clipboard text: this was not copied";

mod actions;
mod display;
mod handler;
mod keys;
mod lifecycle;
mod logging;
mod menus;
mod panes;
mod pointer;
mod prompts;
#[cfg(feature = "bench")]
mod script;
mod view;
mod windows;

use keys::action_for;
use pointer::{resize_cursor, resize_edge};
use prompts::Reply;

pub enum AppEvent {
    Wakeup(PaneId),
    Term(PaneId, TermEvent),
    /// A pane's terminal has started, or could not be.
    Spawned(PaneId, Box<io::Result<Terminal>>),
    /// What happened to the session of a pane's terminal.
    Session(PaneId, SessionEvent),
    /// News of a connection attempt, the second number saying which attempt.
    Link(PaneId, u64, LinkEvent),
    /// Frames have arrived for the remote desktop of a pane.
    DisplayNews(PaneId, u64),
    /// An upload of a pane's remote desktop has finished.
    DisplayReady(PaneId, u64),
    /// The remote clipboard now holds this text.
    DisplayClipboard(PaneId, u64, String),
    /// The remote pointer of a pane's desktop changed shape.
    DisplayCursor(PaneId, u64, rmx_display::RemoteCursor),
    /// A file pane's listings and transfers have moved on.
    Files(PaneId, u64, FilesEvent),
    /// Something off the main thread needs the user.
    Prompt(Prompt),
    /// The serial ports this machine has.
    SerialPorts(Vec<rmx_ui::widgets::SerialPortChoice>),
    Vault(VaultEvent),
    /// The log of a pane, to the file at this path, has stopped for this reason.
    LogFailed(PaneId, std::path::PathBuf, String),
}

struct ProxySink(EventLoopProxy<AppEvent>);

impl Sink for ProxySink {
    fn wakeup(&self, id: u64) {
        let _ = self.0.send_event(AppEvent::Wakeup(id));
    }

    fn event(&self, id: u64, event: TermEvent) {
        let _ = self.0.send_event(AppEvent::Term(id, event));
    }

    fn session(&self, id: u64, event: SessionEvent) {
        let _ = self.0.send_event(AppEvent::Session(id, event));
    }
}

/// Something the user can ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    NewTab,
    ClosePane,
    /// The focused session again beside it, or a local shell where that makes no sense.
    Split(Axis),
    /// A local shell beside the focused pane.
    SplitLocal(Axis),
    Focus(Direction),
    Zoom,
    CycleTab(isize),
    SelectTab(usize),
    Copy,
    Paste,
    QuickOpen,
    NewConnection,
    Snippets,
    ToggleSidebar,
    OpenSettings,
    ToggleTheme,
    Lock,
    SyncNow,
    SendCtrlAltDel,
    ToggleScale,
}

/// What the window's cursor is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WindowCursor {
    Icon(CursorIcon),
    /// A remote desktop's pointer.
    Custom(CustomCursor),
    Hidden,
}

impl From<&crate::remote::DisplayCursor> for WindowCursor {
    fn from(cursor: &crate::remote::DisplayCursor) -> Self {
        use crate::remote::DisplayCursor;
        match cursor {
            DisplayCursor::Default => Self::Icon(CursorIcon::Default),
            DisplayCursor::Hidden => Self::Hidden,
            DisplayCursor::Custom(custom) => Self::Custom(custom.clone()),
        }
    }
}

/// A divider being dragged.
#[derive(Clone, Copy)]
struct DividerDrag {
    index: usize,
    /// Pointer offset from the divider's middle at the press.
    grab: f32,
    /// The ratio at the press, which Escape goes back to.
    ratio: f32,
}

/// Numbers the windows, for as long as the application runs. Zero is no window.
type WinKey = u64;

/// What becomes of a window whose last tab has just gone, if it turns out to be the last window.
/// With another window left it goes either way. Decided once the current event is over, when
/// everything that event batch closed is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WhenLast {
    /// It stays, offering something new to open.
    ShowEmpty,
    /// The application ends with it.
    Quit,
}

/// Something that needs more than one window, done once the current event is over.
enum Op {
    /// Change the theme of every window.
    SetTheme(ThemeMode),
    /// Give this tab or pane of the window a window of its own.
    Tear { from: WinKey, what: Moved },
    /// Move this tab or pane of a window to another window, in front of the tab at index `at`.
    Move { from: WinKey, what: Moved, to: WinKey, at: usize },
}

/// What moves between windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    Tab(u64),
    Pane(PaneId),
}

/// A tab or pane on its way out of its window, by a drag let go outside it or from a menu.
///
/// Whether it was let go over another window of ours can only be told by that window hearing of
/// the pointer once the button is up: a client is not told where its windows are on Wayland, and
/// until the button is up the pointer belongs to the window it was pressed in.
struct Landing {
    from: WinKey,
    what: Moved,
    /// Past this, nothing claimed it and it gets a window of its own.
    deadline: Instant,
    /// Whether another window may claim it.
    claimable: bool,
    /// What its own window is made of, prepared meanwhile; none when it cannot have one.
    early: Option<Early>,
}

/// What all windows share. Declared so that the clipboard, and the GL objects of the panes, drop
/// while the windows exist.
struct Core {
    clipboard: Clipboard,
    panes: Vec<Pane>,
    next_id: PaneId,
    /// Panes whose terminal ended before it had arrived from the thread that started it.
    ended_early: Vec<PaneId>,
    sink: Arc<ProxySink>,
    /// Sessions, and everything else that must not run here.
    runtime: Runtime,
    vault: VaultHandle,
    proxy: EventLoopProxy<AppEvent>,
    /// Questions waiting their turn for the dialog.
    prompts: VecDeque<Prompt>,
    /// Where the answer to the dialog now showing goes, and the pane it is about.
    answer: Option<(Reply, Option<PaneId>)>,
    theme: ThemeMode,
    /// Ask before a close ends live sessions: see [`crate::closing`].
    confirm_close: bool,
    /// When the vault's thread last heard the user is there.
    touched: Instant,
    /// Whether the first tab has been seen to: a shell opened for the vault, as happens the first
    /// time it unlocks, or the last session reopened, even if that had no tabs left in it.
    opened_first_tab: bool,
    /// What the interfaces of all windows show alike.
    ui: Common,
    /// Work needing other windows than the current one.
    ops: Vec<Op>,
    /// The window the user was last in.
    active: WinKey,
    /// Every window has to show what just changed in [`Core::ui`].
    broadcast: bool,
    next_window: WinKey,
    /// Whether frames are paced by the compositor's callbacks.
    wayland: bool,
    /// The windows, in the order they opened, for the menus that offer them.
    window_keys: Vec<WinKey>,
    /// What was let go outside its window and has not landed yet.
    landing: Option<Landing>,
    /// The windows of the last session, until the vault is open to reopen them.
    restoring: Option<SavedSession>,
    /// Further windows of the last session, whose parts are being prepared.
    reopening: VecDeque<(Early, SavedWindow<SavedPane>)>,
    /// When the session is next to be written down, after the windows have changed.
    session_due: Option<Instant>,
    /// For measuring only: the panes need no vault.
    #[cfg(feature = "bench")]
    pub vaultless: bool,
    /// What the clipboard held when the script last looked.
    #[cfg(feature = "bench")]
    clipboard_seen: std::cell::RefCell<String>,
}

/// One window, with its own GL resources and frame pacing.
struct Win {
    key: WinKey,
    workspace: Workspace,
    motion: Motion,
    /// Where the panes of the active tab are at rest.
    rects: Vec<(PaneId, Rect)>,
    pacer: Pacer,
    /// Whether the first frame has been shown.
    started: bool,
    /// Whether the next frame has to redraw everything.
    full: bool,
    /// Whether the last frame caught an animation midway, so that it still has to come to rest.
    was_animating: bool,
    /// What the last frame drew, kept so that building the next one does not allocate.
    scene: Vec<PaneDraw>,
    pointer: PhysicalPosition<f64>,
    drag: Option<DividerDrag>,
    /// The divider last pressed, and when, to tell double clicks.
    last_divider: Option<(usize, Instant)>,
    /// The button whose press opened a menu or only focused a pane: its release is nobody's.
    swallow_release: Option<MouseButton>,
    /// The pane a button was pressed in, which keeps getting the pointer.
    captured: Option<PaneId>,
    /// Everything around the panes, and the input method's unfinished text.
    shell: Shell,
    preedit: Option<Preedit>,
    /// The cursor last shown, to tell the window only when it changes.
    cursor: WindowCursor,
    /// Where the input method was last told text goes.
    ime_area: Option<(i32, i32, u32, u32)>,
    maximized: bool,
    /// The window is to go, once the current event is over.
    exit: bool,
    /// Its last tab has gone, in this event: see [`WhenLast`].
    vacated: Option<WhenLast>,
    /// Whether the window has the keyboard.
    has_focus: bool,
    /// The remote desktop that last had the keyboard, to offer it the clipboard when that changes.
    display_focus: Option<PaneId>,
    /// Whether the window lets the input method in. Not while a remote desktop has the keyboard.
    ime_allowed: bool,
    /// The unsaved connection whose panes the connection form, open to save it, is for.
    adopting: Option<Arc<ConnectionDraft>>,
    /// For measuring only: how wide the terminal was before the script resized the window.
    #[cfg(feature = "bench")]
    columns_before_resize: Option<usize>,
    /// Whether the last frame ran egui, for the measurements.
    #[cfg(feature = "bench")]
    ran_ui: bool,
    chrome: Chrome,
    gfx: Gfx,
}

impl Drop for Win {
    fn drop(&mut self) {
        // The GL objects must go while the context does.
        self.gfx.make_current();
        self.chrome.destroy();
    }
}

impl Win {
    /// The size to give a window made from this one, in logical pixels.
    fn logical_size(&self) -> (f64, f64) {
        let size = self.gfx.window.inner_size().to_logical::<f64>(self.gfx.window.scale_factor());
        (size.width, size.height)
    }

    /// Take a tab out of this window, or a pane out of its tab.
    fn take(&mut self, what: Moved) -> Option<Tab> {
        match what {
            Moved::Tab(id) => {
                let index = self.workspace.tab_index(id)?;
                self.workspace.take_tab(index)
            },
            Moved::Pane(pane) => self.workspace.take_pane(pane),
        }
    }

    fn new(key: WinKey, gfx: Gfx, chrome: Chrome, shell: Shell, wayland: bool) -> Self {
        let refresh = refresh_interval(&gfx.window);
        Self {
            key,
            workspace: Workspace::default(),
            motion: Motion::default(),
            rects: Vec::new(),
            pacer: Pacer::new(wayland, refresh),
            started: false,
            full: true,
            was_animating: false,
            scene: Vec::new(),
            pointer: PhysicalPosition::default(),
            drag: None,
            last_divider: None,
            swallow_release: None,
            captured: None,
            shell,
            preedit: None,
            cursor: WindowCursor::Icon(CursorIcon::Default),
            ime_area: None,
            maximized: false,
            exit: false,
            vacated: None,
            has_focus: true,
            display_focus: None,
            ime_allowed: true,
            adopting: None,
            #[cfg(feature = "bench")]
            columns_before_resize: None,
            #[cfg(feature = "bench")]
            ran_ui: false,
            chrome,
            gfx,
        }
    }
}

/// Every window, and what they share. The windows go last: see [`Core`].
struct Running {
    core: Core,
    windows: Vec<Win>,
}

/// One window and what all windows share: most of what happens concerns just one window.
struct At<'a> {
    core: &'a mut Core,
    win: &'a mut Win,
}

impl Running {
    /// The window numbered `key`.
    fn index_of(&self, key: WinKey) -> Option<usize> {
        self.windows.iter().position(|win| win.key == key)
    }

    /// The window `id`, for acting on it with what it shares.
    fn at_window(&mut self, id: WindowId) -> Option<At<'_>> {
        let Self { core, windows } = self;
        let win = windows.iter_mut().find(|win| win.gfx.window.id() == id)?;
        Some(At { core, win })
    }

    /// The window at `index`.
    fn at_index(&mut self, index: usize) -> Option<At<'_>> {
        let Self { core, windows } = self;
        Some(At { core, win: windows.get_mut(index)? })
    }

    /// The window pane `id` is in.
    fn at_pane(&mut self, id: PaneId) -> Option<At<'_>> {
        let key = self.core.panes.iter().find(|pane| pane.id == id)?.window;
        let index = self.index_of(key)?;
        self.at_index(index)
    }

    /// The window the user was last in.
    fn at_active(&mut self) -> Option<At<'_>> {
        let index = self.index_of(self.core.active).unwrap_or(0);
        self.at_index(index)
    }
}

pub struct App {
    proxy: EventLoopProxy<AppEvent>,
    boot: Option<JoinHandle<io::Result<Boot>>>,
    running: Option<Running>,
    modifiers: ModifiersState,
    #[cfg(feature = "bench")]
    started: Instant,
    #[cfg(feature = "bench")]
    bench: crate::bench::Recorder,
    /// Which window the bench script is acting on.
    #[cfg(feature = "bench")]
    script_window: usize,
}

impl App {
    pub fn new(
        proxy: EventLoopProxy<AppEvent>,
        _started: Instant,
        boot: JoinHandle<io::Result<Boot>>,
    ) -> Self {
        Self {
            proxy,
            boot: Some(boot),
            running: None,
            modifiers: ModifiersState::empty(),
            #[cfg(feature = "bench")]
            started: _started,
            #[cfg(feature = "bench")]
            bench: {
                crate::bench::sampler::start();
                crate::bench::Recorder::from_env()
            },
            #[cfg(feature = "bench")]
            script_window: 0,
        }
    }

    /// The event loop has ended.
    pub fn finish(&self) {
        #[cfg(feature = "bench")]
        self.bench.write();
    }

    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Box<dyn std::error::Error>> {
        let boot = self.boot.take().ok_or("the application started twice")?;
        let Boot { runtime, paths, state, layout, session } =
            boot.join().map_err(|_| "the thread starting up panicked")??;

        #[cfg(feature = "bench")]
        let (width, height) = crate::bench::window_size().unwrap_or((layout.width, layout.height));
        #[cfg(not(feature = "bench"))]
        let (width, height) = (layout.width, layout.height);

        // The window needs no fonts, and the fonts need no window, only the scale to size them for.
        let scale = event_loop.primary_monitor().map_or(1., |monitor| monitor.scale_factor());
        let early = Early::start(scale, (width, height), layout.theme);

        let attributes = window_attributes((width, height), layout.maximized);
        #[cfg(feature = "bench")]
        let attributes = crate::bench::present::fullscreen_on(event_loop, attributes)?;
        let window = event_loop.create_window(attributes)?;
        window.set_ime_allowed(true);
        crate::mark("window");

        let raw_display = window.display_handle()?.as_raw();
        let wayland = matches!(raw_display, RawDisplayHandle::Wayland(_));
        let clipboard = Clipboard::new(raw_display);

        const FIRST: WinKey = 1;
        let (gfx, chrome, mut shell) = Gfx::new(window, early, layout.theme, FIRST, None)?;
        shell.sidebar_open = layout.sidebar_open;
        shell.sidebar_width = layout.panels.sidebar;
        let panels = layout.panels;
        let vault = store::spawn(runtime.handle().clone(), self.proxy.clone(), paths, state)?;
        #[cfg(feature = "bench")]
        {
            self.bench.renderer = Gfx::renderer_name();
        }

        let mut common = Common::for_window(FIRST);
        common.panels = panels;
        let core = Core {
            clipboard,
            panes: Vec::new(),
            next_id: 0,
            ended_early: Vec::new(),
            sink: Arc::new(ProxySink(self.proxy.clone())),
            runtime,
            vault,
            proxy: self.proxy.clone(),
            prompts: VecDeque::new(),
            answer: None,
            theme: layout.theme,
            confirm_close: layout.confirm_close,
            touched: Instant::now(),
            opened_first_tab: false,
            ui: common,
            ops: Vec::new(),
            active: FIRST,
            broadcast: false,
            next_window: FIRST + 1,
            wayland,
            window_keys: vec![FIRST],
            landing: None,
            restoring: session,
            reopening: VecDeque::new(),
            session_due: None,
            #[cfg(feature = "bench")]
            vaultless: crate::bench::vaultless(),
            #[cfg(feature = "bench")]
            clipboard_seen: std::cell::RefCell::new(String::new()),
        };
        #[cfg_attr(not(feature = "bench"), allow(unused_mut))]
        let mut running =
            Running { core, windows: vec![Win::new(FIRST, gfx, chrome, shell, wayland)] };
        #[cfg(feature = "bench")]
        if running.core.vaultless {
            running.core.ui.gate = Gate::Open;
            if let Some(mut at) = running.at_index(0) {
                at.perform(Action::NewTab);
            }
        }
        self.running = Some(running);
        Ok(())
    }
}

fn claim_wait() -> Duration {
    #[cfg(feature = "bench")]
    if let Some(wait) = crate::bench::claim_wait() {
        return wait;
    }
    CLAIM_WAIT
}

/// What the pane `id` is called.
fn pane_label(panes: &[Pane], id: PaneId) -> &str {
    panes.iter().find(|pane| pane.id == id).map_or("", Pane::label)
}

/// A serial port as the connection form offers it.
fn port_choice(port: &rmx_serial::serial::PortInfo) -> rmx_ui::widgets::SerialPortChoice {
    let path = port.path.display().to_string();
    let label =
        port.usb.as_ref().and_then(|usb| usb.product.clone()).unwrap_or_else(|| path.clone());
    rmx_ui::widgets::SerialPortChoice { path, label }
}

/// What every window is made with: the app draws its own frame.
fn window_attributes(size: (f64, f64), maximized: bool) -> WindowAttributes {
    // Wayland's app id and X11's class, by which the desktop finds `rmx.desktop`.
    #[cfg(not(any(target_os = "macos", windows)))]
    let attributes = {
        use winit::platform::{
            wayland::WindowAttributesExtWayland as Wayland, x11::WindowAttributesExtX11 as X11,
        };
        X11::with_name(Wayland::with_name(Window::default_attributes(), "rmx", "rmx"), "rmx", "rmx")
    };
    #[cfg(any(target_os = "macos", windows))]
    let attributes = Window::default_attributes();
    attributes
        .with_title("rmx")
        .with_decorations(false)
        .with_min_inner_size(LogicalSize::new(MIN_WINDOW.0, MIN_WINDOW.1))
        .with_inner_size(LogicalSize::new(size.0, size.1))
        .with_maximized(maximized)
}

fn menu_item(glyph: char, label: &str, action: UiAction, danger: bool) -> MenuItem {
    MenuItem::Action { glyph: Some(glyph), label: label.to_owned(), shortcut: None, action, danger }
}

/// A row that the keyboard reaches too, with the keys shown.
fn menu_keyed(glyph: char, label: &str, keys: &str, action: UiAction) -> MenuItem {
    let shortcut = Some(keys.to_owned());
    MenuItem::Action {
        glyph: Some(glyph),
        label: label.to_owned(),
        shortcut,
        action,
        danger: false,
    }
}

/// The connection `user@host:port` stands for, to connect to without saving it.
fn quick_draft(text: &str) -> Option<ConnectionDraft> {
    let text = text.trim();
    let (user, place) = text.rsplit_once('@').map_or(("", text), |(user, place)| (user, place));
    let (host, port) = match place.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
        None => (place, None),
    };
    if host.is_empty() {
        return None;
    }
    let mut draft = ConnectionDraft::new(Protocol::Ssh);
    text.clone_into(&mut draft.name);
    host.clone_into(&mut draft.host);
    user.clone_into(&mut draft.username);
    if let Some(port) = port {
        draft.port = port.to_string();
    }
    Some(draft)
}

/// Whether two presses make a double click on the divider `index`.
fn divider_double_click(last: Option<(usize, Instant)>, index: usize, now: Instant) -> bool {
    last.is_some_and(|(last, at)| last == index && now.saturating_duration_since(at) < DOUBLE_CLICK)
}

/// A key file as typed: none when left empty.
fn key_path(text: &str) -> Option<std::path::PathBuf> {
    let text = text.trim();
    (!text.is_empty()).then(|| std::path::PathBuf::from(text))
}

/// The command to run instead of the user's shell: `rmx -e program args...`.
fn startup_shell() -> Option<(String, Vec<String>)> {
    let mut args = std::env::args().skip_while(|arg| arg != "-e").skip(1);
    let program = args.next()?;
    Some((program, args.collect()))
}

fn refresh_interval(window: &Window) -> Duration {
    window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .map_or(DEFAULT_REFRESH, |mhz| Duration::from_secs_f64(1000. / f64::from(mhz)))
}

#[cfg(test)]
mod tests {
    use super::keys::files_key_for;
    use super::*;

    #[test]
    fn quick_connect_reads_user_host_and_port() {
        let draft = quick_draft("alice@db.example.com:2222").unwrap();
        assert_eq!(
            (draft.username.as_str(), draft.host.as_str(), draft.port.as_str()),
            ("alice", "db.example.com", "2222")
        );
        let draft = quick_draft("db.example.com").unwrap();
        assert_eq!((draft.username.as_str(), draft.port.as_str()), ("", "22"));
        assert!(quick_draft("alice@:22").is_none());
        assert!(quick_draft("host:notaport").is_none());
    }

    #[test]
    fn two_quick_presses_on_one_divider_are_a_double_click() {
        let now = Instant::now();
        assert!(divider_double_click(Some((1, now)), 1, now + Duration::from_millis(300)));
        assert!(!divider_double_click(Some((0, now)), 1, now + Duration::from_millis(300)));
        assert!(!divider_double_click(Some((1, now)), 1, now + DOUBLE_CLICK));
        assert!(!divider_double_click(None, 1, now));
    }

    #[test]
    fn keys_of_a_file_pane_go_unmodified_or_with_shift() {
        let none = ModifiersState::empty();
        let down = || Key::Named(NamedKey::ArrowDown);
        assert_eq!(files_key_for(down(), none), Some((FilesKey::Down, false)));
        assert_eq!(files_key_for(down(), ModifiersState::SHIFT), Some((FilesKey::Down, true)));
        assert_eq!(files_key_for(down(), ModifiersState::CONTROL), None);
        assert_eq!(files_key_for(Key::Named(NamedKey::Tab), none), Some((FilesKey::Switch, false)));
        assert_eq!(files_key_for(Key::Named(NamedKey::F5), none), Some((FilesKey::Copy, false)));
        assert_eq!(files_key_for(Key::Character("a"), none), None);
    }

    #[test]
    fn the_keys_offered_on_the_empty_page_do_what_it_says() {
        let keys = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(action_for(Key::Character("t"), keys), Some(Action::NewTab));
        assert_eq!(action_for(Key::Character("T"), keys), Some(Action::NewTab));
        assert_eq!(action_for(Key::Character("n"), keys), Some(Action::NewConnection));
        assert_eq!(action_for(Key::Character("t"), ModifiersState::CONTROL), None);
    }

    #[test]
    fn the_edges_of_the_window_resize_it_and_its_corners_take_twice_the_band() {
        let edge = |x, y| resize_edge(x, y, 800., 600., 5.);
        assert!(matches!(edge(0., 300.), Some(ResizeDirection::West)));
        assert!(matches!(edge(799., 300.), Some(ResizeDirection::East)));
        assert!(matches!(edge(400., 0.), Some(ResizeDirection::North)));
        assert!(matches!(edge(400., 599.), Some(ResizeDirection::South)));
        assert!(matches!(edge(2., 2.), Some(ResizeDirection::NorthWest)));
        assert!(matches!(edge(9., 2.), Some(ResizeDirection::NorthWest)));
        assert!(matches!(edge(797., 598.), Some(ResizeDirection::SouthEast)));
        assert!(edge(400., 300.).is_none());
        assert!(edge(-1., 300.).is_none());
    }
}
