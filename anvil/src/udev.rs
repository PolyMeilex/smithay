use std::{
    cell::RefCell,
    collections::hash_map::{Entry, HashMap},
    io::Error as IoError,
    os::unix::io::{AsRawFd, RawFd},
    path::PathBuf,
    rc::Rc,
    sync::atomic::Ordering,
    time::Duration,
};

use image::{ImageBuffer, Rgba};
use slog::Logger;

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        drm::{DrmDevice, DrmError, DrmEvent, GbmBufferedSurface},
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            gles2::{Gles2Renderer, Gles2Texture},
            Bind, Frame, Renderer, Transform,
        },
        session::{auto::AutoSession, Session, Signal as SessionSignal},
        udev::{UdevBackend, UdevEvent},
        SwapBuffersError,
    },
    reexports::{
        calloop::{
            timer::{Timer, TimerHandle},
            Dispatcher, EventLoop, LoopHandle, RegistrationToken,
        },
        drm::{
            self,
            control::{
                connector::{Info as ConnectorInfo, State as ConnectorState},
                crtc,
                encoder::Info as EncoderInfo,
                Device as ControlDevice,
            },
        },
        gbm::Device as GbmDevice,
        input::Libinput,
        nix::{fcntl::OFlag, sys::stat::dev_t},
        wayland_server::{
            protocol::{wl_output, wl_surface},
            Display,
        },
    },
    utils::signaling::{Linkable, SignalToken, Signaler},
    wayland::{
        output::{Mode, PhysicalProperties},
        seat::CursorImageStatus,
    },
};
#[cfg(feature = "egl")]
use smithay::{
    backend::{
        drm::DevPath,
        renderer::{ImportDma, ImportEgl},
        udev::primary_gpu,
    },
    wayland::dmabuf::init_dmabuf_global,
};

use crate::state::{AnvilState, Backend};
use crate::{drawing::*, window_map::WindowMap};

#[derive(Clone)]
pub struct SessionFd(RawFd);
impl AsRawFd for SessionFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

struct UdevOutputMap {
    pub device_id: dev_t,
    pub crtc: crtc::Handle,
    pub output_name: String,
}

pub struct UdevData {
    output_map: Vec<UdevOutputMap>,
    pub session: AutoSession,
    #[cfg(feature = "egl")]
    primary_gpu: Option<PathBuf>,
    backends: HashMap<dev_t, BackendData>,
    signaler: Signaler<SessionSignal>,
    pointer_image: ImageBuffer<Rgba<u8>, Vec<u8>>,
    render_timer: TimerHandle<(u64, crtc::Handle)>,
}

impl Backend for UdevData {
    fn seat_name(&self) -> String {
        self.session.seat()
    }
}

