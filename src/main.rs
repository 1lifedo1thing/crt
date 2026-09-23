//! CRT Terminal with Effects
//!
//! Two-pass rendering:
//! 1. Render text to offscreen texture using swash-based glyph cache
//! 2. Composite text with effects (gradient, grid, glow) to screen

mod app;
mod config;
mod font;
mod gpu;
mod input;
#[cfg(target_os = "macos")]
mod menu;
pub mod profiling;
mod render;
mod theme_registry;
mod watcher;
mod window;

use winit::event_loop::{ControlFlow, EventLoop};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Answer --version before any window exists: scripts and bug reports ask
    // for it, and the install kind is the other half of "what am I running?".
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!(
            "crt {} ({})",
            app::updates::running_version(),
            app::updates::current_install_kind().label()
        );
        return;
    }

    // `crt update` is the headless half of the in-app updater: the same code
    // paths, no window, and exit codes a script can branch on.
    if args.first().is_some_and(|a| a == "update") {
        env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("warn,crt=info"),
        )
        .init();
        let check_only = args.iter().any(|a| a == "--check");
        std::process::exit(app::updates::run_cli(check_only));
    }

    // Enable debug logging when profiling is enabled
    let profiling_enabled = std::env::var("CRT_PROFILE").is_ok();
    let default_filter = if profiling_enabled {
        "warn,crt=debug,crt_renderer=debug,crt_theme=debug,crt_core=debug"
    } else {
        "warn,crt=info"
    };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .init();

    if profiling_enabled {
        log::info!("CRT Terminal starting (profiling mode - debug logging enabled)");
    } else {
        log::info!("CRT Terminal starting");
    }

    // Initialize profiling (enabled via CRT_PROFILE=1)
    profiling::init();

    // The loop sleeps between events; PTY output and file changes wake it
    // through a user event and animations schedule their own deadlines.
    let event_loop = EventLoop::<app::WakeReason>::with_user_event()
        .build()
        .unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    event_loop.run_app(&mut app::App::new(proxy)).unwrap();

    // Flush profiling data on exit
    profiling::shutdown();
}
