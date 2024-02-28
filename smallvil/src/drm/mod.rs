use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use smithay::{
    backend::{
        allocator::{gbm::GbmAllocator, Format, Fourcc},
        drm::{
            compositor::DrmCompositor, DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
        },
        egl::{EGLDevice, EGLDisplay},
        input::{InputEvent, KeyState, KeyboardKeyEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            damage::OutputDamageTracker,
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager, MultiRenderer},
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{self, UdevBackend, UdevEvent},
    },
    output::{Output, PhysicalProperties},
    reexports::{
        calloop::{Dispatcher, LoopHandle},
        drm::control::{connector, crtc, ModeTypeFlags},
        gbm::{BufferObjectFlags as GbmBufferFlags, Device as GbmDevice},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Transform},
};
use smithay_drm_extras::{
    drm_scanner::{DrmScanEvent, DrmScanner},
    edid::EdidInfo,
};

const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

type GbmBackend = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;
pub type Renderer<'a> = MultiRenderer<'a, 'a, GbmBackend, GbmBackend>;
pub type GbmDrmCompositor = DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmDevice<DrmDeviceFd>, (), DrmDeviceFd>;

struct OutputSurface {
    gbm_compositor: GbmDrmCompositor,
    output: Output,
    damage_tracker: OutputDamageTracker,
}

impl OutputSurface {
    pub fn new(
        crtc: crtc::Handle,
        connector: &connector::Info,
        renderer_formats: HashSet<Format>,
        drm: &mut DrmDevice,
        gbm: GbmDevice<DrmDeviceFd>,
    ) -> Self {
        let drm_mode = *connector
            .modes()
            .iter()
            .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or_else(|| &connector.modes()[0]);
        let wl_mode = smithay::output::Mode::from(drm_mode);

        let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());

        let (make, model) = EdidInfo::for_connector(drm, connector.handle())
            .map(|info| (info.manufacturer, info.model))
            .unwrap_or_else(|| ("Unknown".into(), "Unknown".into()));

        let (w, h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name,
            PhysicalProperties {
                size: (w as i32, h as i32).into(),
                subpixel: connector.subpixel().into(),
                make,
                model,
            },
        );

        output.set_preferred(wl_mode);
        output.change_current_state(
            Some(wl_mode),
            Some(Transform::Normal),
            Some(smithay::output::Scale::Integer(1)),
            None,
        );

        let damage_tracker = OutputDamageTracker::from_output(&output);

        let drm_surface = drm.create_surface(crtc, drm_mode, &[connector.handle()]).unwrap();
        let planes = drm_surface.planes().clone();

        let gbm_allocator =
            GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);

        let gbm_compositor = DrmCompositor::new(
            &output,
            drm_surface,
            Some(planes),
            gbm_allocator,
            gbm.clone(),
            SUPPORTED_FORMATS,
            renderer_formats,
            drm.cursor_size(),
            Some(gbm.clone()),
        )
        .unwrap();

        Self {
            gbm_compositor,
            output,
            damage_tracker,
        }
    }
}

struct Device {
    drm: DrmDevice,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, OutputSurface>,
    render_node: DrmNode,
    // NOTE: This is not very rusty but gbm device has to be dropped last, after OutputSurface (#1102)
    gbm: GbmDevice<DrmDeviceFd>,
}

pub struct DrmState<D> {
    session: LibSeatSession,
    input: Libinput,
    devices: HashMap<DrmNode, Device>,
    primary_gpu: DrmNode,
    event_loop: LoopHandle<'static, D>,
    gpu_manager: GpuManager<GbmBackend>,
    udev: Dispatcher<'static, UdevBackend, D>,
}

pub struct DrmRenderRequest<'a> {
    pub gbm_compositor: &'a mut GbmDrmCompositor,
    pub output: &'a Output,
    pub renderer: &'a mut Renderer<'a>,
    pub damage_tracker: &'a mut OutputDamageTracker,
}

pub trait DrmBackend: Sized {
    fn drm_state(&self) -> &DrmState<Self>;
    fn drm_state_mut(&mut self) -> &mut DrmState<Self>;

    fn on_input_event(&mut self, event: InputEvent<LibinputInputBackend>);
    fn on_output_added(&mut self, output: &Output);
    fn on_output_removed(&mut self, output: &Output);

    fn render_fn(&mut self) -> (&mut DrmState<Self>, impl FnOnce(DrmRenderRequest) -> bool);
}