pub fn run_udev(
    display: Rc<RefCell<Display>>,
    event_loop: &mut EventLoop<'static, AnvilState<UdevData>>,
    log: Logger,
) -> Result<(), ()> {
    let name = display
        .borrow_mut()
        .add_socket_auto()
        .unwrap()
        .into_string()
        .unwrap();
    info!(log, "Listening on wayland socket"; "name" => name.clone());
    ::std::env::set_var("WAYLAND_DISPLAY", name);
    /*
     * Initialize session
     */
    let (session, notifier) = AutoSession::new(log.clone()).ok_or(())?;
    let session_signal = notifier.signaler();

    /*
     * Initialize the compositor
     */
    let pointer_bytes = include_bytes!("../resources/cursor2.rgba");
    #[cfg(feature = "egl")]
    let primary_gpu = primary_gpu(&session.seat()).unwrap_or_default();

    // setup the timer
    let timer = Timer::new().unwrap();

    let data = UdevData {
        session,
        output_map: Vec::new(),
        #[cfg(feature = "egl")]
        primary_gpu,
        backends: HashMap::new(),
        signaler: session_signal.clone(),
        pointer_image: ImageBuffer::from_raw(64, 64, pointer_bytes.to_vec()).unwrap(),
        render_timer: timer.handle(),
    };
    let mut state = AnvilState::init(display.clone(), event_loop.handle(), data, log.clone());

    // re-render timer
    event_loop
        .handle()
        .insert_source(timer, |(dev_id, crtc), _, anvil_state| {
            anvil_state.render(dev_id, Some(crtc))
        })
        .unwrap();

    /*
     * Initialize the udev backend
     */
    let udev_backend = UdevBackend::new(state.seat_name.clone(), log.clone()).map_err(|_| ())?;

    /*
     * Initialize a fake output (we render one screen to every device in this example)
     */

    /*
     * Initialize libinput backend
     */
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<AutoSession>>(
        state.backend_data.session.clone().into(),
    );
    libinput_context.udev_assign_seat(&state.seat_name).unwrap();
    let mut libinput_backend = LibinputInputBackend::new(libinput_context, log.clone());
    libinput_backend.link(session_signal);

    /*
     * Bind all our objects that get driven by the event loop
     */
    let libinput_event_source = event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, anvil_state| {
            anvil_state.process_input_event(event)
        })
        .unwrap();
    let session_event_source = event_loop
        .handle()
        .insert_source(notifier, |(), &mut (), _anvil_state| {})
        .unwrap();
    for (dev, path) in udev_backend.device_list() {
        state.device_added(dev, path.into())
    }

    // init dmabuf support with format list from all gpus
    // TODO: We need to update this list, when the set of gpus changes
    // TODO2: This does not necessarily depend on egl, but mesa makes no use of it without wl_drm right now
    #[cfg(feature = "egl")]
    {
        let mut formats = Vec::new();
        for backend_data in state.backend_data.backends.values() {
            formats.extend(backend_data.renderer.borrow().dmabuf_formats().cloned());
        }

        init_dmabuf_global(
            &mut *display.borrow_mut(),
            formats,
            |buffer, mut ddata| {
                let anvil_state = ddata.get::<AnvilState<UdevData>>().unwrap();
                for backend_data in anvil_state.backend_data.backends.values() {
                    if backend_data.renderer.borrow_mut().import_dmabuf(buffer).is_ok() {
                        return true;
                    }
                }
                false
            },
            log.clone(),
        );
    }

    let udev_event_source = event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Added { device_id, path } => state.device_added(device_id, path),
            UdevEvent::Changed { device_id } => state.device_changed(device_id),
            UdevEvent::Removed { device_id } => state.device_removed(device_id),
        })
        .map_err(|e| -> IoError { e.into() })
        .unwrap();

    /*
     * Start XWayland if supported
     */
    #[cfg(feature = "xwayland")]
    state.start_xwayland();

    /*
     * And run our loop
     */

    while state.running.load(Ordering::SeqCst) {
        if event_loop
            .dispatch(Some(Duration::from_millis(16)), &mut state)
            .is_err()
        {
            state.running.store(false, Ordering::SeqCst);
        } else {
            display.borrow_mut().flush_clients(&mut state);
            state.window_map.borrow_mut().refresh();
            state.output_map.borrow_mut().refresh();
        }
    }

    // Cleanup stuff
    state.window_map.borrow_mut().clear();

    event_loop.handle().remove(session_event_source);
    event_loop.handle().remove(libinput_event_source);
    event_loop.handle().remove(udev_event_source);

    Ok(())
}

pub type RenderSurface = GbmBufferedSurface<SessionFd>;

struct BackendData {
    _restart_token: SignalToken,
    surfaces: Rc<RefCell<HashMap<crtc::Handle, Rc<RefCell<RenderSurface>>>>>,
    pointer_image: Gles2Texture,
    renderer: Rc<RefCell<Gles2Renderer>>,
    gbm: GbmDevice<SessionFd>,
    registration_token: RegistrationToken,
    event_dispatcher: Dispatcher<'static, DrmDevice<SessionFd>, AnvilState<UdevData>>,
    dev_id: u64,
}

