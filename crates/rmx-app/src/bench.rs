//! Measurements for `xtask bench`, and scripted actions to measure.
//!
//! `RMX_BENCH_OUT` names the report file. `RMX_BENCH_SCRIPT` is a comma separated list of steps,
//! one performed every `RMX_BENCH_STEP_MS`: `split-right`, `split-right-local`, `split-down`, `new-tab`, `next-tab`,
//! `close`, `zoom`, `left`, `right`, `up`, `down`, `palette`, `dialog`, `sidebar`, `wait`, `quit`,
//! `shot:<path>` to write a fully redrawn frame, and `peek:<path>` to write a frame as partial
//! redraws leave it. `move:<x>:<y>`, `press` and `release` drive the pointer, `text:<text>` types
//! into the focused field, `escape` and `enter` press those keys in it, and `preedit:<text>` shows
//! that text as the input method's.
//!
//! Steps driving the real interface, where fields are separated by `|` since `,` ends a step:
//! `vault-create:<password>[|<key file>]`, `vault-unlock:<password>[|<key file>]`, `vault-lock`,
//! `vault-create-plain`, for a database with no password that the user has agreed to,
//! `conn:<name>|<host>|<port>|<user>|<password>[|<jump>[|<forwards>]]`, `open:<label>`,
//! `dup:<label>`, `del:<label>`, `confirm`, `hostkey:always|once|reject`, `run:<shell line>`,
//! `resize:<width>x<height>`, `disconnect`, `reconnect`, `cancel`, `theme:<light|dark>`,
//! `conn-key:<name>|<host>|<port>|<user>|<key file>`, `answer:<text>`, `edit:<label>`,
//! `rename:<name>`, `menu:<label>`, `discard`,
//! `sync-set:<kind>|<host>|<port>|<user>|<secret>|<path>`, `sync-now`,
//! `join:<password>|<kind>|<host>|<port>|<user>|<secret>|<path>`,
//! `join-plain:<kind>|<host>|<port>|<user>|<secret>|<path>`, `import-ssh`, `settings`.
//!
//! `conn-as:<protocol>|<name>|<host>|<port>|<user>|<secret>|<jump>|<options>` saves any kind of
//! connection, where options are `;` separated from `native`, `keep`, `tls`, `nla` and `ftps`.
//! `open-split:<right|down>:<label>` opens one beside the focused pane. For remote desktops:
//! `keys:<text>` (`~` is Enter), `center`, `clip-set:<text>`, `clip-offer`, `salute` and
//! `scale-toggle`. For file panes, with `L` or `R` for the column: `files-goto:<side>|<path>`,
//! `files-select:<side>|<name>`, `files-send:<side>`, `files-point:<side>|<name>`,
//! `files-column:<side>`, `files-cancel:<name>`, `files-retry:<name>`, `files-policy:<index>`,
//! `files-open:<side>|<name>`, `files-compare:<local>|<remote>`, `files-close`, and `open-files`
//! from a shell. `vault-switch:<path>` goes to another vault.
//!
//! `await:<condition>` holds the script until the condition is true, failing the run after 30 s:
//! `setup`, `locked`, `unlocked`, `host-key`, `prompt`, `live`, `closed`, `failed`, `synced`,
//! `no-question`, `vault-error`, `remote-size`, `tree:<label>`, `no-tree:<label>`, `text:<substring>`, `panes:<count>`,
//! `display`, `display-fits`, `listed:<side>|<name>`, `at-path:<side>|<path>`,
//! `transfer:<name>|<state>`, `clipboard:<text>`, `notice:<text>`, `unencrypted`,
//! `sync-failed:<text>`. And
//! `idle:<ms>` measures how quiet the app stays for that long.
//!
//! `snippet-new:<name>|<body>` saves a snippet (`~` is a line break) and `snippets` opens its
//! dialog; `await:snippet:<name>` holds until it is known. `files-key:[+]<name>` presses `up`,
//! `down`, `pageup`, `pagedown`, `home`, `end`, `enter`, `back`, `tab` or `copy` in a file pane,
//! `+` being Shift. `await:awaiting` holds for a reopened pane left waiting for the user, and
//! `await:live-shown:<count>` for that many connected panes on screen.
//!
//! For more than one window: `win:<index>` makes the steps after it act on that window,
//! `tab-point:<index>` puts the pointer on a tab, and `await:windows:<count>` and
//! `await:tabs:<count>` hold for that many of them. A pointer at negative coordinates, as in
//! `move:-40:-40`, is outside the window it was pressed in. `tab-close:<index>` closes a tab as
//! its button does, and `shortcut:<letter>` is that letter with Ctrl+Shift, unless a field or
//! something floating has the keyboard.
//!
//! `RMX_BENCH_VAULT=off` puts the panes straight up, to measure them without the vault.
//!
//! `RMX_BENCH_SIZE` is the window size as `WIDTHxHEIGHT`. Frames before `RMX_BENCH_WARMUP` do not
//! count towards the allocation figures, which only hold once caches and buffers have filled.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rmx_layout::split::Axis;
use rmx_layout::workspace::Direction;
use rmx_ui::egui;
use winit::dpi::PhysicalPosition;
use winit::event::{DeviceId, ElementState, Ime, MouseButton, WindowEvent};

use rmx_ui::widgets::{FilesKey, AuthKind, ConnectionDraft, SettingsDraft, SyncDraft, SyncKind};
use rmx_render::probe::{PHASE_NAMES, PHASES, Spent};
use rmx_ui::{FileSide, FilesAction, Protocol, Sensitive, ThemeMode, UiAction};

use crate::app::Action;
use crate::gfx::{FrameStats, Phases};

mod gpu;
pub mod present;

const DEFAULT_STEP_MS: u64 = 400;
const DEFAULT_WARMUP: usize = 240;
/// How long `await:` holds the script at most.
const AWAIT_LIMIT: Duration = Duration::from_secs(30);
/// How many of the slowest frames are reported in full.
const TRACES: usize = 16;

thread_local! {
    // Constant and without a destructor, so that using it from the allocator cannot allocate.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    // Main thread time spent on remote desktops since the last frame, in microseconds.
    static REMOTE_US: Cell<u64> = const { Cell::new(0) };
}

/// The main thread spent `time` on a remote desktop's frames.
pub fn note_remote(time: Duration) {
    REMOTE_US.with(|us| us.set(us.get() + time.as_micros() as u64));
}

/// Counts the calling thread's heap allocations made through Rust, which leaves out C libraries
/// such as the GL driver.
pub struct Counting;

fn count() {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
}

// Safety: everything is passed on to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// The longest a new window took on the main thread, in microseconds.
static WINDOW_OPEN_MAX: AtomicU64 = AtomicU64::new(0);

/// Making a new window took `time` on the main thread.
pub fn note_window_open(time: Duration) {
    WINDOW_OPEN_MAX.fetch_max(time.as_micros() as u64, Ordering::Relaxed);
}

static ARGON2_RUNS: AtomicU64 = AtomicU64::new(0);
static CONNECT_RUNS: AtomicU64 = AtomicU64::new(0);
static SYNC_RUNS: AtomicU64 = AtomicU64::new(0);
static ON_MAIN: AtomicU64 = AtomicU64::new(0);

/// Slow work called `what` was started, on the main thread or not.
pub fn off_main(what: &str, on_main: bool) {
    let runs = match what {
        "argon2" => &ARGON2_RUNS,
        "connect" => &CONNECT_RUNS,
        _ => &SYNC_RUNS,
    };
    runs.fetch_add(1, Ordering::Relaxed);
    if on_main {
        ON_MAIN.fetch_add(1, Ordering::Relaxed);
    }
}

