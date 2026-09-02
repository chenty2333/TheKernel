use alloc::{sync::Arc, vec::Vec};

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
    /// Immutable, device-owned EDID property-blob ID.
    pub edid_blob: u32,
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
    /// VirtIO cursorq is represented as a real cursor plane. It is still
    /// submitted separately from controlq, but its state is owned by the same
    /// atomic KMS transaction as the primary plane.
    pub cursor_plane_id: u32,
    pub preferred_mode: Mode,
    /// The complete advertised list for this one virtual connector.  It is
    /// never synthesized from an active CRTC state.
    pub modes: Vec<Mode>,
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
    /// DRM fourcc retained verbatim from ADDFB2 (or derived by legacy
    /// ADDFB).  Scanout must not infer alpha semantics from bpp alone.
    pub(crate) format: u32,
    /// Byte offset of this framebuffer's first pixel in its GEM backing.
    /// Multiple fbdev pages may therefore share one virtual-height dumb GEM.
    pub(crate) offset: u64,
}
