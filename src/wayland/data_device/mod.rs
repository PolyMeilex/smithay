//! Utilities for manipulating the data devices
//!
//! The data device is wayland's abstraction to represent both selection (copy/paste) and
//! drag'n'drop actions. This module provides logic to handle this part of the protocol.
//! Selection and drag'n'drop are per-seat notions.
//!
//! This module provides 2 main freestanding functions:
//!
//! - [`DataDeviceState::new`]: this function must be called
//!   during the compositor startup to initialize the data device logic
//! - [`set_data_device_focus`]: this function sets
//!   the data device focus for a given seat; you'd typically call it whenever the keyboard focus
//!   changes, to follow it (for example in the focus hook of your keyboards)
//!
//! Using these two functions is enough for your clients to be able to interact with each other using
//! the data devices.
//!
//! The module also provides additional mechanisms allowing your compositor to see and interact with
//! the contents of the data device:
//!
//! - the freestanding function [`set_data_device_selection`]
//!   allows you to set the contents of the selection for your clients
//! - the freestanding function [`start_dnd`] allows you to initiate a drag'n'drop event from the compositor
//!   itself and receive interactions of clients with it via an other dedicated callback.
//!
//! The module defines the role `"dnd_icon"` that is assigned to surfaces used as drag'n'drop icons.
//!
//! ## Initialization
//!

use std::{cell::RefCell, os::unix::prelude::RawFd};

use wayland_server::{
    backend::GlobalId,
    protocol::{
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_source::WlDataSource,
        wl_surface::WlSurface,
    },
    Client, Display, DisplayHandle, GlobalDispatch,
};

use super::{
    seat::{PointerGrabStartData, Seat},
    Serial,
};

mod device;
mod dnd_grab;
mod seat_data;
mod server_dnd_grab;
mod source;

pub use device::DND_ICON_ROLE;
pub use source::{with_source_metadata, SourceMetadata};

use seat_data::{SeatData, Selection};

/// Events that are generated by interactions of the clients with the data device
pub trait DataDeviceHandler: Sized + ClientDndGrabHandler + ServerDndGrabHandler {
    /// [DataDeviceState] getter
    fn data_device_state(&self) -> &DataDeviceState;

    /// Action chooser for DnD negociation
    fn action_choice(&mut self, available: DndAction, preferred: DndAction) -> DndAction {
        default_action_chooser(available, preferred)
    }

    /// A client has set the selection
    #[allow(unused_variables)]
    fn new_selection(&mut self, source: Option<WlDataSource>) {}

    /// A client requested to read the server-set selection
    ///
    /// * `mime_type` - the requested mime type
    /// * `fd` - the fd to write into
    fn send_selection(&mut self, mime_type: String, fd: RawFd);
}

/// Events that are generated during client initiated drag'n'drop
pub trait ClientDndGrabHandler: Sized {
    /// A client started a drag'n'drop as response to a user pointer action
    ///
    /// * `source` - The data source provided by the client.
    ///              If it is `None`, this means the DnD is restricted to surfaces of the
    ///              same client and the client will manage data transfer by itself.
    /// * `icon` - The icon the client requested to be used to be associated with the cursor icon
    ///            during the drag'n'drop.
    /// * `seat` - The seat on which the DnD operation was started
    #[allow(unused_variables)]
    fn started(&mut self, source: Option<WlDataSource>, icon: Option<WlSurface>, seat: Seat<Self>) {}

    /// The drag'n'drop action was finished by the user releasing the buttons
    ///
    /// At this point, any pointer icon should be removed.
    ///
    /// Note that this event will only be generated for client-initiated drag'n'drop session.
    ///
    /// * `seat` - The seat on which the DnD action was finished.
    #[allow(unused_variables)]
    fn dropped(&mut self, seat: Seat<Self>) {}
}

/// Event generated by the interactions of clients with a server initiated drag'n'drop
pub trait ServerDndGrabHandler {
    /// The client chose an action
    #[allow(unused_variables)]
    fn action(&mut self, action: DndAction) {}

    /// The DnD resource was dropped by the user
    ///
    /// After that, the client can still interact with your resource
    fn dropped(&mut self) {}