fn scan_connectors(
    device: &mut DrmDevice<SessionFd>,
    gbm: &GbmDevice<SessionFd>,
    renderer: &mut Gles2Renderer,
    backend_output_map: &mut Vec<UdevOutputMap>,
    output_map: &mut crate::output_map::OutputMap,
    signaler: &Signaler<SessionSignal>,
    logger: &::slog::Logger,
) -> HashMap<crtc::Handle, Rc<RefCell<RenderSurface>>> {
    // Get a set of all modesetting resource handles (excluding planes):
    let res_handles = device.resource_handles().unwrap();

    // Use first connected connector
    let connector_infos: Vec<ConnectorInfo> = res_handles
        .connectors()
        .iter()
        .map(|conn| device.get_connector(*conn).unwrap())
        .filter(|conn| conn.state() == ConnectorState::Connected)
        .inspect(|conn| info!(logger, "Connected: {:?}", conn.interface()))
        .collect();

    let mut backends = HashMap::new();

    // very naive way of finding good crtc/encoder/connector combinations. This problem is np-complete
    for connector_info in connector_infos {
        let encoder_infos = connector_info
            .encoders()
            .iter()
            .filter_map(|e| *e)
            .flat_map(|encoder_handle| device.get_encoder(encoder_handle))
            .collect::<Vec<EncoderInfo>>();
        'outer: for encoder_info in encoder_infos {
            for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                if let Entry::Vacant(entry) = backends.entry(crtc) {
                    info!(
                        logger,
                        "Trying to setup connector {:?}-{} with crtc {:?}",
                        connector_info.interface(),
                        connector_info.interface_id(),
                        crtc,
                    );
                    let mut surface = match device.create_surface(
                        crtc,
                        connector_info.modes()[0],
                        &[connector_info.handle()],
                    ) {
                        Ok(surface) => surface,
                        Err(err) => {
                            warn!(logger, "Failed to create drm surface: {}", err);
                            continue;
                        }
                    };
                    surface.link(signaler.clone());

                    let renderer_formats =
                        Bind::<Dmabuf>::supported_formats(renderer).expect("Dmabuf renderer without formats");

                    let renderer =
                        match GbmBufferedSurface::new(surface, gbm.clone(), renderer_formats, logger.clone())
                        {
                            Ok(renderer) => renderer,
                            Err(err) => {
                                warn!(logger, "Failed to create rendering surface: {}", err);
                                continue;
                            }
                        };

                    let mode = connector_info.modes()[0];
                    let size = mode.size();
                    let mode = Mode {
                        width: size.0 as i32,
                        height: size.1 as i32,
                        refresh: (mode.vrefresh() * 1000) as i32,
                    };

                    let output_name = format!(
                        "{:?}-{}",
                        connector_info.interface(),
                        connector_info.interface_id()
                    );

                    output_map.add(
                        &output_name,
                        PhysicalProperties {
                            width: connector_info.size().unwrap_or((0, 0)).0 as i32,
                            height: connector_info.size().unwrap_or((0, 0)).1 as i32,
                            subpixel: wl_output::Subpixel::Unknown,
                            make: "Smithay".into(),
                            model: "Generic DRM".into(),
                        },
                        mode,
                    );

                    backend_output_map.push(UdevOutputMap {
                        crtc,
                        device_id: device.device_id(),
                        output_name,
                    });

                    entry.insert(Rc::new(RefCell::new(renderer)));
                    break 'outer;
                }
            }
        }
    }

    backends
}

