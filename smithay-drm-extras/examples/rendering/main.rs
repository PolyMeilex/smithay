use std::{collections::HashMap, path::PathBuf, time::Duration};

use smithay_drm_extras::drm_scanner;

use smithay::{
    backend::{
        allocator::{
            dmabuf::DmabufAllocator,
            gbm::{self, GbmAllocator},
        },
        drm::{self, DrmDeviceFd, DrmNode, NodeType},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager},
        },
        session::{libseat::LibSeatSession, Session},
        udev::{self, UdevBackend, UdevEvent},
    },
    reexports::{
        calloop::{timer::Timer, EventLoop, LoopHandle},
        drm::control::crtc,
        input::Libinput,
    },
};

mod surface;
use surface::OutputSurface;

mod handlers;

struct Device {
    drm: drm::DrmDevice,
    gbm: gbm::GbmDevice<DrmDeviceFd>,
    gbm_allocator: DmabufAllocator<GbmAllocator<DrmDeviceFd>>,

    drm_scanner: drm_scanner::DrmScanner,

    surfaces: HashMap<crtc::Handle, OutputSurface>,
    render_node: DrmNode,
    // NOTE: This is not very rusty but gbm device has to be dropped last, before OutputSurface,
    // otherwise we will get segfault
    // gbm: gbm::GbmDevice<DrmDeviceFd>,
}

struct State {
    handle: LoopHandle<'static, Self>,
    session: LibSeatSession,
    primary_gpu: DrmNode,
    gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer>>,
    devices: HashMap<DrmNode, Device>,
    libinput: Libinput,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop = EventLoop::<State>::try_new()?;

    let (session, notify) = LibSeatSession::new().unwrap();

    event_loop
        .handle()
        .insert_source(notify, |event, _, state| match event {
            smithay::backend::session::Event::PauseSession => {
                state.libinput.suspend();

                for backend in state.devices.values() {
                    backend.drm.pause();
                }
            }
            smithay::backend::session::Event::ActivateSession => {
                state.libinput.resume().unwrap();

                let mut rerender = Vec::new();
                for (node, device) in state.devices.iter_mut() {
                    device.drm.activate();

                    for (crtc, surface) in device.surfaces.iter_mut() {
                        surface.gbm_surface.reset_buffers();

                        rerender.push((*node, *crtc));
                    }
                }

                for (node, crtc) in rerender {
                    state.on_drm_event(node, drm::DrmEvent::VBlank(crtc), &mut None);
                }
            }
        })
        .unwrap();

    let (primary_gpu, _) = primary_gpu(&session.seat());

    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput.udev_assign_seat(&session.seat()).unwrap();

    let mut state = State {
        handle: event_loop.handle(),
        session,
        devices: HashMap::default(),
        gpu_manager: GpuManager::new(Default::default()).unwrap(),
        primary_gpu,
        libinput,
    };

    init_input(&state);
    init_udev(&mut state);

    event_loop
        .handle()
        .insert_source(Timer::from_duration(Duration::from_secs(5)), |_, _, _| {
            panic!("Aborted");
        })
        .unwrap();

    event_loop.run(None, &mut state, |_data| {})?;

    Ok(())
}

fn init_udev(state: &mut State) {
    let backend = UdevBackend::new(state.session.seat()).unwrap();
    for (device_id, path) in backend.device_list() {
        state.on_udev_event(UdevEvent::Added {
            device_id,
            path: path.to_owned(),
        });
    }

    state
        .handle
        .insert_source(backend, |event, _, state| state.on_udev_event(event))
        .unwrap();
}

fn init_input(state: &State) {
    let libinput_backend = LibinputInputBackend::new(state.libinput.clone());

    state
        .handle
        .insert_source(libinput_backend, move |event, _, state| {
            if let InputEvent::Keyboard { .. } = event {
                state.session.change_vt(2).unwrap();
                // std::process::exit(0);
            }
        })
        .unwrap();
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
