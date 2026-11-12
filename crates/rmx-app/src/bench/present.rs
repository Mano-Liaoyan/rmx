//! When frames really reached the screen, from `wp_presentation`, for `RMX_BENCH_PRESENT`.
//!
//! The feedback has an event queue of its own on the connection winit and EGL share, so neither of
//! theirs sees it. Nothing here reads the socket while the event loop runs: winit does that anyway,
//! libwayland sorts what it reads into queues, and each frame only dispatches what has arrived.
//!
//! A frame is dropped if its feedback is discarded, or if a refresh went by, between it and the
//! frame shown before it, that its content was pending for at least one full refresh interval
//! before. Other gaps are idle: the app had nothing it could have shown.
//! `RMX_BENCH_PRESENT` names the per-frame CSV. `RMX_BENCH_EXPECT_REFRESH_NS` ends the run early
//! when the output turns out to refresh at another rate. `RMX_BENCH_MONITOR` picks the output.

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::presentation_time::client::wp_presentation::{self, WpPresentation};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, WpPresentationFeedback,
};
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use winit::window::{Fullscreen, Window, WindowAttributes};

use super::gpu::GpuProbe;

const VSYNC: u32 = 0x1;
const HW_CLOCK: u32 = 0x2;
const HW_COMPLETION: u32 = 0x4;
const ZERO_COPY: u32 = 0x8;

/// How long the frames still on their way at the end get to arrive.
const DRAIN: Duration = Duration::from_millis(250);
/// Frames presented before the refresh interval is judged.
const REFRESH_SAMPLES: usize = 60;
/// How far the refresh interval may be from the one expected, in percent.
const REFRESH_TOLERANCE: u64 = 3;

/// The clock the compositor's timestamps are on.
static CLOCK: AtomicI32 = AtomicI32::new(libc::CLOCK_MONOTONIC);
static RESULTS: Mutex<Option<Results>> = Mutex::new(None);
/// What the app was doing, for attributing a stall: the time, a kind and what happened.
static EVENTS: Mutex<Vec<(u64, &'static str, String)>> = Mutex::new(Vec::new());
static NOTING: AtomicBool = AtomicBool::new(false);
/// A long call, one worth looking at when frames stop, in milliseconds.
const SLOW_MS: u128 = 2;
const MAX_EVENTS: usize = 50_000;
/// A gap of this many refreshes gets its own line in the report.
const LONG_GAP: u64 = 8;

/// Log what the app is doing, for the stalls in the report.
pub fn note(kind: &'static str, detail: impl FnOnce() -> String) {
    if !NOTING.load(Ordering::Relaxed) {
        return;
    }
    let at = now_ns();
    if let Ok(mut events) = EVENTS.lock()
        && events.len() < MAX_EVENTS
    {
        events.push((at, kind, detail()));
    }
}

/// Note a change to a window, which a compositor may answer by waiting.
pub fn note_window_event(event: &WindowEvent) {
    match event {
        WindowEvent::Resized(size) => {
            note("window", || format!("resized {}x{}", size.width, size.height))
        },
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
            note("window", || format!("scale {scale_factor}"));
        },
        WindowEvent::Occluded(occluded) => note("window", || format!("occluded {occluded}")),
        WindowEvent::Focused(focused) => note("window", || format!("focused {focused}")),
        WindowEvent::Moved(_) => note("window", || "moved".to_owned()),
        _ => (),
    }
}

/// The name of the kind of a window event, for [`Slow`].
pub fn window_event_name(event: &WindowEvent) -> &'static str {
    match event {
        WindowEvent::RedrawRequested => "redraw",
        WindowEvent::Resized(_) => "resized",
        WindowEvent::Focused(_) => "focused",
        WindowEvent::ScaleFactorChanged { .. } => "scale",
        WindowEvent::Occluded(_) => "occluded",
        _ => "other",
    }
}

/// Notes a stretch of the main thread when it turns out long.
pub struct Slow {
    kind: &'static str,
    what: &'static str,
    started: Instant,
}

impl Slow {
    pub fn new(kind: &'static str, what: &'static str) -> Self {
        Self { kind, what, started: Instant::now() }
    }
}

impl Drop for Slow {
    fn drop(&mut self) {
        let taken = self.started.elapsed().as_millis();
        if taken >= SLOW_MS {
            note("slow", || format!("{} {} {taken}ms", self.kind, self.what));
        }
    }
}

