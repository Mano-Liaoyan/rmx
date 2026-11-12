//! Measures rmx against its performance and size budgets: `cargo run -p xtask -- bench`.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

mod e2e;
mod package;

/// Strip-ped release binary with every protocol in it, in bytes.
const MAX_BINARY: u64 = 20 * 1024 * 1024;
/// Main thread time of a frame, in microseconds.
const MAX_FRAME_P99: u64 = 3_000;
const MAX_FRAME: u64 = 4_000;
/// Main thread time of one user action, which has to fit before the next vblank like a frame does.
const MAX_ACTION: u64 = 4_000;
/// From process start to the first frame drawn, in microseconds.
pub const MAX_FIRST_FRAME: u64 = 150_000;
/// Heap allocations drawing a frame takes once caches and buffers have filled, among the frames
/// that only replay the interface: running egui for an animation or input allocates, and
/// is reported separately.
const MAX_DRAW_ALLOCATIONS: u64 = 0;
/// egui passes in a frame that only has terminal output to show.
const MAX_UI_PASSES_FOR_OUTPUT: u64 = 0;
/// Panes redrawn per partial frame while one of four has output, in percent. Above 100 because a
/// frame may also have to catch up an older back buffer.
const MAX_PANES_PER_FRAME: u64 = 125;
/// Frames drawn, and times the event loop wakes, in three seconds of an idle SSH pane.
const MAX_IDLE: u64 = 0;
/// Runs of Argon2 or connecting that were on the main thread.
const MAX_ON_MAIN: u64 = 0;
/// Main thread time a frame of a 4K remote desktop's full-screen updates takes, in microseconds.
const MAX_REMOTE_FRAME: u64 = 1_000;

const CORPUS_BYTES: usize = 128 * 1024 * 1024;
/// The standard scene fills a 4K window, as on the machines the budgets are set for.
const SCENE_SIZE: (u32, u32) = (3840, 2160);
/// Cycles of split and zoom animations in the standard scene, overridden by `RMX_BENCH_CYCLES`.
const SCENE_CYCLES: usize = 15;
/// How long the control scene runs on a real display.
const CONTROL_SECONDS: usize = 45;
/// Long enough for each animation to finish before the next begins.
const SCENE_STEP_MS: &str = "300";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // For machines without libopenssl-devel.
    let vendored = args.iter().any(|arg| arg == "--vendored-openssl");
    match args.first().map(String::as_str) {
        Some("bench") if args.iter().any(|arg| arg == "--real-display") => {
            real_display(&args, vendored)
        },
        Some("bench") => bench(vendored),
        Some("e2e") => end_to_end(vendored, args.get(1).filter(|arg| !arg.starts_with("--"))),
        Some("package") => match package::package() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("FAIL {err}");
                ExitCode::FAILURE
            },
        },
        _ => {
            eprintln!(
                "usage: cargo run -p xtask -- <bench | e2e [shots dir] | package> \
                 [--vendored-openssl]"
            );
            eprintln!(
                "       cargo run -p xtask -- bench --real-display --output <name> [--expect-hz \
                 <hz>] \\\n         [--wayland-display <socket>] [--kwin-pid <pid>] [--keep \
                 <dir>] [--max-seconds <s>] [--no-build]"
            );
            ExitCode::from(2)
        },
    }
}

/// Build the bench version of the app, which every scripted run uses.
fn build_bench(vendored: bool) -> Option<PathBuf> {
    let bench_target = target_dir().join("bench");
    let features = if vendored { "bench,vendored-openssl" } else { "bench" };
    let build = ["build", "--release", "-p", "rmx-app", "--features", features, "--target-dir"];
    run(Command::new("cargo").args(build).arg(&bench_target)).then_some(bench_target)
}