/// How long without input the vault locks, for testing that it does.
pub fn auto_lock() -> Option<Duration> {
    let ms = std::env::var("RMX_BENCH_AUTO_LOCK_MS").ok()?;
    Some(Duration::from_millis(ms.parse().ok()?))
}

/// Whether to measure the panes without a vault in front of them.
pub fn vaultless() -> bool {
    std::env::var("RMX_BENCH_VAULT").is_ok_and(|value| value == "off")
}

/// Samples of the main thread's program counter by CPU time, for `RMX_BENCH_SAMPLES`: which
/// library a spike is spent in, without tracing anything.
pub mod sampler {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const SLOTS: usize = 1 << 21;
    static SAMPLES: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static MAIN: AtomicU64 = AtomicU64::new(0);

    extern "C" fn on_tick(_: libc::c_int, _: *mut libc::siginfo_t, context: *mut libc::c_void) {
        // Only what is async-signal-safe: atomics and clock_gettime.
        if unsafe { libc::syscall(libc::SYS_gettid) } as u64 != MAIN.load(Ordering::Relaxed) {
            return;
        }
        let context = unsafe { &*context.cast::<libc::ucontext_t>() };
        let pc = context.uc_mcontext.gregs[libc::REG_RIP as usize] as u64;
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
        let at = now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1_000;
        let slot = NEXT.fetch_add(2, Ordering::Relaxed);
        if slot + 1 < SLOTS {
            SAMPLES[slot].store(at, Ordering::Relaxed);
            SAMPLES[slot + 1].store(pc, Ordering::Relaxed);
        }
    }

    /// Begin sampling the calling thread, if `RMX_BENCH_SAMPLES` names where the samples go.
    pub fn start() {
        if std::env::var_os("RMX_BENCH_SAMPLES").is_none() {
            return;
        }
        MAIN.store(unsafe { libc::syscall(libc::SYS_gettid) } as u64, Ordering::Relaxed);
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_tick as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigaction(libc::SIGPROF, &action, std::ptr::null_mut());
            // The thread's own CPU clock, so that other threads don't dilute the samples.
            let mut event: libc::sigevent = std::mem::zeroed();
            event.sigev_notify = libc::SIGEV_THREAD_ID;
            event.sigev_signo = libc::SIGPROF;
            event.sigev_notify_thread_id = libc::syscall(libc::SYS_gettid) as i32;
            let mut timer: libc::timer_t = std::ptr::null_mut();
            libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut event, &mut timer);
            let tick = libc::timespec { tv_sec: 0, tv_nsec: 250_000 };
            let spec = libc::itimerspec { it_interval: tick, it_value: tick };
            libc::timer_settime(timer, 0, &spec, std::ptr::null_mut());
        }
    }

    /// Write the samples, and the map of loaded libraries to make sense of them.
    pub fn write() {
        let Ok(path) = std::env::var("RMX_BENCH_SAMPLES") else { return };
        // The timer stops with the thread; a late sample lands past `count` and is ignored.
        let count = NEXT.load(Ordering::Relaxed).min(SLOTS) / 2;
        let mut text = String::new();
        for sample in 0..count {
            text.push_str(&format!(
                "{} {:x}\n",
                SAMPLES[sample * 2].load(Ordering::Relaxed),
                SAMPLES[sample * 2 + 1].load(Ordering::Relaxed),
            ));
        }
        let _ = std::fs::write(&path, text);
        let _ = std::fs::copy("/proc/self/maps", format!("{path}.maps"));
    }
}

static STARTED: OnceLock<Instant> = OnceLock::new();
static MARKS: Mutex<Vec<(&str, u128)>> = Mutex::new(Vec::new());

/// The process started at `at`.
pub fn start(at: Instant) {
    let _ = STARTED.set(at);
}

/// A stage of starting up has finished, on any thread.
pub fn mark(stage: &'static str) {
    if let (Some(started), Ok(mut marks)) = (STARTED.get(), MARKS.lock()) {
        marks.push((stage, started.elapsed().as_micros()));
    }
}

/// CPU time this thread has used so far, in microseconds. What a frame took beyond this it was
/// waiting: for the GPU, the compositor, or another process to get off the CPU.
pub use rmx_render::probe::thread_cpu_us;

/// How many allocations this thread has made so far.
pub fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

/// How long a tab let go outside its window waits to be claimed, if not as long as usual: a
/// script takes longer between steps than a compositor between events.
pub fn claim_wait() -> Option<Duration> {
    let ms = std::env::var("RMX_BENCH_CLAIM_MS").ok()?;
    Some(Duration::from_millis(ms.parse().ok()?))
}

/// The window size asked for, if any.
pub fn window_size() -> Option<(f64, f64)> {
    let size = std::env::var("RMX_BENCH_SIZE").ok()?;
    let (width, height) = size.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// How the control scene draws, if this run is one: `RMX_BENCH_CONTROL`, `tiny` or `full` for how
/// much of the surface each frame says has changed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Control {
    TinyDamage,
    FullDamage,
}

pub fn control() -> Option<Control> {
    match std::env::var("RMX_BENCH_CONTROL").ok()?.as_str() {
        "tiny" => Some(Control::TinyDamage),
        "full" => Some(Control::FullDamage),
        _ => None,
    }
}

/// A place on screen that scripts move the pointer to, wherever the layout has put it.
#[derive(Clone, Debug)]
pub enum Point {
    /// This far from where the pointer is, in pixels.
    By(f32, f32),
    /// A place in the pane at this index, as fractions of its size.
    Pane(usize, f32, f32),
    /// The empty part of the bar of the pane at this index.
    Bar(usize),
    /// A button on the bar of the pane at this index, counted from the right.
    BarButton(usize, usize),
    Divider(usize),
    SidebarEdge,
    FilesSplit,
    FilesQueue,
    /// The button folding the queue.
    FilesFold,
    MenuRow(String),
}