/// The time on the compositor's presentation clock, in nanoseconds.
pub fn now_ns() -> u64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(CLOCK.load(Ordering::Relaxed), &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// Make the window borderless fullscreen on the output `RMX_BENCH_MONITOR` names, if it does.
pub fn fullscreen_on(
    event_loop: &ActiveEventLoop,
    attributes: WindowAttributes,
) -> Result<WindowAttributes, String> {
    let Ok(name) = std::env::var("RMX_BENCH_MONITOR") else { return Ok(attributes) };
    let monitors: Vec<_> = event_loop.available_monitors().collect();
    match monitors.iter().find(|monitor| monitor.name().as_deref() == Some(name.as_str())) {
        Some(monitor) => {
            Ok(attributes.with_fullscreen(Some(Fullscreen::Borderless(Some(monitor.clone())))))
        },
        None => {
            let known: Vec<String> = monitors.iter().filter_map(|monitor| monitor.name()).collect();
            Err(format!("no output named {name}, only {known:?}"))
        },
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Outcome {
    #[default]
    Waiting,
    Presented,
    Discarded,
}

/// One committed frame, with its times on the presentation clock in nanoseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Record {
    /// Right before the swap, and right after it.
    submit_ns: u64,
    swapped_ns: u64,
    /// When the app first had something new to show for this frame, or 0 if unknown.
    pending_ns: u64,
    presented_ns: u64,
    refresh_ns: u32,
    seq: u64,
    flags: u32,
    outcome: Outcome,
    /// When the redraw that made this frame began.
    start_ns: u64,
    /// The GPU's time between the first and the last command of the frame's drawing.
    pub gpu_us: u32,
    /// When the GPU finished the frame and its swap.
    pub gpu_complete_ns: u64,
    /// When the frame's fence was seen signalled, which can only be later.
    pub fence_seen_ns: u64,
    /// CPU time the GPU probe took for this frame.
    probe_ns: u32,
}

#[derive(Default)]
struct State {
    presentation: Option<WpPresentation>,
    frames: Vec<Record>,
    waiting: usize,
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = event
            && interface == WpPresentation::interface().name
        {
            state.presentation = Some(registry.bind(name, 1, queue, ()));
        }
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            CLOCK.store(clk_id as i32, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WpPresentationFeedback, usize> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        frame: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wp_presentation_feedback::Event;

        let Some(record) = state.frames.get_mut(*frame) else { return };
        match event {
            Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, refresh, seq_hi, seq_lo, flags } => {
                let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                record.presented_ns = seconds * 1_000_000_000 + u64::from(tv_nsec);
                record.refresh_ns = refresh;
                record.seq = (u64::from(seq_hi) << 32) | u64::from(seq_lo);
                record.flags = match flags {
                    WEnum::Value(kind) => kind.bits(),
                    WEnum::Unknown(bits) => bits,
                };
                record.outcome = Outcome::Presented;
                state.waiting -= 1;
            },
            Event::Discarded => {
                record.outcome = Outcome::Discarded;
                state.waiting -= 1;
            },
            _ => (),
        }
    }
}

/// The feedback of one window's surface.
pub struct Presentation {
    connection: Connection,
    queue: EventQueue<State>,
    handle: QueueHandle<State>,
    presentation: WpPresentation,
    surface: WlSurface,
    state: State,
    expected_ns: Option<u64>,
    judged: bool,
    wrong_refresh: bool,
    monitor: String,
    fullscreen: bool,
    size: (u32, u32),
    gpu: Option<GpuProbe>,
    /// CPU time the GPU probe has taken for the frame being made.
    probe_ns: u64,
}

/// What `Recorder::write` reports from.
struct Results {
    frames: Vec<Record>,
    gpu_measured: bool,
    gpu_skipped: u64,
    wrong_refresh: bool,
    monitor: String,
    fullscreen: bool,
    size: (u32, u32),
}

impl Presentation {
    /// Start watching `window`, if `RMX_BENCH_PRESENT` is set and the compositor can tell.
    pub fn attach(window: &Window) -> Option<Self> {
        std::env::var_os("RMX_BENCH_PRESENT")?;
        match Self::connect(window) {
            Ok(presentation) => Some(presentation),
            Err(err) => {
                eprintln!("rmx: no presentation feedback: {err}");
                None
            },
        }
    }

    fn connect(window: &Window) -> Result<Self, String> {
        NOTING.store(true, Ordering::Relaxed);
        let display = window.display_handle().map_err(|err| err.to_string())?.as_raw();
        let handle = window.window_handle().map_err(|err| err.to_string())?.as_raw();
        let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(handle)) =
            (display, handle)
        else {
            return Err("not on Wayland".to_owned());
        };

        // Safety: the display and surface outlive this, which `Gfx` declares before its window.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let mut queue = connection.new_event_queue::<State>();
        let queue_handle = queue.handle();
        let _registry = connection.display().get_registry(&queue_handle, ());
        let mut state = State::default();
        state.frames.reserve(1 << 18);
        // The first announces the global, which is bound, and the second brings its clock.
        for _ in 0..2 {
            queue.roundtrip(&mut state).map_err(|err| err.to_string())?;
        }
        let presentation = state
            .presentation
            .clone()
            .ok_or_else(|| "the compositor has no wp_presentation".to_owned())?;
        // Safety: this is the live wl_surface of `window`.
        let id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), handle.surface.as_ptr().cast()) }
                .map_err(|err| err.to_string())?;
        let surface = WlSurface::from_id(&connection, id).map_err(|err| err.to_string())?;

        let expected_ns =
            std::env::var("RMX_BENCH_EXPECT_REFRESH_NS").ok().and_then(|ns| ns.parse().ok());
        Ok(Self {
            connection,
            queue,
            handle: queue_handle,
            presentation,
            surface,
            state,
            expected_ns,
            judged: false,
            wrong_refresh: false,
            monitor: String::new(),
            fullscreen: false,
            size: (0, 0),
            gpu: GpuProbe::new(),
            probe_ns: 0,
        })
    }

    /// Run `work` on the GPU probe, if there is one, and count the time it takes.
    fn with_gpu(&mut self, work: impl FnOnce(&mut GpuProbe, &mut [Record])) {
        let Some(gpu) = &mut self.gpu else { return };
        let start = now_ns();
        work(gpu, &mut self.state.frames);
        self.probe_ns += now_ns() - start;
    }

    /// A frame's drawing is about to start.
    pub fn begin_frame(&mut self) {
        self.with_gpu(|gpu, frames| {
            gpu.collect(frames);
            gpu.begin();
        });
    }

    /// Ask about the commit the coming swap makes, and pick up what has arrived.
    pub fn before_swap(&mut self) {
        self.with_gpu(|gpu, _| gpu.end_draw());
        let _ = self.queue.dispatch_pending(&mut self.state);
        self.judge_refresh();
        let frame = self.state.frames.len();
        self.state.frames.push(Record { submit_ns: now_ns(), ..Record::default() });
        self.state.waiting += 1;
        let _ = self.presentation.feedback(&self.surface, &self.handle, frame);
    }

    pub fn after_swap(&mut self) {
        let swapped_ns = now_ns();
        let frame = self.state.frames.len().saturating_sub(1);
        self.with_gpu(|gpu, _| gpu.mark(frame));
        let probe_ns = std::mem::take(&mut self.probe_ns) as u32;
        if let Some(record) = self.state.frames.last_mut() {
            record.swapped_ns = swapped_ns;
            record.probe_ns = probe_ns;
        }
    }

    /// The frame just committed first had something to show at `pending_ns`, and its redraw began
    /// at `start_ns`.
    pub fn note_frame(&mut self, pending_ns: Option<u64>, start_ns: u64) {
        if let Some(record) = self.state.frames.last_mut() {
            record.pending_ns = pending_ns.unwrap_or(0);
            record.start_ns = start_ns;
        }
    }

    fn judge_refresh(&mut self) {
        let Some(expected) = self.expected_ns.filter(|_| !self.judged) else { return };
        let presented =
            self.state.frames.iter().filter(|frame| frame.outcome == Outcome::Presented);
        let mut refresh: Vec<u64> = presented.map(|frame| u64::from(frame.refresh_ns)).collect();
        if refresh.len() < REFRESH_SAMPLES {
            return;
        }
        self.judged = true;
        refresh.sort_unstable();
        let median = refresh[refresh.len() / 2];
        self.wrong_refresh = median.abs_diff(expected) * 100 > expected * REFRESH_TOLERANCE;
    }

    /// Whether the output is not the one this run was meant for.
    pub fn wrong_refresh(&self) -> bool {
        self.wrong_refresh
    }

    /// Wait for the frames still on their way, while the surface still exists to be shown.
    pub fn finish(&mut self, window: &Window) {
        self.monitor =
            window.current_monitor().and_then(|monitor| monitor.name()).unwrap_or_default();
        self.fullscreen = window.fullscreen().is_some();
        let size = window.inner_size();
        self.size = (size.width, size.height);

        let deadline = Instant::now() + DRAIN;
        loop {
            let _ = self.queue.dispatch_pending(&mut self.state);
            self.with_gpu(GpuProbe::collect);
            let gpu_busy = self.gpu.as_ref().is_some_and(GpuProbe::busy);
            if (self.state.waiting == 0 && !gpu_busy) || Instant::now() >= deadline {
                return;
            }
            let _ = self.connection.flush();
            if self.state.waiting == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let Some(guard) = self.queue.prepare_read() else { continue };
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 20) } > 0 {
                let _ = guard.read();
            }
        }
    }
}

