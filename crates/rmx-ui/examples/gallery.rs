//! Every widget, in either theme, on a real GL context.
//!
//! Run only inside `scripts/headless.sh`. Controlled by environment variables:
//!
//! - `RMX_GALLERY_THEME`: `dark` (default) or `light`
//! - `RMX_GALLERY_PAGE`: `shell` (default), `board`, `dialog`, `palette`, `hostkey` or `vault`
//! - `RMX_GALLERY_SCALE`: points to pixels, default 1
//! - `RMX_GALLERY_SHOT`: once the frame is settled, write it to this PPM and exit
//! - `RMX_GALLERY_BENCH`: run the measurements, print them and exit

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs};

use egui::{Event, Id, Pos2, Rect, Ui, UiBuilder, pos2, vec2};
use egui_glow::glow::{self, HasContext};
use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, Version,
};
use glutin::display::{Display, DisplayApiPreference, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, WindowSurface};
use rmx_ui::theme::{self, CONTROL_HEIGHT, PANE_BAR_HEIGHT, SIDEBAR_WIDTH, STRIP_HEIGHT};
use rmx_ui::widgets::{
    ButtonKind, ContextMenu, HostKeyStatus, HostKeyView, InputOptions, MenuItem,
    NewConnectionState, NodeKind, PaneBarView, PaneStatus, Reveal, SessionDot, QuickItem, QuickOpenState,
    TabStripState, TabView, TitleBarView, TreeModel, TreeNode, TreeState, VaultState, VaultView,
    button, button_full, context_menu, host_key_body, host_key_dialog, icon_button,
    new_connection_body, new_connection_dialog, pane_bar, quick_open, quick_open_body,
    segmented, sidebar, text_input, title_bar, vault_unlock,
};
use rmx_ui::{Chrome, NodeId, PaneId, Protocol, TabId, ThemeMode, UiAction, icons};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{Window, WindowId};

struct Counting;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Shell,
    Board,
    Dialog,
    Palette,
    HostKey,
    Vault,
}

fn sample_tree() -> Vec<TreeNode> {
    let folder = |id, depth, label: &str, expanded| TreeNode {
        id: NodeId(id),
        depth,
        kind: NodeKind::Folder,
        label: label.into(),
        state: SessionDot::None,
        expanded,
    };
    let conn = |id, depth, protocol, label: &str, live| TreeNode {
        id: NodeId(id),
        depth,
        kind: NodeKind::Connection(protocol),
        label: label.into(),
        state: if live { SessionDot::Live } else { SessionDot::None },
        expanded: false,
    };
    vec![
        folder(1, 0, "Production", true),
        conn(2, 1, Protocol::Ssh, "web-01.prod.internal", true),
        conn(3, 1, Protocol::Ssh, "web-02.prod.internal", true),
        conn(4, 1, Protocol::Ssh, "db-primary", false),
        conn(5, 1, Protocol::Sftp, "backups", false),
        folder(6, 1, "Windows", true),
        conn(7, 2, Protocol::Rdp, "dc-01.corp.example", false),
        conn(8, 2, Protocol::Vnc, "kiosk-lobby", true),
        folder(9, 0, "Staging", false),
        folder(10, 0, "Lab", true),
        conn(11, 1, Protocol::Serial, "switch console", false),
        conn(12, 1, Protocol::Telnet, "old-router", false),
        conn(13, 1, Protocol::Ftp, "firmware-mirror", false),
        conn(14, 1, Protocol::Ssh, "实验室服务器 跳板机", true),
        conn(
            15,
            0,
            Protocol::Ssh,
            "a-rather-long-hostname-that-will-not-fit-in-the-sidebar.example.com",
            false,
        ),
    ]
}

fn big_tree(rows: u64) -> Vec<TreeNode> {
    (0..rows)
        .map(|i| {
            let is_folder = i % 500 == 0;
            TreeNode {
                id: NodeId(i + 1),
                depth: u16::from(!is_folder),
                kind: if is_folder {
                    NodeKind::Folder
                } else {
                    NodeKind::Connection(Protocol::Ssh)
                },
                label: if is_folder {
                    format!("group-{i}")
                } else {
                    format!("host-{i}.example.internal")
                },
                state: if i % 7 == 0 { SessionDot::Live } else { SessionDot::None },
                expanded: true,
            }
        })
        .collect()
}

