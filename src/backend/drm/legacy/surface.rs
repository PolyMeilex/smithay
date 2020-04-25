use drm::buffer::Buffer;
use drm::control::{
    connector, crtc, dumbbuffer::DumbBuffer, encoder, framebuffer, Device as ControlDevice, Mode,
    PageFlipFlags,
};
use drm::Device as BasicDevice;

use std::collections::HashSet;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::RwLock;

use crate::backend::drm::{common::Error, DevPath, RawSurface, Surface};
use crate::backend::graphics::CursorBackend;
use crate::backend::graphics::SwapBuffersError;

use super::Dev;

use failure::{Fail, ResultExt};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct State {
    pub mode: Option<Mode>,
    pub connectors: HashSet<connector::Handle>,
}

pub(super) struct LegacyDrmSurfaceInternal<A: AsRawFd + 'static> {
    pub(super) dev: Rc<Dev<A>>,
    pub(super) crtc: crtc::Handle,
    pub(super) state: RwLock<State>,
    pub(super) pending: RwLock<State>,
    pub(super) logger: ::slog::Logger,
}

impl<A: AsRawFd + 'static> AsRawFd for LegacyDrmSurfaceInternal<A> {
    fn as_raw_fd(&self) -> RawFd {
        self.dev.as_raw_fd()
    }
}

impl<A: AsRawFd + 'static> BasicDevice for LegacyDrmSurfaceInternal<A> {}
impl<A: AsRawFd + 'static> ControlDevice for LegacyDrmSurfaceInternal<A> {}

impl<A: AsRawFd + 'static> CursorBackend for LegacyDrmSurfaceInternal<A> {
    type CursorFormat = dyn Buffer;
    type Error = Error;

    fn set_cursor_position(&self, x: u32, y: u32) -> Result<(), Error> {
        trace!(self.logger, "Move the cursor to {},{}", x, y);
        self.move_cursor(self.crtc, (x as i32, y as i32))
            .compat()
            .map_err(|source| Error::Access {
                errmsg: "Error moving cursor",
                dev: self.dev_path(),
                source,
            })
    }

    fn set_cursor_representation(
        &self,
        buffer: &Self::CursorFormat,
        hotspot: (u32, u32),
    ) -> Result<(), Error> {
        trace!(self.logger, "Setting the new imported cursor");

        if self
            .set_cursor2(self.crtc, Some(buffer), (hotspot.0 as i32, hotspot.1 as i32))
            .is_err()
        {
            self.set_cursor(self.crtc, Some(buffer))
                .compat()
                .map_err(|source| Error::Access {
                    errmsg: "Failed to set cursor",
                    dev: self.dev_path(),
                    source,
                })?;
        }

        Ok(())
    }
}

impl<A: AsRawFd + 'static> Surface for LegacyDrmSurfaceInternal<A> {
    type Error = Error;
    type Connectors = HashSet<connector::Handle>;

    fn crtc(&self) -> crtc::Handle {
        self.crtc
    }

    fn current_connectors(&self) -> Self::Connectors {
        self.state.read().unwrap().connectors.clone()
    }

    fn pending_connectors(&self) -> Self::Connectors {
        self.pending.read().unwrap().connectors.clone()
    }

    fn current_mode(&self) -> Option<Mode> {
        self.state.read().unwrap().mode
    }

    fn pending_mode(&self) -> Option<Mode> {
        self.pending.read().unwrap().mode
    }

    fn add_connector(&self, conn: connector::Handle) -> Result<(), Error> {
        let mut pending = self.pending.write().unwrap();

        if self.check_connector(conn, pending.mode.as_ref().unwrap())? {
            pending.connectors.insert(conn);
        }

        Ok(())
    }

    fn remove_connector(&self, connector: connector::Handle) -> Result<(), Error> {
        self.pending.write().unwrap().connectors.remove(&connector);
        Ok(())
    }

    fn set_connectors(&self, connectors: &[connector::Handle]) -> Result<(), Self::Error> {
        let mut pending = self.pending.write().unwrap();

        if connectors
            .iter()
            .map(|conn| self.check_connector(*conn, pending.mode.as_ref().unwrap()))
            .collect::<Result<Vec<bool>, _>>()?
            .iter()
            .all(|v| *v)
        {
            pending.connectors = connectors.iter().cloned().collect();
        }

        Ok(())
    }

    fn use_mode(&self, mode: Option<Mode>) -> Result<(), Error> {
        let mut pending = self.pending.write().unwrap();

        // check the connectors to see if this mode is supported
        if let Some(mode) = mode {
            for connector in &pending.connectors {
                if !self
                    .get_connector(*connector)
                    .compat()
                    .map_err(|source| Error::Access {
                        errmsg: "Error loading connector info",
                        dev: self.dev_path(),
                        source,
                    })?
                    .modes()
                    .contains(&mode)
                {
                    return Err(Error::ModeNotSuitable(mode));
                }
            }
        }

        pending.mode = mode;

        Ok(())
    }
}