#[derive(Clone, Debug)]
pub enum Step {
    Wait,
    Quit,
    Point(Point),
    Action(Action),
    Shot(String),
    Peek(String),
    Input(WindowEvent),
    Text(String),
    Key(egui::Key),
    /// A step of the interface, named for the report.
    Ui(&'static str, UiAction),
    Await(Condition),
    Idle(Duration),
    Open(String),
    Delete(String),
    Duplicate(String),
    Run(String),
    Resize(f64, f64),
    Theme(ThemeMode),
    Link(LinkStep),
    /// Open the connection form on this saved connection.
    Edit(String),
    /// Save the open form under this name.
    Rename(String),
    Menu(String),
    /// Act as if a terminal lost output.
    Discard,
    /// Open this connection in a new pane next to the focused one.
    OpenSplit(Axis, String),
    /// Type this into the focused remote desktop.
    Keys(String),
    /// Put the pointer in the middle of the focused pane.
    Center,
    SetClipboard(String),
    OfferClipboard,
    Salute,
    ToggleScale,
    OpenFiles,
    FilesGoto(FileSide, String),
    FilesSelect(FileSide, String),
    /// Copy what is selected in the column to the other one, with the toolbar's button.
    FilesSend(FileSide),
    /// Put the pointer on this row of the column.
    FilesPoint(FileSide, String),
    /// Put the pointer in the middle of the column.
    FilesPointColumn(FileSide),
    FilesCancel(String),
    FilesRetry(String),
    FilesPolicy(usize),
    FilesBody(FilesAction),
    /// Press this key in the focused file pane, with Shift or not.
    FilesKey(FilesKey, bool),
    /// Show the connection form for this protocol.
    Form(Protocol),
    /// The steps after this act on the window at this index.
    Window(usize),
    /// Press this letter with Ctrl+Shift.
    Shortcut(String),
    /// Close the tab at this index of the strip, as its button does.
    CloseTab(usize),
    /// Put the pointer on the tab at this index of the strip.
    TabPoint(usize),
}

impl Step {
    /// What this step is, without what it types or sends.
    pub fn name(&self) -> String {
        match self {
            Step::Ui(what, _) => format!("ui:{what}"),
            Step::Action(action) => format!("{action:?}"),
            Step::Await(condition) => format!("await:{condition:?}"),
            other => {
                let text = format!("{other:?}");
                text.split(['(', ' ', '{']).next().unwrap_or("").to_owned()
            },
        }
    }
}

/// What a pane's connection is told.
#[derive(Clone, Copy, Debug)]
pub enum LinkStep {
    Disconnect,
    Reconnect,
    Cancel,
}

/// What `await:` waits for.
#[derive(Clone, Debug)]
pub enum Condition {
    /// The open database has no master password.
    Unencrypted,
    /// Sync failed saying this.
    SyncFailed(String),
    Setup,
    Locked,
    Unlocked,
    HostKey,
    Prompt,
    Live,
    Closed,
    Failed,
    Synced,
    NoQuestion,
    /// The connection form is showing.
    Dialog,
    /// The last unlock failed.
    VaultError,
    /// The shell printed `size-<lines> <columns>` for the size the terminal has now.
    RemoteSize,
    Tree(String),
    NoTree(String),
    Text(String),
    Panes(usize),
    /// So many windows are open.
    Windows(usize),
    /// The window acted on has so many tabs.
    Tabs(usize),
    /// A snippet of this name is known.
    Snippet(String),
    /// The focused pane is waiting for the user to ask for it to connect.
    Awaiting,
    /// The window acted on has so many panes on screen, each of them connected.
    LiveShown(usize),
    /// The focused remote desktop has drawn a picture.
    Display,
    /// The focused remote desktop has taken the size of its pane.
    DisplayFits,
    /// A column of the focused file pane lists this name.
    Listed(FileSide, String),
    /// A column of the focused file pane shows this directory.
    AtPath(FileSide, String),
    /// A transfer with this in its label has reached this state.
    Transfer(String, String),
    Clipboard(String),
    /// The active tab is zoomed, or not.
    Zoomed(bool),
    /// The context menu is open, or not.
    Menu(bool),
    /// The context menu has a row with this label.
    MenuOffers(String),
    /// The sidebar's width is between these, in points.
    Sidebar(f32, f32),
    FilesSplit(f32, f32),
    FilesQueue(f32, f32),
    FilesFolded(bool),
    /// The open file holds this.
    FilesText(String),
    /// A comparison of this many rows is on screen.
    FilesDiff(usize),
    /// No comparison is on screen.
    NoDiff,
    /// Neither a file nor a comparison is open.
    FilesColumns,
    /// The vault's path ends in this.
    VaultNamed(String),
    /// The first divider of the active tab has a ratio between these.
    Ratio(f32, f32),
    /// The first split of the active tab is along this axis.
    FirstSplit(Axis),
    TabName(String),
    /// The focused pane's notice says this.
    Notice(String),
}

fn file_side(text: &str) -> Option<FileSide> {
    match text {
        "L" => Some(FileSide::Local),
        "R" => Some(FileSide::Remote),
        _ => None,
    }
}

/// A pointer, for events made up here.
fn device() -> DeviceId {
    DeviceId::dummy()
}

fn button_event(state: ElementState) -> WindowEvent {
    WindowEvent::MouseInput { device_id: device(), state, button: MouseButton::Left }
}

fn right_event(state: ElementState) -> WindowEvent {
    WindowEvent::MouseInput { device_id: device(), state, button: MouseButton::Right }
}

fn modifiers_event(state: winit::keyboard::ModifiersState) -> WindowEvent {
    WindowEvent::ModifiersChanged(state.into())
}

/// `pointer-by:12:0`, `pane-at:0:0.5:0.5`, `bar:0`, `bar-button:0:1`, `divider:0`, `sidebar-edge`,
/// `files-split`, `files-queue` and `menu-row:Close`.
fn parse_point(step: &str) -> Option<Point> {
    if let Some(rest) = step.strip_prefix("pointer-by:") {
        let (dx, dy) = rest.split_once(':')?;
        return Some(Point::By(dx.parse().ok()?, dy.parse().ok()?));
    }
    if let Some(rest) = step.strip_prefix("pane-at:") {
        let mut parts = rest.split(':');
        return Some(Point::Pane(parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?));
    }
    if let Some(index) = step.strip_prefix("bar:") {
        return Some(Point::Bar(index.parse().ok()?));
    }
    if let Some(rest) = step.strip_prefix("bar-button:") {
        let (index, button) = rest.split_once(':')?;
        return Some(Point::BarButton(index.parse().ok()?, button.parse().ok()?));
    }
    if let Some(index) = step.strip_prefix("divider:") {
        return Some(Point::Divider(index.parse().ok()?));
    }
    if let Some(label) = step.strip_prefix("menu-row:") {
        return Some(Point::MenuRow(label.to_owned()));
    }
    match step {
        "sidebar-edge" => Some(Point::SidebarEdge),
        "files-split" => Some(Point::FilesSplit),
        "files-queue" => Some(Point::FilesQueue),
        "files-fold" => Some(Point::FilesFold),
        _ => None,
    }
}

fn parse(step: &str) -> Option<Step> {
    if let Some(point) = parse_point(step) {
        return Some(Step::Point(point));
    }
    if let Some(path) = step.strip_prefix("shot:") {
        return Some(Step::Shot(path.into()));
    }
    if let Some(path) = step.strip_prefix("peek:") {
        return Some(Step::Peek(path.into()));
    }
    if let Some(at) = step.strip_prefix("move:") {
        let (x, y) = at.split_once(':')?;
        let position = PhysicalPosition::new(x.parse().ok()?, y.parse().ok()?);
        return Some(Step::Input(WindowEvent::CursorMoved { device_id: device(), position }));
    }
    if let Some(text) = step.strip_prefix("text:") {
        return Some(Step::Text(text.into()));
    }
    if let Some(rest) = step.strip_prefix("files-key:") {
        let (name, extend) = match rest.strip_prefix('+') {
            Some(name) => (name, true),
            None => (rest, false),
        };
        let key = match name {
            "up" => FilesKey::Up,
            "down" => FilesKey::Down,
            "pageup" => FilesKey::PageUp,
            "pagedown" => FilesKey::PageDown,
            "home" => FilesKey::Home,
            "end" => FilesKey::End,
            "enter" => FilesKey::Open,
            "back" => FilesKey::Back,
            "tab" => FilesKey::Switch,
            "copy" => FilesKey::Copy,
            "escape" => FilesKey::Escape,
            _ => return None,
        };
        return Some(Step::FilesKey(key, extend));
    }
    if let Some(rest) = step.strip_prefix("snippet-new:") {
        let (name, body) = rest.split_once('|')?;
        let action = UiAction::SnippetSave { id: None, name: name.into(), body: body.replace('~', "\n") };
        return Some(Step::Ui("snippet-save", action));
    }
    if let Some(label) = step.strip_prefix("form:") {
        return Protocol::ALL.into_iter().find(|p| p.label() == label).map(Step::Form);
    }
    if let Some(index) = step.strip_prefix("win:") {
        return Some(Step::Window(index.parse().ok()?));
    }
    if let Some(letter) = step.strip_prefix("shortcut:") {
        return Some(Step::Shortcut(letter.into()));
    }
    if let Some(index) = step.strip_prefix("tab-close:") {
        return Some(Step::CloseTab(index.parse().ok()?));
    }
    if let Some(index) = step.strip_prefix("tab-point:") {
        return Some(Step::TabPoint(index.parse().ok()?));
    }
    if let Some(text) = step.strip_prefix("preedit:") {
        let cursor = Some((text.len(), text.len()));
        return Some(Step::Input(WindowEvent::Ime(Ime::Preedit(text.into(), cursor))));
    }
    if let Some(rest) = step.strip_prefix("await:") {
        return parse_condition(rest).map(Step::Await);
    }
    if let Some(ms) = step.strip_prefix("idle:") {
        return Some(Step::Idle(Duration::from_millis(ms.parse().ok()?)));
    }
    if let Some(step) = parse_interface_step(step) {
        return Some(step);
    }
    let action = match step {
        "wait" => return Some(Step::Wait),
        "quit" => return Some(Step::Quit),
        "press-right" => return Some(Step::Input(right_event(ElementState::Pressed))),
        "release-right" => return Some(Step::Input(right_event(ElementState::Released))),
        "shift-down" => return Some(Step::Input(modifiers_event(winit::keyboard::ModifiersState::SHIFT))),
        "shift-up" => return Some(Step::Input(modifiers_event(winit::keyboard::ModifiersState::empty()))),
        "split-right-local" => Action::SplitLocal(Axis::Row),
        "split-right" => Action::Split(Axis::Row),
        "split-down" => Action::Split(Axis::Column),
        "new-tab" => Action::NewTab,
        "next-tab" => Action::CycleTab(1),
        "close" => Action::ClosePane,
        "zoom" => Action::Zoom,
        "left" => Action::Focus(Direction::Left),
        "right" => Action::Focus(Direction::Right),
        "up" => Action::Focus(Direction::Up),
        "down" => Action::Focus(Direction::Down),
        "palette" => Action::QuickOpen,
        "snippets" => Action::Snippets,
        "dialog" => Action::NewConnection,
        "sidebar" => Action::ToggleSidebar,
        "escape" => return Some(Step::Key(egui::Key::Escape)),
        "enter" => return Some(Step::Key(egui::Key::Enter)),
        "press" => return Some(Step::Input(button_event(ElementState::Pressed))),
        "release" => return Some(Step::Input(button_event(ElementState::Released))),
        _ => return None,
    };
    Some(Step::Action(action))
}

fn parse_condition(text: &str) -> Option<Condition> {
    let range = |rest: &str| -> Option<(f32, f32)> {
        let (low, high) = rest.split_once(':')?;
        Some((low.parse().ok()?, high.parse().ok()?))
    };
    if let Some(label) = text.strip_prefix("menu-offers:") {
        return Some(Condition::MenuOffers(label.to_owned()));
    }
    if let Some(rest) = text.strip_prefix("sidebar:") {
        let (low, high) = range(rest)?;
        return Some(Condition::Sidebar(low, high));
    }
    if let Some(rest) = text.strip_prefix("files-split:") {
        let (low, high) = range(rest)?;
        return Some(Condition::FilesSplit(low, high));
    }
    if let Some(folded) = text.strip_prefix("files-folded:") {
        return Some(Condition::FilesFolded(folded == "1"));
    }
    if let Some(found) = text.strip_prefix("files-text:") {
        return Some(Condition::FilesText(found.to_owned()));
    }
    if let Some(rows) = text.strip_prefix("files-diff:") {
        return Some(Condition::FilesDiff(rows.parse().ok()?));
    }
    if text == "no-diff" {
        return Some(Condition::NoDiff);
    }
    if text == "files-columns" {
        return Some(Condition::FilesColumns);
    }
    if let Some(name) = text.strip_prefix("vault-named:") {
        return Some(Condition::VaultNamed(name.to_owned()));
    }
    if let Some(rest) = text.strip_prefix("files-queue:") {
        let (low, high) = range(rest)?;
        return Some(Condition::FilesQueue(low, high));
    }
    if let Some(rest) = text.strip_prefix("ratio:") {
        let (low, high) = range(rest)?;
        return Some(Condition::Ratio(low, high));
    }
    if let Some(name) = text.strip_prefix("tab-name:") {
        return Some(Condition::TabName(name.to_owned()));
    }
    match text {
        "zoomed" => return Some(Condition::Zoomed(true)),
        "not-zoomed" => return Some(Condition::Zoomed(false)),
        "menu" => return Some(Condition::Menu(true)),
        "no-menu" => return Some(Condition::Menu(false)),
        "split-row" => return Some(Condition::FirstSplit(Axis::Row)),
        "split-column" => return Some(Condition::FirstSplit(Axis::Column)),
        _ => (),
    }
    if let Some(label) = text.strip_prefix("tree:") {
        return Some(Condition::Tree(label.into()));
    }
    if let Some(label) = text.strip_prefix("no-tree:") {
        return Some(Condition::NoTree(label.into()));
    }
    if let Some(found) = text.strip_prefix("text:") {
        return Some(Condition::Text(found.into()));
    }
    if let Some(count) = text.strip_prefix("panes:") {
        return Some(Condition::Panes(count.parse().ok()?));
    }
    if let Some(count) = text.strip_prefix("windows:") {
        return Some(Condition::Windows(count.parse().ok()?));
    }
    if let Some(name) = text.strip_prefix("snippet:") {
        return Some(Condition::Snippet(name.into()));
    }
    if let Some(count) = text.strip_prefix("live-shown:") {
        return Some(Condition::LiveShown(count.parse().ok()?));
    }
    if text == "awaiting" {
        return Some(Condition::Awaiting);
    }
    if let Some(count) = text.strip_prefix("tabs:") {
        return Some(Condition::Tabs(count.parse().ok()?));
    }
    if let Some(rest) = text.strip_prefix("listed:") {
        let (side, name) = rest.split_once('|')?;
        return Some(Condition::Listed(file_side(side)?, name.into()));
    }
    if let Some(rest) = text.strip_prefix("at-path:") {
        let (side, path) = rest.split_once('|')?;
        return Some(Condition::AtPath(file_side(side)?, path.into()));
    }
    if let Some(rest) = text.strip_prefix("transfer:") {
        let (name, state) = rest.split_once('|')?;
        return Some(Condition::Transfer(name.into(), state.into()));
    }
    if let Some(found) = text.strip_prefix("sync-failed:") {
        return Some(Condition::SyncFailed(found.into()));
    }
    if let Some(found) = text.strip_prefix("clipboard:") {
        return Some(Condition::Clipboard(found.into()));
    }
    if let Some(found) = text.strip_prefix("notice:") {
        return Some(Condition::Notice(found.into()));
    }
    Some(match text {
        "unencrypted" => Condition::Unencrypted,
        "setup" => Condition::Setup,
        "locked" => Condition::Locked,
        "unlocked" => Condition::Unlocked,
        "host-key" => Condition::HostKey,
        "prompt" => Condition::Prompt,
        "live" => Condition::Live,
        "closed" => Condition::Closed,
        "failed" => Condition::Failed,
        "synced" => Condition::Synced,
        "no-question" => Condition::NoQuestion,
        "dialog" => Condition::Dialog,
        "vault-error" => Condition::VaultError,
        "remote-size" => Condition::RemoteSize,
        "display" => Condition::Display,
        "display-fits" => Condition::DisplayFits,
        _ => return None,
    })
}

fn sync_kind(name: &str) -> SyncKind {
    match name {
        "local" => SyncKind::Local,
        "webdav" => SyncKind::WebDav,
        "s3" => SyncKind::S3,
        "sftp" => SyncKind::Sftp,
        _ => SyncKind::None,
    }
}

fn sync_draft(fields: &[&str]) -> Option<SyncDraft> {
    let [kind, host, port, user, secret, path] = *fields else { return None };
    Some(SyncDraft {
        kind: sync_kind(kind),
        host: host.into(),
        port: port.into(),
        user: user.into(),
        secret: secret.into(),
        path: path.into(),
        ..SyncDraft::default()
    })
}

fn parse_interface_step(step: &str) -> Option<Step> {
    let password = |text: &str| Sensitive::new(text);
    if step == "vault-create-plain" {
        let action = UiAction::VaultCreate { password: password(""), key_file: String::new(), plain_ok: true };
        return Some(Step::Ui("vault-create", action));
    }
    if let Some(text) = step.strip_prefix("vault-create:") {
        let (text, key_file) = text.split_once('|').unwrap_or((text, ""));
        let action = UiAction::VaultCreate { password: password(text), key_file: key_file.into(), plain_ok: false };
        return Some(Step::Ui("vault-create", action));
    }
    if let Some(text) = step.strip_prefix("join-plain:") {
        let fields: Vec<&str> = text.split('|').collect();
        let sync = Box::new(sync_draft(&fields)?);
        let action = UiAction::VaultJoin { password: password(""), key_file: String::new(), sync, plain_ok: true };
        return Some(Step::Ui("vault-join", action));
    }
    if let Some(text) = step.strip_prefix("vault-unlock:") {
        let (text, key_file) = text.split_once('|').unwrap_or((text, ""));
        let action = UiAction::VaultUnlock { password: password(text), key_file: key_file.into() };
        return Some(Step::Ui("vault-unlock", action));
    }
    if let Some(text) = step.strip_prefix("join:") {
        let fields: Vec<&str> = text.split('|').collect();
        let (&secret, rest) = fields.split_first()?;
        let sync = Box::new(sync_draft(rest)?);
        let action = UiAction::VaultJoin { password: password(secret), key_file: String::new(), sync, plain_ok: false };
        return Some(Step::Ui("vault-join", action));
    }
    if let Some(text) = step.strip_prefix("conn:") {
        let fields: Vec<&str> = text.split('|').collect();
        let [name, host, port, user, secret, ..] = *fields else { return None };
        let mut draft = ConnectionDraft::new(Protocol::Ssh);
        draft.name = name.into();
        draft.host = host.into();
        draft.port = port.into();
        draft.username = user.into();
        draft.password = secret.into();
        draft.auth = AuthKind::Password;
        draft.jump = fields.get(5).unwrap_or(&"").to_string();
        draft.forwards = fields.get(6).unwrap_or(&"").to_string();
        return Some(Step::Ui("connection-save", UiAction::ConnectionSubmit(Box::new(draft))));
    }
    if let Some(text) = step.strip_prefix("conn-as:") {
        let fields: Vec<&str> = text.split('|').collect();
        let [protocol, name, host, port, user, secret, jump, options] = *fields else { return None };
        let protocol = Protocol::ALL.into_iter().find(|p| p.label().eq_ignore_ascii_case(protocol))?;
        let mut draft = ConnectionDraft::new(protocol);
        draft.name = name.into();
        draft.host = host.into();
        draft.port = port.into();
        draft.username = user.into();
        draft.password = secret.into();
        draft.jump = jump.into();
        if protocol == Protocol::Serial {
            draft.device = host.into();
            if !port.is_empty() {
                draft.baud = port.into();
            }
        }
        for option in options.split(';') {
            match option {
                "native" => draft.scale = rmx_ui::widgets::ScaleChoice::Native,
                "keep" => draft.follow_size = false,
                "tls" => draft.rdp_security = rmx_ui::widgets::RdpSecurity::Tls,
                "nla" => draft.rdp_security = rmx_ui::widgets::RdpSecurity::Nla,
                "ftps" => draft.ftps = true,
                "" => (),
                _ => return None,
            }
        }
        return Some(Step::Ui("connection-save", UiAction::ConnectionSubmit(Box::new(draft))));
    }
    if let Some(rest) = step.strip_prefix("open-split:") {
        let (axis, label) = rest.split_once(':')?;
        let axis = if axis == "down" { Axis::Column } else { Axis::Row };
        return Some(Step::OpenSplit(axis, label.into()));
    }
    if let Some(text) = step.strip_prefix("keys:") {
        return Some(Step::Keys(text.replace('~', "\n")));
    }
    if let Some(text) = step.strip_prefix("clip-set:") {
        return Some(Step::SetClipboard(text.into()));
    }
    if let Some(rest) = step.strip_prefix("files-goto:") {
        let (side, path) = rest.split_once('|')?;
        return Some(Step::FilesGoto(file_side(side)?, path.into()));
    }
    if let Some(rest) = step.strip_prefix("files-select:") {
        let (side, name) = rest.split_once('|')?;
        return Some(Step::FilesSelect(file_side(side)?, name.into()));
    }
    if let Some(side) = step.strip_prefix("files-send:") {
        return Some(Step::FilesSend(file_side(side)?));
    }
    if let Some(rest) = step.strip_prefix("files-point:") {
        let (side, name) = rest.split_once('|')?;
        return Some(Step::FilesPoint(file_side(side)?, name.into()));
    }
    if let Some(side) = step.strip_prefix("files-column:") {
        return Some(Step::FilesPointColumn(file_side(side)?));
    }
    if let Some(name) = step.strip_prefix("files-cancel:") {
        return Some(Step::FilesCancel(name.into()));
    }
    if let Some(name) = step.strip_prefix("files-retry:") {
        return Some(Step::FilesRetry(name.into()));
    }
    if let Some(rest) = step.strip_prefix("files-open:") {
        let (side, name) = rest.split_once('|')?;
        return Some(Step::FilesBody(FilesAction::Open { side: file_side(side)?, name: name.into() }));
    }
    if let Some(rest) = step.strip_prefix("files-compare:") {
        let (local, remote) = rest.split_once('|')?;
        return Some(Step::FilesBody(FilesAction::Compare { local: local.into(), remote: remote.into() }));
    }
    if step == "files-close" {
        return Some(Step::FilesBody(FilesAction::CloseBody));
    }
    if let Some(path) = step.strip_prefix("vault-switch:") {
        return Some(Step::Ui("vault-switch", UiAction::VaultSwitch(path.to_owned())));
    }
    if let Some(index) = step.strip_prefix("files-policy:") {
        return Some(Step::FilesPolicy(index.parse().ok()?));
    }
    if let Some(text) = step.strip_prefix("sync-set:") {
        let fields: Vec<&str> = text.split('|').collect();
        let draft = SettingsDraft {
            theme: ThemeMode::Dark,
            auto_lock_minutes: "15".into(),
            sync: sync_draft(&fields)?,
            sync_on_save: true,
            sync_interval_minutes: String::new(),
        };
        return Some(Step::Ui("settings-save", UiAction::SettingsSave(Box::new(draft))));
    }
    if let Some(label) = step.strip_prefix("open:") {
        return Some(Step::Open(label.into()));
    }
    if let Some(label) = step.strip_prefix("dup:") {
        return Some(Step::Duplicate(label.into()));
    }
    if let Some(label) = step.strip_prefix("del:") {
        return Some(Step::Delete(label.into()));
    }
    if let Some(line) = step.strip_prefix("run:") {
        return Some(Step::Run(line.into()));
    }
    if let Some(answer) = step.strip_prefix("answer:") {
        let fields = answer.split('|').map(Sensitive::new).collect();
        return Some(Step::Ui("answer", UiAction::PromptSubmit(fields)));
    }
    if let Some(label) = step.strip_prefix("edit:") {
        return Some(Step::Edit(label.into()));
    }
    if let Some(name) = step.strip_prefix("rename:") {
        return Some(Step::Rename(name.into()));
    }
    if let Some(label) = step.strip_prefix("menu:") {
        return Some(Step::Menu(label.into()));
    }
    if let Some(text) = step.strip_prefix("conn-key:") {
        let fields: Vec<&str> = text.split('|').collect();
        let [name, host, port, user, key] = *fields else { return None };
        let mut draft = ConnectionDraft::new(Protocol::Ssh);
        draft.name = name.into();
        draft.host = host.into();
        draft.port = port.into();
        draft.username = user.into();
        draft.auth = AuthKind::Key;
        draft.key_path = key.into();
        return Some(Step::Ui("connection-save", UiAction::ConnectionSubmit(Box::new(draft))));
    }
    if let Some(size) = step.strip_prefix("resize:") {
        let (width, height) = size.split_once('x')?;
        return Some(Step::Resize(width.parse().ok()?, height.parse().ok()?));
    }
    if let Some(mode) = step.strip_prefix("theme:") {
        return Some(Step::Theme(if mode == "light" { ThemeMode::Light } else { ThemeMode::Dark }));
    }
    if let Some(answer) = step.strip_prefix("hostkey:") {
        let action = match answer {
            "always" => UiAction::HostKeyAcceptAlways,
            "once" => UiAction::HostKeyAcceptOnce,
            _ => UiAction::HostKeyReject,
        };
        return Some(Step::Ui("host-key", action));
    }
    Some(match step {
        "vault-lock" => Step::Ui("vault-lock", UiAction::VaultLock),
        "sync-now" => Step::Ui("sync-now", UiAction::SyncNow),
        "import-ssh" => Step::Ui("import-ssh", UiAction::ImportSshConfig),
        "settings" => Step::Ui("settings", UiAction::OpenSettings),
        "confirm" => Step::Ui("confirm", UiAction::PromptSubmit(Vec::new())),
        "disconnect" => Step::Link(LinkStep::Disconnect),
        "reconnect" => Step::Link(LinkStep::Reconnect),
        "cancel" => Step::Link(LinkStep::Cancel),
        "discard" => Step::Discard,
        "center" => Step::Center,
        "clip-offer" => Step::OfferClipboard,
        "salute" => Step::Salute,
        "scale-toggle" => Step::ToggleScale,
        "open-files" => Step::OpenFiles,
        _ => return None,
    })
}

#[derive(Default)]
pub struct Recorder {
    first_frame: Option<Duration>,
    first_frame_at: Option<Instant>,
    frames: Vec<u32>,
    /// What of each frame was spent on the CPU, not waiting.
    on_cpu: Vec<u32>,
    /// The slowest frame: its index, and its snapshot, draw and swap times.
    slowest: (usize, Phases),
    slowest_us: u32,
    slowest_on_cpu: u32,
    worst: Phases,
    action_max_us: u32,
    action_slowest: String,
    /// Times of each phase, for its median.
    phase_times: [Vec<u32>; 3],
    /// Drawing's allocations made inside winit, requesting the frame callback: not ours to avoid.
    winit_allocations: u64,
    warmup: usize,
    /// Frames that redrew everything, and those that did not.
    full_frames: u32,
    partial_frames: u32,
    /// Over the partial frames: panes on screen, and panes redrawn.
    partial_shown: u64,
    partial_drawn: u64,
    /// Panes drawn as they last were because their terminal's lock was held, over all frames.
    stale_panes: u64,
    drawn_panes: u64,
    max_age: u32,
    /// After the warmup: allocations made drawing frames, and in the worst frame.
    draw_allocations: u64,
    draw_allocations_max: u64,
    steady_frames: u32,
    /// After the warmup: allocations of the main thread between frames no action came between.
    loop_allocations: u64,
    loop_frames: u32,
    /// Allocations seen at the end of the last frame.
    last_allocations: u64,
    /// Whether an action ran since the last frame, which legitimately allocates.
    action_since_frame: bool,
    /// Frames that ran egui, whose layout and tessellation times these are, in microseconds.
    ui_times: Vec<u32>,
    /// Frames that ran egui, and what they allocated, after the warmup.
    ui_frames: u32,
    ui_allocations: u64,
    /// The slowest frames: time, index, phases, and the layout and tessellation of their egui
    /// pass.
    slow_frames: Vec<(u32, usize, Phases, (u32, u32))>,
    /// egui passes in frames that only had terminal output to show: their full redraws don't
    /// count, as a pass that changes nothing never forces one.
    ui_passes_on_partial: u32,
    /// What draws, because headless runs may not have a GPU.
    pub renderer: String,
    script: VecDeque<Step>,
    step: Duration,
    due: Option<Instant>,
    /// What the script is holding for, and until when.
    waiting: Option<(Condition, Instant)>,
    /// The condition the script gave up on, if it did.
    script_failed: Option<String>,
    /// When the quiet period being measured ends.
    idle_until: Option<Instant>,
    idle_frames: u32,
    idle_wakeups: u32,
    /// Whether a quiet period was measured.
    idle_measured: bool,
    /// Main thread time spent on remote desktops per frame that had any, in microseconds.
    remote_times: Vec<u32>,
    /// Each frame's time in each phase, by the wall clock.
    phase_walls: Vec<[u32; PHASES]>,
    /// The slowest frames, in full.
    traces: Vec<Trace>,
    /// Every frame's wall, CPU, run queue wait, and wall and CPU per phase, for `RMX_BENCH_FRAMES`.
    frame_rows: Vec<(u32, u32, u32, [Spent; PHASES])>,
    /// What each frame did to the scheduler, summed over the run.
    sched_totals: Sched,
    /// The action since the last frame, if any.
    last_action: String,
}

/// How the scheduler has treated this thread so far.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sched {
    /// Microseconds spent runnable while another took the CPU.
    run_wait_us: u64,
    /// Times this thread went to sleep, and times it was put off the CPU.
    voluntary: u64,
    involuntary: u64,
    /// Page faults served without I/O.
    minor_faults: u64,
}