    /// The Dnd was cancelled
    ///
    /// The client can no longer interact
    fn cancelled(&mut self) {}

    /// The client requested for data to be sent
    ///
    /// * `mime_type` - The requested mime type
    /// * `fd` - The FD to write into
    fn send(&mut self, mime_type: String, fd: RawFd);

    /// The client has finished interacting with the resource
    ///
    /// This can only happen after the resource was dropped.
    fn finished(&mut self) {}
}

/// State of data device
#[derive(Debug)]
pub struct DataDeviceState {
    log: slog::Logger,
    manager_global_id: GlobalId,
}

impl DataDeviceState {
    /// Regiseter new [WlDataDeviceManager] global
    pub fn new<D, L>(display: &mut Display<D>, logger: L) -> Self
    where
        L: Into<Option<::slog::Logger>>,
        D: GlobalDispatch<WlDataDeviceManager, GlobalData = ()> + 'static,
        D: DataDeviceHandler,
    {
        let log = crate::slog_or_fallback(logger).new(slog::o!("smithay_module" => "data_device_mgr"));

        let manager_global_id = display.create_global::<WlDataDeviceManager>(3, ());

        Self {
            log,
            manager_global_id,
        }
    }

    /// [WlDataDeviceManager] GlobalId getter
    pub fn global_id(&self) -> GlobalId {
        self.manager_global_id.clone()
    }
}

/// A simple action chooser for DnD negociation
///
/// If the preferred action is available, it'll pick it. Otherwise, it'll pick the first
/// available in the following order: Ask, Copy, Move.
pub fn default_action_chooser(available: DndAction, preferred: DndAction) -> DndAction {
    // if the preferred action is valid (a single action) and in the available actions, use it
    // otherwise, follow a fallback stategy
    if [DndAction::Move, DndAction::Copy, DndAction::Ask].contains(&preferred)
        && available.contains(preferred)
    {
        preferred
    } else if available.contains(DndAction::Ask) {
        DndAction::Ask
    } else if available.contains(DndAction::Copy) {
        DndAction::Copy
    } else if available.contains(DndAction::Move) {
        DndAction::Move
    } else {
        DndAction::empty()
    }
}