const TAB_LABELS: [(&str, char); 5] = [
    ("web-01.prod.internal", icons::SQUARE_TERMINAL),
    ("db-primary", icons::SQUARE_TERMINAL),
    ("backups", icons::FOLDER_SYNC),
    ("kiosk-lobby", icons::MONITOR),
    ("local shell", icons::TERMINAL),
];

const QUICK: [(&str, &str, char); 7] = [
    ("web-01.prod.internal", "Production", icons::SQUARE_TERMINAL),
    ("web-02.prod.internal", "Production", icons::SQUARE_TERMINAL),
    ("db-primary", "Production", icons::SQUARE_TERMINAL),
    ("dc-01.corp.example", "Windows", icons::MONITOR_COG),
    ("New connection", "Command", icons::PLUS),
    ("Toggle theme", "Command", icons::SETTINGS),
    ("Lock vault", "Command", icons::LOCK),
];

struct Gallery {
    page: Page,
    tabs: TabStripState,
    tree: TreeModel,
    tree_state: TreeState,
    bar_reveal: Reveal,
    new_connection: NewConnectionState,
    new_tiles: NewConnectionState,
    new_form: NewConnectionState,
    quick: QuickOpenState,
    quick_body: QuickOpenState,
    vault: VaultState,
    menu: ContextMenu,
    search: String,
    filled: String,
    password: String,
    segment: usize,
    last: Option<UiAction>,
}

impl Gallery {
    fn new(page: Page, nodes: Vec<TreeNode>) -> Self {
        let mut new_connection = NewConnectionState::default();
        new_connection.open_form(Protocol::Ssh);
        let mut new_tiles = NewConnectionState::default();
        new_tiles.open();
        let mut new_form = NewConnectionState::default();
        new_form.open_form(Protocol::Ssh);
        if let Some(draft) = new_form.draft_mut() {
            "web-03".clone_into(&mut draft.name);
            "Production".clone_into(&mut draft.group);
            "web-03.prod.internal".clone_into(&mut draft.host);
            "deploy".clone_into(&mut draft.username);
        }
        let mut quick = QuickOpenState::default();
        quick.open();
        let mut quick_body = QuickOpenState::default();
        quick_body.open();
        let mut tree_state = TreeState::default();
        tree_state.selected = Some(NodeId(3));
        let mut menu = ContextMenu::default();
        menu.open(pos2(SIDEBAR_WIDTH + 560., 300.));
        Self {
            page,
            tabs: TabStripState::default(),
            tree: TreeModel::new(nodes),
            tree_state,
            bar_reveal: Reveal::default(),
            new_connection,
            new_tiles,
            new_form,
            quick,
            quick_body,
            vault: VaultState::default(),
            menu,
            search: String::new(),
            filled: "deploy@web-01".into(),
            password: "hunter2".into(),
            segment: 1,
            last: None,
        }
    }