impl Sched {
    fn since(self, before: Self) -> Self {
        Self {
            run_wait_us: self.run_wait_us - before.run_wait_us,
            voluntary: self.voluntary - before.voluntary,
            involuntary: self.involuntary - before.involuntary,
            minor_faults: self.minor_faults - before.minor_faults,
        }
    }
}

/// The calling thread's scheduling so far. Reads `/proc/thread-self/schedstat`, whose second field
/// is the time spent on a run queue, and `getrusage` for the context switches.
pub fn sched() -> Sched {
    use std::os::unix::fs::FileExt;

    thread_local! {
        static SCHEDSTAT: Option<std::fs::File> = std::fs::File::open("/proc/thread-self/schedstat").ok();
    }

    let mut buffer = [0u8; 96];
    let read = SCHEDSTAT.with(|file| file.as_ref().map_or(0, |file| file.read_at(&mut buffer, 0).unwrap_or(0)));
    let text = std::str::from_utf8(&buffer[..read]).unwrap_or("");
    let run_wait_ns: u64 = text.split(' ').nth(1).and_then(|field| field.parse().ok()).unwrap_or(0);

    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) };
    Sched {
        run_wait_us: run_wait_ns / 1_000,
        voluntary: usage.ru_nvcsw as u64,
        involuntary: usage.ru_nivcsw as u64,
        minor_faults: usage.ru_minflt as u64,
    }
}