impl<D: DrmBackend> DrmBackendImpl for D {}
trait DrmBackendImpl: DrmBackend {
    fn on_session_event(&mut self, event: SessionEvent) {
        let state = self.drm_state_mut();
        match event {
            SessionEvent::PauseSession => {
                state.input.suspend();

                for device in state.devices.values_mut() {
                    device.drm.pause();
                }
            }
            SessionEvent::ActivateSession => {
                state.input.resume().unwrap();

                let mut rerender = Vec::new();
                for (node, device) in state.devices.iter_mut() {
                    device.drm.activate(false).unwrap();

                    for (crtc, surface) in device.surfaces.iter_mut() {
                        surface.gbm_compositor.reset_state().unwrap();
                        rerender.push((*node, *crtc));
                    }
                }

                for (node, crtc) in rerender {
                    self.render(node, crtc);
                }
            }
        }
    }

    fn render(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let (state, render_fn) = self.render_fn();
        let Some(device) = state.devices.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        let mut renderer = if state.primary_gpu == device.render_node {
            state.gpu_manager.single_renderer(&device.render_node).unwrap()
        } else {
            state
                .gpu_manager
                .renderer(
                    &state.primary_gpu,
                    &device.render_node,
                    surface.gbm_compositor.format(),
                )
                .unwrap()
        };

        let rendered = render_fn(DrmRenderRequest {
            gbm_compositor: &mut surface.gbm_compositor,
            output: &surface.output,
            renderer: &mut renderer,
            damage_tracker: &mut surface.damage_tracker,
        });

        if rendered {
            surface.gbm_compositor.queue_frame(()).unwrap();
        } else {
            state.event_loop.insert_idle(move |state| {
                state.render(node, crtc);
            });
        }
    }

    fn on_drm_event(&mut self, node: DrmNode, event: DrmEvent, _meta: &mut Option<DrmEventMetadata>) {
        match event {
            DrmEvent::VBlank(crtc) => {
                let state = self.drm_state_mut();
                let Some(device) = state.devices.get_mut(&node) else {
                    return;
                };
                let Some(surface) = device.surfaces.get_mut(&crtc) else {
                    return;
                };

                surface.gbm_compositor.frame_submitted().unwrap();

                self.render(node, crtc);
            }
            DrmEvent::Error(_) => todo!(),
        }
    }

    fn on_udev_event(&mut self, event: UdevEvent) {
        match event {
            UdevEvent::Added { device_id, path } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    self.on_device_added(node, path);
                }
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    self.on_device_changed(node);
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    self.on_device_removed(node);
                }
            }
        }
    }

    fn on_device_added(&mut self, node: DrmNode, path: PathBuf) {
        let state = self.drm_state_mut();

        let fd = state
            .session
            .open(
                &path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .unwrap();

        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, drm_notifier) = DrmDevice::new(fd, false).unwrap();

        let gbm = GbmDevice::new(drm.device_fd().clone()).unwrap();

        // Make sure display is dropped before we call add_node
        let render_node =
            match EGLDevice::device_for_display(&unsafe { EGLDisplay::new(gbm.clone()).unwrap() })
                .ok()
                .and_then(|x| x.try_get_render_node().ok().flatten())
            {
                Some(node) => node,
                None => node,
            };

        state
            .gpu_manager
            .as_mut()
            .add_node(render_node, gbm.clone())
            .unwrap();

        state
            .event_loop
            .insert_source(drm_notifier, move |event, meta, state| {
                state.on_drm_event(node, event, meta);
            })
            .unwrap();

        state.devices.insert(
            node,
            Device {
                drm,
                gbm,

                drm_scanner: Default::default(),

                surfaces: Default::default(),
                render_node,
            },
        );

        self.on_device_changed(node);
    }

    fn on_device_changed(&mut self, node: DrmNode) {
        let state = self.drm_state_mut();
        if let Some(device) = state.devices.get_mut(&node) {
            for event in device.drm_scanner.scan_connectors(&device.drm) {
                self.on_connector_event(node, event);
            }
        }
    }

    fn on_device_removed(&mut self, node: DrmNode) {
        let state = self.drm_state_mut();
        if let Some(device) = state.devices.get_mut(&node) {
            state.gpu_manager.as_mut().remove_node(&device.render_node);
        }
    }

    fn on_connector_event(&mut self, node: DrmNode, event: DrmScanEvent) {
        let state = self.drm_state_mut();
        let Some(device) = state.devices.get_mut(&node) else {
            return;
        };

        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => {
                let mut renderer = state.gpu_manager.single_renderer(&device.render_node).unwrap();

                let surface = OutputSurface::new(
                    crtc,
                    &connector,
                    renderer.as_mut().egl_context().dmabuf_render_formats().clone(),
                    &mut device.drm,
                    device.gbm.clone(),
                );

                let output = surface.output.clone();
                device.surfaces.insert(crtc, surface);

                // Kick off rendering loop
                self.render(node, crtc);
                self.on_output_added(&output);
            }
            DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => {
                if let Some(surface) = device.surfaces.remove(&crtc) {
                    self.on_output_removed(&surface.output);
                }
            }
            _ => {}
        }
    }
}