    fn draw(&mut self, ui: &mut Ui, now: Instant) {
        let screen = ui.ctx().content_rect();
        let p = theme_of(ui);
        if self.page == Page::Vault {
            let view = VaultView {
                name: "~/.local/share/rmx/vault.db",
                busy: false,
                error: Some("Wrong password"),
            };
            let action = vault_unlock(ui, screen, &mut self.vault, &view);
            self.note(action);
            return;
        }

        let labels = TAB_LABELS;
        let tabs: Vec<TabView<'_>> = labels
            .iter()
            .enumerate()
            .map(|(i, (label, glyph))| TabView {
                protocol: None,
                id: TabId(i as u64),
                label,
                glyph: *glyph,
                active: i == 0,
                status: PaneStatus::Connected,
            })
            .collect();
        let bar = Rect::from_min_size(screen.min, vec2(screen.width(), STRIP_HEIGHT));
        let view = TitleBarView { tabs: &tabs, maximized: false, sidebar_open: true, sidebar_width: SIDEBAR_WIDTH };
        let action = title_bar(ui, bar, &view, &mut self.tabs, now);
        self.note(action);

        let side = Rect::from_min_max(pos2(0., STRIP_HEIGHT), pos2(SIDEBAR_WIDTH, screen.bottom()));
        let action = sidebar(ui, side, "vault", &mut self.tree, &mut self.tree_state);
        self.note(action);

        let pane = Rect::from_min_max(pos2(SIDEBAR_WIDTH, STRIP_HEIGHT), screen.max);
        ui.painter().rect_filled(pane, 0., p.background);
        let top = Rect::from_min_max(pane.min, pos2(pane.right(), pane.top() + PANE_BAR_HEIGHT));
        let bar = PaneBarView {
            pane: PaneId(1),
            status: PaneStatus::Connected,
            title: "web-01",
            tooltip: "120 x 42",
            focused: true,
            grouped: false,
            zoomed: false,
            files: true,
        };
        let action = pane_bar(ui, top, &bar, &mut self.bar_reveal, std::time::Instant::now());
        self.note(action);

        let content = Rect::from_min_max(pos2(pane.left(), top.bottom()), pane.max);
        if self.page == Page::Board {
            self.board(ui, content);
        } else {
            self.controls(ui, content);
        }

        let ctx = ui.ctx().clone();
        match self.page {
            Page::Dialog => {
                let action = new_connection_dialog(&ctx, &mut self.new_connection);
                self.note(action);
            },
            Page::Palette => {
                let items = quick_items();
                let action = quick_open(&ctx, &mut self.quick, &items);
                self.note(action);
            },
            Page::HostKey => {
                let action = host_key_dialog(&ctx, &changed_key());
                self.note(action);
            },
            Page::Shell => {
                let items = menu_items();
                let action = context_menu(&ctx, Id::unique("gallery-menu"), &mut self.menu, &items);
                self.note(action);
            },
            Page::Board | Page::Vault => {},
        }
    }

    fn note(&mut self, action: Option<UiAction>) {
        if action.is_some() {
            self.last = action;
        }
    }

    fn controls(&mut self, ui: &mut Ui, rect: Rect) {
        let p = theme_of(ui);
        let area = rect.shrink(24.);
        let mut child = ui.new_child(UiBuilder::new().max_rect(area));
        let ui = &mut child;
        ui.spacing_mut().item_spacing = vec2(8., 12.);
        ui.heading("Controls");
        ui.label(
            egui::RichText::new("Primary and secondary actions, text fields, segmented options")
                .color(p.muted_fg),
        );
        ui.horizontal(|ui| {
            button(ui, "Connect", ButtonKind::Primary);
            button(ui, "Cancel", ButtonKind::Secondary);
            button(ui, "Details", ButtonKind::Ghost);
            button_full(ui, Some(icons::PLUS), "Add host", ButtonKind::Secondary, true, None);
            button_full(ui, None, "Disabled", ButtonKind::Primary, false, None);
            icon_button(ui, icons::SETTINGS, CONTROL_HEIGHT, 14.);
        });
        let width = Some(320.);
        ui.horizontal(|ui| {
            text_input(ui, Id::unique("g-a"), &mut self.search, InputOptions {
                hint: "Empty input",
                width,
                ..Default::default()
            });
            text_input(ui, Id::unique("g-b"), &mut self.filled, InputOptions {
                width,
                ..Default::default()
            });
        });
        ui.horizontal(|ui| {
            text_input(ui, Id::unique("g-c"), &mut self.password, InputOptions {
                password: true,
                width,
                ..Default::default()
            });
            ui.allocate_ui(vec2(320., CONTROL_HEIGHT), |ui| {
                segmented(
                    ui,
                    Id::unique("g-d"),
                    &["Password", "Key file", "Agent"],
                    &mut self.segment,
                );
            });
        });
        ui.horizontal(|ui| {
            for (name, color) in [
                ("success", p.success),
                ("warning", p.warning),
                ("info", p.info),
                ("danger", p.danger),
            ] {
                ui.label(egui::RichText::new(name).color(color));
            }
        });
        ui.label("中文回退字体：连接到服务器 — 日本語 한국어");
    }

