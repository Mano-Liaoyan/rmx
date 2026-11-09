//! The application: routing window and terminal events to panes.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rmx_render::probe::{self, Phase};
use rmx_layout::dnd::{Source, Target};
use rmx_layout::motion::Motion;
use rmx_layout::session::{self, SavedTab, SavedWindow};
use rmx_layout::split::{Axis, Divider};
use rmx_layout::workspace::{Closed, Direction, Tab, Workspace};
use rmx_layout::{PaneId, Rect};
use rmx_ssh::SecretString;
use rmx_terminal::{
    ClipboardType, Output, SessionEvent, Sink, TermEvent, Terminal, Wheel, encode_key,
};
use rmx_ui::theme::{PANE_BAR_HEIGHT, STRIP_HEIGHT};
use rmx_ui::widgets::{ConnectionDraft, FilesKey, MenuItem, NodeKind, PromptField, PromptState};
use rmx_ui::{Chrome, FilesAction, NodeId, Protocol, Sensitive, TabId, ThemeMode, UiAction, egui, icons, snippet};
use rmx_vault::Secret;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowAttributes, WindowId};

use crate::background::{Boot, Layout, SavedPane, SavedSession};
use crate::clipboard::Clipboard;
use crate::connect::{self, ConnectRequest, Goal, HostKeyAnswer, LinkEvent, Prompt, PromptKind, Ready};
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

mod windows;

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
    /// A file pane's listings and transfers have moved on.
    Files(PaneId, u64, FilesEvent),
    /// Something off the main thread needs the user.
    Prompt(Prompt),
    /// The serial ports this machine has.
    SerialPorts(Vec<rmx_ui::widgets::SerialPortChoice>),
    Vault(VaultEvent),
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

/// The action bound to a key, if any.
fn action_for(key: Key<&str>, mods: ModifiersState) -> Option<Action> {
    let ctrl_shift = mods.control_key() && mods.shift_key() && !mods.alt_key();

    if mods.control_key() && key == Key::Named(NamedKey::Tab) {
        return Some(Action::CycleTab(if mods.shift_key() { -1 } else { 1 }));
    }
    if !ctrl_shift {
        return None;
    }

    match key {
        Key::Named(NamedKey::ArrowLeft) => Some(Action::Focus(Direction::Left)),
        Key::Named(NamedKey::ArrowRight) => Some(Action::Focus(Direction::Right)),
        Key::Named(NamedKey::ArrowUp) => Some(Action::Focus(Direction::Up)),
        Key::Named(NamedKey::ArrowDown) => Some(Action::Focus(Direction::Down)),
        Key::Character(text) => match text.to_lowercase().as_str() {
            "t" => Some(Action::NewTab),
            "w" => Some(Action::ClosePane),
            "e" => Some(Action::Split(Axis::Row)),
            "o" => Some(Action::Split(Axis::Column)),
            "z" => Some(Action::Zoom),
            "c" => Some(Action::Copy),
            "v" => Some(Action::Paste),
            "p" => Some(Action::QuickOpen),
            "n" => Some(Action::NewConnection),
            "s" => Some(Action::Snippets),
            "b" => Some(Action::ToggleSidebar),
            digit => match digit.parse::<usize>() {
                Ok(n @ 1..=9) => Some(Action::SelectTab(n - 1)),
                _ => None,
            },
        },
        _ => None,
    }
}

/// Where the answer to the question on screen goes.
enum Reply {
    HostKey(oneshot::Sender<HostKeyAnswer>),
    Fields(oneshot::Sender<Option<Vec<SecretString>>>),
    NewFolder(Option<u64>),
    RenameFolder(u64),
    DeleteConnection(u64),
    DeleteFolder(u64),
    /// The values of a snippet's placeholders: send it with them.
    Snippet(String),
    FilesNewFolder(PaneId, rmx_ui::FileSide),
    FilesRename(PaneId, rmx_ui::FileSide, Arc<str>),
    FilesDelete(PaneId, rmx_ui::FileSide, Vec<Arc<str>>),
    /// Write over a file that changed since it was opened.
    FilesOverwrite(PaneId),
    /// Go on with `then`, losing the edits in these panes.
    LoseEdits(Vec<PaneId>, UiAction),
    /// Go to the vault at the path typed.
    VaultPath,
}

/// What `key` does in a file pane, and whether Shift is adding to the selection.
fn files_key_for(key: Key<&str>, mods: ModifiersState) -> Option<(FilesKey, bool)> {
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return None;
    }
    let Key::Named(name) = key else { return None };
    let found = match name {
        NamedKey::ArrowUp => FilesKey::Up,
        NamedKey::ArrowDown => FilesKey::Down,
        NamedKey::PageUp => FilesKey::PageUp,
        NamedKey::PageDown => FilesKey::PageDown,
        NamedKey::Home => FilesKey::Home,
        NamedKey::End => FilesKey::End,
        NamedKey::Enter => FilesKey::Open,
        NamedKey::Backspace => FilesKey::Back,
        NamedKey::Tab => FilesKey::Switch,
        NamedKey::F5 => FilesKey::Copy,
        NamedKey::Escape => FilesKey::Escape,
        _ => return None,
    };
    Some((found, mods.shift_key()))
}

/// Which edge of a window the point `(x, y)` is on, if any: the corners take twice the band.
fn resize_edge(x: f32, y: f32, width: f32, height: f32, band: f32) -> Option<ResizeDirection> {
    if x < 0. || y < 0. || x >= width || y >= height {
        return None;
    }
    let corner = band * 2.;
    let (left, right) = (x < band, x >= width - band);
    let (top, bottom) = (y < band, y >= height - band);
    let (near_left, near_right) = (x < corner, x >= width - corner);
    let (near_top, near_bottom) = (y < corner, y >= height - corner);
    Some(match () {
        () if (top && near_left) || (near_top && left) => ResizeDirection::NorthWest,
        () if (top && near_right) || (near_top && right) => ResizeDirection::NorthEast,
        () if (bottom && near_left) || (near_bottom && left) => ResizeDirection::SouthWest,
        () if (bottom && near_right) || (near_bottom && right) => ResizeDirection::SouthEast,
        () if top => ResizeDirection::North,
        () if bottom => ResizeDirection::South,
        () if left => ResizeDirection::West,
        () if right => ResizeDirection::East,
        () => return None,
    })
}

fn resize_cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::West => CursorIcon::WResize,
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
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
    cursor: CursorIcon,
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
            cursor: CursorIcon::Default,
            ime_area: None,
            maximized: false,
            exit: false,
            vacated: None,
            has_focus: true,
            display_focus: None,
            ime_allowed: true,
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
        let mut running = Running { core, windows: vec![Win::new(FIRST, gfx, chrome, shell, wayland)] };
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
    let label = port.usb.as_ref().and_then(|usb| usb.product.clone()).unwrap_or_else(|| path.clone());
    rmx_ui::widgets::SerialPortChoice { path, label }
}

/// What every window is made with: the app draws its own frame.
fn window_attributes(size: (f64, f64), maximized: bool) -> WindowAttributes {
    // Wayland's app id and X11's class, by which the desktop finds `rmx.desktop`.
    use winit::platform::{wayland::WindowAttributesExtWayland as Wayland, x11::WindowAttributesExtX11 as X11};
    let attributes = Wayland::with_name(Window::default_attributes(), "rmx", "rmx");
    X11::with_name(attributes, "rmx", "rmx")
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
    MenuItem::Action { glyph: Some(glyph), label: label.to_owned(), shortcut, action, danger: false }
}

/// The saved connection `user@host:port` stands for.
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