/// Everything against real servers: sessions, then sync between two instances.
fn end_to_end(vendored: bool, shots: Option<&String>) -> ExitCode {
    let Some(bench_target) = build_bench(vendored) else { return ExitCode::FAILURE };
    let shots = shots.map_or_else(|| bench_target.join("shots"), PathBuf::from);
    if let Err(err) = fs::create_dir_all(&shots) {
        eprintln!("FAIL could not make {}: {err}", shots.display());
        return ExitCode::FAILURE;
    }
    // `RMX_E2E_ONLY=vnc,rdp` runs just those, for working on one protocol.
    let only = std::env::var("RMX_E2E_ONLY").unwrap_or_default();
    let wanted = |name: &str| only.is_empty() || only.split(',').any(|part| part == name);
    type Test<'a> = (&'a str, Box<dyn Fn() -> Result<(), String> + 'a>);
    let tests: Vec<Test<'_>> = vec![
        ("ssh", Box::new(|| e2e::ssh(&bench_target, &shots))),
        ("sync", Box::new(|| e2e::sync(&bench_target))),
        ("files", Box::new(|| e2e::files(&bench_target, &shots))),
        ("vaults", Box::new(|| e2e::vaults(&bench_target, &shots))),
        ("vnc", Box::new(|| e2e::vnc(&bench_target, &shots))),
        ("rdp", Box::new(|| e2e::rdp(&bench_target, &shots))),
        ("windows", Box::new(|| e2e::windows(&bench_target, &shots))),
        ("terminals", Box::new(|| e2e::terminals(&bench_target, &shots))),
        ("no-passphrase", Box::new(|| e2e::no_passphrase(&bench_target, &shots))),
        ("last-tab", Box::new(|| e2e::last_tab(&bench_target, &shots))),
        ("panes", Box::new(|| e2e::panes(&bench_target, &shots))),
    ];
    let results: Vec<(&str, Result<(), String>)> = tests
        .into_iter()
        .filter(|(name, _)| wanted(name))
        .map(|(name, test)| (name, test()))
        .collect();
    let mut passed = true;
    for (name, result) in results {
        match result {
            Ok(()) => println!("ok   {name}"),
            Err(err) => {
                passed = false;
                println!("FAIL {name}: {err}");
            },
        }
    }
    if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn run(command: &mut Command) -> bool {
    command.status().is_ok_and(|status| status.success())
}

/// Where cargo builds, honouring `CARGO_TARGET_DIR` like cargo does.
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from("target"), PathBuf::from)
}

/// What the app measured in one run.
pub struct Report {
    pub text: String,
    values: HashMap<String, u64>,
}

impl Report {
    pub fn value(&self, key: &str) -> u64 {
        self.values.get(key).copied().unwrap_or(u64::MAX)
    }
}

/// One run of the bench build, in an invisible compositor of its own: never on the desktop.
/// With `shell` empty the panes get the user's shell, as they do outside the bench.
pub fn measure(
    bench_target: &Path,
    size: Option<(u32, u32)>,
    env: &[(&str, &str)],
    shell: &str,
) -> Option<Report> {
    let out = bench_target.join("report.txt");
    let _ = fs::remove_file(&out);
    if let Some(real) = REAL.get() {
        return real.measure(bench_target, &out, size, env, shell);
    }

    let mut app = Command::new("scripts/headless.sh");
    app.arg(bench_target.join("release/rmx")).env("RMX_BENCH_OUT", &out);
    if let Some((width, height)) = size {
        app.env("RMX_BENCH_SIZE", format!("{width}x{height}"))
            .env("HEADLESS_WIDTH", width.to_string())
            .env("HEADLESS_HEIGHT", height.to_string());
    }
    app.envs(env.iter().copied());
    if !shell.is_empty() {
        app.args(["-e", "sh", "-c", shell]);
    }
    if !run(&mut app) {
        eprintln!("rmx did not exit cleanly");
        return None;
    }

    let text = fs::read_to_string(&out).ok()?;
    keep_report(&out);
    Some(parse_report(text))
}

fn parse_report(text: String) -> Report {
    let values = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter_map(|(key, value)| Some((key.to_owned(), value.parse().ok()?)))
        .collect();
    Report { text, values }
}

/// Copy a report to `RMX_BENCH_KEEP`, if set: each run overwrites the one report file.
fn keep_report(report: &Path) {
    let Ok(keep) = std::env::var("RMX_BENCH_KEEP") else { return };
    let _ = fs::create_dir_all(&keep);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let _ = fs::copy(report, Path::new(&keep).join(format!("report-{stamp}.txt")));
    // The frame-by-frame table, if the run wrote one: `RMX_BENCH_FRAMES`.
    if let Ok(frames) = std::env::var("RMX_BENCH_FRAMES") {
        let _ = fs::copy(frames, Path::new(&keep).join(format!("frames-{stamp}.csv")));
    }
}