/// Everything known about one frame.
#[derive(Clone, Debug)]
struct Trace {
    wall_us: u32,
    index: usize,
    at_ms: u32,
    on_cpu_us: u32,
    sched: Sched,
    stats: FrameStats,
    ui: Option<(u32, u32)>,
    phases: [Spent; PHASES],
    action: String,
    /// When the frame ended, in microseconds since the epoch, to line up with outside traces.
    end_us: u64,
}

impl Trace {
    fn line(&self) -> String {
        let mut line = format!(
            "wall={} on_cpu={} run_wait={} off_cpu_blocked={} vcs={} ics={} minflt={} end_us={} frame={} at_ms={} full={} shown={} drawn={} stale={} age={} ui={:?} action={:?} |",
            self.wall_us,
            self.on_cpu_us,
            self.sched.run_wait_us,
            self.wall_us.saturating_sub(self.on_cpu_us + self.sched.run_wait_us as u32),
            self.sched.voluntary,
            self.sched.involuntary,
            self.sched.minor_faults,
            self.end_us,
            self.index,
            self.at_ms,
            !self.stats.partial,
            self.stats.shown,
            self.stats.drawn,
            self.stats.stale,
            self.stats.age,
            self.ui,
            self.action,
        );
        for (name, spent) in PHASE_NAMES.iter().zip(self.phases) {
            if spent.wall > 0 || spent.cpu > 0 {
                line.push_str(&format!(" {name}={}/{}", spent.wall, spent.cpu));
            }
        }
        line
    }
}

