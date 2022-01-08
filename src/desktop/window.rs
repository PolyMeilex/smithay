use crate::{
    backend::renderer::{utils::draw_surface_tree, Frame, ImportAll, Renderer, Texture},
    desktop::{utils::*, PopupManager, Space},
    utils::{user_data::UserDataMap, Logical, Point, Rectangle},
    wayland::{
        compositor::with_states,
        output::Output,
        shell::xdg::{SurfaceCachedState, ToplevelSurface},
    },
};
use std::{
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};
use wayland_protocols::xdg_shell::server::xdg_toplevel;
use wayland_server::{protocol::wl_surface, DisplayHandle, Resource};

crate::utils::ids::id_gen!(next_window_id, WINDOW_ID, WINDOW_IDS);

/// Abstraction around different toplevel kinds
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// xdg-shell [`ToplevelSurface`]
    Xdg(ToplevelSurface),
    /// XWayland surface (TODO)
    #[cfg(feature = "xwayland")]
    X11(X11Surface),
}

/// TODO
#[derive(Debug, Clone)]
pub struct X11Surface {
    surface: wl_surface::WlSurface,
}

impl std::cmp::PartialEq for X11Surface {
    fn eq(&self, other: &Self) -> bool {
        self.surface == other.surface
    }
}

impl X11Surface {
    /// Checks if the surface is still alive.
    pub fn alive(&self, cx: &mut DisplayHandle<'_>) -> bool {
        cx.object_info(self.surface.id()).is_ok()
    }

    /// Returns the underlying [`WlSurface`](wl_surface::WlSurface), if still any.
    pub fn get_surface(&self, cx: &mut DisplayHandle<'_>) -> Option<&wl_surface::WlSurface> {
        if self.alive(cx) {
            Some(&self.surface)
        } else {
            None
        }
    }
}

impl Kind {
    /// Checks if the surface is still alive.
    pub fn alive(&self, cx: &mut DisplayHandle<'_>) -> bool {
        match *self {
            Kind::Xdg(ref t) => t.alive(cx),
            #[cfg(feature = "xwayland")]
            Kind::X11(ref t) => t.alive(cx),
        }
    }

    /// Returns the underlying [`WlSurface`](wl_surface::WlSurface), if still any.
    pub fn get_surface(&self, cx: &mut DisplayHandle<'_>) -> Option<&wl_surface::WlSurface> {
        match *self {
            Kind::Xdg(ref t) => t.get_surface(cx),
            #[cfg(feature = "xwayland")]
            Kind::X11(ref t) => t.get_surface(cx),
        }
    }
}

#[derive(Debug)]
pub(super) struct WindowInner {
    pub(super) id: usize,
    toplevel: Kind,
    bbox: Mutex<Rectangle<i32, Logical>>,
    user_data: UserDataMap,
}

impl Drop for WindowInner {
    fn drop(&mut self) {
        WINDOW_IDS.lock().unwrap().remove(&self.id);
    }
}

/// Represents a single application window
#[derive(Debug, Clone)]
pub struct Window(pub(super) Arc<WindowInner>);

impl PartialEq for Window {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for Window {}

impl Hash for Window {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl Window {
    /// Construct a new [`Window`] from a given compatible toplevel surface
    pub fn new(toplevel: Kind) -> Window {
        let id = next_window_id();

        Window(Arc::new(WindowInner {
            id,
            toplevel,
            bbox: Mutex::new(Rectangle::from_loc_and_size((0, 0), (0, 0))),
            user_data: UserDataMap::new(),
        }))
    }

    /// Returns the geometry of this window.
    pub fn geometry(&self, cx: &mut DisplayHandle<'_>) -> Rectangle<i32, Logical> {
        // It's the set geometry with the full bounding box as the fallback.
        with_states(self.0.toplevel.get_surface(cx).unwrap(), |states| {
            states.cached_state.current::<SurfaceCachedState>().geometry
        })
        .unwrap()
        .unwrap_or_else(|| self.bbox(cx))
    }

    /// Returns a bounding box over this window and its children.
    pub fn bbox(&self, cx: &mut DisplayHandle<'_>) -> Rectangle<i32, Logical> {
        if self.0.toplevel.get_surface(cx).is_some() {
            *self.0.bbox.lock().unwrap()
        } else {
            Rectangle::from_loc_and_size((0, 0), (0, 0))
        }
    }

    /// Returns a bounding box over this window and children including popups.
    ///
    /// Note: You need to use a [`PopupManager`] to track popups, otherwise the bounding box
    /// will not include the popups.
    pub fn bbox_with_popups(&self, cx: &mut DisplayHandle<'_>) -> Rectangle<i32, Logical> {
        let mut bounding_box = self.bbox(cx);
        if let Some(surface) = self.0.toplevel.get_surface(cx) {
            for (popup, location) in PopupManager::popups_for_surface(cx, surface)
                .ok()
                .into_iter()
                .flatten()
            {
                if let Some(surface) = popup.get_surface(cx) {
                    let offset = self.geometry(cx).loc + location - popup.geometry(cx).loc;
                    bounding_box = bounding_box.merge(bbox_from_surface_tree(surface, offset));
                }
            }
        }
        bounding_box
    }

    /// Activate/Deactivate this window
    pub fn set_activated(&self, cx: &mut DisplayHandle<'_>, active: bool) -> bool {
        match self.0.toplevel {
            Kind::Xdg(ref t) => t
                .with_pending_state(cx, |state| {
                    if active {
                        state.states.set(xdg_toplevel::State::Activated)
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated)
                    }
                })
                .unwrap_or(false),
            #[cfg(feature = "xwayland")]
            Kind::X11(ref _t) => unimplemented!(),
        }
    }