    fn board(&mut self, ui: &mut Ui, rect: Rect) {
        let cell = |ui: &mut Ui, x: f32, y: f32, w: f32, add: &mut dyn FnMut(&mut Ui)| {
            let area = Rect::from_min_size(rect.min + vec2(x, y), vec2(w, 600.));
            let mut child = ui.new_child(UiBuilder::new().max_rect(area));
            rmx_ui::widgets::float_frame(&child).show(&mut child, |ui| {
                ui.set_width(w - 2.);
                ui.spacing_mut().item_spacing = vec2(8., 8.);
                add(ui);
            });
        };
        let tiles = &mut self.new_tiles;
        cell(ui, 24., 24., 540., &mut |ui| {
            new_connection_body(ui, tiles);
        });
        let form = &mut self.new_form;
        cell(ui, 24., 354., 540., &mut |ui| {
            new_connection_body(ui, form);
        });
        let items = quick_items();
        let quick = &mut self.quick_body;
        cell(ui, 596., 24., 562., &mut |ui| {
            quick_open_body(ui, quick, &items);
        });
        cell(ui, 596., 354., 520., &mut |ui| {
            host_key_body(ui, &unknown_key());
        });
    }
}

fn theme_of(ui: &Ui) -> &'static theme::Palette {
    if ui.visuals().dark_mode { &theme::DARK } else { &theme::LIGHT }
}

fn quick_items() -> Vec<QuickItem<'static>> {
    QUICK
        .iter()
        .enumerate()
        .map(|(i, (label, detail, glyph))| QuickItem { id: i as u64, glyph: *glyph, label, detail })
        .collect()
}

fn menu_items() -> Vec<MenuItem> {
    let entry = |glyph, label: &str, shortcut: Option<&str>, command: u64, danger| MenuItem::Action {
        glyph: Some(glyph),
        label: label.into(),
        shortcut: shortcut.map(Into::into),
        action: UiAction::TreeOpen(NodeId(command)),
        danger,
    };
    vec![
        entry(icons::SQUARE_TERMINAL, "Connect", Some("Enter"), 1, false),
        entry(icons::COPY, "Duplicate", None, 2, false),
        entry(icons::PENCIL, "Edit", Some("F2"), 3, false),
        MenuItem::Separator,
        entry(icons::TRASH_2, "Delete", None, 4, true),
    ]
}

fn unknown_key() -> HostKeyView<'static> {
    HostKeyView {
        status: HostKeyStatus::Unknown,
        host: "web-03.prod.internal",
        port: 22,
        algorithm: "ssh-ed25519",
        fingerprint: "SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8",
        previous: None,
    }
}

fn changed_key() -> HostKeyView<'static> {
    HostKeyView {
        status: HostKeyStatus::Changed,
        previous: Some("SHA256:Fy1c0mLKVhQ8m2l7VdXrKvXj9kq5mZ4pQ9eTt4Vb0Zo"),
        ..unknown_key()
    }
}

struct Gl {
    window: Arc<Window>,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    gl: Arc<glow::Context>,
    chrome: Chrome,
}

struct App {
    mode: ThemeMode,
    page: Page,
    ppp: f32,
    shot: Option<String>,
    bench: bool,
    gl: Option<Gl>,
    gallery: Option<Gallery>,
    started: Option<Instant>,
    frames: u32,
}

