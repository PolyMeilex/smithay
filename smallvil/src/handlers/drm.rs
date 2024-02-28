use smithay::{
    backend::{input::InputEvent, libinput::LibinputInputBackend},
    desktop::utils::surface_primary_scanout_output,
    output::Output,
    reexports::wayland_server::backend::GlobalId,
};

use crate::{drm, state::Smallvil, Backend, CalloopData};
use std::{cell::RefCell, time::Duration};

#[derive(Default)]
struct OutputUserData {
    global_id: RefCell<Option<GlobalId>>,
}

impl drm::DrmBackend for CalloopData {
    fn drm_state(&self) -> &drm::DrmState<Self> {
        if let Backend::Drm(state) = &self.backend {
            state
        } else {
            unreachable!("Uinitialized backend")
        }
    }

    fn drm_state_mut(&mut self) -> &mut drm::DrmState<Self> {
        if let Backend::Drm(state) = &mut self.backend {
            state
        } else {
            unreachable!("Uinitialized backend")
        }
    }

    fn on_output_added(&mut self, output: &Output) {
        self.state.space.map_output(output, (0, 0));
        let global_id = output.create_global::<Smallvil>(&self.display_handle);
        output
            .user_data()
            .get_or_insert(OutputUserData::default)
            .global_id
            .replace(Some(global_id));
    }

    fn on_output_removed(&mut self, output: &Output) {
        self.state.space.unmap_output(output);
        if let Some(id) = output
            .user_data()
            .get::<OutputUserData>()
            .and_then(|data| data.global_id.borrow_mut().take())
        {
            self.display_handle.remove_global::<Smallvil>(id);
        }
    }

    fn on_input_event(&mut self, event: InputEvent<LibinputInputBackend>) {
        self.state.process_input_event(event);
    }

    fn render_fn(
        &mut self,
    ) -> (
        &mut drm::DrmState<Self>,
        impl FnOnce(drm::DrmRenderRequest) -> bool,
    ) {
        let Backend::Drm(drm_state) = &mut self.backend else {
            unreachable!("Uinitialized backend")
        };

        let state = &mut self.state;
        let render = |request: drm::DrmRenderRequest| {
            let elements = smithay::desktop::space::space_render_elements(
                request.renderer,
                [&state.space],
                request.output,
                1.0,
            )
            .unwrap();

            let res = request
                .gbm_compositor
                .render_frame(request.renderer, &elements, [0.1, 0.1, 0.1, 1.0])
                .unwrap();

            state
                .space
                .elements_for_output(request.output)
                .for_each(|window| {
                    window.send_frame(
                        request.output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        surface_primary_scanout_output,
                    )
                });

            !res.is_empty
        };

        (drm_state, render)
    }
}