    /// Commit any changes to this window
    pub fn configure(&self, cx: &mut DisplayHandle<'_>) {
        match self.0.toplevel {
            Kind::Xdg(ref t) => t.send_configure(cx),
            #[cfg(feature = "xwayland")]
            Kind::X11(ref _t) => unimplemented!(),
        }
    }

    /// Sends the frame callback to all the subsurfaces in this
    /// window that requested it
    pub fn send_frame(&self, cx: &mut DisplayHandle<'_>, time: u32) {
        if let Some(surface) = self.0.toplevel.get_surface(cx) {
            send_frames_surface_tree(cx, surface, time);
            for (popup, _) in PopupManager::popups_for_surface(cx, surface)
                .ok()
                .into_iter()
                .flatten()
            {
                if let Some(surface) = popup.get_surface(cx) {
                    send_frames_surface_tree(cx, surface, time);
                }
            }
        }
    }

    /// Updates internal values
    ///
    /// Needs to be called whenever the toplevel surface or any unsynchronized subsurfaces of this window are updated
    /// to correctly update the bounding box of this window.
    pub fn refresh(&self, cx: &mut DisplayHandle<'_>) {
        if let Some(surface) = self.0.toplevel.get_surface(cx) {
            *self.0.bbox.lock().unwrap() = bbox_from_surface_tree(surface, (0, 0));
        }
    }

    /// Finds the topmost surface under this point if any and returns it together with the location of this
    /// surface.
    ///
    /// - `point` should be relative to (0,0) of the window.
    pub fn surface_under<P: Into<Point<f64, Logical>>>(
        &self,
        cx: &mut DisplayHandle<'_>,
        point: P,
    ) -> Option<(wl_surface::WlSurface, Point<i32, Logical>)> {
        let point = point.into();
        if let Some(surface) = self.0.toplevel.get_surface(cx) {
            for (popup, location) in PopupManager::popups_for_surface(cx, surface)
                .ok()
                .into_iter()
                .flatten()
            {
                let offset = self.geometry(cx).loc + location - popup.geometry(cx).loc;
                if let Some(result) = popup
                    .get_surface(cx)
                    .and_then(|surface| under_from_surface_tree(surface, point, offset))
                {
                    return Some(result);
                }
            }

            under_from_surface_tree(surface, point, (0, 0))
        } else {
            None
        }
    }

    /// Damage of all the surfaces of this window.
    ///
    /// If `for_values` is `Some(_)` it will only return the damage on the
    /// first call for a given [`Space`] and [`Output`], if the buffer hasn't changed.
    /// Subsequent calls will return an empty vector until the buffer is updated again.
    pub fn accumulated_damage(
        &self,
        cx: &mut DisplayHandle<'_>,
        for_values: Option<(&Space, &Output)>,
    ) -> Vec<Rectangle<i32, Logical>> {
        let mut damage = Vec::new();
        if let Some(surface) = self.0.toplevel.get_surface(cx) {
            damage.extend(
                damage_from_surface_tree(surface, (0, 0), for_values)
                    .into_iter()
                    .flat_map(|rect| rect.intersection(self.bbox(cx))),
            );
            for (popup, location) in PopupManager::popups_for_surface(cx, surface)
                .ok()
                .into_iter()
                .flatten()
            {
                if let Some(surface) = popup.get_surface(cx) {
                    let offset = self.geometry(cx).loc + location - popup.geometry(cx).loc;
                    let bbox = bbox_from_surface_tree(surface, offset);
                    let popup_damage = damage_from_surface_tree(surface, offset, for_values);
                    damage.extend(popup_damage.into_iter().flat_map(|rect| rect.intersection(bbox)));
                }
            }
        }
        damage
    }

    /// Returns the underlying toplevel
    pub fn toplevel(&self) -> &Kind {
        &self.0.toplevel
    }

    /// Returns a [`UserDataMap`] to allow associating arbitrary data with this window.
    pub fn user_data(&self) -> &UserDataMap {
        &self.0.user_data
    }
}

/// Renders a given [`Window`] using a provided renderer and frame.
///
/// - `scale` needs to be equivalent to the fractional scale the rendered result should have.
/// - `location` is the position the window should be drawn at.
/// - `damage` is the set of regions of the window that should be drawn.
///
/// Note: This function will render nothing, if you are not using
/// [`crate::backend::renderer::utils::on_commit_buffer_handler`]
/// to let smithay handle buffer management.
pub fn draw_window<R, E, F, T, P>(
    cx: &mut DisplayHandle<'_>,
    renderer: &mut R,
    frame: &mut F,
    window: &Window,
    scale: f64,
    location: P,
    damage: &[Rectangle<i32, Logical>],
    log: &slog::Logger,
) -> Result<(), R::Error>
where
    R: Renderer<Error = E, TextureId = T, Frame = F> + ImportAll,
    F: Frame<Error = E, TextureId = T>,
    E: std::error::Error,
    T: Texture + 'static,
    P: Into<Point<i32, Logical>>,
{
    let location = location.into();
    if let Some(surface) = window.toplevel().get_surface(cx) {
        draw_surface_tree(renderer, frame, surface, scale, location, damage, log)?;
        for (popup, p_location) in PopupManager::popups_for_surface(cx, surface)
            .ok()
            .into_iter()
            .flatten()
        {
            if let Some(surface) = popup.get_surface(cx) {
                let offset = window.geometry(cx).loc + p_location - popup.geometry(cx).loc;
                let damage = damage
                    .iter()
                    .cloned()
                    .map(|mut geo| {
                        geo.loc -= offset;
                        geo
                    })
                    .collect::<Vec<_>>();
                draw_surface_tree(renderer, frame, surface, scale, location + offset, &damage, log)?;
            }
        }
    }
    Ok(())
}
