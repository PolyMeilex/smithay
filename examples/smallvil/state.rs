use std::{os::unix::prelude::AsRawFd, sync::Arc};

use calloop::{generic::Generic, EventLoop, Interest, PostAction};
use slog::Logger;
use smithay::{
    desktop::Space,
    utils::{Logical, Point},
    wayland::{
        compositor::CompositorState,
        output::OutputManagerState,
        seat::{KeyboardHandle, PointerHandle, SeatState},
        shell::xdg::XdgShellState,
        shm::ShmState,
    },
};
use wayland_server::{
    backend::{ClientData, ClientId, DisconnectReason},
    protocol::wl_surface::WlSurface,
    socket::ListeningSocket,
    Client, Display, DisplayHandle,
};

use crate::CalloopData;

pub struct Smallvil {
    pub keyboard: KeyboardHandle,
    pub pointer: PointerHandle<Smallvil>,
    pub pointer_location: Point<f64, Logical>,

    pub start_time: std::time::Instant,
    pub clients: Vec<Client>,

    pub space: Space,
    pub log: slog::Logger,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Smallvil>,
}

impl Smallvil {
    pub fn new(event_loop: &mut EventLoop<CalloopData>, display: &mut Display<Self>, log: Logger) -> Self {
        let start_time = std::time::Instant::now();

        let compositor_state = CompositorState::new(display, None);
        let xdg_shell_state = XdgShellState::new(display, None).0;
        let shm_state = ShmState::new(display, vec![], None);
        let output_manager_state = OutputManagerState::new_with_xdg_output(display);
        let mut seat_state = SeatState::new(display, "winit".into(), None);

        let keyboard = seat_state
            .add_keyboard(&mut display.handle(), Default::default(), 200, 200, |_, _| {})
            .unwrap();

        let pointer = seat_state.add_pointer(&mut display.handle(), |_| {});

        Self::init_wayland_listener(event_loop);

        Self {
            keyboard,
            pointer,
            pointer_location: Default::default(),

            start_time,
            clients: Vec::new(),

            space: Space::new(log.clone()),
            log,

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
        }
    }

    fn init_wayland_listener(event_loop: &mut EventLoop<CalloopData>) {
        use calloop::Mode;

        let handle = event_loop.handle();
        let listener = ListeningSocket::bind("wayland-5").unwrap();

        event_loop
            .handle()
            .insert_source(
                Generic::from_fd(listener.as_raw_fd(), Interest::READ, Mode::Level),
                move |_, _, state| {
                    let display = &mut state.display;
                    let state = &mut state.state;

                    match listener.accept().unwrap() {
                        Some(stream) => {
                            handle
                                .insert_source(
                                    Generic::from_fd(stream.as_raw_fd(), Interest::READ, Mode::Level),
                                    |_, _, data| {
                                        data.display.dispatch_clients(&mut data.state).unwrap();
                                        Ok(PostAction::Continue)
                                    },
                                )
                                .unwrap();

                            let client = display.insert_client(stream, Arc::new(ClientState)).unwrap();
                            state.clients.push(client);
                        }
                        None => {}
                    }

                    display.dispatch_clients(state).unwrap();

                    Ok(PostAction::Continue)
                },
            )
            .expect("Failed to init the wayland event source.");
    }

    pub fn surface_under_pointer(&self, cx: &mut DisplayHandle) -> Option<(WlSurface, Point<i32, Logical>)> {
        let pos = self.pointer_location;

        if let Some(window) = self.space.window_under(cx, pos) {
            let window_loc = self.space.window_geometry(cx, window).unwrap().loc;
            window
                .surface_under(cx, pos - window_loc.to_f64())
                .map(|(s, loc)| (s, loc + window_loc))
        } else {
            None
        }
    }
}

pub struct ClientState;
impl ClientData<Smallvil> for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
