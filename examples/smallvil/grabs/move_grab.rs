use smithay::{
    desktop::Window,
    utils::{Logical, Point},
    wayland::{
        seat::{AxisFrame, GrabStartData, PointerGrab, PointerInnerHandle},
        Serial,
    },
};
use wayland_server::{
    protocol::{wl_pointer::ButtonState, wl_surface::WlSurface},
    DisplayHandle,
};

use crate::Smallvil;

pub struct MoveSurfaceGrab {
    pub start_data: GrabStartData,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<Smallvil> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        cx: &mut DisplayHandle,
        _handle: &mut PointerInnerHandle<'_, Smallvil>,
        location: Point<f64, Logical>,
        _focus: Option<(WlSurface, Point<i32, Logical>)>,
        _serial: Serial,
        _time: u32,
    ) {
        let delta = location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;

        data.space
            .map_window(cx, &self.window, new_location.to_i32_round(), true);
    }

    fn button(
        &mut self,
        _data: &mut Smallvil,
        cx: &mut DisplayHandle<'_>,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        button: u32,
        state: ButtonState,
        serial: Serial,
        time: u32,
    ) {
        handle.button(cx, button, state, serial, time);
        if handle.current_pressed().is_empty() {
            // No more buttons are pressed, release the grab.
            handle.unset_grab(cx, serial, time);
        }
    }

    fn axis(
        &mut self,
        _data: &mut Smallvil,
        cx: &mut DisplayHandle<'_>,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        details: AxisFrame,
    ) {
        handle.axis(cx, details)
    }

    fn start_data(&self) -> &GrabStartData {
        &self.start_data
    }
}
