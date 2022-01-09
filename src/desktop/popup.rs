use crate::{
    utils::{DeadResource, Logical, Point, Rectangle},
    wayland::{
        compositor::{get_role, with_states},
        shell::xdg::{PopupSurface, SurfaceCachedState, XdgPopupSurfaceRoleAttributes, XDG_POPUP_ROLE},
    },
};
use std::sync::{Arc, Mutex};
use wayland_server::{protocol::wl_surface::WlSurface, DisplayHandle};

/// Helper to track popups.
#[derive(Debug)]
pub struct PopupManager {
    unmapped_popups: Vec<PopupKind>,
    popup_trees: Vec<PopupTree>,
    logger: ::slog::Logger,
}

impl PopupManager {
    /// Create a new [`PopupManager`].
    pub fn new<L: Into<Option<::slog::Logger>>>(logger: L) -> Self {
        PopupManager {
            unmapped_popups: Vec::new(),
            popup_trees: Vec::new(),
            logger: crate::slog_or_fallback(logger),
        }
    }

    /// Start tracking a new popup.
    pub fn track_popup(&mut self, cx: &mut DisplayHandle<'_>, kind: PopupKind) -> Result<(), DeadResource> {
        if kind.parent(cx).is_some() {
            self.add_popup(cx, kind)
        } else {
            slog::trace!(self.logger, "Adding unmapped popups: {:?}", kind);
            self.unmapped_popups.push(kind);
            Ok(())
        }
    }

    /// Needs to be called for [`PopupManager`] to correctly update its internal state.
    pub fn commit(&mut self, cx: &mut DisplayHandle<'_>, surface: &WlSurface) {
        if get_role(cx, surface) == Some(XDG_POPUP_ROLE) {
            if let Some(i) = self
                .unmapped_popups
                .iter()
                .position(|p| p.get_surface(cx) == Some(surface))
            {
                slog::trace!(self.logger, "Popup got mapped");
                let popup = self.unmapped_popups.swap_remove(i);
                // at this point the popup must have a parent,
                // or it would have raised a protocol error
                let _ = self.add_popup(cx, popup);
            }
        }
    }

    fn add_popup(&mut self, cx: &mut DisplayHandle<'_>, popup: PopupKind) -> Result<(), DeadResource> {
        let mut parent = popup.parent(cx).unwrap();
        while get_role(cx, &parent) == Some(XDG_POPUP_ROLE) {
            parent = with_states(&parent, |states| {
                states
                    .data_map
                    .get::<Mutex<XdgPopupSurfaceRoleAttributes>>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .parent
                    .as_ref()
                    .cloned()
                    .unwrap()
            })?;
        }

        with_states(&parent, |states| {
            let tree = PopupTree::default();
            if states.data_map.insert_if_missing(|| tree.clone()) {
                self.popup_trees.push(tree);
            };
            let tree = states.data_map.get::<PopupTree>().unwrap();
            if !tree.alive() {
                // if it previously had no popups, we likely removed it from our list already
                self.popup_trees.push(tree.clone());
            }
            slog::trace!(self.logger, "Adding popup {:?} to parent {:?}", popup, parent);
            tree.insert(cx, popup);
        })
    }

    /// Finds the popup belonging to a given [`WlSurface`], if any.
    pub fn find_popup(&self, cx: &mut DisplayHandle<'_>, surface: &WlSurface) -> Option<PopupKind> {
        self.unmapped_popups
            .iter()
            .find(|p| p.get_surface(cx) == Some(surface))
            .cloned()
            .or_else(|| {
                self.popup_trees
                    .iter()
                    .map(|tree| tree.iter_popups())
                    .flatten()
                    .find(|(p, _)| p.get_surface(cx) == Some(surface))
                    .map(|(p, _)| p)
            })
    }

    /// Returns the popups and their relative positions for a given toplevel surface, if any.
    pub fn popups_for_surface(
        surface: &WlSurface,
    ) -> Result<impl Iterator<Item = (PopupKind, Point<i32, Logical>)>, DeadResource> {
        with_states(surface, |states| {
            states
                .data_map
                .get::<PopupTree>()
                .map(|x| x.iter_popups())
                .into_iter()
                .flatten()
        })
    }

    /// Needs to be called periodically (but not necessarily frequently)
    /// to cleanup internal resources.
    pub fn cleanup(&mut self, cx: &mut DisplayHandle<'_>) {
        // retain_mut is sadly still unstable
        self.popup_trees.iter_mut().for_each(|tree| tree.cleanup(cx));
        self.popup_trees.retain(|tree| tree.alive());
        self.unmapped_popups.retain(|surf| surf.alive(cx));
    }
}

#[derive(Debug, Default, Clone)]
struct PopupTree(Arc<Mutex<Vec<PopupNode>>>);

