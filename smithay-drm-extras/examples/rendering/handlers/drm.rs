use smithay::backend::{
    allocator::Fourcc,
    drm::{self, DrmNode},
};
use smithay_drm_extras::drm_scanner::{self, DrmScanEvent};

use crate::{surface::OutputSurface, State};

const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

// Drm
impl State {
    pub fn on_drm_event(
        &mut self,
        node: DrmNode,
        event: drm::DrmEvent,
        _meta: &mut Option<drm::DrmEventMetadata>,
    ) {
        match event {
            drm::DrmEvent::VBlank(crtc) => {
                if let Some(device) = self.devices.get_mut(&node) {
                    if let Some(surface) = device.surfaces.get_mut(&crtc) {
                        let mut renderer = if self.primary_gpu == device.render_node {
                            self.gpu_manager.single_renderer(&device.render_node).unwrap()
                        } else {
                            self.gpu_manager
                                .renderer(
                                    &self.primary_gpu,
                                    &device.render_node,
                                    &mut device.gbm_allocator,
                                    surface.gbm_surface.format(),
                                )
                                .unwrap()
                        };

                        surface.gbm_surface.frame_submitted().unwrap();
                        surface.next_buffer(&mut renderer);
                    }
                }
            }
            drm::DrmEvent::Error(_) => {}
        }
    }

    pub fn on_drm_connector_event(&mut self, node: DrmNode, event: drm_scanner::DrmScanEvent) {
        let device = if let Some(device) = self.devices.get_mut(&node) {
            device
        } else {
            return;
        };

        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => {
                let mut renderer = self.gpu_manager.single_renderer(&device.render_node).unwrap();

                let mut surface = OutputSurface::new(
                    crtc,
                    &connector,
                    SUPPORTED_FORMATS,
                    renderer.as_mut().egl_context().dmabuf_render_formats().clone(),
                    &device.drm,
                    device.gbm.clone(),
                );

                surface.next_buffer(renderer.as_mut());

                device.surfaces.insert(crtc, surface);
            }
            DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => {
                device.surfaces.remove(&crtc);
            }
            _ => {}
        }
    }
}