impl<A: AsRawFd + 'static> RawSurface for LegacyDrmSurfaceInternal<A> {
    fn commit_pending(&self) -> bool {
        *self.pending.read().unwrap() != *self.state.read().unwrap()
    }

    fn commit(&self, framebuffer: framebuffer::Handle) -> Result<(), Error> {
        let mut current = self.state.write().unwrap();
        let pending = self.pending.read().unwrap();

        {
            let removed = current.connectors.difference(&pending.connectors);
            let added = pending.connectors.difference(&current.connectors);

            let mut conn_removed = false;
            for conn in removed {
                if let Ok(info) = self.get_connector(*conn) {
                    info!(self.logger, "Removing connector: {:?}", info.interface());
                } else {
                    info!(self.logger, "Removing unknown connector");
                }
                // if the connector was mapped to our crtc, we need to ack the disconnect.
                // the graphics pipeline will not be freed otherwise
                conn_removed = true;
            }

            if conn_removed {
                // We need to do a null commit to free graphics pipelines
                self.set_crtc(self.crtc, None, (0, 0), &[], None)
                    .compat()
                    .map_err(|source| Error::Access {
                        errmsg: "Error setting crtc",
                        dev: self.dev_path(),
                        source,
                    })?;
            }

            for conn in added {
                if let Ok(info) = self.get_connector(*conn) {
                    info!(self.logger, "Adding connector: {:?}", info.interface());
                } else {
                    info!(self.logger, "Adding unknown connector");
                }
            }

            if current.mode != pending.mode {
                info!(
                    self.logger,
                    "Setting new mode: {:?}",
                    pending.mode.as_ref().unwrap().name()
                );
            }
        }

        debug!(self.logger, "Setting screen");
        self.set_crtc(
            self.crtc,
            Some(framebuffer),
            (0, 0),
            &pending
                .connectors
                .iter()
                .copied()
                .collect::<Vec<connector::Handle>>(),
            pending.mode,
        )
        .compat()
        .map_err(|source| Error::Access {
            errmsg: "Error setting crtc",
            dev: self.dev_path(),
            source,
        })?;

        *current = pending.clone();

        ControlDevice::page_flip(
            self,
            self.crtc,
            framebuffer,
            &[PageFlipFlags::PageFlipEvent],
            None,
        )
        .map_err(|source| Error::Access {
            errmsg: "Failed to queue page flip",
            dev: self.dev_path(),
            source: source.compat(),
        })
    }

    fn page_flip(&self, framebuffer: framebuffer::Handle) -> ::std::result::Result<(), SwapBuffersError> {
        trace!(self.logger, "Queueing Page flip");

        ControlDevice::page_flip(
            self,
            self.crtc,
            framebuffer,
            &[PageFlipFlags::PageFlipEvent],
            None,
        )
        .map_err(|_| SwapBuffersError::ContextLost)
    }
}