/// The script of the standard scene: four panes with the interface around them, sidebar open, then
/// split and zoom animations over them, the pointer over the interface and the palette opening.
fn scene_script(cycles: usize) -> String {
    let setup = ["split-right", "split-down", "left", "split-down"].join(",");
    let cycle = "zoom,zoom,split-right,close,move:120:150,move:600:20,palette,wait,palette,wait";
    format!("{setup},{},quit", vec![cycle; cycles].join(","))
}

/// Set for a run on a real display: every scene then goes there instead of a private compositor.
static REAL: OnceLock<RealDisplay> = OnceLock::new();

struct RealDisplay {
    output: String,
    socket: String,
    expect_ns: Option<u64>,
    keep: PathBuf,
    limit: Duration,
    scene: Mutex<String>,
    /// The compositor's main thread, if found, to see how the scheduler treats it.
    kwin: Option<u32>,
    /// Whether this is the user's session, where KWin can be asked about itself.
    live: bool,
}

impl RealDisplay {
    /// One run on the display named by `socket`, fullscreen on `output`, with its GPU sampled.
    /// Everything the app keeps goes to throwaway directories, away from the user's own state.
    fn measure(
        &self,
        bench_target: &Path,
        out: &Path,
        size: Option<(u32, u32)>,
        env: &[(&str, &str)],
        shell: &str,
    ) -> Option<Report> {
        let scene = self.scene.lock().ok()?.clone();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let label = format!("{scene}-{stamp}");
        let present = self.keep.join(format!("present-{label}.csv"));
        let gpu_file = self.keep.join(format!("gpu-{label}.csv"));

        let private = bench_target.join(format!("real-{}-{stamp}", std::process::id()));
        for dir in ["home", "config", "data", "cache", "state"] {
            fs::create_dir_all(private.join(dir)).ok()?;
        }
        let private_dir = |name: &str| private.join(name).canonicalize().ok();

        let mut app = Command::new(bench_target.join("release/rmx"));
        app.env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .env("WAYLAND_DISPLAY", &self.socket)
            .env("RMX_BENCH_OUT", out)
            .env("RMX_BENCH_PRESENT", &present)
            .env("RMX_BENCH_MONITOR", &self.output)
            .env("HOME", private_dir("home")?)
            .env("XDG_CONFIG_HOME", private_dir("config")?)
            .env("XDG_DATA_HOME", private_dir("data")?)
            .env("XDG_CACHE_HOME", private_dir("cache")?)
            .env("XDG_STATE_HOME", private_dir("state")?);
        if let Some(ns) = self.expect_ns {
            app.env("RMX_BENCH_EXPECT_REFRESH_NS", ns.to_string());
        }
        if let Some((width, height)) = size {
            app.env("RMX_BENCH_SIZE", format!("{width}x{height}"));
        }
        app.envs(env.iter().copied()).args(["-e", "sh", "-c", shell]);

        let load_before = load();
        let busy_before = busiest();
        if self.live {
            let info = Command::new("qdbus6")
                .args(["org.kde.KWin", "/KWin", "org.kde.KWin.supportInformation"])
                .output();
            if let Ok(info) = info {
                let _ = fs::write(self.keep.join(format!("kwin-info-{label}.txt")), info.stdout);
            }
        }
        let wall_start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let kwin = self
            .kwin
            .map(|pid| KwinSampler::start(pid, self.keep.join(format!("kwin-{label}.csv"))));
        let sampler = sample_gpu(&gpu_file);
        let started = Instant::now();
        let finished = run_limited(&mut app, self.limit);
        let elapsed = started.elapsed();
        if let Some(mut sampler) = sampler {
            let _ = sampler.kill();
            let _ = sampler.wait();
        }
        let load_after = load();
        let busy_after = busiest();
        let _ = fs::remove_dir_all(&private);
        let kwin = kwin.map_or_else(|| "not sampled".to_owned(), KwinSampler::stop);
        let journal = if self.live {
            kwin_journal(wall_start, &self.keep.join(format!("kwin-journal-{label}.log")))
        } else {
            0
        };

        let gpu = summarize_gpu(&gpu_file);
        let summary = format!(
            "scene={scene}\nwindow_seconds={:.1}\nexit={finished:?}\nload_before={load_before}\\
             nload_after={load_after}\ngpu={gpu}\nkwin={kwin}\nkwin_journal_lines={journal}\\
             nbusy_before=\n{busy_before}\nbusy_after=\n{busy_after}\n",
            elapsed.as_secs_f64(),
        );
        let report = fs::read_to_string(out).ok();
        let text = report.clone().unwrap_or_default();
        let _ = fs::write(self.keep.join(format!("report-{label}.txt")), &text);
        let _ = fs::write(self.keep.join(format!("summary-{label}.txt")), &summary);
        println!("{summary}");
        for line in text.lines().filter(|line| {
            line.starts_with("present_")
                || line.starts_with("frame_")
                || line.starts_with("frames=")
                || line.starts_with("gl=")
                || line.starts_with("script_")
        }) {
            println!("{line}");
        }
        println!("per-frame: {}", present.display());
        if !matches!(finished, Finished::Clean) {
            eprintln!("rmx did not exit cleanly: {finished:?}");
            return None;
        }
        Some(parse_report(report?))
    }
}