impl AnvilState<UdevData> {
    fn device_added(&mut self, device_id: dev_t, path: PathBuf) {
        // Try to open the device
        if let Some((mut device, gbm)) = self
            .backend_data
            .session
            .open(
                &path,
                OFlag::O_RDWR | OFlag::O_CLOEXEC | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            )
            .ok()
            .and_then(|fd| {
                match {
                    let fd = SessionFd(fd);
                    (
                        DrmDevice::new(fd.clone(), true, self.log.clone()),
                        GbmDevice::new(fd),
                    )
                } {
                    (Ok(drm), Ok(gbm)) => Some((drm, gbm)),
                    (Err(err), _) => {
                        warn!(
                            self.log,
                            "Skipping device {:?}, because of drm error: {}", device_id, err
                        );
                        None
                    }
                    (_, Err(err)) => {
                        // TODO try DumbBuffer allocator in this case
                        warn!(
                            self.log,
                            "Skipping device {:?}, because of gbm error: {}", device_id, err
                        );
                        None
                    }
                }
            })
        {
            let egl = match EGLDisplay::new(&gbm, self.log.clone()) {
                Ok(display) => display,
                Err(err) => {
                    warn!(
                        self.log,
                        "Skipping device {:?}, because of egl display error: {}", device_id, err
                    );
                    return;
                }
            };

            let context = match EGLContext::new(&egl, self.log.clone()) {
                Ok(context) => context,
                Err(err) => {
                    warn!(
                        self.log,
                        "Skipping device {:?}, because of egl context error: {}", device_id, err
                    );
                    return;
                }
            };

            let renderer = Rc::new(RefCell::new(unsafe {
                Gles2Renderer::new(context, self.log.clone()).unwrap()
            }));

            #[cfg(feature = "egl")]
            if path.canonicalize().ok() == self.backend_data.primary_gpu {
                info!(self.log, "Initializing EGL Hardware Acceleration via {:?}", path);
                renderer
                    .borrow_mut()
                    .bind_wl_display(&*self.display.borrow())
                    .expect("Unable to bind Wl Display?");
            }

            let backends = Rc::new(RefCell::new(scan_connectors(
                &mut device,
                &gbm,
                &mut *renderer.borrow_mut(),
                &mut self.backend_data.output_map,
                &mut *self.output_map.borrow_mut(),
                &self.backend_data.signaler,
                &self.log,
            )));

            let pointer_image = renderer
                .borrow_mut()
                .import_bitmap(&self.backend_data.pointer_image)
                .expect("Failed to load pointer");

            let dev_id = device.device_id();
            let handle = self.handle.clone();
            let restart_token = self.backend_data.signaler.register(move |signal| match signal {
                SessionSignal::ActivateSession | SessionSignal::ActivateDevice { .. } => {
                    handle.insert_idle(move |anvil_state| anvil_state.render(dev_id, None));
                }
                _ => {}
            });

            device.link(self.backend_data.signaler.clone());
            let event_dispatcher = Dispatcher::new(
                device,
                move |event, _, anvil_state: &mut AnvilState<_>| match event {
                    DrmEvent::VBlank(crtc) => anvil_state.render(dev_id, Some(crtc)),
                    DrmEvent::Error(error) => {
                        error!(anvil_state.log, "{:?}", error);
                    }
                },
            );
            let registration_token = self.handle.register_dispatcher(event_dispatcher.clone()).unwrap();

            trace!(self.log, "Backends: {:?}", backends.borrow().keys());
            for backend in backends.borrow_mut().values() {
                // render first frame
                trace!(self.log, "Scheduling frame");
                schedule_initial_render(backend.clone(), renderer.clone(), &self.handle, self.log.clone());
            }

            self.backend_data.backends.insert(
                dev_id,
                BackendData {
                    _restart_token: restart_token,
                    registration_token,
                    event_dispatcher,
                    surfaces: backends,
                    renderer,
                    gbm,
                    pointer_image,
                    dev_id,
                },
            );
        }
    }

    fn device_changed(&mut self, device: dev_t) {
        //quick and dirty, just re-init all backends
        if let Some(ref mut backend_data) = self.backend_data.backends.get_mut(&device) {
            let logger = self.log.clone();
            let loop_handle = self.handle.clone();
            let signaler = self.backend_data.signaler.clone();
            let removed_outputs = self
                .backend_data
                .output_map
                .iter()
                .filter(|o| o.device_id == device)
                .map(|o| o.output_name.as_str());

            for output in removed_outputs {
                self.output_map.borrow_mut().remove(output);
            }

            self.backend_data
                .output_map
                .retain(|output| output.device_id != device);

            let mut source = backend_data.event_dispatcher.as_source_mut();
            let mut backends = backend_data.surfaces.borrow_mut();
            *backends = scan_connectors(
                &mut *source,
                &backend_data.gbm,
                &mut *backend_data.renderer.borrow_mut(),
                &mut self.backend_data.output_map,
                &mut *self.output_map.borrow_mut(),
                &signaler,
                &logger,
            );

            for renderer in backends.values() {
                let logger = logger.clone();
                // render first frame
                schedule_initial_render(
                    renderer.clone(),
                    backend_data.renderer.clone(),
                    &loop_handle,
                    logger,
                );
            }
        }
    }