impl<A: AsRawFd + 'static> LegacyDrmSurfaceInternal<A> {
    fn check_connector(&self, conn: connector::Handle, mode: &Mode) -> Result<bool, Error> {
        let info = self
            .get_connector(conn)
            .compat()
            .map_err(|source| Error::Access {
                errmsg: "Error loading connector info",
                dev: self.dev_path(),
                source,
            })?;

        // check if the connector can handle the current mode
        if info.modes().contains(mode) {
            // check if there is a valid encoder
            let encoders = info
                .encoders()
                .iter()
                .filter(|enc| enc.is_some())
                .map(|enc| enc.unwrap())
                .map(|encoder| {
                    self.get_encoder(encoder)
                        .compat()
                        .map_err(|source| Error::Access {
                            errmsg: "Error loading encoder info",
                            dev: self.dev_path(),
                            source,
                        })
                })
                .collect::<Result<Vec<encoder::Info>, _>>()?;

            // and if any encoder supports the selected crtc
            let resource_handles = self.resource_handles().compat().map_err(|source| Error::Access {
                errmsg: "Error loading resources",
                dev: self.dev_path(),
                source,
            })?;
            if !encoders
                .iter()
                .map(|encoder| encoder.possible_crtcs())
                .all(|crtc_list| resource_handles.filter_crtcs(crtc_list).contains(&self.crtc))
            {
                Ok(false)
            } else {
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }
}

impl<A: AsRawFd + 'static> Drop for LegacyDrmSurfaceInternal<A> {
    fn drop(&mut self) {
        // ignore failure at this point
        let _ = self.set_cursor(self.crtc, Option::<&DumbBuffer>::None);
    }
}

/// Open raw crtc utilizing legacy mode-setting
pub struct LegacyDrmSurface<A: AsRawFd + 'static>(pub(super) Rc<LegacyDrmSurfaceInternal<A>>);

impl<A: AsRawFd + 'static> AsRawFd for LegacyDrmSurface<A> {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl<A: AsRawFd + 'static> BasicDevice for LegacyDrmSurface<A> {}
impl<A: AsRawFd + 'static> ControlDevice for LegacyDrmSurface<A> {}

impl<A: AsRawFd + 'static> CursorBackend for LegacyDrmSurface<A> {
    type CursorFormat = dyn Buffer;
    type Error = Error;

    fn set_cursor_position(&self, x: u32, y: u32) -> Result<(), Error> {
        self.0.set_cursor_position(x, y)
    }

    fn set_cursor_representation(
        &self,
        buffer: &Self::CursorFormat,
        hotspot: (u32, u32),
    ) -> Result<(), Error> {
        self.0.set_cursor_representation(buffer, hotspot)
    }
}

impl<A: AsRawFd + 'static> Surface for LegacyDrmSurface<A> {
    type Error = Error;
    type Connectors = HashSet<connector::Handle>;

    fn crtc(&self) -> crtc::Handle {
        self.0.crtc()
    }

    fn current_connectors(&self) -> Self::Connectors {
        self.0.current_connectors()
    }

    fn pending_connectors(&self) -> Self::Connectors {
        self.0.pending_connectors()
    }

    fn current_mode(&self) -> Option<Mode> {
        self.0.current_mode()
    }

    fn pending_mode(&self) -> Option<Mode> {
        self.0.pending_mode()
    }

    fn add_connector(&self, connector: connector::Handle) -> Result<(), Error> {
        self.0.add_connector(connector)
    }

    fn remove_connector(&self, connector: connector::Handle) -> Result<(), Error> {
        self.0.remove_connector(connector)
    }

    fn set_connectors(&self, connectors: &[connector::Handle]) -> Result<(), Self::Error> {
        self.0.set_connectors(connectors)
    }

    fn use_mode(&self, mode: Option<Mode>) -> Result<(), Error> {
        self.0.use_mode(mode)
    }
}

impl<A: AsRawFd + 'static> RawSurface for LegacyDrmSurface<A> {
    fn commit_pending(&self) -> bool {
        self.0.commit_pending()
    }

    fn commit(&self, framebuffer: framebuffer::Handle) -> Result<(), Error> {
        self.0.commit(framebuffer)
    }

    fn page_flip(&self, framebuffer: framebuffer::Handle) -> ::std::result::Result<(), SwapBuffersError> {
        RawSurface::page_flip(&*self.0, framebuffer)
    }
}