impl At<'_> {
    fn scale(&self) -> f32 {
        self.win.gfx.scale()
    }

    fn spacing(&self) -> f32 {
        (SPACING * self.scale()).round()
    }

    /// The width of the sidebar, in pixels, none while it is closed.
    fn sidebar_width(&self) -> f32 {
        if self.win.shell.sidebar_open { (self.win.shell.sidebar_width * self.scale()).round() } else { 0. }
    }

    /// The height of the strip of tabs, in pixels.
    fn strip_height(&self) -> f32 {
        (STRIP_HEIGHT * self.scale()).round()
    }

    /// The height of a pane's bar, in pixels.
    fn bar(&self) -> f32 {
        (PANE_BAR_HEIGHT * self.scale()).round()
    }

    /// The space the panes tile, in the window, right of the sidebar and below the strip.
    fn area(&self) -> Rect {
        let (width, height) = self.win.gfx.window_size();
        let (margin, left, top) = (self.spacing(), self.sidebar_width(), self.strip_height());
        Rect::new(
            left + margin,
            top + margin,
            (width - left - 2. * margin).max(1.),
            (height - top - 2. * margin).max(1.),
        )
    }

    fn min_pane(&self) -> (f32, f32) {
        let (cell_w, cell_h) = self.win.gfx.cell();
        (MIN_CELLS.0 * cell_w, MIN_CELLS.1 * cell_h + self.bar())
    }

    /// The part of a pane the terminal draws in.
    fn terminal_rect(&self, panel: Rect) -> Rect {
        shell::terminal_rect(panel, self.bar())
    }

    fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.core.panes.iter().find(|pane| pane.id == id)
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.core.panes.iter_mut().find(|pane| pane.id == id)
    }

    fn focused_mut(&mut self) -> Option<&mut Pane> {
        let id = self.win.workspace.focused()?;
        self.pane_mut(id)
    }

    /// Ask for a frame, when one can be drawn.
    fn request_frame(&mut self) {
        if self.win.pacer.mark_dirty() {
            self.win.gfx.window.request_redraw();
        }
    }

    /// Everything has to be redrawn.
    fn redraw_all(&mut self) {
        self.win.full = true;
        self.request_frame();
    }

    /// Show this icon, which winit passes to the compositor even when it is the same.
    fn set_cursor(&mut self, icon: CursorIcon) {
        if self.win.cursor != icon {
            self.win.cursor = icon;
            self.win.gfx.window.set_cursor(icon);
        }
    }

    /// What goes in the state file about the window.
    fn layout(&self) -> Layout {
        let size = self.win.gfx.window.inner_size().to_logical::<f64>(self.win.gfx.window.scale_factor());
        Layout {
            width: size.width,
            height: size.height,
            maximized: self.win.maximized,
            sidebar_open: self.win.shell.sidebar_open,
            theme: self.core.theme,
            panels: self.core.ui.panels,
        }
    }

    /// Tell the vault's thread the user is still there. Costs nothing per event.
    fn touch(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.core.touched) >= TOUCH_EVERY {
            self.core.touched = now;
            self.core.vault.send(Request::Touch);
        }
    }

    /// Something about a pane's connection changed: its overlay and footer, the tree's dot.
    fn link_changed(&mut self, id: PaneId) {
        let Some(pane) = self.pane_mut(id) else { return };
        pane.dirty = true;
        let node = pane.remote.as_ref().map(|remote| remote.node);
        if let Some(node) = node {
            let dot = shell::session_dot(&self.core.panes, node);
            self.core.ui.tree.set_state(NodeId(node), dot);
            self.core.broadcast = true;
        }
        self.sync_chrome_area();
        self.win.chrome.invalidate();
        self.request_frame();
    }

    /// Which edge of the window `point` resizes, if it can be resized.
    fn resize_edge_at(&self, point: (f32, f32)) -> Option<ResizeDirection> {
        if self.win.maximized {
            return None;
        }
        let (width, height) = self.win.gfx.window_size();
        resize_edge(point.0, point.1, width, height, RESIZE_BAND * self.scale())
    }

    /// Tell the chrome where the panes are, and which parts of them are its own.
    fn sync_chrome_area(&mut self) {
        let scale = self.scale();
        if self.core.ui.locked() || self.win.rects.is_empty() {
            // Everything is the interface's: the panes are out of sight, or there are none.
            self.win.chrome.set_pane_area(None);
            self.win.chrome.set_pane_holes([]);
            return;
        }
        let area = self.area();
        self.win.chrome.set_pane_area(Some(egui::Rect::from_min_size(
            egui::pos2(area.x / scale, area.y / scale),
            egui::vec2(area.w / scale, area.h / scale),
        )));

        let bar = self.bar();
        let panes = &self.core.panes;
        let holes = self.win.rects.iter().flat_map(|&(id, rect)| {
            let content = shell::content_points(rect, bar, scale);
            let pane = panes.iter().find(|pane| pane.id == id);
            std::iter::once(shell::bar_rect(rect, scale))
                .chain(pane.into_iter().flat_map(move |pane| pane.interface_rects(content)))
        });
        self.win.chrome.set_pane_holes(holes);
    }

    /// The tree or the window changed: put every shown pane where it now goes.
    fn relayout(&mut self, animate: bool) {
        let now = Instant::now();
        let (area, spacing) = (self.area(), self.spacing());
        let shown = std::mem::take(&mut self.win.rects);
        if !self.core.ui.locked() {
            self.win.rects = self.win.workspace.rects(area, spacing);
        }

        for index in 0..self.win.rects.len() {
            let (id, rect) = self.win.rects[index];
            let size = self.win.gfx.size_for(self.terminal_rect(rect));
            // A pane out of sight has not been keeping its snapshot up to date.
            let appeared = !shown.iter().any(|&(pane, _)| pane == id);
            let settled = self.terminal_rect(rect);
            if let Some(pane) = self.pane_mut(id) {
                pane.content.resize(size);
                pane.dirty |= appeared;
                if let Some(display) = pane.content.display_mut() {
                    display.pane_resized(settled.w as u32, settled.h as u32, now);
                }
            }
        }

        self.win.motion.place(&self.win.rects, now, animate);
        self.core.session_changed();
        self.sync_focus(now);
        self.sync_chrome_area();
        self.win.chrome.invalidate();
        self.redraw_all();
    }

    /// Show who has the keyboard, on the borders and in the terminals.
    fn sync_focus(&mut self, now: Instant) {
        let focused = self.win.workspace.focused();
        if let Some(focused) = focused {
            self.win.motion.focus(focused, now);
        }
        let (window, has_focus) = (self.win.key, self.win.has_focus);
        for pane in self.core.panes.iter_mut().filter(|pane| pane.window == window) {
            if pane.content.set_focused(has_focus && Some(pane.id) == focused) {
                pane.dirty = true;
            }
        }

        let title = focused.and_then(|id| self.pane(id)).and_then(|pane| pane.title.as_deref());
        self.win.gfx.window.set_title(title.unwrap_or("rmx"));
        self.win.chrome.invalidate();

        let display = focused.filter(|&id| self.pane(id).is_some_and(|p| p.content.display().is_some()));
        if display != self.win.display_focus {
            self.win.display_focus = display;
            if let Some(id) = display {
                self.offer_clipboard(id);
            }
        }
        self.sync_ime_allowed();
    }

    /// Give the keyboard to the remote desktop or take it away: while it has it, the input method
    /// on this side must not be in the way of the one on the remote desktop.
    fn sync_ime_allowed(&mut self) {
        let keys_to_interface = self.win.chrome.ctx().egui_wants_keyboard_input();
        let allowed = self.win.display_focus.is_none() || keys_to_interface;
        if allowed != self.win.ime_allowed {
            self.win.ime_allowed = allowed;
            self.win.gfx.window.set_ime_allowed(allowed);
        }
    }

    /// Tell the desktop of `id` what the local clipboard holds, if that is something it can carry.
    fn offer_clipboard(&mut self, id: PaneId) {
        let text = self.core.clipboard.load(ClipboardType::Clipboard);
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(display) = pane.content.display_mut() else { return };
        if let ClipboardOutcome::Withheld { first_time: true } = display.clipboard_out(&text) {
            pane.notice = Some(VNC_CLIPBOARD_NOTICE.to_owned());
            self.link_changed(id);
        }
    }

    /// Take what the session of pane `id` has sent, in order, and show it soon.
    fn display_news(&mut self, id: PaneId, generation: u64) {
        #[cfg(feature = "bench")]
        let started = Instant::now();
        if self.pane_generation(id) != Some(generation) {
            return;
        }
        let proxy = self.core.proxy.clone();
        let Some(index) = self.core.panes.iter().position(|pane| pane.id == id) else { return };
        let (panes, gfx) = (&mut self.core.panes, &self.win.gfx);
        gfx.make_current();
        let pane = &mut panes[index];
        let Some(display) = pane.content.display_mut() else { return };
        let ready = display.ready.clone();
        let mut size_changed = false;
        let mut reallocated = false;
        let mut failed = None;
        display.take_frames(|display, item| match item {
            Incoming::Resized { width, height } => {
                display.set_desktop(width, height);
                display.scroll = [0, 0];
                size_changed = true;
            },
            Incoming::Update(update) => {
                // Only with pixels to show does the texture take the new size: until then the
                // old picture stays, stretched, rather than a black pane.
                if display.texture_stale() {
                    let [width, height] = display.desktop().unwrap_or([update.width, update.height]);
                    let flag = ready.clone();
                    let result = match &mut display.texture {
                        Some(texture) => texture.resize(width, height),
                        None => {
                            let (proxy, flag) = (proxy.clone(), flag);
                            gfx.create_remote_texture(width, height, move || {
                                if !flag.swap(true, std::sync::atomic::Ordering::AcqRel) {
                                    let _ = proxy.send_event(AppEvent::DisplayReady(id, generation));
                                }
                            })
                            .map(|texture| display.texture = Some(texture))
                        },
                    };
                    match result {
                        Ok(()) => reallocated = true,
                        Err(err) => failed = Some(err.to_string()),
                    }
                }
                display.submit(update);
            },
        });
        if let Some(reason) = failed {
            pane.link = Link::Failed { reason };
            pane.hang_up();
        }
        pane.dirty |= size_changed;
        let _ = reallocated;
        #[cfg(feature = "bench")]
        if !reallocated {
            crate::bench::note_remote(started.elapsed());
        }
        if size_changed {
            self.link_changed(id);
        }
        self.request_frame();
    }

    /// The pane's connection attempt that is current, if any.
    fn pane_generation(&self, id: PaneId) -> Option<u64> {
        self.pane(id).and_then(|pane| pane.remote.as_ref()).map(|remote| remote.generation)
    }

    /// An upload has finished: pick it up on the next frame.
    fn display_ready(&mut self, id: PaneId, generation: u64) {
        if self.pane_generation(id) != Some(generation) {
            return;
        }
        let shown = self.win.rects.iter().any(|&(pane, _)| pane == id);
        if let Some(display) = self.pane_mut(id).and_then(|pane| pane.content.display_mut()) {
            display.ready.store(false, std::sync::atomic::Ordering::Release);
            display.upload_due = true;
        }
        if shown {
            self.request_frame();
        }
    }

    /// The remote clipboard changed: believe it if it can be believed.
    fn display_clipboard(&mut self, id: PaneId, generation: u64, text: String) {
        if self.pane_generation(id) != Some(generation) {
            return;
        }
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(display) = pane.content.display_mut() else { return };
        match display.clipboard_in(&text) {
            ClipboardOutcome::Pass => self.core.clipboard.store(ClipboardType::Clipboard, text),
            ClipboardOutcome::Withheld { first_time } => {
                if first_time {
                    pane.notice = Some(VNC_CLIPBOARD_NOTICE.to_owned());
                    self.link_changed(id);
                }
            },
        }
    }

    /// Upload what is due for the panes on screen, marking those whose picture changed.
    fn upload_displays(&mut self, now: Instant) {
        #[cfg(feature = "bench")]
        let started = Instant::now();
        let mut uploaded = false;
        for index in 0..self.win.rects.len() {
            let id = self.win.rects[index].0;
            let Some(pane) = self.pane_mut(id) else { continue };
            let Some(display) = pane.content.display_mut() else { continue };
            if display.upload_due {
                uploaded = true;
                pane.dirty |= display.upload();
                display.next_poll = display.upload_due.then_some(now + remote::UPLOAD_POLL);
            }
        }
        if uploaded {
            self.win.gfx.flush();
            #[cfg(feature = "bench")]
            crate::bench::note_remote(started.elapsed());
        }
    }

    /// Ask for frames still owed to desktops on screen, and for the sizes of panes that have
    /// settled. Returns when next to look; nothing at all when no desktop has anything pending,
    /// so an idle pane costs no wakeups.
    fn tick_displays(&mut self, now: Instant) -> Option<Instant> {
        let animating = self.win.motion.running(now);
        let mut wake: Option<Instant> = None;
        let mut frame = false;
        for index in 0..self.win.rects.len() {
            let id = self.win.rects[index].0;
            let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut()) else { continue };
            if let Some(at) = display.next_poll {
                if at <= now {
                    frame = true;
                    display.next_poll = None;
                } else {
                    wake = Some(wake.map_or(at, |wake| wake.min(at)));
                }
            }
            if let Some(due) = display.resize_deadline() {
                if due > now {
                    wake = Some(wake.map_or(due, |wake| wake.min(due)));
                } else if animating {
                    // Never during an animation: frames are running, so this is looked at again.
                    wake = Some(now + Duration::from_millis(16));
                } else {
                    display.settle(now);
                }
            }
        }
        if frame {
            self.request_frame();
        }
        wake
    }

    /// Where the pointer at window position `point` lands on the desktop of `id`.
    fn display_layout(&self, id: PaneId) -> Option<(rmx_render::RemoteLayout, Rect)> {
        let pane = self.pane(id)?;
        let display = pane.content.display()?;
        let desktop = display.desktop()?;
        let &(_, rest) = self.win.rects.iter().find(|&&(pane, _)| pane == id)?;
        let now = Instant::now();
        let rect = self.win.motion.frame(id, now).unwrap_or(rest);
        let (rect, rest) = (self.terminal_rect(rect), self.terminal_rect(rest));
        Some((self.win.gfx.remote_layout(display.mode(), desktop, rect, rest), rest))
    }

    fn display_point(&self, id: PaneId) -> Option<(u32, u32, bool)> {
        let (layout, _) = self.display_layout(id)?;
        let (_, height) = self.win.gfx.window_size();
        let at = layout.pointer(height, self.win.pointer.x, self.win.pointer.y);
        Some((at.x, at.y, at.inside))
    }

    /// Send Ctrl+Alt+Del to the desktop of `id`.
    fn salute(&mut self, id: PaneId) {
        if let Some(display) = self.pane_mut(id).and_then(|pane| pane.content.display_mut()) {
            display.ctrl_alt_del();
        }
    }

    /// Show the desktop of `id` the other way.
    fn toggle_scale(&mut self, id: PaneId) {
        use rmx_ui::widgets::ScaleChoice;
        if let Some(pane) = self.pane_mut(id)
            && let Some(display) = pane.content.display_mut()
        {
            display.scale = match display.scale {
                ScaleChoice::Fit => ScaleChoice::Native,
                ScaleChoice::Native => ScaleChoice::Fit,
            };
            display.scroll = [0, 0];
            pane.dirty = true;
        }
        self.redraw_all();
    }

    /// Browse the files of the SSH pane `id`, at its working directory if it has said what that
    /// is, in a tab of its own on a connection of its own.
    fn open_files_here(&mut self, id: PaneId) {
        let Some(pane) = self.pane(id) else { return };
        let Some(remote) = &pane.remote else { return };
        let directory = pane.content.terminal().and_then(Terminal::working_directory).map(|cwd| cwd.path);
        let node = remote.node;
        self.open_node(node, Goal::Files { directory });
    }

    fn set_title(&mut self, id: PaneId, title: Option<String>) {
        if let Some(pane) = self.pane_mut(id) {
            pane.title = title;
        }
        self.sync_focus(Instant::now());
    }

    /// Start a terminal for the pane `id`, which is already in the workspace and stays blank
    /// until it arrives. Starting a process takes too long for the main thread.
    fn spawn(&mut self, id: PaneId) -> bool {
        let (area, spacing) = (self.area(), self.spacing());
        let rect = self.win.workspace.rects(area, spacing).into_iter().find(|&(pane, _)| pane == id);
        let size = self.win.gfx.size_for(self.terminal_rect(rect.map_or(area, |(_, rect)| rect)));

        let sink: Arc<dyn Sink> = self.core.sink.clone();
        let proxy = self.core.sink.0.clone();
        let theme = *self.win.gfx.theme();
        let spawn = move || {
            let shell = startup_shell();
            #[cfg(feature = "bench")]
            let started = Instant::now();
            let terminal = Terminal::spawn(id, size, shell, theme, sink);
            #[cfg(feature = "bench")]
            crate::bench::present::note("pane", || format!("terminal {id} spawned in {}us", started.elapsed().as_micros()));
            let _ = proxy.send_event(AppEvent::Spawned(id, Box::new(terminal)));
        };
        match std::thread::Builder::new().spawn(spawn) {
            Ok(_) => true,
            Err(err) => {
                log::error!("Could not start a terminal: {err}");
                false
            },
        }
    }

    /// A terminal has arrived from the thread that started it.
    fn spawned(&mut self, id: PaneId, terminal: io::Result<Terminal>) {
        let mut terminal = match terminal {
            Ok(terminal) => terminal,
            Err(err) => {
                log::error!("Could not start a terminal: {err}");
                self.program_ended(id);
                return;
            },
        };

        // Closed while it started.
        if !self.win.workspace.contains(id) {
            self.retire(PaneContent::Terminal(Box::new(terminal)));
            return;
        }
        if let Some(index) = self.core.ended_early.iter().position(|&pane| pane == id) {
            self.core.ended_early.remove(index);
            self.program_ended(id);
            return;
        }

        // The layout may have changed while it started.
        let (area, spacing) = (self.area(), self.spacing());
        if let Some((_, rect)) =
            self.win.workspace.rects(area, spacing).into_iter().find(|&(pane, _)| pane == id)
        {
            terminal.resize(self.win.gfx.size_for(self.terminal_rect(rect)));
        }
        if let Some(pane) = self.pane_mut(id) {
            pane.content = PaneContent::Terminal(Box::new(terminal));
            pane.dirty = true;
        }
        self.sync_focus(Instant::now());
        self.request_frame();
    }

    /// The program of pane `id` ended by itself, or could not start.
    ///
    /// `rmx -e` is a terminal for that one program, as `scripts/headless.sh` and the throughput
    /// bench rely on: once it is over and nothing else is showing, so is the application.
    fn program_ended(&mut self, id: PaneId) {
        let local = self.pane(id).is_some_and(|pane| pane.remote.is_none());
        self.close(id);
        if local && self.win.workspace.tabs().is_empty() && startup_shell().is_some() {
            self.win.vacated = Some(WhenLast::Quit);
        }
    }

    fn retire(&self, content: PaneContent) {
        self.core.retire(content);
    }

    /// Rows a file pane no longer shows, which are too many to free on this thread.
    fn retire_rows(&self, rows: Option<Arc<Vec<rmx_ui::widgets::FileRow>>>) {
        if let Some(rows) = rows {
            self.core.runtime.spawn_blocking(move || drop(rows));
        }
    }

    /// News from the tasks behind file pane `id`.
    fn files_event(&mut self, id: PaneId, generation: u64, event: FilesEvent) {
        let Some(files) = self.pane_mut(id).and_then(|pane| pane.content.files_mut()) else { return };
        if files.generation() != generation {
            return;
        }
        let applied = files.apply(event);
        if let Some(side) = applied.moved {
            self.core.ui.files_reset(id, side);
        }
        if let Some(path) = &applied.conflict {
            let mut state = PromptState::new(format!("{path} has changed since you opened it"), "", Vec::new());
            state.submit = "Overwrite".to_owned();
            self.ask_here(state, Reply::FilesOverwrite(id));
        }
        if let Some(files) = self.pane_mut(id).and_then(|pane| pane.content.files_mut()) {
            for (side, directory) in &applied.refresh {
                files.refresh_if_showing(*side, directory);
            }
        }
        self.retire_rows(applied.retired);
        self.win.chrome.invalidate();
        self.request_frame();
    }

    /// Open the saved connection `node` in a new tab, which shows at once that it is connecting.
    fn open_connection(&mut self, node: u64) {
        self.open_node(node, Goal::Session);
    }

    fn open_node(&mut self, node: u64, goal: Goal) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        if info.kind == NodeKind::Folder {
            return;
        }
        let name = info.name.clone();
        let id = self.new_id();
        self.add_pane(Pane::remote(id, node, name, goal));
        self.win.workspace.add_tab(id);
        self.start_session(id);
        self.relayout(false);
    }

    /// Start connecting the remote pane `id`, whose earlier screen stays until there is a new one.
    fn start_session(&mut self, id: PaneId) {
        #[cfg(feature = "bench")]
        crate::bench::present::note("pane", || format!("session {id}"));
        let (area, spacing) = (self.area(), self.spacing());
        let rect = self.win.workspace.rects(area, spacing).into_iter().find(|&(pane, _)| pane == id);
        let content = self.terminal_rect(rect.map_or(area, |(_, rect)| rect));
        let size = self.win.gfx.size_for(content);
        let pixels = (content.w as u32, content.h as u32);
        let theme = *self.win.gfx.theme();
        let vault = self.core.vault.clone();
        let proxy = self.core.proxy.clone();
        let sink: Arc<dyn Sink> = self.core.sink.clone();
        let runtime = self.core.runtime.handle().clone();

        let Some(pane) = self.pane_mut(id) else { return };
        let Some(remote) = pane.remote.as_mut() else { return };
        let (cancel_sender, cancel) = oneshot::channel();
        remote.generation += 1;
        remote.cancel = Some(cancel_sender);
        remote.forwards = 0;
        let request = ConnectRequest {
            pane: id,
            generation: remote.generation,
            node: remote.node,
            size,
            pixels,
            goal: remote.goal.clone(),
            theme,
            vault,
            proxy,
            sink,
            runtime: runtime.clone(),
            cancel,
        };
        pane.link = Link::Connecting { stage: "Reading the vault".to_owned() };
        pane.notice = None;
        pane.dirty = true;
        runtime.spawn(connect::run(request));
        self.link_changed(id);
    }

    /// Give up on what the pane `id` is doing, leaving the pane.
    fn hang_up(&mut self, id: PaneId, reason: &str) {
        self.dismiss_prompts(id);
        let Some(pane) = self.pane_mut(id) else { return };
        pane.hang_up();
        if matches!(pane.link, Link::Connecting { .. }) {
            pane.link = Link::Failed { reason: reason.to_owned() };
        }
        self.link_changed(id);
    }

    /// News of a connection attempt.
    fn link_event(&mut self, id: PaneId, generation: u64, event: LinkEvent) {
        let current = self
            .pane(id)
            .and_then(|pane| pane.remote.as_ref())
            .is_some_and(|remote| remote.generation == generation);
        if !current {
            match event {
                LinkEvent::Ready(Ready::Terminal(terminal)) => {
                    self.retire(PaneContent::Terminal(terminal));
                },
                // Nothing of these is on the GPU yet, and dropping them ends the session.
                LinkEvent::Ready(Ready::Display(_) | Ready::Files(_)) => (),
                _ => (),
            }
            return;
        }
        match event {
            LinkEvent::Stage(stage) => {
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Connecting { stage };
                }
            },
            LinkEvent::Target(target) => {
                if let Some(remote) = self.pane_mut(id).and_then(|pane| pane.remote.as_mut()) {
                    remote.target = target;
                }
            },
            LinkEvent::Ready(ready) => {
                // The window may have changed while it connected.
                let (area, spacing) = (self.area(), self.spacing());
                let rect = self.win.workspace.rects(area, spacing).into_iter().find(|r| r.0 == id);
                let content = match ready {
                    Ready::Terminal(mut terminal) => {
                        if let Some((_, rect)) = rect {
                            terminal.resize(self.win.gfx.size_for(self.terminal_rect(rect)));
                        }
                        PaneContent::Terminal(terminal)
                    },
                    Ready::Display(link) => {
                        let mut display = Display::new(*link);
                        if let Some((_, rect)) = rect {
                            let settled = self.terminal_rect(rect);
                            display.pane_resized(settled.w as u32, settled.h as u32, Instant::now());
                        }
                        PaneContent::Display(Box::new(display))
                    },
                    Ready::Files(link) => {
                        let (runtime, proxy) = (self.core.runtime.handle().clone(), self.core.proxy.clone());
                        PaneContent::Files(Box::new(FilesPane::new(
                            id,
                            generation,
                            *link,
                            files::local_home(),
                            runtime,
                            proxy,
                        )))
                    },
                };
                let Some(pane) = self.pane_mut(id) else { return };
                let old = std::mem::replace(&mut pane.content, content);
                pane.link = Link::Live;
                pane.dirty = true;
                self.retire(old);
                self.sync_focus(Instant::now());
            },
            LinkEvent::Missing(why) => {
                self.dismiss_prompts(id);
                if let Some(pane) = self.pane_mut(id) {
                    let quiet = pane.remote.as_ref().is_some_and(|remote| remote.quiet);
                    pane.link = if quiet { Link::Awaiting { why } } else { Link::Failed { reason: why } };
                    pane.hang_up();
                }
            },
            LinkEvent::Closed(reason) => {
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Closed { reason };
                    pane.hang_up();
                }
            },
            LinkEvent::Failed(reason) => {
                self.dismiss_prompts(id);
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Failed { reason };
                    pane.hang_up();
                }
            },
            LinkEvent::Forwards(running, errors) => {
                if let Some(pane) = self.pane_mut(id) {
                    if let Some(remote) = pane.remote.as_mut() {
                        remote.forwards = running;
                    }
                    if !errors.is_empty() {
                        pane.notice = Some(format!("Port forward failed: {}", errors.join("; ")));
                    }
                }
            },
        }
        self.link_changed(id);
    }

    /// What happened to the session of a terminal.
    fn session_event(&mut self, id: PaneId, event: SessionEvent) {
        match event {
            SessionEvent::OutputDiscarded(bytes) => {
                if let Some(pane) = self.pane_mut(id) {
                    let amount = if bytes >= MIB {
                        format!("{} MiB", bytes / MIB)
                    } else {
                        format!("{} KiB", bytes.div_ceil(1024))
                    };
                    pane.notice = Some(format!("Output could not keep up: {amount} of it was lost"));
                }
                self.link_changed(id);
            },
            SessionEvent::Closed(reason) => {
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Closed { reason: reason.to_string() };
                    pane.hang_up();
                }
                self.link_changed(id);
            },
            _ => (),
        }
    }

    /// Queue a question for the user, and show it if nothing else is showing.
    fn ask(&mut self, prompt: Prompt) {
        if let Some(id) = prompt.pane
            && self.pane(id).and_then(|pane| pane.remote.as_ref()).is_some_and(|remote| remote.quiet)
        {
            self.leave_awaiting(id, &prompt);
            return;
        }
        self.core.prompts.push_back(prompt);
        self.next_prompt();
    }

    /// A session reopened at startup needs the user: stop it there, saying what for. Dropping the
    /// prompt tells the attempt there will be no answer.
    fn leave_awaiting(&mut self, id: PaneId, prompt: &Prompt) {
        let why = match &prompt.kind {
            PromptKind::HostKey(..) => "The server's key needs your word".to_owned(),
            PromptKind::Fields { title, .. } => title.clone(),
        };
        let Some(pane) = self.pane_mut(id) else { return };
        pane.hang_up();
        if let Some(remote) = pane.remote.as_mut() {
            // Whatever it reports from here on is about the attempt given up on.
            remote.generation += 1;
        }
        pane.link = Link::Awaiting { why };
        self.link_changed(id);
    }

    /// Ask something of the app's own, ahead of anything waiting.
    fn ask_here(&mut self, state: PromptState, reply: Reply) {
        if self.core.answer.is_none() {
            self.core.ui.asking = Some(Asking::Fields(state));
            self.core.ui.asking_window = self.win.key;
            self.core.answer = Some((reply, None));
            self.core.broadcast = true;
        }
    }

    /// Put the next question that still matters in the dialog. While the vault is locked only a
    /// question from syncing can be asked: sessions are out of sight.
    fn next_prompt(&mut self) {
        if self.core.answer.is_some() {
            return;
        }
        let locked = self.core.ui.locked();
        self.core.prompts.retain(|prompt| !prompt.abandoned());
        let Some(index) = self.core.prompts.iter().position(|p| !locked || p.pane.is_none()) else { return };
        let Some(Prompt { pane, kind }) = self.core.prompts.remove(index) else { return };
        let (asking, reply) = match kind {
            PromptKind::HostKey(prompt, reply) => (Asking::HostKey(prompt), Reply::HostKey(reply)),
            PromptKind::Fields { title, detail, fields, reply } => {
                (Asking::Fields(PromptState::new(title, detail, fields)), Reply::Fields(reply))
            },
        };
        // Over the pane it is about; a question about nothing goes where the user is.
        let window = pane.and_then(|id| self.pane(id)).map_or(self.core.active, |pane| pane.window);
        self.core.ui.asking = Some(asking);
        self.core.ui.asking_window = window;
        self.core.answer = Some((reply, pane));
        self.core.broadcast = true;
    }

    /// The questions about `pane` no longer matter.
    fn dismiss_prompts(&mut self, pane: PaneId) {
        self.core.prompts.retain(|prompt| prompt.pane != Some(pane));
        if matches!(&self.core.answer, Some((_, Some(id))) if *id == pane) {
            self.core.answer = None;
            self.core.ui.asking = None;
            self.core.broadcast = true;
            self.next_prompt();
        }
    }

    /// Finish the question on screen.
    fn answered(&mut self, host_key: Option<HostKeyAnswer>, fields: Option<Vec<Sensitive>>) {
        let Some((reply, _)) = self.core.answer.take() else { return };
        self.core.ui.asking = None;
        let first = fields.as_ref().and_then(|fields| fields.first()).map(|f| f.expose().trim().to_owned());
        match reply {
            Reply::HostKey(reply) => {
                let _ = reply.send(host_key.unwrap_or(HostKeyAnswer::Reject));
            },
            Reply::Fields(reply) => {
                let answers = fields.map(|fields| {
                    fields.iter().map(|field| SecretString::new(field.expose().to_owned())).collect()
                });
                let _ = reply.send(answers);
            },
            Reply::NewFolder(parent) => {
                if let Some(name) = first.filter(|name| !name.is_empty()) {
                    self.core.vault.send(Request::NewFolder { parent, name });
                }
            },
            Reply::RenameFolder(node) => {
                if let Some(name) = first.filter(|name| !name.is_empty()) {
                    self.core.vault.send(Request::RenameFolder { node, name });
                }
            },
            Reply::Snippet(body) => {
                if let Some(fields) = &fields {
                    let names = snippet::variables(&body);
                    let values: Vec<(&str, &str)> =
                        names.iter().zip(fields).map(|(variable, field)| (variable.name, field.expose())).collect();
                    let text = snippet::expand(&body, &values);
                    self.send_text(&text);
                }
            },
            Reply::FilesNewFolder(pane, side) => {
                if let Some(name) = first.filter(|name| !name.is_empty()) {
                    self.files_action(pane, FilesAction::NewFolder { side, name });
                }
            },
            Reply::FilesRename(pane, side, from) => {
                if let Some(to) = first.filter(|name| !name.is_empty() && *name != *from) {
                    self.files_action(pane, FilesAction::Rename { side, from, to });
                }
            },
            Reply::FilesDelete(pane, side, names) => {
                if fields.is_some() {
                    self.files_action(pane, FilesAction::Delete { side, names });
                }
            },
            Reply::FilesOverwrite(pane) => {
                if fields.is_some() {
                    self.files_action(pane, FilesAction::SaveAnyway);
                }
            },
            Reply::LoseEdits(panes, then) => {
                if fields.is_some() {
                    for pane in panes {
                        self.core.ui.files_forget_edits(pane);
                    }
                    if then == UiAction::WindowClose {
                        self.win.exit = true;
                    } else {
                        self.ui_action(then);
                    }
                }
            },
            Reply::VaultPath => {
                if let Some(path) = first.filter(|path| !path.is_empty()) {
                    self.core.ui.vault_error = None;
                    self.core.vault.send(Request::Switch(path));
                }
            },
            Reply::DeleteConnection(node) => {
                if fields.is_some() {
                    self.core.vault.send(Request::DeleteConnection(node));
                }
            },
            Reply::DeleteFolder(node) => {
                if fields.is_some() {
                    self.core.vault.send(Request::DeleteFolder(node));
                }
            },
        }
        self.core.broadcast = true;
        self.next_prompt();
    }

    /// Hand a typed password to the vault's thread, and show that it is working.
    fn start_vault_work(&mut self) {
        self.core.ui.vault_busy = true;
        self.core.ui.vault_error = None;
        self.core.broadcast = true;
    }

    /// Lay this window out as it was left, starting what was in it. What no longer exists is
    /// left out.
    fn restore(&mut self, saved: SavedWindow<SavedPane>) {
        let SavedWindow { sidebar_open, maximized, active, tabs, .. } = saved;
        self.win.shell.sidebar_open = sidebar_open;
        self.win.maximized = maximized;
        let mut start = Vec::new();
        let active_id = active;
        for (index, tab) in tabs.into_iter().enumerate() {
            let SavedTab { layout, focus, zoomed, name } = tab;
            let mut gone = Vec::new();
            let order = layout.panes();
            let focused = order.get(focus).copied();
            let mut focus_id = None;
            let layout = layout.map(&mut |saved| {
                let id = self.new_id();
                if Some(saved) == focused && focus_id.is_none() {
                    focus_id = Some(id);
                }
                match saved {
                    SavedPane::Local => {
                        self.add_pane(Pane::local(id));
                        start.push(id);
                    },
                    SavedPane::Node(node) | SavedPane::Files(node) => {
                        let goal = match saved {
                            SavedPane::Files(_) => Goal::Files { directory: None },
                            _ => Goal::Session,
                        };
                        match self.core.ui.nodes.get(&node).map(|info| info.name.clone()) {
                            Some(name) => {
                                self.add_pane(Pane::remote(id, node, name, goal));
                                start.push(id);
                            },
                            None => gone.push(id),
                        }
                    },
                }
                id
            });
            let mut remaining = Some(layout);
            for id in gone {
                remaining = remaining.and_then(|layout| layout.remove(id));
            }
            if let Some(layout) = remaining {
                let panes = layout.panes();
                let focus = focus_id.filter(|id| panes.contains(id)).unwrap_or(panes[0]);
                self.win.workspace.restore_tab(layout, focus, zoomed, name, index == active_id);
            }
        }
        for id in start {
            let is_remote = self.pane(id).is_some_and(|pane| pane.remote.is_some());
            if is_remote {
                if let Some(remote) = self.pane_mut(id).and_then(|pane| pane.remote.as_mut()) {
                    remote.quiet = true;
                }
                self.start_session(id);
            } else if !self.spawn(id) {
                self.close(id);
            }
        }
        self.relayout(false);
    }

    /// Count `pane` as this window's.
    fn add_pane(&mut self, mut pane: Pane) {
        #[cfg(feature = "bench")]
        crate::bench::present::note("pane", || format!("added {}", pane.id));
        pane.window = self.win.key;
        self.core.panes.push(pane);
    }

    fn new_id(&mut self) -> PaneId {
        self.core.next_id += 1;
        self.core.next_id
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::NewTab => {
                let id = self.new_id();
                self.add_pane(Pane::local(id));
                self.win.workspace.add_tab(id);
                if self.spawn(id) {
                    self.relayout(false);
                } else {
                    self.close(id);
                }
            },
            Action::Split(axis) => self.split_focused(axis),
            Action::SplitLocal(axis) => self.split_local(axis),
            Action::ClosePane => {
                if let Some(id) = self.win.workspace.focused() {
                    self.ui_action(UiAction::PaneClose(rmx_ui::PaneId(id)));
                }
            },
            Action::Focus(direction) => {
                let (area, spacing) = (self.area(), self.spacing());
                let zoomed = self.win.workspace.active().is_some_and(|tab| tab.zoomed);
                if self.win.workspace.focus_direction(direction, area, spacing) {
                    if zoomed {
                        self.relayout(true);
                    } else {
                        self.sync_focus(Instant::now());
                        self.redraw_all();
                    }
                }
            },
            Action::Zoom => {
                self.win.workspace.toggle_zoom();
                self.relayout(true);
            },
            Action::CycleTab(delta) => {
                if self.win.workspace.cycle(delta) {
                    self.relayout(false);
                }
            },
            Action::SelectTab(index) => {
                if self.win.workspace.select(index) {
                    self.relayout(false);
                }
            },
            Action::Copy => {
                let text = self.focused_terminal().and_then(Terminal::selection);
                if let Some(text) = text {
                    self.core.clipboard.store(ClipboardType::Clipboard, text);
                }
            },
            Action::Paste => {
                let text = self.core.clipboard.load(ClipboardType::Clipboard);
                if let Some(terminal) = self.focused_terminal() {
                    terminal.paste(&text);
                } else if let Some(id) = self.win.display_focus {
                    self.offer_clipboard(id);
                }
            },
            Action::SendCtrlAltDel => {
                if let Some(id) = self.win.workspace.focused() {
                    self.salute(id);
                }
            },
            Action::ToggleScale => {
                if let Some(id) = self.win.workspace.focused() {
                    self.toggle_scale(id);
                }
            },
            Action::QuickOpen => {
                if self.win.shell.quick.is_open() {
                    self.win.shell.quick.close();
                } else {
                    self.win.shell.open_palette(&self.win.workspace, &self.core.panes, &self.core.ui);
                }
                self.win.chrome.invalidate();
            },
            Action::NewConnection => {
                self.win.shell.dialog.open();
                self.win.chrome.invalidate();
            },
            Action::Snippets => {
                self.win.shell.snippets.open();
                self.win.chrome.invalidate();
            },
            Action::ToggleSidebar => {
                self.win.shell.sidebar_open = !self.win.shell.sidebar_open;
                self.core.vault.send(Request::SaveLayout(self.layout()));
                self.relayout(false);
            },
            Action::OpenSettings => self.core.vault.send(Request::LoadSettings { theme: self.core.theme }),
            Action::ToggleTheme => {
                let mode = if self.core.theme == ThemeMode::Dark { ThemeMode::Light } else { ThemeMode::Dark };
                self.core.ops.push(Op::SetTheme(mode));
            },
            Action::Lock => self.core.vault.send(Request::Lock),
            Action::SyncNow => {
                if self.core.ui.sync != SyncState::Off {
                    self.core.ui.sync = SyncState::Running;
                    self.win.chrome.invalidate();
                }
                self.core.vault.send(Request::SyncNow);
            },
        }
    }

    fn focused_terminal(&self) -> Option<&Terminal> {
        let id = self.win.workspace.focused()?;
        self.pane(id)?.content.terminal()
    }

    fn pane_is_ssh(&self, pane: PaneId) -> bool {
        let node = self.pane(pane).and_then(|found| found.remote.as_ref()).map(|remote| remote.node);
        node.and_then(|node| self.core.ui.nodes.get(&node))
            .is_some_and(|info| info.kind == NodeKind::Connection(Protocol::Ssh))
    }

    /// Open the session of `pane` again, as a saved connection and for the same thing.
    fn clone_of(&mut self, pane: PaneId) -> Option<(u64, String, Goal)> {
        let remote = self.pane(pane)?.remote.as_ref()?;
        let (node, goal) = (remote.node, remote.goal.clone());
        let info = self.core.ui.nodes.get(&node)?;
        Some((node, info.name.clone(), goal))
    }

    /// Put a new pane next to the focused one: the same session where another one makes sense,
    /// else a local shell.
    fn split_focused(&mut self, axis: Axis) {
        let same = self.win.workspace.focused().and_then(|pane| self.clone_of(pane)).filter(|(node, _, goal)| {
            let protocol = self.core.ui.nodes.get(node).map(|info| info.kind);
            *goal != Goal::Session
                || matches!(
                    protocol,
                    Some(NodeKind::Connection(Protocol::Ssh | Protocol::Telnet | Protocol::Sftp | Protocol::Ftp))
                )
        });
        let Some((node, name, goal)) = same else {
            self.split_local(axis);
            return;
        };
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::remote(id, node, name, goal));
            self.start_session(id);
            self.relayout(true);
        }
    }

    fn split_local(&mut self, axis: Axis) {
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::local(id));
            if self.spawn(id) {
                self.relayout(true);
            } else {
                self.close(id);
            }
        }
    }

    /// Open the same session as `pane` again, in a tab of its own.
    fn duplicate_session(&mut self, pane: PaneId) {
        match self.clone_of(pane) {
            Some((node, _, goal)) => self.open_node(node, goal),
            None => self.perform(Action::NewTab),
        }
    }

    /// Open the saved connection `node` beside the focused pane, or in a tab with none to go beside.
    fn open_node_split(&mut self, node: u64, axis: Axis) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        if info.kind == NodeKind::Folder {
            return;
        }
        let name = info.name.clone();
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::remote(id, node, name, Goal::Session));
            self.start_session(id);
            self.relayout(true);
        } else {
            self.open_node(node, Goal::Session);
        }
    }

    /// The panes of this window connected to `node`, most recent last.
    fn sessions_of(&self, node: u64) -> impl Iterator<Item = &Pane> {
        let key = self.win.key;
        self.core.panes.iter().filter(move |pane| {
            pane.window == key && pane.remote.as_ref().is_some_and(|remote| remote.node == node)
        })
    }

    /// Bring `pane` into view, with the keyboard.
    fn show_pane(&mut self, pane: PaneId) {
        let tab = self.win.workspace.tabs().iter().position(|tab| tab.layout.panes().contains(&pane));
        if let Some(tab) = tab {
            self.win.workspace.select(tab);
            self.win.workspace.focus(pane);
            self.relayout(false);
        }
    }

    /// What can be done from the menu of a node of the tree.
    fn tree_menu(&self, node: NodeId) -> Vec<MenuItem> {
        let Some(info) = self.core.ui.nodes.get(&node.0) else { return Vec::new() };
        if info.kind == NodeKind::Folder {
            return vec![
                menu_item(icons::PLUS, "New connection", UiAction::NewConnection, false),
                menu_item(icons::FOLDER, "New folder", UiAction::TreeNewFolder(Some(node)), false),
                menu_item(icons::PENCIL, "Rename", UiAction::TreeRenameFolder(node), false),
                MenuItem::Separator,
                menu_item(icons::TRASH_2, "Delete folder", UiAction::TreeDelete(node), true),
            ];
        }
        let mut items = vec![
            menu_item(icons::PLUG, "Connect", UiAction::TreeOpen(node), false),
            menu_item(icons::COLUMNS_2, "Connect in split right", UiAction::TreeOpenSplit { id: node, down: false }, false),
            menu_item(icons::ROWS_2, "Connect in split down", UiAction::TreeOpenSplit { id: node, down: true }, false),
        ];
        if info.kind == NodeKind::Connection(Protocol::Ssh) {
            items.push(menu_item(icons::FOLDER_SYNC, "Files", UiAction::TreeOpenFiles(node), false));
        }
        let has_session = self.sessions_of(node.0).next().is_some();
        let live = self.sessions_of(node.0).any(|pane| pane.link == Link::Live);
        let down = self.sessions_of(node.0).any(|pane| matches!(pane.link, Link::Failed { .. } | Link::Closed { .. } | Link::Awaiting { .. }));
        if has_session {
            items.push(MenuItem::Separator);
            items.push(menu_item(icons::SEARCH, "Show session", UiAction::TreeShow(node), false));
            if down {
                items.push(menu_item(icons::PLUG, "Reconnect", UiAction::TreeReconnect(node), false));
            }
            if live {
                items.push(menu_item(icons::X, "Disconnect", UiAction::TreeDisconnect(node), false));
            }
        }
        items.push(MenuItem::Separator);
        items.push(menu_item(icons::PENCIL, "Edit", UiAction::TreeEdit(node), false));
        items.push(menu_item(icons::COPY, "Duplicate", UiAction::TreeDuplicate(node), false));
        if !info.host.is_empty() {
            items.push(menu_item(icons::COPY, "Copy host", UiAction::CopyText(info.host.clone()), false));
        }
        items.push(MenuItem::Separator);
        items.push(menu_item(icons::TRASH_2, "Delete", UiAction::TreeDelete(node), true));
        items
    }

    /// What can be done about the session of `pane`.
    fn session_items(&self, pane: PaneId, items: &mut Vec<MenuItem>) {
        let id = rmx_ui::PaneId(pane);
        let Some(found) = self.pane(pane) else { return };
        match &found.link {
            Link::Live => {
                if found.content.display().is_some() {
                    items.push(menu_item(icons::MONITOR, "Send Ctrl+Alt+Del", UiAction::PaneCtrlAltDel(id), false));
                    items.push(menu_item(icons::MONITOR_COG, "Toggle scale", UiAction::PaneToggleScale(id), false));
                }
                if found.content.terminal().is_some() && self.pane_is_ssh(pane) {
                    items.push(menu_item(icons::FOLDER_SYNC, "Files here", UiAction::PaneOpenFiles(id), false));
                }
                items.push(menu_item(icons::PLUG, "Disconnect", UiAction::PaneDisconnect(id), false));
            },
            Link::Closed { .. } | Link::Failed { .. } | Link::Awaiting { .. } => {
                items.push(menu_item(icons::PLUG, "Reconnect", UiAction::PaneReconnect(id), false));
            },
            Link::Connecting { .. } => {
                items.push(menu_item(icons::X, "Cancel", UiAction::PaneCancel(id), false));
            },
            Link::Local => (),
        }
        items.push(menu_item(icons::COPY, "Duplicate session", UiAction::PaneDuplicate(id), false));
    }

    /// Splitting and zooming `pane`.
    fn layout_items(&self, pane: PaneId, items: &mut Vec<MenuItem>) {
        let id = rmx_ui::PaneId(pane);
        items.push(menu_keyed(icons::COLUMNS_2, "Split right", "Ctrl+Shift+E", UiAction::PaneSplitRight(id)));
        items.push(menu_keyed(icons::ROWS_2, "Split down", "Ctrl+Shift+O", UiAction::PaneSplitDown(id)));
        let tab = self.win.workspace.tabs().iter().find(|tab| tab.layout.panes().contains(&pane));
        if tab.is_some_and(|tab| tab.layout.panes().len() > 1) {
            let zoomed = tab.is_some_and(|tab| tab.zoomed);
            let (glyph, label) = if zoomed { (icons::COPY, "Unzoom") } else { (icons::SQUARE, "Zoom") };
            items.push(menu_keyed(glyph, label, "Ctrl+Shift+Z", UiAction::PaneZoom(id)));
        }
    }

    /// What can be done from the menu of a pane.
    fn pane_menu(&self, pane: PaneId) -> Vec<MenuItem> {
        let id = rmx_ui::PaneId(pane);
        let mut items = Vec::new();
        if let Some(terminal) = self.pane(pane).and_then(|found| found.content.terminal()) {
            if terminal.selection().is_some() {
                items.push(menu_keyed(icons::COPY, "Copy", "Ctrl+Shift+C", UiAction::PaneCopy(id)));
            }
            items.push(menu_keyed(icons::CORNER_DOWN_LEFT, "Paste", "Ctrl+Shift+V", UiAction::PanePaste(id)));
            items.push(MenuItem::Separator);
        }
        let before = items.len();
        self.session_items(pane, &mut items);
        if items.len() > before {
            items.push(MenuItem::Separator);
        }
        self.layout_items(pane, &mut items);

        let tabs = self.win.workspace.tabs();
        let grouped = tabs.iter().any(|tab| tab.layout.panes().len() > 1 && tab.layout.panes().contains(&pane));
        if grouped || tabs.len() > 1 {
            items.push(MenuItem::Separator);
        }
        if grouped {
            items.push(menu_item(icons::ARROW_RIGHT, "Move to new tab", UiAction::PaneToTab(id), false));
        }
        if grouped || tabs.len() > 1 {
            items.push(menu_item(icons::SQUARE, "Move to new window", UiAction::PaneToWindow(id), false));
        }
        items.push(MenuItem::Separator);
        items.push(menu_keyed(icons::X, "Close", "Ctrl+Shift+W", UiAction::PaneClose(id)));
        items
    }

    /// What can be done from the menu of the tab `id`.
    fn tab_menu(&self, id: TabId) -> Vec<MenuItem> {
        let Some(index) = self.win.workspace.tab_index(id.0) else { return Vec::new() };
        let tab = &self.win.workspace.tabs()[index];
        let mut items = vec![menu_item(icons::PENCIL, "Rename", UiAction::TabStartRename(id), false)];
        if tab.name.is_some() {
            items.push(menu_item(icons::CORNER_DOWN_LEFT, "Reset name", UiAction::TabRename { id, name: None }, false));
        }
        items.push(MenuItem::Separator);
        self.session_items(tab.focus, &mut items);
        items.push(MenuItem::Separator);
        self.layout_items(tab.focus, &mut items);
        items.push(MenuItem::Separator);
        if tab.layout.panes().len() > 1 {
            items.push(menu_item(icons::COLUMNS_2, "Ungroup", UiAction::TabUngroup(id), false));
        }
        if self.win.workspace.tabs().len() > 1 {
            items.push(menu_item(icons::SQUARE, "Move to new window", UiAction::TabDetach(id), false));
        }
        for (number, &window) in self.core.window_keys.iter().enumerate() {
            if window != self.win.key {
                let label = format!("Move to window {}", number + 1);
                items.push(menu_item(icons::CORNER_DOWN_LEFT, &label, UiAction::TabToWindow(id, window), false));
            }
        }
        items.push(menu_item(icons::X, "Close tab", UiAction::TabClose(id), true));
        items
    }

    /// What can be done from the menu of a column of a file pane.
    fn files_menu(&self, pane: PaneId, side: rmx_ui::FileSide, name: Option<Arc<str>>) -> Vec<MenuItem> {
        let id = rmx_ui::PaneId(pane);
        let act = |action| UiAction::Files(id, action);
        let Some(files) = self.pane(pane).and_then(|found| found.content.files()) else { return Vec::new() };
        let Some(name) = name else {
            return vec![
                menu_item(icons::FOLDER, "New folder", UiAction::FilesAskNewFolder(id, side), false),
                menu_item(icons::FOLDER_SYNC, "Refresh", act(FilesAction::Refresh { side }), false),
            ];
        };
        let names = self.core.ui.files_selected_or(pane, side, &name);
        let row = files.rows(side).iter().find(|row| row.name == name);
        let mut items = Vec::new();
        if row.is_some_and(rmx_ui::widgets::FileRow::openable) {
            items.push(menu_item(icons::FOLDER_OPEN, "Open", act(FilesAction::Open { side, name: name.clone() }), false));
        }
        if let Some((local, remote)) = self.core.ui.files_compare_pair(pane, &files.view(id), side, &name) {
            items.push(menu_item(icons::COLUMNS_2, "Compare", act(FilesAction::Compare { local, remote }), false));
        }
        let (glyph, label) = match side {
            rmx_ui::FileSide::Local => (icons::ARROW_RIGHT, "Upload"),
            rmx_ui::FileSide::Remote => (icons::ARROW_LEFT, "Download"),
        };
        items.push(menu_item(glyph, label, act(FilesAction::Transfer { from: side, names: names.clone() }), false));
        let path = format!("{}/{name}", files.path(side).trim_end_matches('/'));
        items.push(menu_item(icons::COPY, "Copy path", UiAction::CopyText(path), false));
        items.push(MenuItem::Separator);
        if names.len() == 1 {
            items.push(menu_item(icons::PENCIL, "Rename", UiAction::FilesAskRename(id, side, name.clone()), false));
        }
        items.push(menu_item(icons::FOLDER, "New folder", UiAction::FilesAskNewFolder(id, side), false));
        items.push(MenuItem::Separator);
        items.push(menu_item(icons::TRASH_2, "Delete", UiAction::FilesAskDelete(id, side, names), true));
        items
    }

    /// What can be done from the vault switcher.
    fn vault_menu(&self) -> Vec<MenuItem> {
        let ui = &self.core.ui;
        let row = |glyph, path: &str| menu_item(glyph, &shell::short_path(path), UiAction::VaultSwitch(path.to_owned()), false);
        let mut items = vec![row(icons::CHECK, &ui.vault_path)];
        items.extend(ui.vault_others.iter().map(|path| row(icons::KEY_ROUND, path)));
        items.push(MenuItem::Separator);
        items.push(menu_item(icons::FOLDER_OPEN, "Open vault", UiAction::VaultAskPath { create: false }, false));
        items.push(menu_item(icons::PLUS, "New vault", UiAction::VaultAskPath { create: true }, false));
        items
    }

    /// Ask before `then` loses edits made in any of `panes`. Returns whether it was asked.
    fn ask_before_losing_edits(&mut self, panes: Vec<PaneId>, then: UiAction) -> bool {
        let dirty: Vec<PaneId> = panes
            .into_iter()
            .filter(|&pane| {
                self.core.ui.files_dirty(pane)
                    && self.pane(pane).and_then(|found| found.content.files()).is_some_and(FilesPane::has_document)
            })
            .collect();
        if dirty.is_empty() {
            return false;
        }
        let mut state = PromptState::new("Discard your changes?", "", Vec::new());
        state.submit = "Discard".to_owned();
        self.ask_here(state, Reply::LoseEdits(dirty, then));
        true
    }

    fn ask_delete(&mut self, node: u64) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        let folder = info.kind == NodeKind::Folder;
        let name = info.name.clone();
        let (title, detail, reply) = if folder {
            (
                format!("Delete the folder {name}?"),
                "What is in it moves up to the top of the tree.",
                Reply::DeleteFolder(node),
            )
        } else {
            (
                format!("Delete {name}?"),
                "Its saved password and port forwards go with it.",
                Reply::DeleteConnection(node),
            )
        };
        let mut state = PromptState::new(title, detail, Vec::new());
        state.submit = "Delete".to_owned();
        self.ask_here(state, reply);
    }

    fn ask_folder_name(&mut self, title: &str, value: String, reply: Reply) {
        let field = PromptField { label: String::new(), secret: false, value };
        self.ask_here(PromptState::new(title, "", vec![field]), reply);
    }

    /// Give the keyboard to `pane`, as a click on it would.
    fn focus_pane(&mut self, pane: PaneId) {
        if self.win.workspace.focus(pane) {
            self.sync_focus(Instant::now());
            self.redraw_all();
        }
    }

    /// Do what the interface asked. Returns false once the application should end.
    fn ui_action(&mut self, action: UiAction) -> bool {
        // Only what the lock screen itself offers does anything while it is up.
        if self.core.ui.locked()
            && !matches!(
                action,
                UiAction::WindowMinimize
                    | UiAction::WindowToggleMaximize
                    | UiAction::WindowClose
                    | UiAction::WindowDrag
                    | UiAction::VaultUnlock { .. }
                    | UiAction::VaultCreate { .. }
                    | UiAction::VaultJoin { .. }
                    | UiAction::VaultMenu(_)
                    | UiAction::VaultSwitch(_)
                    | UiAction::VaultAskPath { .. }
                    | UiAction::HostKeyAcceptAlways
                    | UiAction::HostKeyAcceptOnce
                    | UiAction::HostKeyReject
                    | UiAction::PromptSubmit(_)
                    | UiAction::PromptCancel
            )
        {
            return true;
        }
        match action {
            UiAction::WindowMinimize => self.win.gfx.window.set_minimized(true),
            UiAction::WindowToggleMaximize => {
                let maximized = !self.win.gfx.window.is_maximized();
                self.win.gfx.window.set_maximized(maximized);
            },
            UiAction::WindowClose => {
                let key = self.win.key;
                let here = self.core.panes.iter().filter(|pane| pane.window == key).map(|pane| pane.id).collect();
                if !self.ask_before_losing_edits(here, UiAction::WindowClose) {
                    return false;
                }
            },
            UiAction::WindowDrag => {
                if let Err(err) = self.win.gfx.window.drag_window() {
                    log::warn!("Could not move the window: {err}");
                }
                self.win.chrome.cancel_pointer();
            },
            UiAction::ToggleSidebar => self.perform(Action::ToggleSidebar),
            UiAction::TabActivate(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    self.perform(Action::SelectTab(index));
                }
            },
            UiAction::TabClose(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    let panes = self.win.workspace.tabs()[index].layout.panes().to_vec();
                    if !self.ask_before_losing_edits(panes, UiAction::TabClose(TabId(id))) {
                        self.close_tab(index);
                    }
                }
            },
            UiAction::TabNew => self.perform(Action::NewTab),
            UiAction::TabReorder { id: TabId(id), to } => {
                if let Some(from) = self.win.workspace.tab_index(id) {
                    self.win.workspace.reorder_tab(from, to);
                    self.relayout(false);
                }
            },
            UiAction::TabContextMenu { id, pos } => {
                let items = self.tab_menu(id);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::TabStartRename(id) => {
                if let Some(tab) = self.win.workspace.tab_index(id.0).map(|index| &self.win.workspace.tabs()[index]) {
                    let name = tab.name.as_deref().unwrap_or(pane_label(&self.core.panes, tab.focus)).to_owned();
                    self.win.shell.start_tab_rename(id, &name);
                }
            },
            UiAction::TabRename { id: TabId(id), name } => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    self.win.workspace.set_tab_name(index, name);
                    self.core.session_changed();
                }
            },
            UiAction::TabUngroup(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id)
                    && self.win.workspace.ungroup(index)
                {
                    self.relayout(false);
                }
            },
            UiAction::TabDetach(TabId(id)) => {
                self.core.ops.push(Op::Tear { from: self.win.key, what: Moved::Tab(id) });
            },
            UiAction::TabToWindow(TabId(id), to) => {
                let (from, what) = (self.win.key, Moved::Tab(id));
                self.core.ops.push(Op::Move { from, what, to, at: usize::MAX });
            },
            UiAction::TabDragOut(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    self.win.shell.start_tab_drag(&self.win.workspace, index);
                }
            },
            UiAction::PaneSplitRight(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Split(Axis::Row));
            },
            UiAction::PaneSplitDown(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Split(Axis::Column));
            },
            UiAction::PaneMenu { id: rmx_ui::PaneId(pane), pos } => {
                self.focus_pane(pane);
                let items = self.pane_menu(pane);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::PaneZoom(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Zoom);
            },
            UiAction::PaneDuplicate(rmx_ui::PaneId(pane)) => self.duplicate_session(pane),
            UiAction::PaneToTab(rmx_ui::PaneId(pane)) => {
                let at = self.win.workspace.active_index() + 1;
                if self.win.workspace.detach(pane, at) {
                    self.relayout(false);
                }
            },
            UiAction::PaneToWindow(rmx_ui::PaneId(pane)) => {
                self.core.ops.push(Op::Tear { from: self.win.key, what: Moved::Pane(pane) });
            },
            UiAction::PaneCopy(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Copy);
            },
            UiAction::PanePaste(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Paste);
            },
            UiAction::PaneDragStart(rmx_ui::PaneId(pane)) => {
                self.win.shell.start_pane_drag(&self.win.workspace, pane);
            },
            UiAction::QuickOpenPick(id) => match self.win.shell.picked(id) {
                Some(Picked::Run(action)) => self.perform(action),
                Some(Picked::Connection(id)) => self.open_connection(id),
                Some(Picked::Snippet(id)) => self.run_snippet(id),
                None => (),
            },
            UiAction::NewConnection => self.perform(Action::NewConnection),
            UiAction::ConnectionSubmit(draft) => self.core.vault.send(Request::SaveConnection(draft)),
            UiAction::ConnectionCancel
            | UiAction::QuickOpenDismiss
            | UiAction::TreeSelect(_) => (),
            UiAction::TreeOpen(NodeId(node)) => self.open_connection(node),
            UiAction::TreeOpenSplit { id: NodeId(node), down } => {
                self.open_node_split(node, if down { Axis::Column } else { Axis::Row });
            },
            UiAction::TreeOpenFiles(NodeId(node)) => self.open_node(node, Goal::Files { directory: None }),
            UiAction::TreeShow(NodeId(node)) => {
                if let Some(pane) = self.sessions_of(node).last().map(|pane| pane.id) {
                    self.show_pane(pane);
                }
            },
            UiAction::TreeReconnect(NodeId(node)) => {
                let down: Vec<PaneId> = self
                    .sessions_of(node)
                    .filter(|pane| matches!(pane.link, Link::Failed { .. } | Link::Closed { .. } | Link::Awaiting { .. }))
                    .map(|pane| pane.id)
                    .collect();
                for pane in down {
                    self.ui_action(UiAction::PaneReconnect(rmx_ui::PaneId(pane)));
                }
            },
            UiAction::TreeDisconnect(NodeId(node)) => {
                let live: Vec<PaneId> =
                    self.sessions_of(node).filter(|pane| pane.link == Link::Live).map(|pane| pane.id).collect();
                for pane in live {
                    self.hang_up(pane, "Disconnected");
                }
            },
            UiAction::CopyText(text) => self.core.clipboard.store(ClipboardType::Clipboard, text),
            UiAction::QuickConnect(text) => {
                if let Some(draft) = quick_draft(&text) {
                    self.core.vault.send(Request::SaveConnection(Box::new(draft)));
                }
            },
            UiAction::SidebarResize(width) => {
                self.win.shell.sidebar_width = width;
                self.core.ui.panels.sidebar = width;
                self.relayout(false);
            },
            UiAction::PanelsChanged => self.core.vault.send(Request::SaveLayout(self.layout())),
            UiAction::FilesMenu { pane: rmx_ui::PaneId(pane), side, name, pos } => {
                self.focus_pane(pane);
                let items = self.files_menu(pane, side, name);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::FilesPolicyMenu { pane: rmx_ui::PaneId(pane), pos } => {
                let current = self.pane(pane).and_then(|found| found.content.files()).map_or(0, FilesPane::policy);
                let act = |index| UiAction::Files(rmx_ui::PaneId(pane), FilesAction::Policy(index));
                let row = |(index, label): (usize, &&str)| MenuItem::Action {
                    glyph: (index == current).then_some(icons::CHECK),
                    label: (*label).to_owned(),
                    shortcut: None,
                    action: act(index),
                    danger: false,
                };
                let items = rmx_ui::widgets::CONFLICT_POLICIES.iter().enumerate().map(row).collect();
                self.win.shell.open_menu(pos, items);
            },
            UiAction::FilesAskNewFolder(rmx_ui::PaneId(pane), side) => {
                self.ask_folder_name("New folder", String::new(), Reply::FilesNewFolder(pane, side));
            },
            UiAction::FilesAskRename(rmx_ui::PaneId(pane), side, name) => {
                self.ask_folder_name("Rename", name.to_string(), Reply::FilesRename(pane, side, name));
            },
            UiAction::FilesAskDelete(rmx_ui::PaneId(pane), side, names) => {
                let title = match names.as_slice() {
                    [one] => format!("Delete {one}?"),
                    many => format!("Delete {} items?", many.len()),
                };
                let mut state = PromptState::new(title, "This cannot be undone.", Vec::new());
                state.submit = "Delete".to_owned();
                self.ask_here(state, Reply::FilesDelete(pane, side, names));
            },
            UiAction::TreeFolderToggled { id, expanded } => {
                self.core.vault.send(Request::FolderToggled { node: id.0, expanded });
            },
            UiAction::TreeContextMenu { id, pos } => {
                let items = self.tree_menu(id);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::TreeEdit(NodeId(node)) => self.core.vault.send(Request::EditConnection(node)),
            UiAction::TreeDuplicate(NodeId(node)) => self.core.vault.send(Request::Duplicate(node)),
            UiAction::TreeDelete(NodeId(node)) => self.ask_delete(node),
            UiAction::TreeNewFolder(parent) => {
                let parent = parent.map(|id| id.0);
                self.ask_folder_name("New folder", String::new(), Reply::NewFolder(parent));
            },
            UiAction::TreeRenameFolder(NodeId(node)) => {
                let name = self.core.ui.nodes.get(&node).map(|info| info.name.clone());
                self.ask_folder_name("Rename folder", name.unwrap_or_default(), Reply::RenameFolder(node));
            },
            UiAction::VaultLock => self.core.vault.send(Request::Lock),
            UiAction::VaultMenu(pos) => {
                let items = self.vault_menu();
                self.win.shell.open_menu(pos, items);
            },
            UiAction::VaultSwitch(path) => {
                self.core.ui.vault_error = None;
                self.core.vault.send(Request::Switch(path));
            },
            UiAction::VaultAskPath { create } => {
                let directory = std::path::Path::new(&self.core.ui.vault_path).parent().map_or(String::new(), |dir| dir.display().to_string());
                let (title, value) = if create {
                    ("New vault", format!("{directory}/new.rmxdb"))
                } else {
                    ("Open vault", format!("{directory}/"))
                };
                let field = PromptField { label: String::new(), secret: false, value };
                self.ask_here(PromptState::new(title, "", vec![field]), Reply::VaultPath);
            },
            UiAction::VaultUnlock { mut password, key_file } => {
                let password = Secret::new(password.take());
                self.core.vault.send(Request::Unlock { password, key_file: key_path(&key_file) });
                self.start_vault_work();
            },
            UiAction::VaultCreate { mut password, key_file, plain_ok } => {
                let password = Secret::new(password.take());
                self.core.vault.send(Request::Create { password, key_file: key_path(&key_file), plain_ok });
                self.start_vault_work();
            },
            UiAction::VaultJoin { mut password, key_file, sync, plain_ok } => {
                let password = Secret::new(password.take());
                self.core.vault.send(Request::Join { password, key_file: key_path(&key_file), sync, plain_ok });
                self.start_vault_work();
            },
            UiAction::HostKeyReject => self.answered(Some(HostKeyAnswer::Reject), None),
            UiAction::HostKeyAcceptOnce => self.answered(Some(HostKeyAnswer::Once), None),
            UiAction::HostKeyAcceptAlways => self.answered(Some(HostKeyAnswer::Always), None),
            UiAction::PromptSubmit(fields) => self.answered(None, Some(fields)),
            UiAction::PromptCancel => self.answered(None, None),
            UiAction::PaneCancel(rmx_ui::PaneId(pane)) => self.hang_up(pane, "Cancelled"),
            UiAction::PaneDisconnect(rmx_ui::PaneId(pane)) => self.hang_up(pane, "Disconnected"),
            UiAction::PaneReconnect(rmx_ui::PaneId(pane)) => {
                if let Some(remote) = self.pane_mut(pane).and_then(|found| found.remote.as_mut()) {
                    remote.quiet = false;
                }
                self.start_session(pane);
            },
            UiAction::PaneClose(rmx_ui::PaneId(pane)) => {
                if !self.ask_before_losing_edits(vec![pane], UiAction::PaneClose(rmx_ui::PaneId(pane))) {
                    self.close(pane);
                }
            },
            UiAction::PaneFocus(rmx_ui::PaneId(pane)) => self.focus_pane(pane),
            UiAction::PaneCtrlAltDel(rmx_ui::PaneId(pane)) => self.salute(pane),
            UiAction::PaneToggleScale(rmx_ui::PaneId(pane)) => self.toggle_scale(pane),
            UiAction::PaneOpenFiles(rmx_ui::PaneId(pane)) => self.open_files_here(pane),
            UiAction::Files(rmx_ui::PaneId(pane), action) => self.files_action(pane, action),
            UiAction::PaneDismissNotice(rmx_ui::PaneId(pane)) => {
                if let Some(found) = self.pane_mut(pane) {
                    found.notice = None;
                }
                self.link_changed(pane);
            },
            UiAction::OpenSettings => self.perform(Action::OpenSettings),
            UiAction::SettingsSave(draft) => {
                self.core.ops.push(Op::SetTheme(draft.theme));
                self.core.vault.send(Request::SaveSettings(draft));
            },
            UiAction::SettingsCancel => (),
            UiAction::SyncNow => self.perform(Action::SyncNow),
            UiAction::ImportSshConfig => self.core.vault.send(Request::ImportSshConfig),
            UiAction::SnippetSave { id, name, body } => {
                self.core.vault.send(Request::SaveSnippet { id, name, body });
            },
            UiAction::SnippetDelete(id) => self.core.vault.send(Request::DeleteSnippet(id)),
            UiAction::ListSerialPorts => {
                let proxy = self.core.proxy.clone();
                self.core.runtime.spawn(async move {
                    let ports = rmx_serial::serial::list_ports().await;
                    let _ = proxy.send_event(AppEvent::SerialPorts(ports.iter().map(port_choice).collect()));
                });
            },
            UiAction::SnippetSend(id) => self.run_snippet(id),
        }
        true
    }

    /// Send a saved snippet to the focused terminal, first asking for what it leaves open.
    fn run_snippet(&mut self, id: u64) {
        let Some(row) = self.core.ui.snippets.iter().find(|row| row.id == id).cloned() else { return };
        let fields: Vec<PromptField> = snippet::variables(&row.body)
            .into_iter()
            .map(|variable| PromptField {
                label: variable.name.to_owned(),
                secret: false,
                value: variable.default.unwrap_or_default().to_owned(),
            })
            .collect();
        if fields.is_empty() {
            self.send_text(&row.body);
            return;
        }
        let mut state = PromptState::new(row.name.clone(), "", fields);
        state.submit = "Send".to_owned();
        self.ask_here(state, Reply::Snippet(row.body));
    }

    /// Type `text` into the focused terminal, a line break being the Enter key.
    fn send_text(&mut self, text: &str) {
        let bytes = text.replace('\n', "\r").into_bytes();
        let terminal = self.focused_mut().and_then(|pane| {
            pane.dirty = true;
            pane.content.terminal_mut()
        });
        match terminal {
            Some(terminal) => {
                terminal.scroll_to_bottom();
                terminal.write(bytes);
                self.request_frame();
            },
            None => {
                self.core.ui.status_note = Some(("Snippets go to a terminal: focus one first".to_owned(), true));
                self.core.broadcast = true;
            },
        }
    }

    /// A key meant for the focused pane, if it is a file pane's. Returns whether it was used.
    fn files_key(&mut self, key: &KeyEvent, mods: ModifiersState) -> bool {
        if key.state != ElementState::Pressed {
            return false;
        }
        match files_key_for(key.logical_key.as_ref(), mods) {
            Some((key, extend)) => self.press_files_key(key, extend),
            None => false,
        }
    }

    /// Have the focused pane take `key`, if it is a file pane. Returns whether it was.
    fn press_files_key(&mut self, key: FilesKey, extend: bool) -> bool {
        if self.core.ui.locked() || self.core.ui.asking.is_some() || self.win.chrome.has_keyboard() {
            return false;
        }
        let Some(id) = self.win.workspace.focused() else { return false };
        let Core { panes, ui, .. } = &mut *self.core;
        let Some(files) = panes.iter().find(|pane| pane.id == id).and_then(|pane| pane.content.files()) else {
            return false;
        };
        let view = files.view(rmx_ui::PaneId(id));
        let action = ui.files_key(id, key, extend, &view);
        if let Some(action) = action {
            self.files_action(id, action);
        }
        self.win.chrome.invalidate();
        self.request_frame();
        true
    }

    /// Do what a file pane asked.
    fn files_action(&mut self, id: PaneId, action: FilesAction) {
        let editing = self.pane(id).and_then(|pane| pane.content.files()).is_some_and(FilesPane::editing);
        if action == FilesAction::CloseBody && editing && self.core.ui.files_dirty(id) {
            let then = UiAction::Files(rmx_ui::PaneId(id), FilesAction::CloseBody);
            self.ask_before_losing_edits(vec![id], then);
            return;
        }
        let Some(files) = self.pane_mut(id).and_then(|pane| pane.content.files_mut()) else { return };
        if let Some(side) = files.action(action) {
            self.core.ui.files_reset(id, side);
        }
        self.win.chrome.invalidate();
        self.request_frame();
    }

    /// What `source` holds has been let go outside the window: see [`Landing`].
    fn pointer_left_with(&mut self, source: Source, pane: PaneId) {
        let tab = match source {
            Source::Tab(tab) | Source::Pane(tab, _) => tab,
        };
        let what = match source {
            Source::Tab(_) => self.win.workspace.tabs().get(tab).map(|tab| Moved::Tab(tab.id)),
            Source::Pane(..) => Some(Moved::Pane(pane)),
        };
        if let Some(what) = what {
            self.send_out(what, true);
        }
    }

    /// Start moving `what` out of this window, to another one if `claimable` and one claims it,
    /// else to one of its own, if this window would still have something to show.
    fn send_out(&mut self, what: Moved, claimable: bool) {
        let tabs = self.win.workspace.tabs();
        let can_have_own = match what {
            Moved::Tab(_) => tabs.len() > 1,
            Moved::Pane(pane) => {
                tabs.len() > 1 || tabs.first().is_some_and(|tab| tab.layout.panes().len() > 1 && tab.layout.panes().contains(&pane))
            },
        };
        let (scale, size) = (self.win.gfx.window.scale_factor(), self.win.logical_size());
        let early = can_have_own.then(|| Early::start(scale, size, self.core.theme));
        // A window ought to show at least the one tab: left alone it just stays, unless claimed.
        self.core.landing = Some(Landing {
            from: self.win.key,
            what,
            deadline: Instant::now() + claim_wait(),
            claimable,
            early,
        });
    }

    /// `panes` have left for another window, leaving this one maybe empty.
    fn departed(&mut self, panes: &[PaneId]) {
        if self.win.captured.is_some_and(|id| panes.contains(&id)) {
            self.win.captured = None;
        }
        if self.win.workspace.tabs().is_empty() {
            self.win.exit = true;
        } else {
            self.relayout(false);
        }
    }

    /// Close everything in this window, which is ending.
    fn close_all(&mut self) {
        let panes: Vec<PaneId> =
            self.win.workspace.tabs().iter().flat_map(|tab| tab.layout.panes()).collect();
        self.win.gfx.make_current();
        for pane in panes {
            self.remove_pane(pane);
        }
    }

    /// Land a dragged pane.
    fn drop_pane(&mut self, drop: PaneDrop) {
        let PaneDrop { pane, source, target } = drop;
        let (area, spacing, min) = (self.area(), self.spacing(), self.min_pane());
        let animate = match target {
            Target::Dock { pane: at, axis, side } => {
                let onto = self.win.workspace.active().and_then(|tab| tab.layout.panes().get(at).copied());
                onto.is_some_and(|onto| self.win.workspace.dock(pane, onto, axis, side))
            },
            Target::Merge(into) => {
                self.win.workspace.merge(pane, into, area, min, spacing);
                false
            },
            Target::Detach(slot) => {
                self.win.workspace.detach(pane, slot);
                false
            },
            Target::Reorder(slot) => {
                if let Source::Tab(from) = source {
                    self.win.workspace.reorder_tab(from, slot);
                }
                false
            },
            Target::Window => {
                self.pointer_left_with(source, pane);
                false
            },
        };
        self.relayout(animate);
    }

    /// Close every pane of the tab at `index`.
    fn close_tab(&mut self, index: usize) {
        let closed = self.win.workspace.close_tab(index);
        for &id in &closed {
            self.remove_pane(id);
        }
        if self.win.workspace.tabs().is_empty() {
            self.win.vacated = Some(WhenLast::ShowEmpty);
        }
        self.relayout(false);
    }

    /// Run egui if it needs to, and do what comes of it. Returns whether what it draws changed.
    fn update_chrome(&mut self, now: Instant) -> bool {
        let (width, height) = self.win.gfx.window_size();
        let size = [width as u32, height as u32];
        let ppp = self.scale();

        let mut changed = false;
        let mut ran = false;
        // Twice at most: what the first pass made happen is shown by the second, in this frame.
        for _ in 0..2 {
            let view = ShellView {
                window: self.win.key,
                workspace: &self.win.workspace,
                panes: &self.core.panes,
                rects: &self.win.rects,
                motion: &self.win.motion,
                now,
                area: self.area(),
                spacing: self.spacing(),
                bar: self.bar(),
                min_pane: self.min_pane(),
                maximized: self.win.maximized,
            };
            let (shell, common) = (&mut self.win.shell, &mut self.core.ui);
            if !self.win.chrome.update(now, size, ppp, |ui| shell.show(ui, &view, common)) {
                break;
            }
            ran = true;
            changed |= self.win.chrome.last_pass_changed();
            self.apply_chrome_output();
            self.sync_ime_allowed();

            let acted = !self.win.shell.actions.is_empty() || self.win.shell.dropped.is_some();
            for action in std::mem::take(&mut self.win.shell.actions) {
                self.win.exit |= !self.ui_action(action);
            }
            if let Some(drop) = self.win.shell.dropped.take() {
                self.drop_pane(drop);
            }
            if !acted {
                break;
            }
            self.win.chrome.invalidate();
        }

        #[cfg(feature = "bench")]
        {
            self.win.ran_ui = ran;
        }
        let _ = ran;
        changed
    }

    /// What egui asked of the platform: a cursor, the clipboard, where the input method goes.
    fn apply_chrome_output(&mut self) {
        let output = self.win.chrome.take_output();
        if let Some(text) = output.copied {
            self.core.clipboard.store(ClipboardType::Clipboard, text);
        }
        let point = (self.win.pointer.x as f32, self.win.pointer.y as f32);
        if self.win.chrome.pointer_on_interface() && self.resize_edge_at(point).is_none() {
            self.set_cursor(rmx_ui::input::cursor(output.cursor).unwrap_or(CursorIcon::Default));
        }

        if let Some(rect) = output.ime_rect {
            let scale = self.scale();
            self.set_ime_area((
                (rect.left() * scale) as i32,
                (rect.top() * scale) as i32,
                (rect.width() * scale) as u32,
                (rect.height() * scale) as u32,
            ));
        }
    }

    /// Tell the input method where text goes. Only when that changes: on Wayland each call comes
    /// back as an input method event, and so as another egui pass.
    fn set_ime_area(&mut self, area: (i32, i32, u32, u32)) {
        if self.win.ime_area != Some(area) {
            self.win.ime_area = Some(area);
            let (x, y, width, height) = area;
            self.win.gfx
                .window
                .set_ime_cursor_area(PhysicalPosition::new(x, y), PhysicalSize::new(width, height));
        }
    }

    /// Tell the input method where the terminal cursor is, when it has moved.
    fn sync_ime_area(&mut self) {
        if self.win.chrome.ctx().egui_wants_keyboard_input() {
            return;
        }
        let Some(id) = self.win.workspace.focused() else { return };
        let Some(&(_, panel)) = self.win.rects.iter().find(|&&(pane, _)| pane == id) else { return };
        let rect = self.terminal_rect(panel);
        let Some(terminal) = self.pane(id).and_then(|pane| pane.content.terminal()) else { return };
        let point = terminal.last_snapshot().cursor.point();
        let area = self.win.gfx.cell_rect(rect, point.line, point.column.0);
        self.set_ime_area(area);
    }

    /// The input method has new unfinished text, or has finished.
    fn ime(&mut self, ime: Ime) {
        let preedit = match ime {
            Ime::Preedit(text, cursor) => {
                (!text.is_empty()).then(|| Preedit { text, cursor: cursor.map(|at| at.0) })
            },
            Ime::Commit(text) => {
                if let Some(terminal) = self.focused_terminal() {
                    terminal.write(text.into_bytes());
                }
                None
            },
            Ime::Enabled | Ime::Disabled => None,
        };
        if self.win.preedit != preedit {
            self.win.preedit = preedit;
            if let Some(pane) = self.focused_mut() {
                pane.dirty = true;
            }
            self.request_frame();
        }
    }

    /// Whether the app itself is following a held button.
    fn any_dirty(&self) -> bool {
        self.core.panes.iter().any(|pane| pane.window == self.win.key && pane.dirty)
    }

    fn holding_pointer(&self) -> bool {
        self.win.captured.is_some() || self.win.drag.is_some()
    }

    /// Remove a pane, leaving the window empty if it was the last.
    fn close(&mut self, id: PaneId) {
        self.remove_pane(id);
        match self.win.workspace.close(id) {
            Closed::NotFound => (),
            Closed::Empty => {
                self.win.vacated = Some(WhenLast::ShowEmpty);
                self.relayout(true);
            },
            Closed::Remaining => self.relayout(true),
        }
    }

    /// Forget the pane `id`, ending whatever it was connected to.
    fn remove_pane(&mut self, id: PaneId) {
        self.dismiss_prompts(id);
        let Some(index) = self.core.panes.iter().position(|pane| pane.id == id) else { return };
        let mut pane = self.core.panes.remove(index);
        let node = pane.remote.as_ref().map(|remote| remote.node);
        pane.hang_up();
        self.retire(pane.content);
        if let Some(node) = node {
            let dot = shell::session_dot(&self.core.panes, node);
            self.core.ui.tree.set_state(NodeId(node), dot);
        }
    }

    fn divider_at(&self, point: (f32, f32)) -> Option<Divider> {
        let band = (DIVIDER_BAND * self.scale()).max(self.spacing());
        let tab = self.win.workspace.active().filter(|tab| !tab.zoomed)?;
        let dividers = tab.layout.dividers(self.area(), self.spacing());
        dividers.into_iter().find(|divider| {
            let area = divider.area;
            match divider.axis {
                Axis::Row => {
                    (point.0 - divider.at).abs() <= band / 2.
                        && point.1 >= area.y
                        && point.1 < area.bottom()
                },
                Axis::Column => {
                    (point.1 - divider.at).abs() <= band / 2.
                        && point.0 >= area.x
                        && point.0 < area.right()
                },
            }
        })
    }

    fn drag_divider(&mut self, drag: DividerDrag, point: (f32, f32)) {
        let (area, spacing, min) = (self.area(), self.spacing(), self.min_pane());
        let snap = DIVIDER_SNAP * self.scale();
        let Some(tab) = self.win.workspace.active_mut() else { return };
        let dividers = tab.layout.dividers(area, spacing);
        let Some(divider) = dividers.get(drag.index).copied() else { return };
        let Some((low, high)) = tab.layout.ratio_range(drag.index, divider.area, min, spacing)
        else {
            return;
        };

        let (at, start, length) = match divider.axis {
            Axis::Row => (point.0, divider.area.x, divider.area.w),
            Axis::Column => (point.1, divider.area.y, divider.area.h),
        };
        let wanted = at - drag.grab;
        let ratio = if (wanted - (start + length / 2.)).abs() <= snap {
            0.5
        } else {
            (wanted - spacing / 2. - start) / (length - spacing)
        };
        tab.layout.set_ratio(drag.index, ratio.clamp(low, high));
        self.relayout(false);
        self.show_divider_pill(Some(divider), point, true);
    }

    /// The ratio of the divider `divider` of the active tab, as it is now.
    fn divider_ratio(&self, divider: &Divider) -> f32 {
        let spacing = self.spacing();
        let (start, length) = match divider.axis {
            Axis::Row => (divider.area.x, divider.area.w),
            Axis::Column => (divider.area.y, divider.area.h),
        };
        (divider.at - spacing / 2. - start) / (length - spacing).max(1.)
    }

    /// Set the divider `index` of the active tab to `ratio` as far as its panes allow, sliding.
    fn set_divider(&mut self, index: usize, ratio: f32) {
        let (area, spacing, min) = (self.area(), self.spacing(), self.min_pane());
        let Some(tab) = self.win.workspace.active_mut() else { return };
        let dividers = tab.layout.dividers(area, spacing);
        let Some(divider) = dividers.get(index).copied() else { return };
        let (low, high) = tab.layout.ratio_range(index, divider.area, min, spacing).unwrap_or((0.5, 0.5));
        tab.layout.set_ratio(index, ratio.clamp(low, high));
        self.relayout(true);
    }

    /// Show the pill on `divider`, or hide it, in the interface's next pass if that changed it.
    fn show_divider_pill(&mut self, divider: Option<Divider>, point: (f32, f32), dragged: bool) {
        let scale = self.scale();
        let band = divider.map(|divider| {
            let area = divider.area;
            let (rect, upright, along) = match divider.axis {
                Axis::Row => (Rect::new(divider.at - scale * 2., area.y, scale * 4., area.h), true, point.1),
                Axis::Column => (Rect::new(area.x, divider.at - scale * 2., area.w, scale * 4.), false, point.0),
            };
            let rect = egui::Rect::from_min_size(
                egui::pos2(rect.x / scale, rect.y / scale),
                egui::vec2(rect.w / scale, rect.h / scale),
            );
            (rect, upright, along / scale)
        });
        if self.win.shell.hover_divider(band, dragged, Instant::now()) {
            self.win.chrome.invalidate();
            self.request_frame();
        }
    }

    /// Let go of the divider being dragged, as a press of Escape or a lost focus can do.
    fn cancel_divider_drag(&mut self) {
        if let Some(drag) = self.win.drag.take() {
            self.win.last_divider = None;
            self.set_divider(drag.index, drag.ratio);
            self.show_divider_pill(None, (0., 0.), false);
        }
    }

    /// The pane under `point`, and the point relative to what it shows.
    fn pane_at(&self, point: (f32, f32)) -> Option<(PaneId, f32, f32)> {
        let position = (f64::from(point.0), f64::from(point.1));
        let bar = self.bar();
        self.win.rects.iter().find_map(|&(id, rect)| {
            let content = shell::terminal_rect(rect, bar);
            content.contains(position).then_some((id, point.0 - content.x, point.1 - content.y))
        })
    }

    fn pointer_in(&self, id: PaneId, point: (f32, f32)) -> Option<(f32, f32)> {
        let &(_, rect) = self.win.rects.iter().find(|&&(pane, _)| pane == id)?;
        let content = shell::terminal_rect(rect, self.bar());
        Some((point.0 - content.x, point.1 - content.y))
    }

    /// A right press in the pane `id`. Returns whether that was all it did: it gives an unfocused
    /// pane the keyboard, and opens the pane's menu unless what is inside takes the mouse and Shift
    /// is not held. Control is left to the terminal, for extending a selection.
    fn right_press(&mut self, id: PaneId, mods: ModifiersState) -> bool {
        if mods.control_key() {
            return false;
        }
        let focused = self.win.workspace.focused() == Some(id);
        if !focused {
            self.focus_pane(id);
        }
        let takes_mouse = self.pane(id).is_some_and(|pane| {
            pane.content.display().is_some()
                || pane.content.terminal().is_some_and(|terminal| {
                    let mode = terminal.mode();
                    mode.intersects(rmx_terminal::TermMode::MOUSE_MODE) && !mode.contains(rmx_terminal::TermMode::VI)
                })
        });
        if takes_mouse && !mods.shift_key() {
            return !focused;
        }
        let scale = self.scale();
        let pos = egui::pos2(self.win.pointer.x as f32 / scale, self.win.pointer.y as f32 / scale);
        let items = self.pane_menu(id);
        self.win.shell.open_menu(pos, items);
        self.win.chrome.invalidate();
        self.request_frame();
        true
    }

    fn pointer_moved(&mut self, position: PhysicalPosition<f64>, mods: ModifiersState) {
        self.win.pointer = position;
        let point = (position.x as f32, position.y as f32);

        if let Some(drag) = self.win.drag {
            self.drag_divider(drag, point);
            return;
        }

        if let Some(id) = self.win.captured {
            if let Some(at) = self.display_point(id) {
                if let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut()) {
                    display.pointer_moved(at.0, at.1);
                }
                return;
            }
            let Some((x, y)) = self.pointer_in(id, point) else { return };
            let Some(pane) = self.pane_mut(id) else { return };
            let Some(terminal) = pane.content.terminal_mut() else { return };
            if terminal.mouse_moved(x, y, mods) {
                pane.dirty = true;
                self.request_frame();
            }
            return;
        }

        let hovered = self.divider_at(point);
        self.show_divider_pill(hovered, point, false);
        let icon = match hovered.map(|divider| divider.axis) {
            Some(Axis::Row) => CursorIcon::EwResize,
            Some(Axis::Column) => CursorIcon::NsResize,
            None if self.pane_at(point).is_some_and(|(id, ..)| {
                self.pane(id).is_some_and(|pane| pane.content.display().is_some())
            }) =>
            {
                CursorIcon::Default
            },
            None if self.pane_at(point).is_some() => CursorIcon::Text,
            None => CursorIcon::Default,
        };
        self.set_cursor(icon);

        if let Some((id, x, y)) = self.pane_at(point) {
            if let Some(at) = self.display_point(id) {
                if at.2
                    && let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut())
                {
                    display.pointer_moved(at.0, at.1);
                }
                return;
            }
            if let Some(terminal) = self.pane_mut(id).and_then(|p| p.content.terminal_mut()) {
                terminal.mouse_moved(x, y, mods);
            }
        }
    }

    fn mouse_input(&mut self, button: MouseButton, state: ElementState, mods: ModifiersState) {
        let point = (self.win.pointer.x as f32, self.win.pointer.y as f32);
        let pressed = state == ElementState::Pressed;

        if !pressed && self.win.swallow_release == Some(button) {
            self.win.swallow_release = None;
            return;
        }

        if button == MouseButton::Left && !pressed && self.win.drag.take().is_some() {
            let hovered = self.divider_at(point);
            self.show_divider_pill(hovered, point, false);
            return;
        }

        if button == MouseButton::Left && pressed {
            if let Some(divider) = self.divider_at(point) {
                let now = Instant::now();
                let index = divider.index;
                let last = self.win.last_divider.replace((index, now));
                if divider_double_click(last, index, now) {
                    self.win.last_divider = None;
                    self.win.swallow_release = Some(button);
                    self.set_divider(index, 0.5);
                    return;
                }
                let at = match divider.axis {
                    Axis::Row => point.0,
                    Axis::Column => point.1,
                };
                let ratio = self.divider_ratio(&divider);
                self.win.drag = Some(DividerDrag { index, grab: at - divider.at, ratio });
                self.show_divider_pill(Some(divider), point, true);
                return;
            }

            // Pressing another pane only gives it the keyboard.
            if let Some((id, ..)) = self.pane_at(point)
                && self.win.workspace.focused() != Some(id)
            {
                self.win.workspace.focus(id);
                self.sync_focus(Instant::now());
                self.redraw_all();
                return;
            }
        }

        // A right click is the pane's, not its program's, unless the program asks for the mouse.
        if button == MouseButton::Right
            && pressed
            && self.win.captured.is_none()
            && let Some((id, ..)) = self.pane_at(point)
            && self.right_press(id, mods)
        {
            self.win.swallow_release = Some(button);
            return;
        }

        let target = self.win.captured.or_else(|| self.pane_at(point).map(|(id, ..)| id));
        let Some(id) = target else { return };
        self.win.captured = if pressed { Some(id) } else { None };

        if self.pane(id).is_some_and(|pane| pane.content.display().is_some()) {
            if let (Some(at), Some(bit)) = (self.display_point(id), remote::button_bit(button))
                && (at.2 || !pressed)
                && let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut())
            {
                display.button(at.0, at.1, bit, pressed);
            }
            if !pressed
                && self.pane(id).and_then(|p| p.content.display()).is_some_and(Display::any_button)
            {
                self.win.captured = Some(id);
            }
            return;
        }

        let Some((x, y)) = self.pointer_in(id, point) else { return };
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(terminal) = pane.content.terminal_mut() else { return };
        let selected = terminal.mouse_button(x, y, button, state, mods);
        pane.dirty = true;
        if let Some(text) = selected {
            self.core.clipboard.store(ClipboardType::Selection, text);
        }
        if button == MouseButton::Middle && pressed {
            let text = self.core.clipboard.load(ClipboardType::Selection);
            if let Some(terminal) = self.pane(id).and_then(|pane| pane.content.terminal()) {
                terminal.paste(&text);
            }
        }
        self.request_frame();
    }

    fn wheel(&mut self, delta: MouseScrollDelta, mods: ModifiersState) {
        let wheel = match delta {
            MouseScrollDelta::LineDelta(_, y) => Wheel::Lines(f64::from(y)),
            MouseScrollDelta::PixelDelta(position) => Wheel::Pixels(position.y),
        };
        let point = (self.win.pointer.x as f32, self.win.pointer.y as f32);
        let Some((id, x, y)) = self.pane_at(point) else { return };
        if self.pane(id).is_some_and(|pane| pane.content.display().is_some()) {
            let delta = match delta {
                MouseScrollDelta::LineDelta(x, y) => WheelDelta::Lines([f64::from(x), f64::from(y)]),
                MouseScrollDelta::PixelDelta(position) => WheelDelta::Pixels([position.x, position.y]),
            };
            let Some((layout, _)) = self.display_layout(id) else { return };
            let Some(at) = self.display_point(id) else { return };
            let Some(pane) = self.pane_mut(id) else { return };
            let Some(display) = pane.content.display_mut() else { return };
            // Alt turns the wheel into panning a 1:1 view.
            if mods.alt_key() {
                if display.pan(delta, &layout) {
                    pane.dirty = true;
                    self.request_frame();
                }
            } else if at.2 {
                display.wheel(delta, at.0, at.1);
            }
            return;
        }
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(terminal) = pane.content.terminal_mut() else { return };
        if terminal.mouse_wheel(wheel, x, y, mods) {
            pane.dirty = true;
            self.request_frame();
        }
    }

    /// Draw a frame. Returns whether one was presented.
    fn draw(&mut self) -> bool {
        self.win.gfx.make_current();
        #[cfg(feature = "bench")]
        if let Some(control) = self.win.gfx.control {
            self.win.gfx.draw_control(control);
            return true;
        }
        let now = Instant::now();
        let animating = self.win.motion.running(now);
        // The outlines and footers move with the panes.
        if animating || self.win.was_animating {
            self.win.chrome.invalidate();
        }
        let changed = {
            let _probe = probe::enter(Phase::Chrome);
            self.update_chrome(now)
        };
        let full = std::mem::take(&mut self.win.full) || animating || self.win.was_animating || changed;
        self.win.was_animating = animating;

        let bar = self.bar();
        let focused = self.win.workspace.focused();
        let motion = &self.win.motion;
        let panes = &self.core.panes;
        let scene = &mut self.win.scene;
        let rests = &self.win.rects;
        let mut add = |id: PaneId, rect: Rect| {
            let index = panes.iter().position(|pane| pane.id == id);
            let dirty = index.is_some_and(|index| panes[index].dirty);
            // A pane without a terminal yet is drawn blank, as is one the interface draws.
            let terminal = index.filter(|&index| panes[index].content.terminal().is_some());
            let display = index.filter(|&index| panes[index].content.display().is_some());
            let rest = rests.iter().find(|&&(pane, _)| pane == id).map_or(rect, |&(_, rest)| rest);
            scene.push(PaneDraw {
                terminal,
                display,
                rect: shell::terminal_rect(rect, bar),
                rest: shell::terminal_rect(rest, bar),
                focused: focused == Some(id),
                dirty,
                stale: false,
            });
        };

        // Panes on their way out go under the others.
        if !self.core.ui.locked() {
            for (id, rect) in motion.leaving(now) {
                add(id, rect);
            }
        }
        for &(id, rest) in &self.win.rects {
            add(id, motion.frame(id, now).unwrap_or(rest));
        }

        let preedit = self.win.preedit.as_ref();
        let chrome = &mut self.win.chrome;
        let _ = panes;
        let presented = self.win.gfx.draw(scene, &mut self.core.panes, preedit, full, chrome);

        let window = self.win.key;
        for pane in self.core.panes.iter_mut().filter(|pane| pane.window == window) {
            pane.dirty = false;
        }
        // Panes that could not be read keep asking for frames, drawn as they last were.
        for pane in scene.drain(..).filter(|pane| pane.stale) {
            if let Some(index) = pane.terminal {
                self.core.panes[index].dirty = true;
            }
        }
        let _probe = probe::enter(Phase::Post);
        self.sync_ime_area();
        // Picked up after presenting, not before: the GPU then has a whole frame to take in a
        // full-screen update, instead of holding this frame's swap until it has. It shows on the
        // next frame, which is the one frame of latency the budget allows.
        self.upload_displays(Instant::now());
        presented
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.running.is_some() {
            return;
        }
        if let Err(err) = self.start(event_loop) {
            eprintln!("rmx: {err}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: AppEvent) {
        #[cfg(feature = "bench")]
        let _slow = crate::bench::present::Slow::new("user_event", app_event_name(&event));
        let Some(running) = &mut self.running else { return };
        match event {
            AppEvent::Wakeup(id) => running.wakeup(id),
            AppEvent::Spawned(id, terminal) => {
                let Some(mut at) = running.at_pane(id) else { return };
                at.spawned(id, *terminal);
            },
            AppEvent::Session(id, event) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.session_event(id, event);
                }
            },
            AppEvent::Link(id, generation, event) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.link_event(id, generation, event);
                } else if let LinkEvent::Ready(Ready::Terminal(terminal)) = event {
                    running.core.retire(PaneContent::Terminal(terminal));
                }
            },
            AppEvent::DisplayNews(id, generation) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.display_news(id, generation);
                }
            },
            AppEvent::DisplayReady(id, generation) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.display_ready(id, generation);
                }
            },
            AppEvent::DisplayClipboard(id, generation, text) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.display_clipboard(id, generation, text);
                }
            },
            AppEvent::Files(id, generation, event) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.files_event(id, generation, event);
                }
            },
            AppEvent::Prompt(prompt) => {
                let at = match prompt.pane {
                    Some(id) => running.at_pane(id),
                    None => running.at_active(),
                };
                if let Some(mut at) = at {
                    at.ask(prompt);
                }
            },
            AppEvent::Vault(event) => running.vault_event(event),
            AppEvent::SerialPorts(ports) => {
                for win in &mut running.windows {
                    win.shell.dialog.set_ports(ports.clone());
                }
                running.core.broadcast = true;
            },
            AppEvent::Term(id, event) => {
                let Some(mut at) = running.at_pane(id) else {
                    if matches!(event, TermEvent::Exit | TermEvent::ChildExit(_)) {
                        running.core.ended_early.push(id);
                    }
                    return;
                };
                let theme = *at.win.gfx.theme();
                let Some(terminal) = at.pane(id).and_then(|pane| pane.content.terminal()) else {
                    if matches!(event, TermEvent::Exit | TermEvent::ChildExit(_)) {
                        at.core.ended_early.push(id);
                    }
                    return;
                };
                match terminal.handle(event, &theme) {
                    Output::Title(title) => at.set_title(id, Some(title)),
                    Output::ResetTitle => at.set_title(id, None),
                    Output::Exit => at.program_ended(id),
                    Output::ClipboardStore(ty, text) => at.core.clipboard.store(ty, text),
                    Output::ClipboardLoad(ty, format) => {
                        let text = format(&at.core.clipboard.load(ty));
                        if let Some(terminal) = at.pane(id).and_then(|p| p.content.terminal()) {
                            terminal.write(text.into_bytes());
                        }
                    },
                    Output::Bell | Output::None => (),
                }
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        #[cfg(feature = "bench")]
        let _slow = {
            crate::bench::present::note_window_event(&event);
            crate::bench::present::Slow::new("window_event", crate::bench::present::window_event_name(&event))
        };
        let _ = event_loop;
        let Some(running) = &mut self.running else { return };
        running.note_pointer_arrival(id, &event);
        let Some(mut at) = running.at_window(id) else { return };
        // Focus goes with the window the user is in.
        if matches!(event, WindowEvent::Focused(true) | WindowEvent::MouseInput { .. } | WindowEvent::KeyboardInput { .. }) {
            at.core.active = at.win.key;
        }

        // The edges of the window are the first to get a press, even over the title bar.
        let point = (at.win.pointer.x as f32, at.win.pointer.y as f32);
        if let WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } = event
            && !at.holding_pointer()
            && !at.win.shell.dragging_pane()
            && let Some(direction) = at.resize_edge_at(point)
        {
            if let Err(err) = at.win.gfx.window.drag_resize_window(direction) {
                log::warn!("Could not resize the window: {err}");
            }
            return;
        }

        // Over anything but the menu, a right press closes it and counts for what is there.
        if let WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } = event
            && at.win.shell.menu_open()
        {
            let scale = at.scale();
            let at_point = egui::pos2(at.win.pointer.x as f32 / scale, at.win.pointer.y as f32 / scale);
            if !at.win.shell.menu_contains(at_point) {
                at.win.shell.close_menu();
                at.win.chrome.float_closed();
                at.request_frame();
            }
        }

        if let WindowEvent::KeyboardInput { event: key, .. } = &event
            && at.win.drag.is_some()
            && key.state == ElementState::Pressed
            && key.logical_key == Key::Named(NamedKey::Escape)
        {
            at.cancel_divider_drag();
            return;
        }

        if let WindowEvent::KeyboardInput { event: key, .. } = &event
            && at.win.shell.dragging_pane()
            && key.state == ElementState::Pressed
            && key.logical_key == Key::Named(NamedKey::Escape)
        {
            at.win.shell.cancel_pane_drag(Instant::now());
            at.win.chrome.invalidate();
            return;
        }

        // Ahead of the interface, which would take Tab to mean its next field.
        if let WindowEvent::KeyboardInput { event: key, is_synthetic: false, .. } = &event
            && at.files_key(key, self.modifiers)
        {
            return;
        }

        // Pointer events belong to the app while it follows a held button, and to the interface
        // otherwise, wherever it wants them.
        let held = at.holding_pointer();
        let pointer = matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
        );
        // A release still reaches the chrome, in case it heard the press.
        let release = matches!(event, WindowEvent::MouseInput { state: ElementState::Released, .. });
        let consumed = !(held && pointer && !release) && at.win.chrome.window_event(&event);

        if let (true, WindowEvent::KeyboardInput { event: key, .. }) = (consumed, &event)
            && key.state == ElementState::Pressed
            && self.modifiers.control_key()
            && matches!(key.logical_key.as_ref(), Key::Character(text) if text.eq_ignore_ascii_case("v"))
        {
            let text = at.core.clipboard.load(ClipboardType::Clipboard);
            at.win.chrome.paste(text);
        }

        match event {
            WindowEvent::CloseRequested => {
                let key = at.win.key;
                let here = at.core.panes.iter().filter(|pane| pane.window == key).map(|pane| pane.id).collect();
                if !at.ask_before_losing_edits(here, UiAction::WindowClose) {
                    at.win.exit = true;
                }
            },
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                if at.win.motion.running(now) || at.win.chrome.needs_update(now) {
                    at.win.pacer.mark_dirty();
                }
                if !at.win.pacer.on_redraw() {
                    return;
                }

                #[cfg(feature = "bench")]
                let (frame_start, frame_start_ns, allocs_before, cpu_before, sched_before) = (
                    Instant::now(),
                    crate::bench::present::now_ns(),
                    crate::bench::allocations(),
                    crate::bench::thread_cpu_us(),
                    crate::bench::sched(),
                );

                let presented = at.draw();
                if at.win.exit {
                    return;
                }

                if !presented {
                    at.win.pacer.on_nothing_to_draw();
                    // An upload picked up with nothing else to show still has to be shown.
                    if at.any_dirty() {
                        at.request_frame();
                    }
                    return;
                }

                #[cfg(feature = "bench")]
                if let Some(presentation) = &mut at.win.gfx.presentation {
                    presentation.note_frame(at.win.pacer.pending_since(), frame_start_ns);
                }

                #[cfg(feature = "bench")]
                self.bench.frame(
                    frame_start.elapsed(),
                    crate::bench::thread_cpu_us() - cpu_before,
                    self.started,
                    at.win.gfx.phases,
                    at.win.gfx.stats,
                    (
                        crate::bench::allocations() - allocs_before,
                        at.win.gfx.winit_allocations,
                        at.win.ran_ui.then(|| at.win.chrome.stats()),
                    ),
                    sched_before,
                    rmx_render::probe::take(),
                );

                if !at.win.started {
                    at.win.started = true;
                    at.win.gfx.finish_startup();
                }

                let now = Instant::now();
                if at.win.pacer.on_presented(now) {
                    at.win.gfx.window.request_redraw();
                }
                // Keep frames coming until the animation has come to rest.
                if at.win.motion.running(now)
                    || at.win.was_animating
                    || at.any_dirty()
                {
                    at.win.pacer.mark_dirty();
                }
                #[cfg(feature = "bench")]
                if at.win.gfx.control.is_some() {
                    at.win.pacer.mark_dirty();
                }
            },
            WindowEvent::Resized(_) => {
                at.win.gfx.resize();
                at.win.maximized = at.win.gfx.window.is_maximized();
                at.relayout(false);
            },
            WindowEvent::ScaleFactorChanged { .. } => {
                at.win.pacer.set_refresh(refresh_interval(&at.win.gfx.window));
            },
            WindowEvent::Focused(focused) => {
                at.win.has_focus = focused;
                // The release that would end a drag may never come now.
                if !focused {
                    at.win.drag = None;
                    at.show_divider_pill(None, (0., 0.), false);
                    at.win.shell.cancel_pane_drag(Instant::now());
                }
                if let Some(pane) = at.focused_mut() {
                    pane.content.set_focused(focused);
                    pane.dirty = true;
                }
                if focused && let Some(id) = at.win.display_focus {
                    at.offer_clipboard(id);
                }
                at.request_frame();
            },
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                at.touch();
                // Nothing typed at the lock screen may reach a session behind it.
                if consumed || at.core.ui.locked() {
                    return;
                }
                let mods = self.modifiers;
                if let Some(action) = action_for(event.logical_key.as_ref(), mods) {
                    if event.state == ElementState::Pressed {
                        at.perform(action);
                    }
                    return;
                }

                let Some(pane) = at.focused_mut() else { return };
                if let Some(display) = pane.content.display_mut() {
                    let pressed = event.state == ElementState::Pressed;
                    if pressed
                        && mods.control_key()
                        && mods.alt_key()
                        && remote::is_salute_key(event.physical_key)
                    {
                        display.ctrl_alt_del();
                        return;
                    }
                    if let Some(key) =
                        remote::remote_key(event.physical_key, &event.logical_key, event.location)
                    {
                        display.key(key, pressed, event.repeat);
                    }
                    return;
                }
                let Some(terminal) = pane.content.terminal_mut() else { return };
                let pressed = event.state == ElementState::Pressed;
                let bytes = encode_key(event, mods, terminal.mode());
                if !bytes.is_empty() {
                    if pressed {
                        terminal.scroll_to_bottom();
                        terminal.write(bytes);
                        pane.dirty = true;
                    } else {
                        terminal.write(bytes);
                    }
                }
            },
            WindowEvent::Ime(ime) if !consumed && !at.core.ui.locked() => at.ime(ime),
            WindowEvent::CursorMoved { position, .. } => {
                let point = (position.x as f32, position.y as f32);
                if !held && let Some(direction) = at.resize_edge_at(point) {
                    at.win.pointer = position;
                    at.set_cursor(resize_cursor(direction));
                } else if consumed && at.win.chrome.pointer_on_interface() {
                    at.win.pointer = position;
                    at.show_divider_pill(None, (0., 0.), false);
                } else {
                    at.pointer_moved(position, self.modifiers);
                }
            },
            WindowEvent::MouseInput { button, state, .. } => {
                at.touch();
                if !consumed && !at.core.ui.locked() {
                    at.mouse_input(button, state, self.modifiers);
                }
            },
            WindowEvent::MouseWheel { delta, .. } if !consumed => {
                at.wheel(delta, self.modifiers);
            },
            _ => (),
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        #[cfg(feature = "bench")]
        if let Some(running) = &mut self.running {
            for win in &mut running.windows {
                if win.gfx.presentation.is_some() {
                    win.gfx.make_current();
                }
                if let Some(presentation) = &mut win.gfx.presentation {
                    presentation.finish(&win.gfx.window);
                }
            }
        }
        if let Some(running) = &mut self.running
            && let Some(at) = running.at_index(0)
        {
            let (done, wait) = mpsc::channel();
            let layout = at.layout();
            let session = running.session_text();
            running.core.vault.send(Request::Shutdown(layout, session, done));
            let _ = wait.recv_timeout(SAVE_ON_EXIT);
        }
        // The window and its GL objects must go while the display connection still exists.
        self.running = None;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(feature = "bench")]
        let _slow = crate::bench::present::Slow::new("about_to_wait", "loop");
        #[cfg(feature = "bench")]
        let script_wake = self.run_script(event_loop);
        #[cfg(not(feature = "bench"))]
        let script_wake: Option<Instant> = None;

        let Some(running) = &mut self.running else { return };
        running.settle(event_loop);
        if running.windows.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut wakes: Option<Instant> = script_wake;
        for index in 0..running.windows.len() {
            let Some(mut at) = running.at_index(index) else { continue };
            let (redraw, wake) = at.win.pacer.before_wait(now);
            if redraw {
                at.win.gfx.window.request_redraw();
            }

            // When egui is due it asks for a frame like everything else; only a later wish needs a
            // timer, since waiting for a frame that is on its way must not spin.
            let due = at.win.chrome.next_deadline();
            if due.is_some_and(|due| due <= now) {
                at.request_frame();
            }
            let chrome_wake = due.filter(|&due| due > now);
            let display_wake = at.tick_displays(now);
            wakes = [wakes, wake, chrome_wake, display_wake].into_iter().flatten().min();
        }
        let wake = [wakes, running.core.next_op_wake()].into_iter().flatten().min();
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