impl Recorder {
    pub fn from_env() -> Self {
        let script = std::env::var("RMX_BENCH_SCRIPT").unwrap_or_default();
        let step = std::env::var("RMX_BENCH_STEP_MS").ok().and_then(|ms| ms.parse().ok());
        Self {
            script: script.split(',').filter_map(parse).collect(),
            step: Duration::from_millis(step.unwrap_or(DEFAULT_STEP_MS)),
            warmup: std::env::var("RMX_BENCH_WARMUP")
                .ok()
                .and_then(|frames| frames.parse().ok())
                .unwrap_or(DEFAULT_WARMUP),
            ..Self::default()
        }
    }

    /// The step to perform at `now`, if one is due.
    pub fn due(&mut self, now: Instant) -> Option<Step> {
        let due = *self.due.get_or_insert(now + self.step);
        if now < due {
            return None;
        }
        self.due = Some(now + self.step);
        self.script.pop_front()
    }

    /// When the next step is due, if there is one.
    pub fn next_due(&self) -> Option<Instant> {
        if self.script.is_empty() { None } else { self.due }
    }

    /// Hold the script until `condition` holds.
    pub fn hold(&mut self, condition: Condition, now: Instant) {
        self.waiting = Some((condition, now + AWAIT_LIMIT));
    }