fn create_gl(event_loop: &ActiveEventLoop, mode: ThemeMode) -> Gl {
    let width = env::var("HEADLESS_WIDTH").ok().and_then(|v| v.parse().ok()).unwrap_or(1600);
    let height = env::var("HEADLESS_HEIGHT").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let attributes = Window::default_attributes()
        .with_title("rmx gallery")
        .with_decorations(false)
        .with_inner_size(PhysicalSize::new(width, height));
    let window = Arc::new(event_loop.create_window(attributes).expect("create window"));
    let raw_display = window.display_handle().expect("display").as_raw();
    let raw_window = window.window_handle().expect("window").as_raw();

    // SAFETY: the handles are live for as long as the window and event loop are.
    let display =
        unsafe { Display::new(raw_display, DisplayApiPreference::Egl) }.expect("EGL display");
    let template = ConfigTemplateBuilder::new().with_alpha_size(8).build();
    // SAFETY: `raw_window` is valid, as above.
    let config = unsafe { display.find_configs(template) }
        .expect("configs")
        .reduce(|a, b| if b.num_samples() < a.num_samples() { b } else { a })
        .expect("a config");
    let context_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
        .build(Some(raw_window));
    // SAFETY: the config came from this display.
    let context = unsafe { display.create_context(&config, &context_attributes) }.expect("context");
    let size = window.inner_size();
    let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
        raw_window,
        NonZeroU32::new(size.width.max(1)).unwrap(),
        NonZeroU32::new(size.height.max(1)).unwrap(),
    );
    // SAFETY: as above.
    let surface =
        unsafe { display.create_window_surface(&config, &surface_attributes) }.expect("surface");
    let context = context.make_current(&surface).expect("make current");
    // Vsync would time the virtual output, not this code.
    let _ = surface.set_swap_interval(&context, glutin::surface::SwapInterval::DontWait);
    // SAFETY: the context is current on this thread.
    let gl = Arc::new(unsafe {
        glow::Context::from_loader_function_cstr(|name| display.get_proc_address(name))
    });
    let chrome = Chrome::new(gl.clone(), mode).expect("chrome");
    Gl { window, surface, context, gl, chrome }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gl.is_none() {
            self.gl = Some(create_gl(event_loop, self.mode));
            let nodes = if self.bench { big_tree(100_000) } else { sample_tree() };
            self.gallery = Some(Gallery::new(self.page, nodes));
            self.started = Some(Instant::now());
        }
        event_loop.set_control_flow(ControlFlow::Poll);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                let gl = self.gl.as_ref().unwrap();
                if let (Some(w), Some(h)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    gl.surface.resize(&gl.context, w, h);
                }
                gl.window.request_redraw();
            },
            WindowEvent::RedrawRequested => {
                if self.bench {
                    self.run_bench();
                    event_loop.exit();
                } else if self.frame() {
                    event_loop.exit();
                }
            },
            _ => {},
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(gl) = &self.gl {
            gl.window.request_redraw();
        }
    }
}

impl App {
    /// Draw a frame; true once the screenshot has been taken.
    fn frame(&mut self) -> bool {
        let (gl, gallery) = (self.gl.as_mut().unwrap(), self.gallery.as_mut().unwrap());
        let size = gl.window.inner_size();
        let now = Instant::now();
        let px = [size.width, size.height];
        gl.chrome.update(now, px, self.ppp, |ui| gallery.draw(ui, now));

        let p = self.mode.palette();
        let [r, g, b, _] = egui::Rgba::from(p.background).to_array();
        // SAFETY: the context is current.
        unsafe {
            gl.gl.viewport(0, 0, size.width as i32, size.height as i32);
            gl.gl.clear_color(r, g, b, 1.);
            gl.gl.clear(glow::COLOR_BUFFER_BIT);
        }
        gl.chrome.paint(None);

        self.frames += 1;
        let elapsed = self.started.unwrap().elapsed();
        let settled = !gl.chrome.needs_update(now + Duration::from_millis(1))
            && elapsed > Duration::from_millis(800);
        let mut done = false;
        if let Some(path) = self.shot.as_ref().filter(|_| settled && self.frames > 8) {
            write_ppm(&gl.gl, size, path);
            done = true;
        }
        gl.surface.swap_buffers(&gl.context).expect("swap");
        done
    }