    fn device_removed(&mut self, device: dev_t) {
        // drop the backends on this side
        if let Some(backend_data) = self.backend_data.backends.remove(&device) {
            // drop surfaces
            backend_data.surfaces.borrow_mut().clear();
            debug!(self.log, "Surfaces dropped");
            let removed_outputs = self
                .backend_data
                .output_map
                .iter()
                .filter(|o| o.device_id == device)
                .map(|o| o.output_name.as_str());
            for output_id in removed_outputs {
                self.output_map.borrow_mut().remove(output_id);
            }
            self.backend_data.output_map.retain(|o| o.device_id != device);

            let _device = self.handle.remove(backend_data.registration_token);
            let _device = backend_data.event_dispatcher.into_source_inner();

            // don't use hardware acceleration anymore, if this was the primary gpu
            #[cfg(feature = "egl")]
            if _device.dev_path().and_then(|path| path.canonicalize().ok()) == self.backend_data.primary_gpu {
                backend_data.renderer.borrow_mut().unbind_wl_display();
            }
            debug!(self.log, "Dropping device");
        }
    }

    // If crtc is `Some()`, render it, else render all crtcs
    fn render(&mut self, dev_id: u64, crtc: Option<crtc::Handle>) {
        let device_backend = match self.backend_data.backends.get_mut(&dev_id) {
            Some(backend) => backend,
            None => {
                error!(self.log, "Trying to render on non-existent backend {}", dev_id);
                return;
            }
        };
        // setup two iterators on the stack, one over all surfaces for this backend, and
        // one containing only the one given as argument.
        // They make a trait-object to dynamically choose between the two
        let surfaces = device_backend.surfaces.borrow();
        let mut surfaces_iter = surfaces.iter();
        let mut option_iter = crtc
            .iter()
            .flat_map(|crtc| surfaces.get(&crtc).map(|surface| (crtc, surface)));

        let to_render_iter: &mut dyn Iterator<Item = (&crtc::Handle, &Rc<RefCell<RenderSurface>>)> =
            if crtc.is_some() {
                &mut option_iter
            } else {
                &mut surfaces_iter
            };

        for (&crtc, surface) in to_render_iter {
            let result = render_surface(
                &mut *surface.borrow_mut(),
                &mut *device_backend.renderer.borrow_mut(),
                device_backend.dev_id,
                crtc,
                &mut *self.window_map.borrow_mut(),
                &self.backend_data.output_map,
                &*self.output_map.borrow(),
                &self.pointer_location,
                &device_backend.pointer_image,
                &*self.dnd_icon.lock().unwrap(),
                &mut *self.cursor_status.lock().unwrap(),
                &self.log,
            );
            if let Err(err) = result {
                warn!(self.log, "Error during rendering: {:?}", err);
                let reschedule = match err {
                    SwapBuffersError::AlreadySwapped => false,
                    SwapBuffersError::TemporaryFailure(err) => !matches!(
                        err.downcast_ref::<DrmError>(),
                        Some(&DrmError::DeviceInactive)
                            | Some(&DrmError::Access {
                                source: drm::SystemError::PermissionDenied,
                                ..
                            })
                    ),
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {}", err),
                };

                if reschedule {
                    debug!(self.log, "Rescheduling");
                    self.backend_data.render_timer.add_timeout(
                        Duration::from_millis(1000 /*a seconds*/ / 60 /*refresh rate*/),
                        (device_backend.dev_id, crtc),
                    );
                }
            } else {
                // TODO: only send drawn windows the frames callback
                // Send frame events so that client start drawing their next frame
                self.window_map
                    .borrow()
                    .send_frames(self.start_time.elapsed().as_millis() as u32);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_surface(
    surface: &mut RenderSurface,
    renderer: &mut Gles2Renderer,
    device_id: dev_t,
    crtc: crtc::Handle,
    window_map: &mut WindowMap,
    backend_output_map: &[UdevOutputMap],
    output_map: &crate::output_map::OutputMap,
    pointer_location: &(f64, f64),
    pointer_image: &Gles2Texture,
    dnd_icon: &Option<wl_surface::WlSurface>,
    cursor_status: &mut CursorImageStatus,
    logger: &slog::Logger,
) -> Result<(), SwapBuffersError> {
    surface.frame_submitted()?;

    let output_geometry = backend_output_map
        .iter()
        .find(|o| o.device_id == device_id && o.crtc == crtc)
        .map(|o| o.output_name.as_str())
        .and_then(|name| output_map.find_by_name(name, |_, geometry| geometry).ok());

    let output_geometry = if let Some(geometry) = output_geometry {
        geometry
    } else {
        // Somehow we got called with a non existing output
        return Ok(());
    };

    let dmabuf = surface.next_buffer()?;
    renderer.bind(dmabuf)?;
    // and draw to our buffer
    match renderer
        .render(
            output_geometry.width as u32,
            output_geometry.height as u32,
            Transform::Flipped180, // Scanout is rotated
            |renderer, frame| {
                frame.clear([0.8, 0.8, 0.9, 1.0])?;
                // draw the surfaces
                draw_windows(renderer, frame, window_map, output_geometry, logger)?;

                // get pointer coordinates
                let (ptr_x, ptr_y) = *pointer_location;
                let ptr_x = ptr_x.trunc().abs() as i32 - output_geometry.x;
                let ptr_y = ptr_y.trunc().abs() as i32 - output_geometry.y;

                // set cursor
                if ptr_x >= 0 && ptr_x < output_geometry.width && ptr_y >= 0 && ptr_y < output_geometry.height
                {
                    // draw the dnd icon if applicable
                    {
                        if let Some(ref wl_surface) = dnd_icon.as_ref() {
                            if wl_surface.as_ref().is_alive() {
                                draw_dnd_icon(renderer, frame, wl_surface, (ptr_x, ptr_y), logger)?;
                            }
                        }
                    }
                    // draw the cursor as relevant
                    {
                        // reset the cursor if the surface is no longer alive
                        let mut reset = false;
                        if let CursorImageStatus::Image(ref surface) = *cursor_status {
                            reset = !surface.as_ref().is_alive();
                        }
                        if reset {
                            *cursor_status = CursorImageStatus::Default;
                        }

                        if let CursorImageStatus::Image(ref wl_surface) = *cursor_status {
                            draw_cursor(renderer, frame, wl_surface, (ptr_x, ptr_y), logger)?;
                        } else {
                            frame.render_texture_at(pointer_image, (ptr_x, ptr_y), Transform::Normal, 1.0)?;
                        }
                    }
                }
                Ok(())
            },
        )
        .map_err(Into::<SwapBuffersError>::into)
        .and_then(|x| x)
        .map_err(Into::<SwapBuffersError>::into)
    {
        Ok(()) => surface.queue_buffer().map_err(Into::<SwapBuffersError>::into),
        Err(err) => Err(err),
    }
}

fn schedule_initial_render<Data: 'static>(
    surface: Rc<RefCell<RenderSurface>>,
    renderer: Rc<RefCell<Gles2Renderer>>,
    evt_handle: &LoopHandle<'static, Data>,
    logger: ::slog::Logger,
) {
    let result = {
        let mut surface = surface.borrow_mut();
        let mut renderer = renderer.borrow_mut();
        initial_render(&mut *surface, &mut *renderer)
    };
    if let Err(err) = result {
        match err {
            SwapBuffersError::AlreadySwapped => {}
            SwapBuffersError::TemporaryFailure(err) => {
                // TODO dont reschedule after 3(?) retries
                warn!(logger, "Failed to submit page_flip: {}", err);
                let handle = evt_handle.clone();
                evt_handle.insert_idle(move |_| schedule_initial_render(surface, renderer, &handle, logger));
            }
            SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {}", err),
        }
    }
}

fn initial_render(surface: &mut RenderSurface, renderer: &mut Gles2Renderer) -> Result<(), SwapBuffersError> {
    let dmabuf = surface.next_buffer()?;
    renderer.bind(dmabuf)?;
    // Does not matter if we render an empty frame
    renderer
        .render(1, 1, Transform::Normal, |_, frame| {
            frame
                .clear([0.8, 0.8, 0.9, 1.0])
                .map_err(Into::<SwapBuffersError>::into)
        })
        .map_err(Into::<SwapBuffersError>::into)
        .and_then(|x| x.map_err(Into::<SwapBuffersError>::into))?;
    surface.queue_buffer()?;
    Ok(())
}
