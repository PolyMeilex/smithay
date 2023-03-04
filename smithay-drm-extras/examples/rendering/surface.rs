use std::collections::HashSet;

use smithay_drm_extras::edid::EdidInfo;

use smithay::{
    backend::{
        allocator::{
            dmabuf::Dmabuf,
            gbm::{self, GbmAllocator, GbmBufferFlags},
        },
        allocator::{Format, Fourcc},
        drm::{self, DrmDeviceFd, GbmBufferedSurface},
        renderer::{
            damage::OutputDamageTracker, element::memory::MemoryRenderBufferRenderElement, Bind, ImportMem,
            Renderer,
        },
    },
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::drm::control::{connector, crtc, ModeTypeFlags},
    utils::Transform,
};

pub struct OutputSurface {
    pub gbm_surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>,
    pub output: Output,
    pub damage_tracked_renderer: OutputDamageTracker,

    animation_frame: f32,
}

impl OutputSurface {
    pub fn new(
        crtc: crtc::Handle,
        connector: &connector::Info,
        color_formats: &[Fourcc],
        renderer_formats: HashSet<Format>,
        drm: &drm::DrmDevice,
        gbm: gbm::GbmDevice<DrmDeviceFd>,
    ) -> Self {
        let mode_id = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);

        let drm_mode = connector.modes()[mode_id];

        let drm_surface = drm.create_surface(crtc, drm_mode, &[connector.handle()]).unwrap();

        let gbm_surface = GbmBufferedSurface::new(
            drm_surface,
            GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT),
            color_formats,
            renderer_formats,
        )
        .unwrap();

        let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());

        let (make, model) = EdidInfo::for_connector(drm, connector.handle())
            .map(|info| (info.manufacturer, info.model))
            .unwrap_or_else(|| ("Unknown".into(), "Unknown".into()));

        let (w, h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name,
            PhysicalProperties {
                size: (w as i32, h as i32).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make,
                model,
            },
        );

        let output_mode = WlMode::from(drm_mode);
        output.set_preferred(output_mode);
        output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            Some(smithay::output::Scale::Integer(1)),
            None,
        );

        let damage_tracked_renderer = OutputDamageTracker::from_output(&output);

        Self {
            gbm_surface,
            output,
            damage_tracked_renderer,
            animation_frame: 0.0,
        }
    }

    pub fn next_buffer<R>(&mut self, renderer: &mut R)
    where
        R: Renderer + ImportMem + Bind<Dmabuf>,
        R::TextureId: 'static,
    {
        let (dmabuf, _age) = self.gbm_surface.next_buffer().unwrap();
        renderer.bind(dmabuf).unwrap();

        let (r, g, b) = hsv_to_rgb(self.animation_frame, 1.0, 1.0);
        self.animation_frame = (self.animation_frame + 0.5) % 360.0;

        // Disable damage tracking for now to draw cool clear animation
        let age = 0;

        let res = self
            .damage_tracked_renderer
            .render_output::<MemoryRenderBufferRenderElement<R>, _>(
                renderer,
                age as usize,
                &[],
                [r, g, b, 1.0],
            )
            .unwrap();

        self.gbm_surface
            .queue_buffer(None, res.damage, ())
            .map_err(|err| println!("Error: {err}"))
            .ok();
    }
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r_prime, g_prime, b_prime) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    (r_prime + m, g_prime + m, b_prime + m)
}