    /// Time the chrome at the window's size and print the numbers.
    fn run_bench(&mut self) {
        let (gl, gallery) = (self.gl.as_mut().unwrap(), self.gallery.as_mut().unwrap());
        let size = gl.window.inner_size();
        let px = [size.width, size.height];
        let ppp = self.ppp;
        println!("window {}x{} ppp {ppp}, GL: {}", px[0], px[1], unsafe {
            gl.gl.get_parameter_string(glow::RENDERER)
        });

        // Warm the caches: glyphs get rasterized once.
        for _ in 0..30 {
            let now = Instant::now();
            gl.chrome.invalidate();
            gl.chrome.update(now, px, ppp, |ui| gallery.draw(ui, now));
            gl.chrome.paint(None);
        }

        // A typical frame: the pointer moves over the sidebar, tree rows hover in turn.
        let (mut layout, mut tess, mut paint) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..2000u32 {
            let y = 100. + (i % 900) as f32;
            let now = Instant::now();
            gl.chrome.push_event(Event::PointerMoved(Pos2::new(120., y)));
            gl.chrome.update(now, px, ppp, |ui| gallery.draw(ui, now));
            layout.push(gl.chrome.stats().layout);
            tess.push(gl.chrome.stats().tessellate);
            let begin = Instant::now();
            gl.chrome.paint(None);
            paint.push(begin.elapsed());
        }
        let stats = gl.chrome.stats();
        println!("primitives {} vertices {}", stats.primitives, stats.vertices);
        report("typical: layout", &mut layout);
        report("typical: tessellate", &mut tess);
        report("typical: paint (GL calls)", &mut paint);

        // Scrolling the 100,000-row tree, and rendering what results.
        let (mut scroll, mut scroll_paint) = (Vec::new(), Vec::new());
        gl.chrome.push_event(Event::PointerMoved(Pos2::new(120., 400.)));
        for i in 0..3000u32 {
            let now = Instant::now();
            gl.chrome.push_event(Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0., if i < 2200 { -240. } else { 240. }),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Unknown,
                modifiers: egui::Modifiers::NONE,
            });
            let begin = Instant::now();
            gl.chrome.update(now, px, ppp, |ui| gallery.draw(ui, now));
            scroll.push(begin.elapsed());
            let begin = Instant::now();
            gl.chrome.paint(None);
            scroll_paint.push(begin.elapsed());
        }
        report("scroll 100k: layout + tessellate", &mut scroll);
        report("scroll 100k: paint", &mut scroll_paint);
        println!("scroll 100k: visible rows {}", gallery.tree.visible_len());

        // Searching the same tree.
        let mut search = Vec::new();
        for query in ["host-4", "example", "zzz", "group", "host-99999"].iter().cycle().take(100) {
            let begin = Instant::now();
            gallery.tree.set_query(query);
            search.push(begin.elapsed());
        }
        gallery.tree.set_query("");
        report("search 100k", &mut search);

        // Terminal output only: no egui, just the replay.
        let mut idle_at = Instant::now();
        gl.chrome.invalidate();
        for _ in 0..1000 {
            idle_at += Duration::from_millis(16);
            if !gl.chrome.needs_update(idle_at) {
                break;
            }
            gl.chrome.update(idle_at, px, ppp, |ui| gallery.draw(ui, idle_at));
        }
        gl.chrome.paint(None);
        let mut replay = Vec::new();
        let mut idle = Vec::new();
        let passes = gl.chrome.stats().passes;
        let mut allocations = 0;
        for _ in 0..3000 {
            let begin = Instant::now();
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            gl.chrome.update(idle_at, px, ppp, |ui| gallery.draw(ui, idle_at));
            idle.push(begin.elapsed());
            let begin = Instant::now();
            gl.chrome.paint(None);
            replay.push(begin.elapsed());
            allocations += ALLOCATIONS.load(Ordering::Relaxed) - before;
        }
        assert_eq!(gl.chrome.stats().passes, passes, "the replay must not run egui");
        report("idle update", &mut idle);
        report("replay", &mut replay);
        println!("replay: {} allocations per 3000 frames, 0 egui passes", allocations);

        check_gl_restored(&gl.gl, &mut gl.chrome);

        // SAFETY: the context is current.
        unsafe { gl.gl.finish() };
        let begin = Instant::now();
        for _ in 0..100 {
            gl.chrome.paint(None);
            // SAFETY: the context is current.
            unsafe { gl.gl.finish() };
        }
        println!("replay + glFinish: mean {:.1} us", begin.elapsed().as_secs_f64() * 1e6 / 100.);
    }
}