#[cfg(feature = "bench")]
fn app_event_name(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::Wakeup(_) => "wakeup",
        AppEvent::Term(..) => "term",
        AppEvent::Spawned(..) => "spawned",
        AppEvent::Session(..) => "session",
        AppEvent::Link(..) => "link",
        AppEvent::DisplayNews(..) => "display-news",
        AppEvent::DisplayReady(..) => "display-ready",
        AppEvent::DisplayClipboard(..) => "display-clipboard",
        AppEvent::Files(..) => "files",
        AppEvent::Prompt(_) => "prompt",
        AppEvent::SerialPorts(_) => "serial-ports",
        AppEvent::Vault(_) => "vault",
    }
}

/// How often a held script looks again.
#[cfg(feature = "bench")]
const SCRIPT_POLL: Duration = Duration::from_millis(20);

#[cfg(feature = "bench")]
impl At<'_> {
    fn node_labelled(&self, label: &str) -> Option<u64> {
        self.core.ui.tree.nodes().iter().find(|node| node.label == label).map(|node| node.id.0)
    }

    /// The text of the focused terminal, laid out as it shows.
    fn screen_text(&self) -> String {
        let Some(terminal) = self.focused_terminal() else { return String::new() };
        let snapshot = terminal.last_snapshot();
        let (mut text, mut line, mut column) = (String::new(), 0, 0);
        for cell in &snapshot.cells {
            while line < cell.point.line {
                text.push('\n');
                line += 1;
                column = 0;
            }
            while column < cell.point.column.0 {
                text.push(' ');
                column += 1;
            }
            text.push(cell.character);
            column += 1;
        }
        text
    }

    fn focused_display(&self) -> Option<&Display> {
        self.pane(self.win.workspace.focused()?)?.content.display()
    }

    fn focused_files(&self) -> Option<&FilesPane> {
        self.pane(self.win.workspace.focused()?)?.content.files()
    }

    /// What the local clipboard holds, read through a copy so that a held `&self` can ask.
    fn bench_clipboard(&self) -> String {
        self.core.clipboard_seen.borrow().clone()
    }

    /// Act on the transfer with `name` in its label.
    fn files_row_action(&mut self, name: &str, action: fn(u64) -> FilesAction) {
        let Some(id) = self.win.workspace.focused() else { return };
        let row = self.focused_files().and_then(|files| {
            files.transfers().iter().find(|row| row.label.contains(name)).map(|row| row.id)
        });
        if let Some(row) = row {
            self.ui_action(UiAction::Files(rmx_ui::PaneId(id), action(row)));
        }
    }

    /// Open a connection in a new pane beside the focused one, as dragging it there would.
    fn open_split(&mut self, node: u64, axis: Axis) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        let name = info.name.clone();
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::remote(id, node, name, Goal::Session));
            self.start_session(id);
            self.relayout(true);
        }
    }

    /// The center of a column of the focused file pane, in window pixels.
    fn files_area(&self) -> Option<egui::Rect> {
        let id = self.win.workspace.focused()?;
        let &(_, rect) = self.win.rects.iter().find(|&&(pane, _)| pane == id)?;
        Some(shell::content_points(rect, self.bar(), self.scale()))
    }

    /// Where a script means by `target`, in window pixels.
    fn script_point(&self, target: &crate::bench::Point) -> Option<(f32, f32)> {
        use crate::bench::Point as P;
        let scale = self.scale();
        let panel = |index: usize| self.win.rects.get(index).map(|&(_, rect)| rect);
        match *target {
            P::By(dx, dy) => Some((self.win.pointer.x as f32 + dx, self.win.pointer.y as f32 + dy)),
            P::Pane(index, fx, fy) => {
                let rect = panel(index)?;
                Some((rect.x + rect.w * fx, rect.y + rect.h * fy))
            },
            P::Bar(index) => {
                let rect = panel(index)?;
                Some((rect.x + rect.w * 0.4, rect.y + PANE_BAR_HEIGHT * scale / 2.))
            },
            P::BarButton(index, from_right) => {
                let rect = panel(index)?;
                let (button, step) = (PANE_BAR_HEIGHT - 4., PANE_BAR_HEIGHT - 2.);
                let x = rect.right() - (2. + button / 2. + from_right as f32 * step) * scale;
                Some((x, rect.y + PANE_BAR_HEIGHT * scale / 2.))
            },
            P::Divider(index) => {
                let tab = self.win.workspace.active()?;
                let dividers = tab.layout.dividers(self.area(), self.spacing());
                let divider = dividers.get(index)?;
                Some(match divider.axis {
                    Axis::Row => (divider.at, divider.area.y + divider.area.h / 2.),
                    Axis::Column => (divider.area.x + divider.area.w / 2., divider.at),
                })
            },
            P::SidebarEdge => {
                let (_, height) = self.win.gfx.window_size();
                Some((self.sidebar_width(), height / 2.))
            },
            P::FilesFold => {
                let area = self.files_area()?;
                let at = rmx_ui::widgets::files_fold_point(area, &self.core.ui.panels);
                Some((at.x * scale, at.y * scale))
            },
            P::FilesSplit | P::FilesQueue => {
                let area = self.files_area()?;
                let (x, y) = rmx_ui::widgets::files_edges(area, &self.core.ui.panels);
                Some(match target {
                    P::FilesSplit => (x * scale, (area.top() + 100.) * scale),
                    _ => (area.center().x * scale, y * scale),
                })
            },
            P::MenuRow(ref label) => {
                let at = self.win.shell.menu_offers(label)?;
                Some((at.x * scale, at.y * scale))
            },
        }
    }

    fn condition_holds(&self, condition: &crate::bench::Condition, windows: usize) -> bool {
        use crate::bench::Condition as C;
        let link = || self.win.workspace.focused().and_then(|id| self.pane(id)).map(|pane| &pane.link);
        match condition {
            C::Setup => matches!(self.core.ui.gate, Gate::Setup(_)),
            C::Locked => matches!(self.core.ui.gate, Gate::Locked(_)),
            C::Unlocked => self.core.ui.gate == Gate::Open,
            C::HostKey => matches!(self.core.ui.asking, Some(Asking::HostKey(_))),
            C::Prompt => matches!(self.core.ui.asking, Some(Asking::Fields(_))),
            C::Live => matches!(link(), Some(Link::Live)),
            C::Closed => matches!(link(), Some(Link::Closed { .. })),
            C::Failed => matches!(link(), Some(Link::Failed { .. })),
            C::Synced => matches!(self.core.ui.sync, SyncState::Done(_)),
            C::NoQuestion => self.core.ui.asking.is_none(),
            C::Dialog => self.win.shell.dialog.is_open(),
            C::VaultError => self.core.ui.vault_error.is_some(),
            C::RemoteSize => self.focused_terminal().is_some_and(|terminal| {
                let size = terminal.size();
                let expected = format!("size-{} {}", size.lines_count(), size.columns_count());
                self.win.columns_before_resize != Some(size.columns_count())
                    && self.screen_text().contains(&expected)
            }),
            C::Tree(label) => self.node_labelled(label).is_some(),
            C::NoTree(label) => self.node_labelled(label).is_none(),
            C::Text(found) => self.screen_text().contains(found),
            C::Panes(count) => self.core.panes.len() == *count,
            C::Windows(count) => windows == *count,
            C::Snippet(name) => self.core.ui.snippets.iter().any(|row| row.name == *name),
            C::Awaiting => matches!(link(), Some(Link::Awaiting { .. })),
            C::LiveShown(count) => {
                let live = |&&(id, _): &&(PaneId, Rect)| {
                    self.pane(id).is_some_and(|pane| matches!(pane.link, Link::Live | Link::Local) && pane.content.has_screen())
                };
                self.win.rects.iter().filter(live).count() == *count
            },
            C::Tabs(count) => self.win.workspace.tabs().len() == *count,
            C::Display => self.focused_display().is_some_and(|d| d.pictures > 0),
            C::DisplayFits => self.focused_display().is_some_and(Display::fits_pane),
            C::Listed(side, name) => {
                self.focused_files().is_some_and(|f| f.rows(*side).iter().any(|r| r.name.as_ref() == name))
            },
            C::AtPath(side, path) => self.focused_files().is_some_and(|f| f.path(*side) == path),
            C::Transfer(name, state) => self.focused_files().is_some_and(|f| {
                use rmx_ui::widgets::TransferRowState as S;
                f.transfers().iter().any(|row| {
                    row.label.contains(name.as_str())
                        && match row.state {
                            S::Queued => state == "queued",
                            S::Running => state == "running",
                            S::Done => state == "done",
                            S::Failed => state == "failed",
                            S::Cancelled => state == "cancelled",
                        }
                })
            }),
            C::Unencrypted => self.core.ui.unencrypted,
            C::SyncFailed(found) => {
                matches!(&self.core.ui.sync, SyncState::Failed(reason) if reason.contains(found.as_str()))
            },
            C::Clipboard(found) => self.bench_clipboard().contains(found.as_str()),
            C::Zoomed(zoomed) => self.win.workspace.active().is_some_and(|tab| tab.zoomed == *zoomed),
            C::Menu(open) => self.win.shell.menu_open() == *open,
            C::MenuOffers(label) => self.win.shell.menu_offers(label).is_some(),
            C::Sidebar(low, high) => (*low..=*high).contains(&self.win.shell.sidebar_width),
            C::FilesSplit(low, high) => (*low..=*high).contains(&self.core.ui.panels.files_split),
            C::FilesQueue(low, high) => (*low..=*high).contains(&self.core.ui.panels.files_queue),
            C::FilesFolded(folded) => self.core.ui.panels.files_queue_folded == *folded,
            C::FilesText(found) => self.focused_files().and_then(FilesPane::document_text).is_some_and(|text| text.contains(found.as_str())),
            C::FilesDiff(rows) => self.focused_files().and_then(FilesPane::diff_rows).is_some_and(|found| found.len() == *rows),
            C::NoDiff => self.focused_files().is_some_and(|files| files.diff_rows().is_none()),
            C::FilesColumns => self.focused_files().is_some_and(|files| files.document_text().is_none() && files.diff_rows().is_none()),
            C::VaultNamed(name) => self.core.ui.vault_path.ends_with(name.as_str()),
            C::Ratio(low, high) => {
                let tab = self.win.workspace.active();
                let dividers = tab.map(|tab| tab.layout.dividers(self.area(), self.spacing()));
                dividers.as_ref().and_then(|dividers| dividers.first()).is_some_and(|divider| {
                    (*low..=*high).contains(&self.divider_ratio(divider))
                })
            },
            C::FirstSplit(axis) => {
                let tab = self.win.workspace.active();
                let dividers = tab.map(|tab| tab.layout.dividers(self.area(), self.spacing()));
                dividers.as_ref().and_then(|dividers| dividers.first()).is_some_and(|d| d.axis == *axis)
            },
            C::TabName(name) => self.win.workspace.active().is_some_and(|tab| tab.name.as_deref() == Some(name)),
            C::Notice(found) => self
                .win
                .workspace
                .focused()
                .and_then(|id| self.pane(id))
                .and_then(|pane| pane.notice.as_deref())
                .is_some_and(|notice| notice.contains(found.as_str())),
        }
    }
}