fn init_session<D: DrmBackend>(event_loop: &LoopHandle<D>) -> LibSeatSession {
    let (session, event_source) = LibSeatSession::new().unwrap();

    event_loop
        .insert_source(event_source, |event, _, state| state.on_session_event(event))
        .unwrap();

    session
}

fn init_input<D: DrmBackend>(event_loop: &LoopHandle<D>, session: &LibSeatSession) -> Libinput {
    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());

    libinput.udev_assign_seat(&session.seat()).unwrap();

    let event_source = LibinputInputBackend::new(libinput.clone());
    let mut ctrl_pressed = false;
    let mut alt_pressed = false;
    event_loop
        .insert_source(event_source, move |event, _, state| {
            const KEY_F1: u32 = 59;
            const KEY_F10: u32 = 68;
            const KEY_LEFTCTRL: u32 = 29;
            const KEY_LEFTALT: u32 = 56;

            if let InputEvent::Keyboard { event } = &event {
                let pressed = match event.state() {
                    KeyState::Released => false,
                    KeyState::Pressed => true,
                };
                let key_code = event.key_code();

                match key_code {
                    KEY_LEFTCTRL => ctrl_pressed = pressed,
                    KEY_LEFTALT => alt_pressed = pressed,
                    _ => {}
                }

                if let KEY_F1..=KEY_F10 = event.key_code() {
                    if pressed && ctrl_pressed && alt_pressed {
                        let vt = key_code - KEY_F1 + 1;
                        state.drm_state_mut().session.change_vt(vt as i32).ok();
                        return;
                    }
                }
            }

            state.on_input_event(event)
        })
        .unwrap();

    libinput
}

fn init_udev<D: DrmBackend>(
    event_loop: &LoopHandle<'static, D>,
    session: &LibSeatSession,
) -> Dispatcher<'static, UdevBackend, D> {
    let udev = UdevBackend::new(session.seat()).unwrap();
    let udev = Dispatcher::new(udev, |event, _, state: &mut D| state.on_udev_event(event));
    event_loop.register_dispatcher(udev.clone()).unwrap();
    udev
}

pub fn init<D: DrmBackend>(event_loop: LoopHandle<'static, D>) -> DrmState<D> {
    let session = init_session(&event_loop);
    let libinput = init_input(&event_loop, &session);
    let primary_gpu = primary_gpu(&session.seat()).0;
    let udev = init_udev(&event_loop, &session);

    DrmState {
        event_loop,
        session,
        input: libinput,
        devices: HashMap::new(),
        primary_gpu,
        gpu_manager: GpuManager::new(Default::default()).unwrap(),
        udev,
    }
}

pub fn start<D: DrmBackend>(state: &mut D) {
    let devices: Vec<_> = state
        .drm_state_mut()
        .udev
        .as_source_ref()
        .device_list()
        .map(|(id, path)| (id, path.to_owned()))
        .collect();

    for (device_id, path) in devices {
        state.on_udev_event(UdevEvent::Added {
            device_id,
            path: path.to_owned(),
        });
    }
}

pub fn primary_gpu(seat: &str) -> (DrmNode, PathBuf) {
    // TODO: can't this be in smithay?
    // primary_gpu() does the same thing anyway just without `NodeType::Render` check
    // so perhaps `primary_gpu(seat, node_type)`?
    udev::primary_gpu(seat)
        .unwrap()
        .and_then(|p| {
            DrmNode::from_path(&p)
                .ok()?
                .node_with_type(NodeType::Render)?
                .ok()
                .map(|node| (node, p))
        })
        .unwrap_or_else(|| {
            udev::all_gpus(seat)
                .unwrap()
                .into_iter()
                .find_map(|p| {
                    DrmNode::from_path(&p)
                        .ok()?
                        .node_with_type(NodeType::Render)?
                        .ok()
                        .map(|node| (node, p))
                })
                .expect("No GPU!")
        })
}
