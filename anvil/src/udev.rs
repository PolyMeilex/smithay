use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Error as IoError;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use glium::Surface;

use smithay::image::{ImageBuffer, Rgba};

use slog::Logger;

use smithay::drm::control::{Device as ControlDevice, ResourceInfo};
use smithay::drm::control::connector::{Info as ConnectorInfo, State as ConnectorState};
use smithay::drm::control::crtc;
use smithay::drm::control::encoder::Info as EncoderInfo;
use smithay::drm::result::Error as DrmError;
use smithay::backend::drm::{DevPath, DrmBackend, DrmDevice, DrmHandler};
use smithay::backend::graphics::GraphicsBackend;
use smithay::backend::graphics::egl::wayland::{EGLDisplay, EGLWaylandExtensions};
use smithay::backend::input::InputBackend;
use smithay::backend::libinput::{libinput_bind, LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::{Session, SessionNotifier};
use smithay::backend::session::auto::{auto_session_bind, AutoSession};
use smithay::backend::udev::{primary_gpu, udev_backend_bind, SessionFdDrmDevice, UdevBackend, UdevHandler};
use smithay::wayland::compositor::CompositorToken;
use smithay::wayland::output::{Mode, Output, PhysicalProperties};
use smithay::wayland::seat::Seat;
use smithay::wayland::shm::init_shm_global;
use smithay::wayland_server::{Display, EventLoop};
use smithay::wayland_server::commons::downcast_impl;
use smithay::wayland_server::protocol::wl_output;
use smithay::input::Libinput;

use glium_drawer::GliumDrawer;
use shell::{init_shell, MyWindowMap, Roles, SurfaceData};
use input_handler::AnvilInputHandler;

pub fn run_udev(mut display: Display, mut event_loop: EventLoop, log: Logger) -> Result<(), ()> {
    let name = display.add_socket_auto().unwrap().into_string().unwrap();
    info!(log, "Listening on wayland socket"; "name" => name.clone());
    ::std::env::set_var("WAYLAND_DISPLAY", name);

    let active_egl_context = Rc::new(RefCell::new(None));

    let display = Rc::new(RefCell::new(display));

    /*
     * Initialize the compositor
     */
    init_shm_global(
        &mut display.borrow_mut(),
        event_loop.token(),
        vec![],
        log.clone(),
    );

    let (compositor_token, _, _, window_map) = init_shell(
        &mut display.borrow_mut(),
        event_loop.token(),
        log.clone(),
        active_egl_context.clone(),
    );

    /*
     * Initialize session
     */
    let (session, mut notifier) = AutoSession::new(log.clone()).ok_or(())?;

    let running = Arc::new(AtomicBool::new(true));

    let pointer_location = Rc::new(RefCell::new((0.0, 0.0)));

    /*
     * Initialize the udev backend
     */
    let context = ::smithay::udev::Context::new().map_err(|_| ())?;
    let seat = session.seat();

    let primary_gpu = primary_gpu(&context, &seat).unwrap_or_default();

    let bytes = include_bytes!("../resources/cursor2.rgba");
    let mut udev_backend = UdevBackend::new(
        event_loop.token(),
        &context,
        session.clone(),
        UdevHandlerImpl {
            compositor_token,
            active_egl_context,
            backends: HashMap::new(),
            display: display.clone(),
            primary_gpu,
            window_map: window_map.clone(),
            pointer_location: pointer_location.clone(),
            pointer_image: ImageBuffer::from_raw(64, 64, bytes.to_vec()).unwrap(),
            logger: log.clone(),
        },
        log.clone(),
    ).map_err(|_| ())?;

    let udev_session_id = notifier.register(&mut udev_backend);

    let (mut w_seat, _) = Seat::new(
        &mut display.borrow_mut(),
        event_loop.token(),
        session.seat().into(),
        log.clone(),
    );

    let pointer = w_seat.add_pointer();
    let keyboard = w_seat
        .add_keyboard("", "", "", None, 1000, 500)
        .expect("Failed to initialize the keyboard");

    let (output, _output_global) = Output::new(
        &mut display.borrow_mut(),
        event_loop.token(),
        "Drm".into(),
        PhysicalProperties {
            width: 0,
            height: 0,
            subpixel: wl_output::Subpixel::Unknown,
            maker: "Smithay".into(),
            model: "Generic DRM".into(),
        },
        log.clone(),
    );

    let (w, h) = (1920, 1080); // Hardcode full-hd res
    output.change_current_state(
        Some(Mode {
            width: w as i32,
            height: h as i32,
            refresh: 60_000,
        }),
        None,
        None,
    );
    output.set_preferred(Mode {
        width: w as i32,
        height: h as i32,
        refresh: 60_000,
    });

    /*
     * Initialize libinput backend
     */
    let mut libinput_context =
        Libinput::new_from_udev::<LibinputSessionInterface<AutoSession>>(session.clone().into(), &context);
    let libinput_session_id = notifier.register(&mut libinput_context);
    libinput_context.udev_assign_seat(&seat).unwrap();
    let mut libinput_backend = LibinputInputBackend::new(libinput_context, log.clone());
    libinput_backend.set_handler(AnvilInputHandler::new_with_session(
        log.clone(),
        pointer,
        keyboard,
        window_map.clone(),
        (w, h),
        running.clone(),
        pointer_location,
        session,
    ));
    let libinput_event_source = libinput_bind(libinput_backend, event_loop.token())
        .map_err(|(err, _)| err)
        .unwrap();

    let session_event_source = auto_session_bind(notifier, &event_loop.token())
        .map_err(|(err, _)| err)
        .unwrap();
    let udev_event_source = udev_backend_bind(&event_loop.token(), udev_backend)
        .map_err(|(err, _)| err)
        .unwrap();

    while running.load(Ordering::SeqCst) {
        if let Err(_) = event_loop.dispatch(Some(16)) {
            running.store(false, Ordering::SeqCst);
        } else {
            display.borrow_mut().flush_clients();
            window_map.borrow_mut().refresh();
        }
    }

    let mut notifier = session_event_source.unbind();
    notifier.unregister(udev_session_id);
    notifier.unregister(libinput_session_id);

    libinput_event_source.remove();

    // destroy the udev backend freeing the drm devices
    //
    // udev_event_source.remove() returns a Box<Implementation<..>>, downcast_impl
    // allows us to cast it back to its original type, storing it back into its original
    // variable to simplify type inference.
    udev_backend = *(downcast_impl(udev_event_source.remove()).unwrap_or_else(|_| unreachable!()));
    udev_backend.close();
    Ok(())
}

struct UdevHandlerImpl {
    compositor_token: CompositorToken<SurfaceData, Roles>,
    active_egl_context: Rc<RefCell<Option<EGLDisplay>>>,
    backends: HashMap<u64, Rc<RefCell<HashMap<crtc::Handle, GliumDrawer<DrmBackend<SessionFdDrmDevice>>>>>>,
    display: Rc<RefCell<Display>>,
    primary_gpu: Option<PathBuf>,
    window_map: Rc<RefCell<MyWindowMap>>,
    pointer_location: Rc<RefCell<(f64, f64)>>,
    pointer_image: ImageBuffer<Rgba<u8>, Vec<u8>>,
    logger: ::slog::Logger,
}

impl UdevHandlerImpl {
    pub fn scan_connectors(
        &self,
        device: &mut DrmDevice<SessionFdDrmDevice>,
    ) -> HashMap<crtc::Handle, GliumDrawer<DrmBackend<SessionFdDrmDevice>>> {
        // Get a set of all modesetting resource handles (excluding planes):
        let res_handles = device.resource_handles().unwrap();

        // Use first connected connector
        let connector_infos: Vec<ConnectorInfo> = res_handles
            .connectors()
            .iter()
            .map(|conn| ConnectorInfo::load_from_device(device, *conn).unwrap())
            .filter(|conn| conn.connection_state() == ConnectorState::Connected)
            .inspect(|conn| info!(self.logger, "Connected: {:?}", conn.connector_type()))
            .collect();

        let mut backends = HashMap::new();

        // very naive way of finding good crtc/encoder/connector combinations. This problem is np-complete
        for connector_info in connector_infos {
            let encoder_infos = connector_info
                .encoders()
                .iter()
                .flat_map(|encoder_handle| EncoderInfo::load_from_device(device, *encoder_handle))
                .collect::<Vec<EncoderInfo>>();
            for encoder_info in encoder_infos {
                for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                    if !backends.contains_key(&crtc) {
                        let mode = connector_info.modes()[0]; // Use first mode (usually highest resoltion, but in reality you should filter and sort and check and match with other connectors, if you use more then one.)
                                                              // create a backend
                        let renderer = GliumDrawer::from(
                            device
                                .create_backend(crtc, mode, vec![connector_info.handle()])
                                .unwrap(),
                        );

                        // create cursor
                        renderer
                            .borrow()
                            .set_cursor_representation(&self.pointer_image, (2, 2))
                            .unwrap();

                        // render first frame
                        {
                            let mut frame = renderer.draw();
                            frame.clear_color(0.8, 0.8, 0.9, 1.0);
                            frame.finish().unwrap();
                        }

                        backends.insert(crtc, renderer);
                        break;
                    }
                }
            }
        }

        backends
    }
}

