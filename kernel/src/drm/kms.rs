use alloc::sync::Arc;

use crate::drm::{GemHandle, gem::GemObject};

pub type FramebufferId = u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectorInfo {
    pub id: u32,
    pub connected: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrtcInfo {
    pub id: u32,
    pub mode: Option<Mode>,
    pub framebuffer: Option<FramebufferId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KmsResources {
    pub connector: ConnectorInfo,
    pub encoder_id: u32,
    pub crtc: CrtcInfo,
    pub primary_plane_id: u32,
    pub preferred_mode: Mode,
}

/// Typed legacy page-flip request; `event` asks for one bounded file event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageFlip {
    pub framebuffer: FramebufferId,
    pub event: bool,
    pub user_data: u64,
}

#[derive(Clone)]
pub(crate) struct Framebuffer {
    pub(crate) owner: u64,
    pub(crate) handle: GemHandle,
    /// A framebuffer pins its GEM object after its originating handle closes.
    pub(crate) object: Arc<GemObject>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) bpp: u32,
}
