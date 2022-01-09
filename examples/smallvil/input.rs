use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyboardKeyEvent, PointerAxisEvent,
        PointerButtonEvent, PointerMotionAbsoluteEvent,
    },
    wayland::{
        seat::{AxisFrame, FilterResult},
        SERIAL_COUNTER,
    },
};
use wayland_server::{protocol::wl_pointer, Display};

use crate::state::Smallvil;

impl Smallvil {
    pub fn process_input_event<I: InputBackend>(
        &mut self,
        display: &mut Display<Smallvil>,
        event: InputEvent<I>,
    ) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let cx = &mut display.handle();
                self.keyboard
                    .input::<(), _>(cx, event.key_code(), event.state(), 0.into(), 0, |_, _| {
                        FilterResult::Forward
                    });
            }
            InputEvent::PointerMotion { .. } => {}
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let cx = &mut display.handle();

                let output = self.space.outputs().next().unwrap();

                let output_geo = self.space.output_geometry(output).unwrap();

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_location = pos;

                let serial = SERIAL_COUNTER.next_serial();

                let under = self.surface_under_pointer(cx);

                self.pointer
                    .clone()
                    .motion(self, cx, pos, under, serial, event.time());
            }
            InputEvent::PointerButton { event, .. } => {
                let cx = &mut display.handle();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();
                let button_state = match event.state() {
                    ButtonState::Pressed => wl_pointer::ButtonState::Pressed,
                    ButtonState::Released => wl_pointer::ButtonState::Released,
                };

                if wl_pointer::ButtonState::Pressed == button_state {
                    if !self.pointer.is_grabbed() {
                        if let Some(window) = self.space.window_under(cx, self.pointer_location).cloned() {
                            self.space.raise_window(cx, &window, true);
                            let window_loc = self.space.window_geometry(cx, &window).unwrap().loc;
                            let surface = window
                                .surface_under(cx, self.pointer_location - window_loc.to_f64())
                                .map(|(s, _)| s);
                            self.keyboard.set_focus(cx, surface.as_ref(), serial);

                            window.set_activated(cx, true);
                            window.configure(cx);
                        } else {
                            self.space.windows().for_each(|window| {
                                window.set_activated(cx, false);
                                window.configure(cx);
                            });
                            self.keyboard.set_focus(cx, None, serial);
                        }
                    }
                };

                self.pointer
                    .clone()
                    .button(self, cx, button, button_state, serial, event.time());
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = match event.source() {
                    AxisSource::Continuous => wl_pointer::AxisSource::Continuous,
                    AxisSource::Finger => wl_pointer::AxisSource::Finger,
                    AxisSource::Wheel | AxisSource::WheelTilt => wl_pointer::AxisSource::Wheel,
                };
                let horizontal_amount = event
                    .amount(Axis::Horizontal)
                    .unwrap_or_else(|| event.amount_discrete(Axis::Horizontal).unwrap() * 3.0);
                let vertical_amount = event
                    .amount(Axis::Vertical)
                    .unwrap_or_else(|| event.amount_discrete(Axis::Vertical).unwrap() * 3.0);
                let horizontal_amount_discrete = event.amount_discrete(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_discrete(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(wl_pointer::Axis::HorizontalScroll, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.discrete(wl_pointer::Axis::HorizontalScroll, discrete as i32);
                    }
                } else if source == wl_pointer::AxisSource::Finger {
                    frame = frame.stop(wl_pointer::Axis::HorizontalScroll);
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(wl_pointer::Axis::VerticalScroll, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.discrete(wl_pointer::Axis::VerticalScroll, discrete as i32);
                    }
                } else if source == wl_pointer::AxisSource::Finger {
                    frame = frame.stop(wl_pointer::Axis::VerticalScroll);
                }

                let cx = &mut display.handle();
                self.pointer.clone().axis(self, cx, frame);
            }
            _ => {}
        }
    }
}
