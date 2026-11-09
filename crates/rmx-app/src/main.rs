//! rmx: a remote connection manager.

// No console window behind the application on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod background;
#[cfg(feature = "bench")]
mod bench;
mod clipboard;
mod compare;
mod connect;
mod files;
mod gfx;
mod highlight;
mod options;
mod pacer;
mod pane;
mod remote;
mod sftp_files;
mod shell;
mod store;

use std::time::Instant;

use winit::event_loop::EventLoop;

#[cfg(feature = "bench")]
#[global_allocator]
static ALLOCATOR: bench::Counting = bench::Counting;

/// Note that a stage of starting up has finished, for the bench report.
#[inline]
pub(crate) fn mark(_stage: &'static str) {
    #[cfg(feature = "bench")]
    bench::mark(_stage);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    #[cfg(feature = "bench")]
    bench::start(started);
    background::mark_main_thread();

    // The runtime and the state file, found while the compositor is waited for.
    let boot = background::start();

    // Building the event loop waits for the compositor.
    gfx::preload_driver();

    let event_loop = EventLoop::<app::AppEvent>::with_user_event().build()?;
    mark("event_loop");
    let mut app = app::App::new(event_loop.create_proxy(), started, boot);
    event_loop.run_app(&mut app)?;
    app.finish();

    Ok(())
}
