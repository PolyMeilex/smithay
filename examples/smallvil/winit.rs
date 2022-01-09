use std::time::Duration;

use calloop::timer::Timer;
use smithay::{
    backend::winit::{self, WinitEvent, WinitEventLoop, WinitGraphicsBackend},
    reexports::{calloop::EventLoop, wayland_server::protocol::wl_output},
    wayland::output::{Mode, Output, PhysicalProperties},
};

use slog::Logger;

use crate::CalloopData;

pub fn run_winit(
    event_loop: &mut EventLoop<CalloopData>,
    data: &mut CalloopData,
    log: Logger,
) -> Result<(), Box<dyn std::error::Error>> {
    let display = &mut data.display;
    let state = &mut data.state;

    let (mut backend, mut winit) = winit::init(log.clone())?;

    let mode = Mode {
        size: backend.window_size().physical_size,
        refresh: 60_000,
    };

    let (output, _global) = Output::new(
        display,
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: wl_output::Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
        },
        log.clone(),
    );
    output.change_current_state(
        &mut display.handle(),
        Some(mode),
        Some(wl_output::Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, 1.0, (0, 0));

    std::env::set_var("WAYLAND_DISPLAY", "wayland-5");

    let mut full_redraw = 0u8;

    let timer = Timer::<()>::new().unwrap();

    timer.handle().add_timeout(Duration::ZERO, ());
    event_loop.handle().insert_source(timer, move |_, timer, data| {
        winit_dispatch(&mut backend, &mut winit, data, &output, &mut full_redraw).unwrap();
        timer.add_timeout(Duration::from_millis(16), ());
    })?;

    Ok(())
}

pub fn winit_dispatch(
    backend: &mut WinitGraphicsBackend,
    winit: &mut WinitEventLoop,
    data: &mut CalloopData,
    output: &Output,
    full_redraw: &mut u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let display = &mut data.display;
    let state = &mut data.state;

    winit.dispatch_new_events(|event| match event {
        WinitEvent::Resized { .. } => {}
        WinitEvent::Input(event) => state.process_input_event(display, event),
        _ => (),
    })?;

    *full_redraw = full_redraw.saturating_sub(1);
    let age = if *full_redraw > 0 { 0 } else { backend.buffer_age() };

    let render_res = backend.bind().ok().and_then(|_| {
        state
            .space
            .render_output(
                &mut display.handle(),
                backend.renderer(),
                &output,
                age,
                [0.1, 0.1, 0.1, 1.0],
                &[],
            )
            .unwrap()
    });

    match render_res {
        Some(damage) => {
            let scale = state.space.output_scale(&output).unwrap_or(1.0);
            backend.submit(if age == 0 { None } else { Some(&*damage) }, scale)?;
        }
        None => {}
    }

    state.space.send_frames(
        &mut display.handle(),
        false,
        state.start_time.elapsed().as_millis() as u32,
    );

    state.space.refresh(&mut display.handle());
    display.flush_clients()?;

    Ok(())
}