    /// What the script is holding for.
    pub fn waiting(&self) -> Option<&Condition> {
        self.waiting.as_ref().map(|(condition, _)| condition)
    }

    /// The hold is over: go on at once.
    pub fn release(&mut self, now: Instant) {
        self.waiting = None;
        self.due = Some(now);
    }

    /// Whether the hold has gone on too long, in which case the run is a failure.
    pub fn hold_expired(&mut self, now: Instant) -> bool {
        match &self.waiting {
            Some((condition, limit)) if *limit <= now => {
                self.script_failed = Some(format!("{condition:?}"));
                true
            },
            _ => false,
        }
    }

    /// Start measuring how quiet the application stays for `length`.
    pub fn start_idle(&mut self, now: Instant, length: Duration) {
        self.idle_until = Some(now + length);
        self.idle_frames = 0;
        self.idle_wakeups = 0;
        self.idle_measured = true;
        self.due = self.idle_until;
    }

    /// The event loop woke at `now`.
    pub fn wakeup(&mut self, now: Instant) {
        match self.idle_until {
            Some(end) if now < end => self.idle_wakeups += 1,
            Some(_) => self.idle_until = None,
            None => (),
        }
    }

    /// An action took `time` on the main thread.
    pub fn action(&mut self, time: Duration, what: &str) {
        let micros = time.as_micros() as u32;
        if micros > self.action_max_us {
            self.action_slowest = what.to_owned();
        }
        self.action_max_us = self.action_max_us.max(micros);
        self.action_since_frame = true;
        self.last_action.clear();
        self.last_action.push_str(what);
    }

