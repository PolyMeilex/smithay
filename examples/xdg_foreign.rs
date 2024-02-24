use std::{convert::TryInto, time::Duration};

use protocols::xdg::foreign::zv2::client::{
    zxdg_exported_v2::ZxdgExportedV2, zxdg_exporter_v2::ZxdgExporterV2, zxdg_imported_v2::ZxdgImportedV2,
    zxdg_importer_v2::ZxdgImporterV2,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::EventLoop,
        calloop_wayland_source::WaylandSource,
        client::{self as wayland_client, Dispatch},
        protocols,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

fn main() {
    let conn = Connection::connect_to_env().unwrap();

    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<SimpleWindow> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .unwrap();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell is not available");
    let shm = Shm::bind(&globals, &qh).expect("wl shm is not available.");
    let exporter = globals
        .bind::<ZxdgExporterV2, SimpleWindow, _>(&qh, 0..=1, ())
        .expect("xdg exporter not avalilable");
    let importer = globals
        .bind::<ZxdgImporterV2, SimpleWindow, _>(&qh, 0..=1, ())
        .expect("xdg exporter not avalilable");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("A wayland window");
    window.set_app_id("io.github.smithay.client-toolkit.SimpleWindow");
    window.set_min_size(Some((256, 256)));
    window.commit();

    match std::env::args().nth(1).as_deref() {
        Some("export") => {
            exporter.export_toplevel(window.wl_surface(), &qh, ());
        }
        Some("import") => {
            let handle = std::fs::read_to_string("./handle.txt").expect("handle.txt not found");
            let imported = importer.import_toplevel(handle, &qh, ());
            imported.set_parent_of(window.wl_surface());
        }
        _ => {}
    }

    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create pool");

    let mut simple_window = SimpleWindow {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,

        exit: false,
        first_configure: true,
        pool,
        width: 256,
        height: 256,
        buffer: None,
        window,
    };

    loop {
        event_loop
            .dispatch(Duration::from_millis(16), &mut simple_window)
            .unwrap();

        if simple_window.exit {
            break;
        }
    }
}

struct SimpleWindow {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    window: Window,
}

impl Dispatch<ZxdgExportedV2, ()> for SimpleWindow {
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgExportedV2,
        event: <ZxdgExportedV2 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            protocols::xdg::foreign::zv2::client::zxdg_exported_v2::Event::Handle { handle } => {
                std::fs::write("./handle.txt", handle).unwrap();
            }
            _ => todo!(),
        }
    }
}

delegate_noop!(SimpleWindow: ignore ZxdgExporterV2);
delegate_noop!(SimpleWindow: ignore ZxdgImporterV2);
delegate_noop!(SimpleWindow: ignore ZxdgImportedV2);

impl CompositorHandler for SimpleWindow {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(conn, qh);
    }
}

impl OutputHandler for SimpleWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for SimpleWindow {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        self.buffer = None;
        self.width = configure.new_size.0.map(|v| v.get()).unwrap_or(256);
        self.height = configure.new_size.1.map(|v| v.get()).unwrap_or(256);

        // Initiate the first draw.
        if self.first_configure {
            self.first_configure = false;
            self.draw(conn, qh);
        }
    }
}

impl ShmHandler for SimpleWindow {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SimpleWindow {
    pub fn draw(&mut self, _conn: &Connection, qh: &QueueHandle<Self>) {
        let width = self.width;
        let height = self.height;
        let stride = self.width as i32 * 4;

        let buffer = self.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
                .expect("create buffer")
                .0
        });

        let canvas = match self.pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                // This should be rare, but if the compositor has not released the previous
                // buffer, we need double-buffering.
                let (second_buffer, canvas) = self
                    .pool
                    .create_buffer(
                        self.width as i32,
                        self.height as i32,
                        stride,
                        wl_shm::Format::Argb8888,
                    )
                    .expect("create buffer");
                *buffer = second_buffer;
                canvas
            }
        };

        canvas.chunks_exact_mut(4).enumerate().for_each(|(index, chunk)| {
            let x = (index % width as usize) as u32;
            let y = (index / width as usize) as u32;

            let a = 0xFF;
            let r = u32::min(((width - x) * 0xFF) / width, ((height - y) * 0xFF) / height);
            let g = u32::min((x * 0xFF) / width, ((height - y) * 0xFF) / height);
            let b = u32::min(((width - x) * 0xFF) / width, (y * 0xFF) / height);
            let color = (a << 24) + (r << 16) + (g << 8) + b;

            let array: &mut [u8; 4] = chunk.try_into().unwrap();
            *array = color.to_le_bytes();
        });
        self.window
            .wl_surface()
            .damage_buffer(0, 0, self.width as i32, self.height as i32);
        self.window
            .wl_surface()
            .frame(qh, self.window.wl_surface().clone());
        buffer.attach_to(self.window.wl_surface()).expect("buffer attach");
        self.window.commit();
    }
}

delegate_compositor!(SimpleWindow);
delegate_output!(SimpleWindow);
delegate_shm!(SimpleWindow);

delegate_xdg_shell!(SimpleWindow);
delegate_xdg_window!(SimpleWindow);

delegate_registry!(SimpleWindow);

impl ProvidesRegistryState for SimpleWindow {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}