impl UdevHandler<DrmHandlerImpl> for UdevHandlerImpl {
    fn device_added(&mut self, device: &mut DrmDevice<SessionFdDrmDevice>) -> Option<DrmHandlerImpl> {
        // init hardware acceleration on the primary gpu.
        if device.dev_path().and_then(|path| path.canonicalize().ok()) == self.primary_gpu {
            *self.active_egl_context.borrow_mut() = device.bind_wl_display(&*self.display.borrow()).ok();
        }

        let backends = Rc::new(RefCell::new(self.scan_connectors(device)));
        self.backends.insert(device.device_id(), backends.clone());

        Some(DrmHandlerImpl {
            compositor_token: self.compositor_token.clone(),
            backends,
            window_map: self.window_map.clone(),
            pointer_location: self.pointer_location.clone(),
            logger: self.logger.clone(),
        })
    }

    fn device_changed(&mut self, device: &mut DrmDevice<SessionFdDrmDevice>) {
        //quick and dirt, just re-init all backends
        let backends = self.backends.get(&device.device_id()).unwrap();
        *backends.borrow_mut() = self.scan_connectors(device);
    }

    fn device_removed(&mut self, device: &mut DrmDevice<SessionFdDrmDevice>) {
        // drop the backends on this side
        self.backends.remove(&device.device_id());

        // don't use hardware acceleration anymore, if this was the primary gpu
        if device.dev_path().and_then(|path| path.canonicalize().ok()) == self.primary_gpu {
            *self.active_egl_context.borrow_mut() = None;
        }
    }