    /// A frame took `cpu` on the main thread, and `allocations` allocations.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        cpu: Duration,
        on_cpu_us: u64,
        started: Instant,
        phases: Phases,
        stats: FrameStats,
        (allocations, winit_allocations, ui): (u64, u64, Option<rmx_ui::FrameStats>),
        sched_before: Sched,
        spent: [Spent; PHASES],
    ) {
        let sched = sched().since(sched_before);
        self.sched_totals.run_wait_us += sched.run_wait_us;
        self.sched_totals.voluntary += sched.voluntary;
        self.sched_totals.involuntary += sched.involuntary;
        self.phase_walls.push(spent.map(|phase| phase.wall));
        self.frame_rows.push((cpu.as_micros() as u32, on_cpu_us as u32, sched.run_wait_us as u32, spent));
        if self.idle_until.is_some_and(|end| Instant::now() < end) {
            self.idle_frames += 1;
        }
        let remote = REMOTE_US.with(|us| us.replace(0));
        if remote > 0 {
            self.remote_times.push(remote as u32);
        }
        if let Some(ui) = ui {
            self.ui_times.push((ui.layout + ui.tessellate).as_micros() as u32);
            self.ui_passes_on_partial += u32::from(stats.partial);
        }
        for (times, phase) in self.phase_times.iter_mut().zip(phases) {
            times.push(phase);
        }
        let allocations = allocations - winit_allocations;
        if stats.partial {
            self.partial_frames += 1;
            self.partial_shown += u64::from(stats.shown);
            self.partial_drawn += u64::from(stats.drawn);
        } else {
            self.full_frames += 1;
        }
        self.max_age = self.max_age.max(stats.age);
        self.stale_panes += u64::from(stats.stale);
        self.drawn_panes += u64::from(stats.drawn);

        let total = self::allocations();
        if self.frames.len() >= self.warmup {
            self.winit_allocations += winit_allocations;
            if ui.is_some() {
                self.ui_frames += 1;
                self.ui_allocations += allocations;
            } else {
                // Only frames that replay the interface can hold the zero: egui allocates.
                self.draw_allocations += allocations;
                self.draw_allocations_max = self.draw_allocations_max.max(allocations);
                self.steady_frames += 1;
                if !self.action_since_frame {
                    self.loop_allocations += total - self.last_allocations;
                    self.loop_frames += 1;
                }
            }
        }
        self.last_allocations = total;
        self.action_since_frame = false;

        self.first_frame.get_or_insert_with(|| started.elapsed());
        self.first_frame_at.get_or_insert_with(Instant::now);
        let micros = cpu.as_micros() as u32;
        if micros > self.slowest_us {
            self.slowest_us = micros;
            self.slowest = (self.frames.len(), phases);
            self.slowest_on_cpu = on_cpu_us as u32;
        }
        if !self.frames.is_empty() {
            for (worst, phase) in self.worst.iter_mut().zip(phases) {
                *worst = (*worst).max(phase);
            }
        }
        let ui_times = ui.map_or((0, 0), |ui| (ui.layout.as_micros() as u32, ui.tessellate.as_micros() as u32));
        self.slow_frames.push((micros, self.frames.len(), phases, ui_times));
        self.slow_frames.sort_unstable_by_key(|frame| std::cmp::Reverse(frame.0));
        self.slow_frames.truncate(6);
        let at_ms = self.first_frame_at.map_or(0, |at| at.elapsed().as_millis() as u32);
        self.traces.push(Trace {
            wall_us: micros,
            index: self.frames.len(),
            at_ms,
            on_cpu_us: on_cpu_us as u32,
            sched,
            stats,
            ui: ui.map(|ui| (ui.layout.as_micros() as u32, ui.tessellate.as_micros() as u32)),
            phases: spent,
            action: std::mem::take(&mut self.last_action),
            end_us: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_micros() as u64),
        });
        self.traces.sort_unstable_by_key(|trace| std::cmp::Reverse(trace.wall_us));
        self.traces.truncate(TRACES);
        self.frames.push(micros);
        self.on_cpu.push(on_cpu_us as u32);
    }

    pub fn write(&self) {
        let Ok(path) = std::env::var("RMX_BENCH_OUT") else { return };
        sampler::write();

        let mut frames = self.frames.clone();
        frames.sort_unstable();
        let at = |q: f64| {
            let index = ((frames.len() as f64 - 1.) * q).round() as usize;
            frames.get(index).copied().unwrap_or(0)
        };
        let median = |times: &[u32]| {
            let mut times = times.to_vec();
            times.sort_unstable();
            times.get(times.len() / 2).copied().unwrap_or(0)
        };
        let phase_p50 = [
            median(&self.phase_times[0]),
            median(&self.phase_times[1]),
            median(&self.phase_times[2]),
        ];
        let mut settled = self.frames[self.warmup.min(self.frames.len())..].to_vec();
        settled.sort_unstable();
        let settled_p99 = settled
            .get(((settled.len().max(1) - 1) as f64 * 0.99).round() as usize)
            .copied()
            .unwrap_or(0);
        let settled_max = settled.last().copied().unwrap_or(0);
        let mut on_cpu = self.on_cpu.clone();
        on_cpu.sort_unstable();
        let on_cpu_p99 = on_cpu.get((on_cpu.len().max(1) - 1) * 99 / 100).copied().unwrap_or(0);
        let on_cpu_max = on_cpu.last().copied().unwrap_or(0);
        let mut ui_times = self.ui_times.clone();
        ui_times.sort_unstable();
        let mut remote_times = self.remote_times.clone();
        remote_times.sort_unstable();
        let run = self.first_frame_at.map_or(Duration::ZERO, |at| at.elapsed());

        let report = format!(
            "gl={}\nfirst_frame_us={}\nframes={}\nframe_p50_us={}\nframe_p99_us={}\nframe_max_us={}\n\
             run_us={}\nsettled_p99_us={}\nsettled_max_us={}\naction_max_us={}\nfull_frames={}\npartial_frames={}\n\
             partial_shown={}\npartial_drawn={}\nstale_panes={}\ndrawn_panes={}\nmax_buffer_age={}\nsteady_frames={}\n\
             draw_allocations={}\ndraw_allocations_max={}\nloop_frames={}\n\
             loop_allocations={}\nslowest_index={}\nslowest_phases={:?}\nworst_phases={:?}\n\
             phase_p50={:?}\naction_slowest={}\nwinit_allocations={}\nstartup={:?}\n\
             on_cpu_p99_us={}\non_cpu_max_us={}\nslowest_on_cpu_us={}\nui_passes={}\nui_p99_us={}\n\
             ui_max_us={}\nui_passes_on_partial={}\nui_allocations={}\nui_frames={}\nslow_frames={:?}\n\
             argon2_runs={}\nconnect_runs={}\nsync_runs={}\non_main_violations={}\nidle_measured={}\n\
             idle_frames={}\nidle_wakeups={}\nscript_ok={}\nscript_failed={}\nremote_frames={}\n\
             remote_p99_us={}\nremote_max_us={}\nwindow_open_us={}\n",
            self.renderer,
            self.first_frame.unwrap_or_default().as_micros(),
            frames.len(),
            at(0.5),
            at(0.99),
            frames.last().copied().unwrap_or(0),
            run.as_micros(),
            settled_p99,
            settled_max,
            self.action_max_us,
            self.full_frames,
            self.partial_frames,
            self.partial_shown,
            self.partial_drawn,
            self.stale_panes,
            self.drawn_panes,
            self.max_age,
            self.steady_frames,
            self.draw_allocations,
            self.draw_allocations_max,
            self.loop_frames,
            self.loop_allocations,
            self.slowest.0,
            self.slowest.1,
            self.worst,
            phase_p50,
            self.action_slowest,
            self.winit_allocations,
            MARKS.lock().map(|marks| marks.clone()).unwrap_or_default(),
            on_cpu_p99,
            on_cpu_max,
            self.slowest_on_cpu,
            ui_times.len(),
            ui_times.get((ui_times.len().max(1) - 1) * 99 / 100).copied().unwrap_or(0),
            ui_times.last().copied().unwrap_or(0),
            self.ui_passes_on_partial,
            self.ui_allocations,
            self.ui_frames,
            self.slow_frames,
            ARGON2_RUNS.load(Ordering::Relaxed),
            CONNECT_RUNS.load(Ordering::Relaxed),
            SYNC_RUNS.load(Ordering::Relaxed),
            ON_MAIN.load(Ordering::Relaxed),
            u32::from(self.idle_measured),
            self.idle_frames,
            self.idle_wakeups,
            u32::from(self.script_failed.is_none()),
            self.script_failed.as_deref().unwrap_or(""),
            remote_times.len(),
            remote_times.get((remote_times.len().max(1) - 1) * 99 / 100).copied().unwrap_or(0),
            remote_times.last().copied().unwrap_or(0),
            WINDOW_OPEN_MAX.load(Ordering::Relaxed),
        );
        let phase_stats = (0..PHASES)
            .map(|phase| {
                let mut times: Vec<u32> = self.phase_walls.iter().map(|walls| walls[phase]).collect();
                times.sort_unstable();
                let at = |q: f64| times.get(((times.len().max(1) - 1) as f64 * q).round() as usize).copied().unwrap_or(0);
                format!("{}:{}/{}/{}", PHASE_NAMES[phase], at(0.5), at(0.99), at(1.))
            })
            .collect::<Vec<_>>()
            .join(" ");
        let over = |limit: u32| self.frames.iter().filter(|&&micros| micros > limit).count();
        let traces: String = self.traces.iter().map(|trace| format!("trace={}\n", trace.line())).collect();
        let report = format!(
            "{report}phase_stats={phase_stats}\nover_3000={}\nover_4000={}\nsched_totals={:?}\n{traces}",
            over(3000),
            over(4000),
            self.sched_totals,
        );
        let report = format!("{report}{}", present::report(&self.frames, self.warmup));
        if let Ok(rows) = std::env::var("RMX_BENCH_FRAMES") {
            let header: Vec<String> = PHASE_NAMES.iter().flat_map(|n| [format!("{n}_w"), format!("{n}_c")]).collect();
            let mut text = format!("wall,cpu,run_wait,{}\n", header.join(","));
            for (wall, cpu, wait, spent) in &self.frame_rows {
                let phases: Vec<String> = spent.iter().flat_map(|s| [s.wall.to_string(), s.cpu.to_string()]).collect();
                text.push_str(&format!("{wall},{cpu},{wait},{}\n", phases.join(",")));
            }
            let _ = std::fs::write(rows, text);
        }
        if let Err(err) = std::fs::write(&path, report) {
            eprintln!("rmx: could not write {path}: {err}");
        }
    }
}