#[cfg(feature = "bench")]
impl App {
    /// Perform the next step of the bench script when it is due. Returns when the next one is.
    fn run_script(&mut self, event_loop: &ActiveEventLoop) -> Option<Instant> {
        use crate::bench::Step;

        use crate::gfx::Shot;

        let running = self.running.as_mut()?;
        // A window that has gone falls back on the first.
        self.script_window = self.script_window.min(running.windows.len().saturating_sub(1));
        let window_count = running.windows.len();
        let script_window = self.script_window;
        let mut at = running.at_index(script_window)?;
        // Numbers for the wrong output are worse than none.
        if at.win.gfx.presentation.as_ref().is_some_and(|p| p.wrong_refresh()) {
            event_loop.exit();
            return None;
        }
        let now = Instant::now();
        self.bench.wakeup(now);
        if matches!(self.bench.waiting(), Some(crate::bench::Condition::Clipboard(_))) {
            let text = at.core.clipboard.load(ClipboardType::Clipboard);
            *at.core.clipboard_seen.borrow_mut() = text;
        }
        if let Some(condition) = self.bench.waiting() {
            if at.condition_holds(condition, window_count) {
                crate::bench::present::note("step", || "await released".to_owned());
                self.bench.release(now);
            } else if self.bench.hold_expired(now) {
                event_loop.exit();
                return None;
            } else {
                return Some(now + SCRIPT_POLL);
            }
        }
        let step = self.bench.due(now);
        if let Some(step) = step.as_ref().filter(|step| !matches!(step, Step::Wait)) {
            crate::bench::present::note("step", || step.name());
        }
        match step {
            None | Some(Step::Wait) => (),
            Some(Step::Quit) => event_loop.exit(),
            Some(Step::Action(action)) => {
                let what = action.clone();
                let start = Instant::now();
                at.perform(action);
                self.bench.action(start.elapsed(), &format!("{what:?}"));
            },
            Some(Step::Ui(what, action)) => {
                let start = Instant::now();
                if !at.ui_action(action) {
                    event_loop.exit();
                }
                self.bench.action(start.elapsed(), what);
                at.win.chrome.invalidate();
                at.request_frame();
            },
            Some(Step::Await(condition)) => {
                self.bench.hold(condition, now);
                return Some(now + SCRIPT_POLL);
            },
            Some(Step::Idle(length)) => self.bench.start_idle(now, length),
            Some(Step::Open(label)) => {
                let start = Instant::now();
                if let Some(node) = at.node_labelled(&label) {
                    at.open_connection(node);
                }
                self.bench.action(start.elapsed(), "open-connection");
            },
            Some(Step::Duplicate(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    at.core.vault.send(Request::Duplicate(node));
                }
            },
            Some(Step::Delete(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    at.ask_delete(node);
                }
            },
            Some(Step::Run(line)) => {
                if let Some(terminal) = at.focused_terminal() {
                    terminal.write(format!("{line}\r").into_bytes());
                }
            },
            Some(Step::Resize(width, height)) => {
                at.win.columns_before_resize =
                    at.focused_terminal().map(|terminal| terminal.size().columns_count());
                // On Wayland a size applied at once comes with no event to say so.
                if at.win.gfx.window.request_inner_size(LogicalSize::new(width, height)).is_some() {
                    at.win.gfx.resize();
                    at.relayout(false);
                }
            },
            Some(Step::Theme(mode)) => at.core.ops.push(Op::SetTheme(mode)),
            Some(Step::Edit(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    at.core.vault.send(Request::EditConnection(node));
                }
            },
            Some(Step::Rename(name)) => {
                if let Some(draft) = at.win.shell.dialog.draft_mut() {
                    draft.name = name;
                    let draft = Box::new(draft.clone());
                    at.win.shell.dialog.close();
                    at.ui_action(UiAction::ConnectionSubmit(draft));
                }
            },
            Some(Step::Menu(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    let pos = egui::pos2(120., 140.);
                    at.ui_action(UiAction::TreeContextMenu { id: NodeId(node), pos });
                    at.win.chrome.invalidate();
                    at.request_frame();
                }
            },
            Some(Step::OpenSplit(axis, label)) => {
                let start = Instant::now();
                if let Some(node) = at.node_labelled(&label) {
                    at.open_split(node, axis);
                }
                self.bench.action(start.elapsed(), "open-split");
            },
            Some(Step::Keys(text)) => {
                if let Some(pane) = at.focused_mut()
                    && let Some(display) = pane.content.display_mut()
                {
                    for character in text.chars() {
                        let Some((code, key)) = remote::typed_key(character) else { continue };
                        let physical = winit::keyboard::PhysicalKey::Code(code);
                        let location = winit::keyboard::KeyLocation::Standard;
                        if let Some(remote_key) = remote::remote_key(physical, &key, location) {
                            display.key(remote_key, true, false);
                            display.key(remote_key, false, false);
                        }
                    }
                }
            },
            Some(Step::Center) => {
                let center = at
                    .win
                    .workspace
                    .focused()
                    .and_then(|id| at.win.rects.iter().find(|&&(pane, _)| pane == id))
                    .map(|&(_, rect)| {
                        let rect = at.terminal_rect(rect);
                        (rect.x + rect.w / 2., rect.y + rect.h / 2.)
                    });
                if let Some((x, y)) = center {
                    let position = PhysicalPosition::new(f64::from(x), f64::from(y));
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(event_loop, id, WindowEvent::CursorMoved { device_id, position });
                }
            },
            Some(Step::SetClipboard(text)) => at.core.clipboard.store(ClipboardType::Clipboard, text),
            Some(Step::OfferClipboard) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.offer_clipboard(id);
                }
            },
            Some(Step::Salute) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.salute(id);
                }
            },
            Some(Step::ToggleScale) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.toggle_scale(id);
                }
            },
            Some(Step::OpenFiles) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::PaneOpenFiles(rmx_ui::PaneId(id)));
                }
            },
            Some(Step::FilesGoto(side, path)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::Files(rmx_ui::PaneId(id), FilesAction::Goto { side, path }));
                }
            },
            Some(Step::FilesSelect(side, name)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.core.ui.files_select(id, side, &name);
                    at.win.chrome.invalidate();
                    at.request_frame();
                }
            },
            Some(Step::FilesSend(side)) => {
                if let Some(id) = at.win.workspace.focused() {
                    let names = at.core.ui.files_selected(id, side);
                    let action = FilesAction::Transfer { from: side, names };
                    at.ui_action(UiAction::Files(rmx_ui::PaneId(id), action));
                }
            },
            Some(Step::FilesPoint(side, name)) => {
                let area = at.files_area();
                let files = at.focused_files();
                if let (Some(area), Some(files)) = (area, files)
                    && let Some(point) = files.row_point(area, side, &name, &at.core.ui.panels)
                {
                    let scale = at.scale();
                    let position = PhysicalPosition::new(f64::from(point.x * scale), f64::from(point.y * scale));
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(event_loop, id, WindowEvent::CursorMoved { device_id, position });
                }
            },
            Some(Step::FilesPointColumn(side)) => {
                if let Some(area) = at.files_area() {
                    let scale = at.scale();
                    let x = if side == rmx_ui::FileSide::Local {
                        area.left() + area.width() / 4.
                    } else {
                        area.left() + area.width() * 3. / 4.
                    };
                    let position = PhysicalPosition::new(f64::from(x * scale), f64::from(area.center().y * scale * 0.7));
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(event_loop, id, WindowEvent::CursorMoved { device_id, position });
                }
            },
            Some(Step::FilesCancel(name)) => at.files_row_action(&name, FilesAction::Cancel),
            Some(Step::FilesRetry(name)) => at.files_row_action(&name, FilesAction::Retry),
            Some(Step::FilesBody(action)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::Files(rmx_ui::PaneId(id), action));
                }
            },
            Some(Step::FilesPolicy(index)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::Files(rmx_ui::PaneId(id), FilesAction::Policy(index)));
                }
            },
            Some(Step::Discard) => {
                if let Some(pane) = at.win.workspace.focused() {
                    at.session_event(pane, SessionEvent::OutputDiscarded(5 * MIB));
                }
            },
            Some(Step::Link(step)) => {
                use crate::bench::LinkStep;
                let start = Instant::now();
                if let Some(pane) = at.win.workspace.focused() {
                    let id = rmx_ui::PaneId(pane);
                    let action = match step {
                        LinkStep::Disconnect => UiAction::PaneDisconnect(id),
                        LinkStep::Reconnect => UiAction::PaneReconnect(id),
                        LinkStep::Cancel => UiAction::PaneCancel(id),
                    };
                    at.ui_action(action);
                }
                self.bench.action(start.elapsed(), "link");
            },
            Some(Step::Shortcut(letter)) => {
                // What the keyboard's way through `window_event` does, short of the key itself,
                // which cannot be made up: a script has no way to build a winit `KeyEvent`.
                let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
                if !at.core.ui.locked()
                    && !at.win.chrome.has_keyboard()
                    && let Some(action) = action_for(Key::Character(letter.as_str()), mods)
                {
                    at.perform(action);
                }
            },
            Some(Step::CloseTab(index)) => {
                if let Some(id) = at.win.workspace.tabs().get(index).map(|tab| tab.id) {
                    let start = Instant::now();
                    at.ui_action(UiAction::TabClose(TabId(id)));
                    self.bench.action(start.elapsed(), "close-tab");
                }
            },
            Some(Step::Window(index)) => self.script_window = index,
            Some(Step::Form(protocol)) => {
                at.win.shell.dialog.open_form(protocol);
                at.win.chrome.invalidate();
                at.request_frame();
            },
            Some(Step::FilesKey(key, extend)) => {
                at.press_files_key(key, extend);
            },
            Some(Step::TabPoint(index)) => {
                let point = at.win.shell.tab_center(index);
                if let Some(point) = point {
                    let scale = f64::from(at.scale());
                    let position = PhysicalPosition::new(f64::from(point.x) * scale, f64::from(point.y) * scale);
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(event_loop, id, WindowEvent::CursorMoved { device_id, position });
                }
            },
            Some(Step::Point(target)) => {
                if let Some((x, y)) = at.script_point(&target) {
                    let position = PhysicalPosition::new(f64::from(x), f64::from(y));
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(event_loop, id, WindowEvent::CursorMoved { device_id, position });
                }
            },
            Some(Step::Shot(path)) => {
                at.win.gfx.shot = Some(Shot { path, full: true });
                at.redraw_all();
            },
            Some(Step::Peek(path)) => {
                at.win.gfx.shot = Some(Shot { path, full: false });
                if let Some(pane) = at.focused_mut() {
                    pane.dirty = true;
                }
                at.request_frame();
            },
            Some(Step::Input(input)) => {
                let id = at.win.gfx.window.id();
                self.window_event(event_loop, id, input);
            },
            Some(Step::Text(text)) => {
                at.win.chrome.push_event(egui::Event::Text(text));
            },
            Some(Step::Key(key)) => {
                for pressed in [true, false] {
                    at.win.chrome.push_event(egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
            },
        }
        self.bench.next_due()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_connect_reads_user_host_and_port() {
        let draft = quick_draft("alice@db.example.com:2222").unwrap();
        assert_eq!((draft.username.as_str(), draft.host.as_str(), draft.port.as_str()), ("alice", "db.example.com", "2222"));
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