#[derive(Debug)]
enum Finished {
    Clean,
    Failed,
    /// It went on past the time allowed, and was stopped.
    TimedOut,
}

/// Run `command`, stopping it if it goes on for longer than `limit`.
fn run_limited(command: &mut Command, limit: Duration) -> Finished {
    let Ok(mut child) = command.spawn() else { return Finished::Failed };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() { Finished::Clean } else { Finished::Failed };
            },
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Finished::TimedOut;
            },
        }
    }
}

fn load() -> String {
    fs::read_to_string("/proc/loadavg").unwrap_or_default().trim().to_owned()
}

/// The processes using the most CPU, to say what else was running.
fn busiest() -> String {
    let output = Command::new("ps").args(["-eo", "pcpu,pid,ni,comm", "--sort=-pcpu"]).output();
    let text = output
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    text.lines().take(9).collect::<Vec<_>>().join("\n")
}

/// How the scheduler treats the compositor's main thread, every 50 ms: read from `/proc`, so it
/// asks nothing of the compositor.
struct KwinSampler {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<String>,
}

impl KwinSampler {
    fn start(pid: u32, file: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            let read = || {
                let text = fs::read_to_string(format!("/proc/{pid}/schedstat")).ok()?;
                let mut fields = text.split_whitespace().map(|field| field.parse::<u64>().ok());
                Some((fields.next()??, fields.next()??))
            };
            let start = Instant::now();
            let mut csv = String::from("ms,cpu_us,run_wait_us\n");
            let mut last = read();
            let first = last;
            let (mut max_cpu, mut max_wait) = (0, 0);
            while !stopping.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                let now = read();
                if let (Some(before), Some(after)) = (last, now) {
                    let (cpu, wait) = ((after.0 - before.0) / 1000, (after.1 - before.1) / 1000);
                    (max_cpu, max_wait) = (cpu.max(max_cpu), wait.max(max_wait));
                    csv.push_str(&format!("{},{cpu},{wait}\n", start.elapsed().as_millis()));
                }
                last = now;
            }
            let _ = fs::write(file, csv);
            let (Some(first), Some(end)) = (first, last) else { return "no data".to_owned() };
            format!(
                "pid={pid} cpu_ms={} run_wait_ms={} worst_50ms_window_cpu_us={max_cpu} \
                 worst_50ms_window_run_wait_us={max_wait}",
                (end.0 - first.0) / 1_000_000,
                (end.1 - first.1) / 1_000_000,
            )
        });
        Self { stop, thread }
    }

    fn stop(self) -> String {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap_or_default()
    }
}

/// What KWin wrote to the journal since `since`, kept in `file`. Returns how many lines.
fn kwin_journal(since: u64, file: &Path) -> usize {
    let at = format!("@{since}");
    let args = ["--user", "_COMM=kwin_wayland", "-S", at.as_str(), "--no-pager", "-q"];
    let Ok(output) = Command::new("journalctl").args(args).output() else { return 0 };
    let _ = fs::write(file, &output.stdout);
    String::from_utf8_lossy(&output.stdout).lines().count()
}

