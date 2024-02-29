use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_dmabuf,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

use crate::{state::Backend, Smallvil};

impl DmabufHandler for Smallvil {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        match &mut self.backend {
            Backend::Drm(drm) => {
                drm.dmabuf_imported(global, dmabuf, notifier);
            }
            Backend::Winit => todo!(),
        }
    }
}
delegate_dmabuf!(Smallvil);
