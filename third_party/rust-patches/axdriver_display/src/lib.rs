//! Common traits and types for graphics display device drivers.

#![no_std]

#[doc(no_inline)]
pub use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub fb_base_vaddr: usize,
    pub fb_size: usize,
}

pub struct FrameBuffer<'a> {
    _raw: &'a mut [u8],
}

/// Wire-level, type-erased virgl transport made available to the kernel DRM
/// layer.  Implementations must copy borrowed command and resource-ID slices
/// before submitting them to the device.
pub trait RenderTransport {
    fn capset_info(&mut self, index: u32) -> DevResult<RenderCapsetInfo>;
    fn capset(&mut self, id: u32, version: u32, data: &mut [u8]) -> DevResult<usize>;
    fn create_context(&mut self, name: &[u8]) -> DevResult<u32>;
    fn destroy_context(&mut self, context: u32) -> DevResult;
    fn attach_resource(&mut self, context: u32, resource: u32) -> DevResult;
    fn detach_resource(&mut self, context: u32, resource: u32) -> DevResult;
    fn create_resource_3d(&mut self, resource: RenderResource3D) -> DevResult<u32>;
    fn attach_backing(&mut self, resource: u32, entries: &[(u64, u32)]) -> DevResult;
    fn detach_backing(&mut self, resource: u32) -> DevResult;
    /// Release a render resource only after it has been detached from every
    /// context and its guest backing has been detached successfully.
    fn unref_resource(&mut self, resource: u32) -> DevResult;
    fn transfer_3d(
        &mut self,
        context: u32,
        resource: u32,
        transfer: RenderTransfer3D,
        to_host: bool,
    ) -> DevResult;
    fn submit_3d(&mut self, context: u32, commands: &[u8], resources: &[u32]) -> DevResult;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderCapsetInfo {
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderResource3D {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderTransfer3D {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub offset: u64,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

impl<'a> FrameBuffer<'a> {
    pub unsafe fn from_raw_parts_mut(ptr: *mut u8, len: usize) -> Self {
        Self {
            _raw: core::slice::from_raw_parts_mut(ptr, len),
        }
    }

    pub fn from_slice(slice: &'a mut [u8]) -> Self {
        Self { _raw: slice }
    }
}

/// Operations required by display drivers.
///
/// The DRM methods transfer sole scanout ownership from the legacy framebuffer
/// user to a driver that supports caller-owned pinned backing. Drivers that do
/// not implement this transport remain usable through the framebuffer API.
pub trait DisplayDriverOps: BaseDriverOps {
    fn info(&self) -> DisplayInfo;
    fn fb(&self) -> FrameBuffer<'_>;
    fn need_flush(&self) -> bool;
    fn flush(&mut self) -> DevResult;

    fn supports_drm_transport(&self) -> bool {
        false
    }

    /// A non-virgl display returns `None`; its 2D fallback remains usable.
    fn render_transport(&mut self) -> Option<&mut dyn RenderTransport> {
        None
    }

    fn drm_create_resource(
        &mut self,
        _width: u32,
        _height: u32,
        _entries: &[(u64, u32)],
    ) -> DevResult<u32> {
        Err(DevError::Unsupported)
    }

    fn drm_present_resource(&mut self, _resource: u32, _width: u32, _height: u32) -> DevResult {
        Err(DevError::Unsupported)
    }

    fn drm_destroy_resource(&mut self, _resource: u32) -> DevResult {
        Err(DevError::Unsupported)
    }
}