/// The main process of the live session's compositor, not a private one.
fn live_kwin() -> Option<u32> {
    let output = Command::new("pgrep").args(["-a", "-x", "kwin_wayland"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.lines().find(|line| !line.contains("--virtual"))?.split_whitespace().next()?.parse().ok()
}

/// Sample the GPU every second until killed, one line per sample.
fn sample_gpu(file: &Path) -> Option<std::process::Child> {
    let out = fs::File::create(file).ok()?;
    Command::new("nvidia-smi")
        .args([
            "--query-gpu=timestamp,pstate,clocks.gr,utilization.gpu",
            "--format=csv,noheader,nounits",
            "-l",
            "1",
        ])
        .stdout(out)
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// What the GPU's samples came to: its power states, and the spread of clock and load.
fn summarize_gpu(file: &Path) -> String {
    let text = fs::read_to_string(file).unwrap_or_default();
    let mut states: HashMap<String, u32> = HashMap::new();
    let (mut clocks, mut usage): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let [_, state, clock, use_percent] = fields[..] else { continue };
        *states.entry(state.to_owned()).or_default() += 1;
        clocks.extend(clock.parse::<u32>().ok());
        usage.extend(use_percent.parse::<u32>().ok());
    }
    if states.is_empty() {
        return "no samples: nvidia-smi gave nothing".to_owned();
    }
    let spread = |values: &mut Vec<u32>| {
        values.sort_unstable();
        format!(
            "{}/{}/{}",
            values.first().unwrap_or(&0),
            values.get(values.len() / 2).unwrap_or(&0),
            values.last().unwrap_or(&0),
        )
    };
    let mut states: Vec<_> = states.into_iter().collect();
    states.sort();
    format!(
        "samples={} pstates={states:?} clocks_mhz_min/med/max={} util_pct_min/med/max={}",
        clocks.len(),
        spread(&mut clocks),
        spread(&mut usage),
    )
}

/// This process's nice value: a measured run must not inherit a build's.
fn own_nice() -> Option<i32> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(16)?.parse().ok()
}

/// `xtask bench --real-display --output <name>`: the local and standard scenes on a real output,
/// with the frames' presentation recorded. Runs on the live session, so only when that is wanted.
fn real_display(args: &[String], vendored: bool) -> ExitCode {
    let value = |flag: &str| {
        args.iter().position(|arg| arg == flag).and_then(|at| args.get(at + 1)).cloned()
    };
    let Some(output) = value("--output") else {
        eprintln!("--real-display needs --output <name>");
        return ExitCode::from(2);
    };
    let socket = value("--wayland-display").unwrap_or_else(|| "wayland-1".to_owned());
    let hz: f64 = value("--expect-hz").and_then(|hz| hz.parse().ok()).unwrap_or(240.);
    let limit = value("--max-seconds").and_then(|s| s.parse().ok()).unwrap_or(150);
    let keep = value("--keep").map_or_else(|| target_dir().join("bench/real"), PathBuf::from);

    match own_nice() {
        Some(0) => (),
        nice => {
            eprintln!(
                "refusing to measure at nice {nice:?}: run the built xtask directly, not under \
                 nice"
            );
            return ExitCode::FAILURE;
        },
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_default();
    if !runtime.join(&socket).exists() {
        eprintln!("no Wayland socket {socket} in {}", runtime.display());
        return ExitCode::FAILURE;
    }
    if let Err(err) = fs::create_dir_all(&keep) {
        eprintln!("could not make {}: {err}", keep.display());
        return ExitCode::FAILURE;
    }

    let bench_target = target_dir().join("bench");
    if !args.iter().any(|arg| arg == "--no-build") && build_bench(vendored).is_none() {
        return ExitCode::FAILURE;
    }
    if !bench_target.join("release/rmx").exists() {
        eprintln!("no bench build in {}", bench_target.display());
        return ExitCode::FAILURE;
    }

    let expect_ns = (hz > 0.).then(|| (1e9 / hz) as u64);
    let scene = Mutex::new(String::new());
    // A socket given by path is a private compositor, for rehearsing.
    let live = !socket.contains('/');
    let kwin = value("--kwin-pid")
        .and_then(|pid| pid.parse().ok())
        .or_else(|| live.then(live_kwin).flatten());
    let real = RealDisplay {
        output,
        socket,
        expect_ns,
        keep,
        limit: Duration::from_secs(limit),
        scene,
        kwin,
        live,
    };
    let real = REAL.get_or_init(|| real);

    let corpus = corpus(&bench_target);
    let cycles = std::env::var("RMX_BENCH_CYCLES")
        .ok()
        .and_then(|cycles| cycles.parse().ok())
        .unwrap_or(SCENE_CYCLES);
    let only = std::env::var("RMX_BENCH_ONLY").unwrap_or_default();
    let wanted = |name: &str| only.is_empty() || only.split(',').any(|part| part == name);
    let mut passed = true;
    let set_scene = |name: &str| {
        if let Ok(mut scene) = real.scene.lock() {
            *scene = name.to_owned();
        }
    };

    let control_seconds = std::env::var("RMX_BENCH_CONTROL_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(CONTROL_SECONDS);
    for (name, damage) in [("control", "tiny"), ("control-full", "full")] {
        // Only when asked for by name: a plain run stays the two scenes it has always been.
        if !only.split(',').any(|part| part == name) {
            continue;
        }
        set_scene(name);
        let script = format!("{}quit", "wait,".repeat(control_seconds));
        let env = [
            ("RMX_BENCH_VAULT", "off"),
            ("RMX_BENCH_CONTROL", damage),
            ("RMX_BENCH_SCRIPT", script.as_str()),
            ("RMX_BENCH_STEP_MS", "1000"),
        ];
        passed &=
            real_result(name, measure(&bench_target, Some(SCENE_SIZE), &env, "exec sleep 100000"));
    }

    if passed && wanted("local") {
        set_scene("local");
        let script = scene_script(cycles);
        let env = [
            ("RMX_BENCH_VAULT", "off"),
            ("RMX_BENCH_SCRIPT", script.as_str()),
            ("RMX_BENCH_STEP_MS", SCENE_STEP_MS),
        ];
        let shell = format!("while :; do cat {}; done", corpus.display());
        passed &= real_result("local", measure(&bench_target, Some(SCENE_SIZE), &env, &shell));
    }
    if passed && wanted("standard") {
        set_scene("standard");
        passed &= real_result(
            "standard",
            e2e::standard_scene(&bench_target, cycles).map_err(|err| eprintln!("{err}")).ok(),
        );
    }
    let leftover = Command::new("pgrep")
        .args(["-f", "-a", &bench_target.join("release/rmx").display().to_string()])
        .output();
    match leftover.map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned()) {
        Ok(left) if left.is_empty() => println!("rmx processes left: none"),
        Ok(left) => {
            println!("rmx processes left: {left}");
            passed = false;
        },
        Err(err) => println!("rmx processes left: unknown ({err})"),
    }
    if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Whether a run on a real display gave numbers that can be published.
fn real_result(scene: &str, report: Option<Report>) -> bool {
    let Some(report) = report else {
        println!("FAIL {scene}: the run did not finish");
        return false;
    };
    if report.value("present_frames") == u64::MAX {
        println!("FAIL {scene}: no presentation feedback was recorded");
        return false;
    }
    if report.value("present_wrong_refresh") != 0 {
        println!(
            "FAIL {scene}: the output refreshes every {} ns, not the rate asked for: stopping",
            report.value("present_refresh_ns"),
        );
        return false;
    }
    true
}

fn bench(vendored: bool) -> ExitCode {
    let mut passed = true;
    let mut check = |name: &str, value: u64, max: u64, unit: &str| {
        let ok = value <= max;
        passed &= ok;
        println!("{} {name}: {value} {unit} (budget {max})", if ok { "ok  " } else { "FAIL" });
    };

    let target = target_dir();
    let mut build = Command::new("cargo");
    build.args(["build", "--release", "-p", "rmx-app"]);
    if vendored {
        build.args(["--features", "vendored-openssl"]);
    }
    if !run(&mut build) {
        return ExitCode::FAILURE;
    }
    let size = fs::metadata(target.join("release/rmx")).map_or(u64::MAX, |meta| meta.len());
    let openssl =
        if vendored { "vendored OpenSSL built in" } else { "system OpenSSL, dynamically linked" };
    check(&format!("binary size ({openssl})"), size, MAX_BINARY, "bytes");

    let Some(bench_target) = build_bench(vendored) else { return ExitCode::FAILURE };
    let corpus = corpus(&bench_target);
    let corpus = corpus.display();
    // `RMX_BENCH_ONLY=local,standard` runs just those scenes, for working on frame times.
    let only = std::env::var("RMX_BENCH_ONLY").unwrap_or_default();
    let wanted = |name: &str| only.is_empty() || only.split(',').any(|part| part == name);
    let cycles = std::env::var("RMX_BENCH_CYCLES").ok().and_then(|cycles| cycles.parse().ok());

    if wanted("throughput") {
        // Throughput: one pane drinking the corpus, timed by the app from its first frame, so that
        // the start of the compositor does not count.
        let shell = format!("cat {corpus} {corpus}");
        let vaultless = [("RMX_BENCH_VAULT", "off")];
        let Some(report) = measure(&bench_target, None, &vaultless, &shell) else {
            return ExitCode::FAILURE;
        };
        let run_us = report.value("run_us");
        println!(
            "info throughput: {:.0} MiB/s, {} bytes in {run_us} us over {} frames",
            (CORPUS_BYTES * 2) as f64 / 1024. / 1024. / (run_us as f64 / 1e6),
            CORPUS_BYTES * 2,
            report.value("frames"),
        );
        println!("info gl: {}", report.text.lines().find(|l| l.starts_with("gl=")).unwrap_or("?"));
        check("first frame", report.value("first_frame_us"), MAX_FIRST_FRAME, "us");
    }

    if wanted("local") {
        // The standard scene: four busy panes and animations.
        let script = scene_script(cycles.unwrap_or(SCENE_CYCLES));
        let env = [
            ("RMX_BENCH_VAULT", "off"),
            ("RMX_BENCH_SCRIPT", script.as_str()),
            ("RMX_BENCH_STEP_MS", SCENE_STEP_MS),
        ];
        let shell = format!("while :; do cat {corpus}; done");
        let Some(report) = measure(&bench_target, Some(SCENE_SIZE), &env, &shell) else {
            return ExitCode::FAILURE;
        };
        println!(
            "info scene: {} frames, p50 {} us, {} full and {} partial",
            report.value("frames"),
            report.value("frame_p50_us"),
            report.value("full_frames"),
            report.value("partial_frames"),
        );
        println!("info phases: {}", report.text.lines().skip(5).collect::<Vec<_>>().join(", "));
        check("frame p99", report.value("frame_p99_us"), MAX_FRAME_P99, "us");
        check("frame max", report.value("frame_max_us"), MAX_FRAME, "us");
        check("action max", report.value("action_max_us"), MAX_ACTION, "us");
        println!(
            "info after warmup: frame p99 {} us, frame max {} us",
            report.value("settled_p99_us"),
            report.value("settled_max_us"),
        );
        println!(
            "info interface: {} egui passes, p99 {} us, max {} us; {} allocations over {} frames \
             with a pass",
            report.value("ui_passes"),
            report.value("ui_p99_us"),
            report.value("ui_max_us"),
            report.value("ui_allocations"),
            report.value("ui_frames"),
        );
        let draw_allocations = report.value("draw_allocations");
        check("draw allocations", draw_allocations, MAX_DRAW_ALLOCATIONS, "allocations");
        println!(
            "info allocations per frame: {:.3} drawing, {:.3} whole main thread (over {} and {} \
             frames)",
            draw_allocations as f64 / report.value("steady_frames").max(1) as f64,
            report.value("loop_allocations") as f64 / report.value("loop_frames").max(1) as f64,
            report.value("steady_frames"),
            report.value("loop_frames"),
        );
    }

    if wanted("standard") {
        // The plan's standard scene: three busy SSH terminals and a VNC desktop in full-screen
        // motion, against real servers, with the animations cycling over them.
        match e2e::standard_scene(&bench_target, cycles.unwrap_or(SCENE_CYCLES)) {
            Ok(report) => {
                println!(
                    "info standard scene: {} frames, p50 {} us, {} full and {} partial, {} remote \
                     frames",
                    report.value("frames"),
                    report.value("frame_p50_us"),
                    report.value("full_frames"),
                    report.value("partial_frames"),
                    report.value("remote_frames"),
                );
                for key in [
                    "phase_p50",
                    "worst_phases",
                    "slowest_phases",
                    "on_cpu_p99_us",
                    "on_cpu_max_us",
                    "slow_frames",
                ] {
                    let line = report.text.lines().find(|line| line.starts_with(key));
                    println!("info standard scene {}", line.unwrap_or("?"));
                }
                check(
                    "standard scene frame p99",
                    report.value("frame_p99_us"),
                    MAX_FRAME_P99,
                    "us",
                );
                check("standard scene frame max", report.value("frame_max_us"), MAX_FRAME, "us");
                check("standard scene action max", report.value("action_max_us"), MAX_ACTION, "us");
                check(
                    "standard scene allocations",
                    report.value("draw_allocations"),
                    MAX_DRAW_ALLOCATIONS,
                    "allocations",
                );
                check(
                    "remote frame p99 on main",
                    report.value("remote_p99_us"),
                    MAX_REMOTE_FRAME,
                    "us",
                );
                check(
                    "remote frame max on main",
                    report.value("remote_max_us"),
                    MAX_REMOTE_FRAME,
                    "us",
                );
                check(
                    "connects on the main thread",
                    report.value("on_main_violations"),
                    MAX_ON_MAIN,
                    "runs",
                );
            },
            Err(err) => {
                println!("FAIL standard scene: {err}");
                check("standard scene ran", 1, 0, "failures");
            },
        }
    }

    if wanted("idle-remote") {
        match e2e::idle_remote_pane(&bench_target) {
            Ok(report) => {
                check("idle remote pane frames", report.value("idle_frames"), MAX_IDLE, "frames");
                check(
                    "idle remote pane wakeups",
                    report.value("idle_wakeups"),
                    MAX_IDLE,
                    "wakeups",
                );
            },
            Err(err) => {
                println!("FAIL idle remote pane: {err}");
                check("idle remote pane ran", 1, 0, "failures");
            },
        }
    }

    if wanted("damage") {
        // Damage: one pane of four has output, and only it may be redrawn.
        let lock = bench_target.join(format!("damage.{}", std::process::id()));
        let shell = format!(
            "mkdir {lock} 2>/dev/null || exec sleep 1000; while :; do echo $$; sleep 0.02; done",
            lock = lock.display(),
        );
        let script = format!("split-right,split-down,split-right,{}quit", "wait,".repeat(30));
        let env = [
            ("RMX_BENCH_VAULT", "off"),
            ("RMX_BENCH_SCRIPT", script.as_str()),
            ("RMX_BENCH_STEP_MS", "100"),
        ];
        let Some(report) = measure(&bench_target, None, &env, &shell) else {
            return ExitCode::FAILURE;
        };
        let _ = fs::remove_dir(&lock);
        let partial = report.value("partial_frames");
        println!(
            "info damage: {partial} partial frames showing {} panes redrew {}, buffer age up to {}",
            report.value("partial_shown"),
            report.value("partial_drawn"),
            report.value("max_buffer_age"),
        );
        check(
            "egui passes for terminal output",
            report.value("ui_passes_on_partial"),
            MAX_UI_PASSES_FOR_OUTPUT,
            "passes",
        );
        let per_frame = (partial > 0).then(|| report.value("partial_drawn") * 100 / partial);
        check("panes per partial frame", per_frame.unwrap_or(u64::MAX), MAX_PANES_PER_FRAME, "%");
    }

    if wanted("ssh") {
        // An idle SSH pane against a real server: the session must cost the main thread nothing,
        // and deriving the key and connecting must not run there.
        match e2e::idle_ssh_pane(&bench_target) {
            Ok(report) => {
                println!(
                    "info ssh pane: {} us slowest action ({}), frame max {} us",
                    report.value("action_max_us"),
                    report
                        .text
                        .lines()
                        .find_map(|l| l.strip_prefix("action_slowest="))
                        .unwrap_or("?"),
                    report.value("frame_max_us"),
                );
                check("idle ssh pane frames", report.value("idle_frames"), MAX_IDLE, "frames");
                check("idle ssh pane wakeups", report.value("idle_wakeups"), MAX_IDLE, "wakeups");
                check("ssh scene action max", report.value("action_max_us"), MAX_ACTION, "us");
                check(
                    "argon2 and connect on the main thread",
                    report.value("on_main_violations"),
                    MAX_ON_MAIN,
                    "runs",
                );
                let ran = report.value("argon2_runs").min(report.value("connect_runs"));
                check("argon2 and connect measured", u64::from(ran == 0), 0, "missing");
            },
            Err(err) => {
                println!("FAIL idle ssh pane: {err}");
                passed = false;
            },
        }
    }

    if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// A file of typical terminal output: colored, wrapped text with some wide characters.
fn corpus(bench_target: &Path) -> PathBuf {
    let path = bench_target.join("corpus.txt");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() as usize >= CORPUS_BYTES) {
        return path;
    }

    fs::create_dir_all(bench_target).unwrap();
    let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    let mut written = 0;
    let mut line = 0usize;
    while written < CORPUS_BYTES {
        let text = format!(
            "\x1b[3{}m{line:>8}\x1b[0m drwxr-xr-x  \x1b[1mroot\x1b[0m  远程连接 {} \
             /usr/lib/x86_64/lib{}.so\n",
            line % 8,
            "abcdefghij".repeat(line % 12),
            line % 977,
        );
        file.write_all(text.as_bytes()).unwrap();
        written += text.len();
        line += 1;
    }
    path
}