/// Set the data device focus to a certain client for a given seat
pub fn set_data_device_focus<D>(dh: &mut DisplayHandle<'_>, seat: &Seat<D>, client: Option<Client>)
where
    D: DataDeviceHandler,
    D: 'static,
{
    seat.user_data()
        .insert_if_missing(|| RefCell::new(SeatData::new()));
    let seat_data = seat.user_data().get::<RefCell<SeatData>>().unwrap();
    seat_data.borrow_mut().set_focus::<D>(dh, client);
}

/// Set a compositor-provided selection for this seat
///
/// You need to provide the available mime types for this selection.
///
/// Whenever a client requests to read the selection, your callback will
/// receive a [`DataDeviceEvent::SendSelection`] event.
pub fn set_data_device_selection<D>(dh: &mut DisplayHandle<'_>, seat: &Seat<D>, mime_types: Vec<String>)
where
    D: DataDeviceHandler,
    D: 'static,
{
    seat.user_data()
        .insert_if_missing(|| RefCell::new(SeatData::new()));
    let seat_data = seat.user_data().get::<RefCell<SeatData>>().unwrap();
    seat_data.borrow_mut().set_selection::<D>(
        dh,
        Selection::Compositor(SourceMetadata {
            mime_types,
            dnd_action: DndAction::empty(),
        }),
    );
}

/// Start a drag'n'drop from a resource controlled by the compositor
///
/// You'll receive events generated by the interaction of clients with your
/// drag'n'drop in the provided callback. See [`ServerDndEvent`] for details about
/// which events can be generated and what response is expected from you to them.
pub fn start_dnd<D, C>(
    dh: &mut DisplayHandle<'_>,
    seat: &Seat<D>,
    serial: Serial,
    start_data: PointerGrabStartData,
    metadata: SourceMetadata,
) where
    D: DataDeviceHandler,
    D: 'static,
{
    seat.user_data()
        .insert_if_missing(|| RefCell::new(SeatData::new()));
    if let Some(pointer) = seat.get_pointer() {
        pointer.set_grab(
            dh,
            server_dnd_grab::ServerDnDGrab::new(start_data, metadata, seat.clone()),
            serial,
            0,
        );
    }
}

mod handlers {
    use std::cell::RefCell;

    use slog::error;
    use wayland_server::{
        protocol::{
            wl_data_device::WlDataDevice,
            wl_data_device_manager::{self, WlDataDeviceManager},
            wl_data_source::WlDataSource,
        },
        DelegateDispatch, DelegateDispatchBase, DelegateGlobalDispatch, DelegateGlobalDispatchBase, Dispatch,
        GlobalDispatch,
    };

    use crate::wayland::seat::Seat;

    use super::{device::DataDeviceUserData, seat_data::SeatData, source::DataSourceUserData};
    use super::{DataDeviceHandler, DataDeviceState};

    impl DelegateGlobalDispatchBase<WlDataDeviceManager> for DataDeviceState {
        type GlobalData = ();
    }

    impl<D> DelegateGlobalDispatch<WlDataDeviceManager, D> for DataDeviceState
    where
        D: GlobalDispatch<WlDataDeviceManager, GlobalData = ()>,
        D: Dispatch<WlDataDeviceManager, UserData = ()>,
        D: Dispatch<WlDataSource, UserData = DataSourceUserData>,
        D: Dispatch<WlDataDevice, UserData = DataDeviceUserData>,
        D: DataDeviceHandler,
        D: 'static,
    {
        fn bind(
            _state: &mut D,
            _handle: &mut wayland_server::DisplayHandle<'_>,
            _client: &wayland_server::Client,
            resource: wayland_server::New<WlDataDeviceManager>,
            _global_data: &Self::GlobalData,
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl DelegateDispatchBase<WlDataDeviceManager> for DataDeviceState {
        type UserData = ();
    }

    impl<D> DelegateDispatch<WlDataDeviceManager, D> for DataDeviceState
    where
        D: Dispatch<WlDataDeviceManager, UserData = ()>,
        D: Dispatch<WlDataSource, UserData = DataSourceUserData>,
        D: Dispatch<WlDataDevice, UserData = DataDeviceUserData>,
        D: DataDeviceHandler,
        D: 'static,
    {
        fn request(
            state: &mut D,
            _client: &wayland_server::Client,
            _resource: &WlDataDeviceManager,
            request: wl_data_device_manager::Request,
            _data: &Self::UserData,
            _dhandle: &mut wayland_server::DisplayHandle<'_>,
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            let data_device_state = state.data_device_state();

            match request {
                wl_data_device_manager::Request::CreateDataSource { id } => {
                    data_init.init(id, DataSourceUserData::new());
                }
                wl_data_device_manager::Request::GetDataDevice { id, seat: wl_seat } => {
                    // TODO: Change Seat T to always be equal to D )-:
                    match Seat::<D>::from_resource(&wl_seat) {
                        Some(seat) => {
                            seat.user_data()
                                .insert_if_missing(|| RefCell::new(SeatData::new()));

                            let data_device = data_init.init(id, DataDeviceUserData { wl_seat });

                            let seat_data = seat.user_data().get::<RefCell<SeatData>>().unwrap();
                            seat_data.borrow_mut().add_device(data_device);
                        }
                        None => {
                            error!(&data_device_state.log, "Unmanaged seat given to a data device.");
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

#[allow(missing_docs)] // TODO
#[macro_export]
macro_rules! delegate_data_device {
    ($ty: ty) => {
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::wayland_server::protocol::wl_data_device_manager::WlDataDeviceManager
        ] => $crate::wayland::data_device::DataDeviceState);

        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::wayland_server::protocol::wl_data_device_manager::WlDataDeviceManager,
            $crate::reexports::wayland_server::protocol::wl_data_device::WlDataDevice,
            $crate::reexports::wayland_server::protocol::wl_data_source::WlDataSource
        ] => $crate::wayland::data_device::DataDeviceState);
    };
}