    fn error(&mut self, error: IoError) {
        error!(self.logger, "{:?}", error);
    }
}

pub struct DrmHandlerImpl {
    compositor_token: CompositorToken<SurfaceData, Roles>,
    backends: Rc<RefCell<HashMap<crtc::Handle, GliumDrawer<DrmBackend<SessionFdDrmDevice>>>>>,
    window_map: Rc<RefCell<MyWindowMap>>,
    pointer_location: Rc<RefCell<(f64, f64)>>,
    logger: ::slog::Logger,
}

impl DrmHandler<SessionFdDrmDevice> for DrmHandlerImpl {
    fn ready(
        &mut self,
        _device: &mut DrmDevice<SessionFdDrmDevice>,
        crtc: crtc::Handle,
        _frame: u32,
        _duration: Duration,
    ) {
        if let Some(drawer) = self.backends.borrow().get(&crtc) {
            {
                let (x, y) = *self.pointer_location.borrow();
                let _ = drawer
                    .borrow()
                    .set_cursor_position(x.trunc().abs() as u32, y.trunc().abs() as u32);
            }

            drawer.draw_windows(
                &*self.window_map.borrow(),
                self.compositor_token,
                &self.logger,
            );
        }
    }

    fn error(&mut self, _device: &mut DrmDevice<SessionFdDrmDevice>, error: DrmError) {
        error!(self.logger, "{:?}", error);
    }
}