#[derive(Debug, Clone)]
struct PopupNode {
    surface: PopupKind,
    children: Vec<PopupNode>,
}

impl PopupTree {
    fn iter_popups(&self) -> impl Iterator<Item = (PopupKind, Point<i32, Logical>)> {
        // self.0
        //     .lock()
        //     .unwrap()
        //     .iter()
        //     .map(|n| n.iter_popups_relative_to((0, 0)).map(|(p, l)| (p.clone(), l)))
        //     .flatten()
        //     .collect::<Vec<_>>()
        //     .into_iter()

        todo!();
        Box::new(std::iter::empty())
    }

    fn insert(&self, cx: &mut DisplayHandle<'_>, popup: PopupKind) {
        let children = &mut *self.0.lock().unwrap();
        for child in children.iter_mut() {
            if child.insert(cx, popup.clone()) {
                return;
            }
        }
        children.push(PopupNode::new(popup));
    }

    fn cleanup(&mut self, cx: &mut DisplayHandle<'_>) {
        let mut children = self.0.lock().unwrap();
        for child in children.iter_mut() {
            child.cleanup(cx);
        }
        children.retain(|n| n.surface.alive(cx));
    }

    fn alive(&self) -> bool {
        !self.0.lock().unwrap().is_empty()
    }
}

impl PopupNode {
    fn new(surface: PopupKind) -> Self {
        PopupNode {
            surface,
            children: Vec::new(),
        }
    }

    fn iter_popups_relative_to<P: Into<Point<i32, Logical>>>(
        &self,
        cx: &mut DisplayHandle<'_>,
        loc: P,
    ) -> impl Iterator<Item = (&PopupKind, Point<i32, Logical>)> {
        todo!();
        // let relative_to = loc.into() + self.surface.location(cx);
        // std::iter::once((&self.surface, relative_to)).chain(
        //     self.children
        //         .iter()
        //         .map(move |x| {
        //             Box::new(x.iter_popups_relative_to(cx, relative_to))
        //                 as Box<dyn Iterator<Item = (&PopupKind, Point<i32, Logical>)>>
        //         })
        //         .flatten(),
        // )
        Box::new(std::iter::empty())
    }

    fn insert(&mut self, cx: &mut DisplayHandle<'_>, popup: PopupKind) -> bool {
        let parent = popup.parent(cx).unwrap();
        if self.surface.get_surface(cx) == Some(&parent) {
            self.children.push(PopupNode::new(popup));
            true
        } else {
            for child in &mut self.children {
                if child.insert(cx, popup.clone()) {
                    return true;
                }
            }
            false
        }
    }

    fn cleanup(&mut self, cx: &mut DisplayHandle<'_>) {
        for child in &mut self.children {
            child.cleanup(cx);
        }
        self.children.retain(|n| n.surface.alive(cx));
    }
}

/// Represents a popup surface
#[derive(Debug, Clone)]
pub enum PopupKind {
    /// xdg-shell [`PopupSurface`]
    Xdg(PopupSurface),
}

impl PopupKind {
    fn alive(&self, cx: &mut DisplayHandle<'_>) -> bool {
        match *self {
            PopupKind::Xdg(ref t) => t.alive(cx),
        }
    }

    /// Retrieves the underlying [`WlSurface`]
    pub fn get_surface(&self, cx: &mut DisplayHandle<'_>) -> Option<&WlSurface> {
        match *self {
            PopupKind::Xdg(ref t) => t.get_surface(cx),
        }
    }

    fn parent(&self, cx: &mut DisplayHandle<'_>) -> Option<WlSurface> {
        match *self {
            PopupKind::Xdg(ref t) => t.get_parent_surface(cx),
        }
    }

    /// Returns the surface geometry as set by the client using `xdg_surface::set_window_geometry`
    pub fn geometry(&self, cx: &mut DisplayHandle<'_>) -> Rectangle<i32, Logical> {
        let wl_surface = match self.get_surface(cx) {
            Some(s) => s,
            None => return Rectangle::from_loc_and_size((0, 0), (0, 0)),
        };

        with_states(wl_surface, |states| {
            states
                .cached_state
                .current::<SurfaceCachedState>()
                .geometry
                .unwrap_or_default()
        })
        .unwrap()
    }

    fn location(&self, cx: &mut DisplayHandle<'_>) -> Point<i32, Logical> {
        let wl_surface = match self.get_surface(cx) {
            Some(s) => s,
            None => return (0, 0).into(),
        };
        with_states(wl_surface, |states| {
            states
                .data_map
                .get::<Mutex<XdgPopupSurfaceRoleAttributes>>()
                .unwrap()
                .lock()
                .unwrap()
                .current
                .geometry
        })
        .unwrap_or_default()
        .loc
    }
}

impl From<PopupSurface> for PopupKind {
    fn from(p: PopupSurface) -> PopupKind {
        PopupKind::Xdg(p)
    }
}
