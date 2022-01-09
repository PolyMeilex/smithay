use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    wayland::{
        compositor::{CompositorHandler, CompositorState},
        shm::{ShmHandler, ShmState},
    },
};
use wayland_server::{
    delegate_dispatch, delegate_global_dispatch,
    protocol::{
        wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor, wl_region::WlRegion,
        wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface, wl_surface::WlSurface,
    },
    DisplayHandle,
};

use crate::Smallvil;

impl CompositorHandler for Smallvil {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn commit(&mut self, cx: &mut DisplayHandle, surface: &WlSurface) {
        on_commit_buffer_handler(cx, surface);
        self.space.commit(cx, &surface);
    }
}

impl ShmHandler for Smallvil {
    fn shm_state(&mut self) -> &mut ShmState {
        &mut self.shm_state
    }
}

// Wl Compositor
delegate_global_dispatch!(Smallvil: [WlCompositor] => CompositorState);
delegate_dispatch!(Smallvil: [WlCompositor, WlSurface, WlRegion, WlCallback] => CompositorState);

delegate_global_dispatch!(Smallvil: [WlSubcompositor] => CompositorState);
delegate_dispatch!(Smallvil: [WlSubcompositor, WlSubsurface] => CompositorState);

// Wl Shm
delegate_global_dispatch!(Smallvil: [WlShm] => ShmState);
delegate_dispatch!(Smallvil: [WlShm, WlShmPool, WlBuffer] => ShmState);
