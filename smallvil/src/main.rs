#![allow(irrefutable_let_patterns)]

mod handlers;

mod drm;
mod grabs;
mod input;
mod state;
mod winit;

use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::{Display, DisplayHandle},
};
pub use state::Smallvil;

enum Backend {
    Drm(drm::DrmState<CalloopData>),
    Winit,
}

pub struct CalloopData {
    state: Smallvil,
    display_handle: DisplayHandle,
    backend: Backend,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;

    let display: Display<Smallvil> = Display::new()?;
    let display_handle = display.handle();
    let state = Smallvil::new(&mut event_loop, display);

    let backend = if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
        Backend::Winit
    } else {
        Backend::Drm(drm::init(event_loop.handle()))
    };

    let mut data = CalloopData {
        state,
        display_handle,
        backend,
    };

    match &data.backend {
        Backend::Drm(_) => {
            drm::start(&mut data);
        }
        Backend::Winit => {
            winit::start(&mut event_loop, &mut data)?;
        }
    }

    let mut args = std::env::args().skip(1);
    let flag = args.next();
    let arg = args.next();

    std::env::set_var("WAYLAND_DISPLAY", &data.state.socket_name);
    match (flag.as_deref(), arg) {
        (Some("-c") | Some("--command"), Some(command)) => {
            std::process::Command::new(command).spawn().ok();
        }
        _ => {
            std::process::Command::new("weston-terminal").spawn().ok();
        }
    }

    event_loop.run(None, &mut data, move |data| {
        // Smallvil is running
        data.state.space.refresh();
        data.state.popups.cleanup();
        let _ = data.state.display_handle.flush_clients();
    })?;

    Ok(())
}