impl Drop for Presentation {
    fn drop(&mut self) {
        if let Ok(mut results) = RESULTS.lock() {
            *results = Some(Results {
                frames: std::mem::take(&mut self.state.frames),
                gpu_measured: self.gpu.is_some(),
                gpu_skipped: self.gpu.as_ref().map_or(0, |gpu| gpu.skipped),
                wrong_refresh: self.wrong_refresh,
                monitor: std::mem::take(&mut self.monitor),
                fullscreen: self.fullscreen,
                size: self.size,
            });
        }
    }
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    sorted.get(((sorted.len().max(1) - 1) as f64 * q).round() as usize).copied().unwrap_or(0)
}

/// What became of each frame, and the totals.
#[derive(Debug, Default)]
struct Analysis {
    /// Per frame: refreshes since the last shown frame, and whether it counts as dropped or idle.
    rows: Vec<Row>,
    derived_msc: bool,
    presented: u64,
    discarded: u64,
    waiting: u64,
    /// Frames shown after refreshes went by while the app had one pending.
    late: u64,
    late_after_warmup: u64,
    discarded_after_warmup: u64,
    missed_refreshes: u64,
    idle_gaps: u64,
    same_refresh: u64,
    gaps: BTreeMap<u64, u64>,
    intervals_us: Vec<u64>,
    latencies_us: Vec<u64>,
    refresh_ns: u64,
    flags: [u64; 4],
    gpu_us: Vec<u64>,
    /// Frames whose GPU-complete time is later than the fence was seen signalled, which can't be.
    clock_suspect: u64,
    /// By how much the timestamps can be seen to run early, in nanoseconds: a mark issued after a
    /// swap returned cannot have been reached before it did, so the largest such error is added
    /// to every timestamp used against a deadline.
    clock_bias_ns: u64,
    /// Over dropped frames: GPU-complete against the deadline.
    dropped: Attribution,
    /// How long before the deadline the GPU finished, frames shown on time and dropped frames.
    margins_on_time: Vec<i64>,
    margins_dropped: Vec<i64>,
}

