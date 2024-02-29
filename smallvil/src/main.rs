#![allow(irrefutable_let_patterns)]

mod handlers;

mod drm;
mod grabs;
mod input;
mod state;
mod winit;

use smithay::reexports::calloop::EventLoop;
use state::{Backend, Smallvil};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<Smallvil> = EventLoop::try_new()?;

    let backend = if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
        Backend::Winit
    } else {
        Backend::Drm(drm::init(event_loop.handle()))
    };

    let mut state = Smallvil::new(&mut event_loop, backend);

    match &state.backend {
        Backend::Drm(_) => {
            drm::start(state.display_handle.clone(), &mut state);
        }
        Backend::Winit => {
            winit::start(&mut event_loop, &mut state)?;
        }
    }

    let mut args = std::env::args().skip(1);
    let flag = args.next();
    let arg = args.next();

    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    match (flag.as_deref(), arg) {
        (Some("-c") | Some("--command"), Some(command)) => {
            std::process::Command::new(command).spawn().ok();
        }
        _ => {
            std::process::Command::new("weston-terminal").spawn().ok();
        }
    }

    event_loop.run(None, &mut state, move |state| {
        // Smallvil is running
        state.space.refresh();
        state.popups.cleanup();
        let _ = state.display_handle.flush_clients();
    })?;

    Ok(())
}