/// Paint with distinctive GL state set and check it comes back unchanged.
fn check_gl_restored(gl: &glow::Context, chrome: &mut Chrome) {
    // SAFETY: the context is current; everything created here is deleted below.
    unsafe {
        let vao = gl.create_vertex_array().unwrap();
        let vbo = gl.create_buffer().unwrap();
        let ebo = gl.create_buffer().unwrap();
        let texture = gl.create_texture().unwrap();
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.active_texture(glow::TEXTURE3);
        gl.enable(glow::BLEND);
        gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE, glow::ZERO, glow::SRC_COLOR);
        gl.blend_equation_separate(glow::FUNC_REVERSE_SUBTRACT, glow::MAX);
        gl.enable(glow::SCISSOR_TEST);
        gl.scissor(3, 5, 77, 99);
        gl.viewport(1, 2, 300, 200);
        gl.enable(glow::CULL_FACE);
        gl.enable(glow::DEPTH_TEST);
        gl.enable(glow::STENCIL_TEST);
        gl.color_mask(true, false, true, false);
        gl.enable(glow::FRAMEBUFFER_SRGB);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 8);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 11);

        let snapshot = || {
            let ints = [
                glow::CURRENT_PROGRAM,
                glow::VERTEX_ARRAY_BINDING,
                glow::ARRAY_BUFFER_BINDING,
                glow::ELEMENT_ARRAY_BUFFER_BINDING,
                glow::ACTIVE_TEXTURE,
                glow::BLEND_SRC_RGB,
                glow::BLEND_DST_RGB,
                glow::BLEND_SRC_ALPHA,
                glow::BLEND_DST_ALPHA,
                glow::BLEND_EQUATION_RGB,
                glow::BLEND_EQUATION_ALPHA,
                glow::UNPACK_ALIGNMENT,
                glow::UNPACK_ROW_LENGTH,
                glow::PIXEL_UNPACK_BUFFER_BINDING,
            ]
            .map(|name| gl.get_parameter_i32(name));
            let flags = [
                glow::BLEND,
                glow::SCISSOR_TEST,
                glow::CULL_FACE,
                glow::DEPTH_TEST,
                glow::STENCIL_TEST,
                glow::FRAMEBUFFER_SRGB,
            ]
            .map(|name| gl.is_enabled(name));
            let (mut scissor, mut viewport, mut mask) = ([0; 4], [0; 4], [0; 4]);
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor);
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            gl.get_parameter_i32_slice(glow::COLOR_WRITEMASK, &mut mask);
            gl.active_texture(glow::TEXTURE0);
            let texture0 = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            gl.active_texture(glow::TEXTURE3);
            (ints, flags, scissor, viewport, mask, texture0)
        };
        let before = snapshot();
        chrome.paint(None);
        assert_eq!(snapshot(), before, "GL state must come back as it was");
        println!("GL state restored: ok");

        gl.disable(glow::STENCIL_TEST);
        gl.disable(glow::DEPTH_TEST);
        gl.disable(glow::CULL_FACE);
        gl.disable(glow::SCISSOR_TEST);
        gl.disable(glow::FRAMEBUFFER_SRGB);
        gl.color_mask(true, true, true, true);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);
        gl.delete_vertex_array(vao);
        gl.delete_buffer(vbo);
        gl.delete_buffer(ebo);
        gl.delete_texture(texture);
    }
}

fn report(name: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    let p99 = samples[samples.len() * 99 / 100];
    let max = samples[samples.len() - 1];
    println!("{name}: mean {:.1} us, p99 {:.1} us, max {:.1} us", us(mean), us(p99), us(max));
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn write_ppm(gl: &glow::Context, size: PhysicalSize<u32>, path: &str) {
    let (w, h) = (size.width as usize, size.height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    // SAFETY: the buffer holds exactly the requested pixels.
    unsafe {
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        gl.read_pixels(
            0,
            0,
            w as i32,
            h as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut rgba)),
        );
    }
    let mut out = Vec::with_capacity(w * h * 3 + 32);
    write!(out, "P6\n{w} {h}\n255\n").unwrap();
    for row in rgba.chunks_exact(w * 4).rev() {
        for pixel in row.as_chunks::<4>().0 {
            out.extend_from_slice(&pixel[..3]);
        }
    }
    fs::write(path, out).expect("write the frame");
}

fn main() {
    let mode = match env::var("RMX_GALLERY_THEME").as_deref() {
        Ok("light") => ThemeMode::Light,
        _ => ThemeMode::Dark,
    };
    let page = match env::var("RMX_GALLERY_PAGE").as_deref() {
        Ok("board") => Page::Board,
        Ok("dialog") => Page::Dialog,
        Ok("palette") => Page::Palette,
        Ok("hostkey") => Page::HostKey,
        Ok("vault") => Page::Vault,
        _ => Page::Shell,
    };
    let ppp = env::var("RMX_GALLERY_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.);
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App {
        mode,
        page,
        ppp,
        shot: env::var("RMX_GALLERY_SHOT").ok(),
        bench: env::var_os("RMX_GALLERY_BENCH").is_some(),
        gl: None,
        gallery: None,
        started: None,
        frames: 0,
    };
    event_loop.run_app(&mut app).expect("event loop");
}