/// Why dropped frames may have missed their deadline, as far as can be seen from here.
#[derive(Debug, Default, PartialEq)]
struct Attribution {
    gpu_after: u64,
    gpu_before: u64,
    gpu_unknown: u64,
    /// The redraw began, or the commit came, after the deadline.
    started_after: u64,
    committed_after: u64,
    /// Committed, and finished on the GPU, before the deadline.
    ready_before: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Row {
    gap: u64,
    interval_us: u64,
    latency_us: u64,
    dropped: bool,
    idle: bool,
    /// The refresh this frame was aimed at, on the presentation clock, or 0 if unknown.
    deadline_ns: u64,
    /// How long before the deadline the GPU finished, negative if after.
    margin_us: Option<i64>,
}

/// The refresh a frame was aimed at: the first after the last frame shown and at or after the app
/// had the content for it.
fn deadline(previous_ns: u64, refresh: u64, pending_ns: u64) -> u64 {
    let next = previous_ns + refresh;
    if pending_ns > next { next + (pending_ns - next).div_ceil(refresh) * refresh } else { next }
}

fn analyse(frames: &[Record], warmup: usize) -> Analysis {
    let mut result = Analysis::default();
    let shown = || frames.iter().filter(|frame| frame.outcome == Outcome::Presented);
    let mut refresh: Vec<u64> = shown().map(|frame| u64::from(frame.refresh_ns)).collect();
    refresh.sort_unstable();
    let median_refresh = percentile(&refresh, 0.5);
    result.refresh_ns = median_refresh;
    result.derived_msc = !shown().any(|frame| frame.seq != 0);

    result.clock_bias_ns = frames
        .iter()
        .filter(|frame| frame.gpu_complete_ns != 0)
        .map(|frame| frame.swapped_ns.saturating_sub(frame.gpu_complete_ns))
        .max()
        .unwrap_or(0);

    let mut previous: Option<&Record> = None;
    for (index, frame) in frames.iter().enumerate() {
        let mut row = Row::default();
        let refresh =
            if frame.refresh_ns > 0 { u64::from(frame.refresh_ns) } else { median_refresh };
        if let Some(previous) = previous.filter(|_| refresh > 0) {
            row.deadline_ns = deadline(previous.presented_ns, refresh, frame.pending_ns);
        }
        if row.deadline_ns != 0 && frame.gpu_complete_ns != 0 {
            let complete = frame.gpu_complete_ns + result.clock_bias_ns;
            row.margin_us = Some((row.deadline_ns as i64 - complete as i64) / 1000);
        }
        if frame.gpu_complete_ns != 0 {
            result.gpu_us.push(u64::from(frame.gpu_us));
            result.clock_suspect += u64::from(frame.gpu_complete_ns > frame.fence_seen_ns);
        }
        match frame.outcome {
            Outcome::Waiting => result.waiting += 1,
            Outcome::Discarded => {
                result.discarded += 1;
                result.discarded_after_warmup += u64::from(index >= warmup);
                row.dropped = true;
            },
            Outcome::Presented => {
                result.presented += 1;
                row.latency_us = frame.presented_ns.saturating_sub(frame.swapped_ns) / 1000;
                result.latencies_us.push(row.latency_us);
                for (count, bit) in
                    result.flags.iter_mut().zip([VSYNC, HW_CLOCK, HW_COMPLETION, ZERO_COPY])
                {
                    *count += u64::from(frame.flags & bit != 0);
                }
                if let Some(previous) = previous {
                    let elapsed = frame.presented_ns.saturating_sub(previous.presented_ns);
                    row.gap = if !result.derived_msc {
                        frame.seq.saturating_sub(previous.seq)
                    } else {
                        (elapsed + refresh / 2).checked_div(refresh).unwrap_or(1)
                    };
                    row.interval_us = elapsed / 1000;
                    result.intervals_us.push(row.interval_us);
                    *result.gaps.entry(row.gap).or_default() += 1;
                    result.same_refresh += u64::from(row.gap == 0);
                    if row.gap > 1 {
                        // Content that was not pending a full refresh before the last refresh that
                        // went by could not have been shown at it: that is idleness.
                        let last_missed = previous.presented_ns + (row.gap - 1) * refresh;
                        if frame.pending_ns == 0 || frame.pending_ns + refresh <= last_missed {
                            row.dropped = true;
                            result.late += 1;
                            result.late_after_warmup += u64::from(index >= warmup);
                            result.missed_refreshes += row.gap - 1;
                        } else {
                            row.idle = true;
                            result.idle_gaps += 1;
                        }
                    } else if let Some(margin) = row.margin_us {
                        result.margins_on_time.push(margin);
                    }
                }
                previous = Some(frame);
            },
        }
        if row.dropped {
            attribute(frame, &row, &mut result);
        }
        result.rows.push(row);
    }
    result.margins_on_time.sort_unstable();
    result.margins_dropped.sort_unstable();
    result
}

fn attribute(frame: &Record, row: &Row, result: &mut Analysis) {
    let attribution = &mut result.dropped;
    if row.deadline_ns != 0 {
        attribution.started_after += u64::from(frame.start_ns >= row.deadline_ns);
        attribution.committed_after += u64::from(frame.swapped_ns >= row.deadline_ns);
    }
    match row.margin_us {
        Some(margin) => {
            result.margins_dropped.push(margin);
            if margin < 0 {
                attribution.gpu_after += 1;
            } else {
                attribution.gpu_before += 1;
                attribution.ready_before += u64::from(frame.swapped_ns < row.deadline_ns);
            }
        },
        None => attribution.gpu_unknown += 1,
    }
}

/// The spread of signed values, sorted, as `min/p1/p50/p99/max`.
fn spread(sorted: &[i64]) -> String {
    let at = |q: f64| {
        sorted.get(((sorted.len().max(1) - 1) as f64 * q).round() as usize).copied().unwrap_or(0)
    };
    format!("{}/{}/{}/{}/{}", at(0.), at(0.01), at(0.5), at(0.99), at(1.))
}

/// The lines of the report about presentation, none if it was not measured, and the CSV written.
pub fn report(main_us: &[u32], warmup: usize) -> String {
    let Ok(results) = RESULTS.lock() else { return String::new() };
    let Some(results) = results.as_ref() else { return String::new() };
    let mut analysis = analyse(&results.frames, warmup);
    let events = EVENTS.lock().map(|events| events.clone()).unwrap_or_default();
    if let Ok(path) = std::env::var("RMX_BENCH_PRESENT") {
        let _ = std::fs::write(format!("{path}.events.csv"), events_csv(&results.frames, &events));
        let _ = std::fs::write(path, csv(&results.frames, &analysis, main_us));
    }

    analysis.intervals_us.sort_unstable();
    analysis.latencies_us.sort_unstable();
    analysis.gpu_us.sort_unstable();
    let mut probe: Vec<u64> =
        results.frames.iter().map(|frame| u64::from(frame.probe_ns) / 1000).collect();
    probe.sort_unstable();
    let gaps: Vec<String> =
        analysis.gaps.iter().map(|(gap, count)| format!("{gap}:{count}")).collect();
    let [vsync, hw_clock, hw_completion, zero_copy] = analysis.flags;
    let dropped = &analysis.dropped;
    // A dropped frame the GPU finished earlier than some frame that was shown cannot have been
    // too late for the compositor because of the GPU.
    let earliest_on_time = analysis.margins_on_time.first().copied();
    let ready_as_on_time = earliest_on_time.map_or(0, |floor| {
        analysis.margins_dropped.iter().filter(|&&margin| margin >= floor).count()
    });
    let mut report = format!(
        "present_frames={}\npresent_presented={}\npresent_discarded={}\npresent_unresolved={}\\
         npresent_dropped={}\npresent_dropped_after_warmup={}\npresent_late={}\\
         npresent_missed_refreshes={}\npresent_idle_gaps={}\npresent_same_refresh={}\\
         npresent_gaps={}\npresent_interval_p50_us={}\npresent_interval_p99_us={}\\
         npresent_interval_max_us={}\npresent_latency_p50_us={}\npresent_latency_p99_us={}\\
         npresent_latency_max_us={}\npresent_refresh_ns={}\npresent_msc={}\npresent_vsync={vsync}\\
         npresent_hw_clock={hw_clock}\npresent_hw_completion={hw_completion}\\
         npresent_zero_copy={zero_copy}\npresent_clock={}\npresent_wrong_refresh={}\\
         npresent_monitor={}\npresent_fullscreen={}\npresent_size={}x{}\npresent_gpu_measured={}\\
         npresent_gpu_frames={}\npresent_gpu_skipped={}\npresent_gpu_p50_us={}\\
         npresent_gpu_p99_us={}\npresent_gpu_max_us={}\npresent_gpu_clock_suspect={}\\
         npresent_gpu_clock_bias_us={}\npresent_probe_p50_us={}\npresent_probe_p99_us={}\\
         npresent_probe_max_us={}\npresent_dropped_gpu_after_deadline={}\\
         npresent_dropped_gpu_before_deadline={}\npresent_dropped_gpu_unknown={}\\
         npresent_dropped_started_after_deadline={}\npresent_dropped_committed_after_deadline={}\\
         npresent_dropped_ready_before_deadline={}\\
         npresent_dropped_as_ready_as_a_shown_frame={ready_as_on_time}\\
         npresent_margin_shown_us_min/p1/p50/p99/max={}\npresent_margin_dropped_us_min/p1/p50/p99/\
         max={}\npresent_deadline=the refresh after the last shown frame and at or after the \
         frame's content was pending, on the grid of that frame's timestamp; KWin's own latch \
         point, some time before it, is not visible\n",
        results.frames.len(),
        analysis.presented,
        analysis.discarded,
        analysis.waiting,
        analysis.late + analysis.discarded,
        analysis.late_after_warmup + analysis.discarded_after_warmup,
        analysis.late,
        analysis.missed_refreshes,
        analysis.idle_gaps,
        analysis.same_refresh,
        gaps.join(","),
        percentile(&analysis.intervals_us, 0.5),
        percentile(&analysis.intervals_us, 0.99),
        analysis.intervals_us.last().copied().unwrap_or(0),
        percentile(&analysis.latencies_us, 0.5),
        percentile(&analysis.latencies_us, 0.99),
        analysis.latencies_us.last().copied().unwrap_or(0),
        analysis.refresh_ns,
        if analysis.derived_msc { "derived" } else { "reported" },
        CLOCK.load(Ordering::Relaxed),
        u32::from(results.wrong_refresh),
        results.monitor,
        u32::from(results.fullscreen),
        results.size.0,
        results.size.1,
        u32::from(results.gpu_measured),
        analysis.gpu_us.len(),
        results.gpu_skipped,
        percentile(&analysis.gpu_us, 0.5),
        percentile(&analysis.gpu_us, 0.99),
        analysis.gpu_us.last().copied().unwrap_or(0),
        analysis.clock_suspect,
        analysis.clock_bias_ns / 1000,
        percentile(&probe, 0.5),
        percentile(&probe, 0.99),
        probe.last().copied().unwrap_or(0),
        dropped.gpu_after,
        dropped.gpu_before,
        dropped.gpu_unknown,
        dropped.started_after,
        dropped.committed_after,
        dropped.ready_before,
        spread(&analysis.margins_on_time),
        spread(&analysis.margins_dropped),
    );
    report.push_str(&long_gaps(&results.frames, &analysis, &events, main_us));
    report
}

/// A line for each long gap, saying what the app was doing while it lasted.
fn long_gaps(
    frames: &[Record],
    analysis: &Analysis,
    events: &[(u64, &'static str, String)],
    main_us: &[u32],
) -> String {
    let first = frames.first().map_or(0, |frame| frame.swapped_ns);
    let mut text = String::new();
    let mut previous: Option<&Record> = None;
    for (index, (frame, row)) in frames.iter().zip(&analysis.rows).enumerate() {
        if frame.outcome != Outcome::Presented {
            continue;
        }
        if let (Some(before), true) = (previous, row.gap >= LONG_GAP) {
            let ms = |later: u64, earlier: u64| (later as f64 - earlier as f64) / 1e6;
            let during: Vec<String> = events
                .iter()
                .filter(|event| event.0 >= before.swapped_ns && event.0 <= frame.swapped_ns)
                .take(40)
                .map(|(at, kind, detail)| {
                    format!("+{:.1}ms {kind}:{detail}", ms(*at, before.swapped_ns))
                })
                .collect();
            text.push_str(&format!(
                "present_long_gap frame={index} refreshes={} dropped={} at_ms={:.1} \
                 pending_after_last_swap_ms={:.1} redraw_after_last_swap_ms={:.1} main_us={} \
                 gpu_us={} events=[{}]\n",
                row.gap,
                u32::from(row.dropped),
                ms(frame.swapped_ns, first),
                ms(frame.pending_ns, before.swapped_ns),
                ms(frame.start_ns, before.swapped_ns),
                main_us.get(index).copied().unwrap_or(0),
                frame.gpu_us,
                during.join("; "),
            ));
        }
        previous = Some(frame);
    }
    text
}

fn events_csv(frames: &[Record], events: &[(u64, &'static str, String)]) -> String {
    let mut text = String::from("ns,next_frame,kind,detail\n");
    for (at, kind, detail) in events {
        let next = frames.partition_point(|frame| frame.swapped_ns != 0 && frame.swapped_ns < *at);
        text.push_str(&format!("{at},{next},{kind},{}\n", detail.replace([',', '\n'], " ")));
    }
    text
}

fn csv(frames: &[Record], analysis: &Analysis, main_us: &[u32]) -> String {
    let mut text = String::from(
        "frame,submit_ns,swapped_ns,pending_ns,presented_ns,refresh_ns,seq,vsync,hw_clock,\
         hw_completion,zero_copy,outcome,msc_gap,interval_us,latency_us,dropped,idle_gap,main_us,\
         start_ns,gpu_us,gpu_complete_ns,fence_seen_ns,deadline_ns,gpu_margin_us,probe_us\n",
    );
    for (index, (frame, row)) in frames.iter().zip(&analysis.rows).enumerate() {
        let flag = |bit| u32::from(frame.flags & bit != 0);
        let outcome = match frame.outcome {
            Outcome::Waiting => "unresolved",
            Outcome::Presented => "presented",
            Outcome::Discarded => "discarded",
        };
        let main = main_us.get(index).map_or(String::new(), u32::to_string);
        let margin = row.margin_us.map_or(String::new(), |margin| margin.to_string());
        text.push_str(&format!(
            "{index},{},{},{},{},{},{},{},{},{},{},{outcome},{},{},{},{},{},{main},{},{},{},{},{},\
             {margin},{}\n",
            frame.submit_ns,
            frame.swapped_ns,
            frame.pending_ns,
            frame.presented_ns,
            frame.refresh_ns,
            frame.seq,
            flag(VSYNC),
            flag(HW_CLOCK),
            flag(HW_COMPLETION),
            flag(ZERO_COPY),
            row.gap,
            row.interval_us,
            row.latency_us,
            u32::from(row.dropped),
            u32::from(row.idle),
            frame.start_ns,
            frame.gpu_us,
            frame.gpu_complete_ns,
            frame.fence_seen_ns,
            row.deadline_ns,
            frame.probe_ns / 1000,
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFRESH: u64 = 4_166_666;

    /// A frame shown at refresh `msc`, with its content known `pending_ago` refreshes before that.
    fn shown(msc: u64, pending_ago: f64, seq: bool) -> Record {
        let presented_ns = 1_000_000_000 + msc * REFRESH;
        Record {
            submit_ns: presented_ns - 2_000_000,
            swapped_ns: presented_ns - 1_000_000,
            pending_ns: presented_ns - (pending_ago * REFRESH as f64) as u64,
            presented_ns,
            refresh_ns: REFRESH as u32,
            seq: if seq { msc + 100 } else { 0 },
            flags: VSYNC | HW_CLOCK,
            outcome: Outcome::Presented,
            ..Record::default()
        }
    }

    #[test]
    fn consecutive_refreshes_drop_nothing() {
        let frames: Vec<Record> = (0..5).map(|msc| shown(msc, 0.5, true)).collect();
        let analysis = analyse(&frames, 0);
        assert_eq!((analysis.late, analysis.missed_refreshes, analysis.idle_gaps), (0, 0, 0));
        assert_eq!(analysis.gaps.get(&1), Some(&4));
        assert!(!analysis.derived_msc);
    }

    #[test]
    fn a_gap_with_a_frame_pending_is_dropped() {
        let frames = [shown(0, 0.5, true), shown(3, 3.0, true)];
        let analysis = analyse(&frames, 0);
        assert_eq!((analysis.late, analysis.missed_refreshes), (1, 2));
        assert!(analysis.rows[1].dropped);
    }

    #[test]
    fn a_gap_with_nothing_to_show_is_idle() {
        let frames = [shown(0, 0.5, true), shown(100, 0.5, true)];
        let analysis = analyse(&frames, 0);
        assert_eq!((analysis.late, analysis.idle_gaps), (0, 1));
        assert!(analysis.rows[1].idle);
    }

    #[test]
    fn a_discarded_frame_is_dropped() {
        let frames =
            [shown(0, 0.5, true), Record { outcome: Outcome::Discarded, ..Record::default() }];
        let analysis = analyse(&frames, 0);
        assert_eq!(analysis.discarded, 1);
        assert!(analysis.rows[1].dropped);
    }

    #[test]
    fn refreshes_are_counted_from_timestamps_when_the_compositor_has_no_counter() {
        let frames = [shown(0, 0.5, false), shown(1, 0.5, false), shown(4, 4.0, false)];
        let analysis = analyse(&frames, 0);
        assert!(analysis.derived_msc);
        assert_eq!(analysis.rows[2].gap, 3);
        assert_eq!(analysis.late, 1);
    }

    #[test]
    fn warmup_is_only_ever_a_second_count() {
        let frames =
            [shown(0, 0.5, true), shown(3, 3.0, true), shown(4, 0.5, true), shown(9, 5.0, true)];
        let analysis = analyse(&frames, 2);
        assert_eq!((analysis.late, analysis.late_after_warmup), (2, 1));
    }

    /// A frame like `shown`, the GPU finishing `after_deadline` refreshes after its deadline.
    fn gpu_done(msc: u64, previous_msc: u64, after_deadline: f64) -> Record {
        let deadline = 1_000_000_000 + (previous_msc + 1) * REFRESH;
        Record {
            gpu_complete_ns: (deadline as f64 + after_deadline * REFRESH as f64) as u64,
            fence_seen_ns: u64::MAX,
            start_ns: deadline - 2 * REFRESH,
            swapped_ns: deadline - 2 * REFRESH,
            ..shown(msc, 3.0, true)
        }
    }

    #[test]
    fn a_dropped_frame_says_where_the_gpu_finished_against_its_deadline() {
        let frames =
            [shown(0, 0.5, true), gpu_done(2, 0, 0.5), gpu_done(4, 2, -0.5), gpu_done(5, 4, -0.5)];
        let analysis = analyse(&frames, 0);
        assert_eq!(analysis.dropped.gpu_after, 1);
        assert_eq!(analysis.dropped.gpu_before, 1);
        assert_eq!(analysis.margins_on_time.len(), 1);
        assert_eq!(analysis.margins_dropped.len(), 2);
    }

    /// A frame from the DP-4 runs: `previous` and `presented` are the two timestamps of a gap of
    /// `gap` refreshes of 4166388 ns, and `pending` when the frame's content first existed.
    fn dp4_gap(previous: u64, presented: u64, pending: u64) -> Analysis {
        let shown_at = |presented_ns, pending_ns| Record {
            presented_ns,
            swapped_ns: presented_ns - 1,
            pending_ns,
            refresh_ns: 4_166_388,
            outcome: Outcome::Presented,
            ..Record::default()
        };
        analyse(&[shown_at(previous, 0), shown_at(presented, pending)], 0)
    }

    #[test]
    fn a_zoom_step_arriving_inside_the_gap_is_idle() {
        // Standard scene, frame 1757: `step:Zoom` set the content 6.8 ms before it was shown.
        let analysis = dp4_gap(353_211_198_354_000, 353_211_231_694_000, 353_211_224_942_287);
        assert_eq!(analysis.rows[1].gap, 8);
        assert!(analysis.rows[1].idle);
        assert_eq!(analysis.late, 0);
    }

    #[test]
    fn a_content_change_a_refresh_before_the_missed_refresh_counts() {
        // Standard scene, frame 3160: a script step 8.6 ms before it was shown, 7 refreshes on:
        // more than a refresh before the last one to go by, even though the frame could hardly
        // have made it. The rule is about what was pending, not what the compositor allows.
        let analysis = dp4_gap(353_217_206_649_000, 353_217_235_526_000, 353_217_226_921_757);
        assert_eq!(analysis.rows[1].gap, 7);
        assert_eq!(analysis.late, 1);
    }

    #[test]
    fn a_frame_stuck_behind_its_content_still_counts() {
        // Standard scene, frame 1831: a split was pending from 13 ms before it was shown, and
        // refreshes 1 and 2 of the gap of 3 went by.
        let analysis = dp4_gap(353_211_535_394_000, 353_211_548_351_000, 353_211_525_100_246);
        assert_eq!(analysis.rows[1].gap, 3);
        assert!(analysis.rows[1].dropped);
    }

    #[test]
    fn a_frame_held_up_by_a_stall_still_counts() {
        // Standard scene, frame 4: a 19.5 ms swap, its content pending from long before the end.
        let analysis = dp4_gap(353_203_056_734_000, 353_203_235_629_000, 353_203_208_599_009);
        assert_eq!(analysis.rows[1].gap, 43);
        assert!(analysis.rows[1].dropped);
    }

    #[test]
    fn a_frame_with_no_gpu_reading_is_counted_as_unknown() {
        let frames = [shown(0, 0.5, true), shown(3, 3.0, true)];
        assert_eq!(analyse(&frames, 0).dropped.gpu_unknown, 1);
    }
}
