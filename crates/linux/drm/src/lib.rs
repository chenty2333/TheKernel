//! Linux x86_64 DRM/KMS UAPI definitions plus pure ioctl decoding and admission plans.
//!
//! Pointer-valued UAPI members are `u64` userspace addresses, never kernel pointers.
#![no_std]
#![forbid(unsafe_code)]

use core::mem::{align_of, offset_of, size_of};

pub const DRM_IOCTL_BASE: u8 = b'd';
pub const DRM_COMMAND_BASE: u8 = 0x40;
/// First KMS ioctl number; driver-private commands occupy `0x40..0xa0`.
pub const DRM_COMMAND_END: u8 = 0xa0;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const fn ioc(direction: u64, number: u64, size: usize) -> u64 {
    number | ((DRM_IOCTL_BASE as u64) << 8) | ((size as u64) << 16) | (direction << 30)
}
const fn io(number: u64) -> u64 {
    ioc(0, number, 0)
}
const fn iow<T>(number: u64) -> u64 {
    ioc(IOC_WRITE, number, size_of::<T>())
}
const fn iowr<T>(number: u64) -> u64 {
    ioc(IOC_READ | IOC_WRITE, number, size_of::<T>())
}

pub const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
pub const DRM_CAP_VBLANK_HIGH_CRTC: u64 = 0x2;
pub const DRM_CAP_DUMB_PREFERRED_DEPTH: u64 = 0x3;
pub const DRM_CAP_DUMB_PREFER_SHADOW: u64 = 0x4;
pub const DRM_CAP_PRIME: u64 = 0x5;
pub const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub const DRM_CAP_ASYNC_PAGE_FLIP: u64 = 0x7;
pub const DRM_CAP_CURSOR_WIDTH: u64 = 0x8;
pub const DRM_CAP_CURSOR_HEIGHT: u64 = 0x9;
pub const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;
pub const DRM_CAP_PAGE_FLIP_TARGET: u64 = 0x11;
pub const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
pub const DRM_CAP_SYNCOBJ: u64 = 0x13;
pub const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
pub const DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP: u64 = 0x15;
pub const DRM_PRIME_CAP_IMPORT: u64 = 1;
pub const DRM_PRIME_CAP_EXPORT: u64 = 2;
/// Flags shared by PRIME and syncobj fd conversion ioctls.
pub const DRM_CLOEXEC: u32 = 0x1;
pub const DRM_RDWR: u32 = 0x2;
pub const DRM_CLIENT_CAP_STEREO_3D: u64 = 1;
pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
pub const DRM_CLIENT_CAP_ASPECT_RATIO: u64 = 4;
pub const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
pub const DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;
pub const DRM_CLIENT_CAP_PLANE_COLOR_PIPELINE: u64 = 7;
pub const DRM_CLIENT_CAP_OBJECT_COLOROP: u64 = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: u64,
    pub name: u64,
    pub date_len: u64,
    pub date: u64,
    pub desc_len: u64,
    pub desc: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmAuth {
    pub magic: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmGetCap {
    pub capability: u64,
    pub value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSetVersion {
    pub drm_di_major: i32,
    pub drm_di_minor: i32,
    pub drm_dd_major: i32,
    pub drm_dd_minor: i32,
}

pub const DRM_DISPLAY_MODE_LEN: usize = 32;
pub const DRM_PROP_NAME_LEN: usize = 32;
pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_DISCONNECTED: u32 = 2;
pub const DRM_MODE_UNKNOWNCONNECTION: u32 = 3;
pub const DRM_MODE_FB_INTERLACED: u32 = 1;
pub const DRM_MODE_FB_MODIFIERS: u32 = 2;
pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 1;
pub const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 2;
pub const DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE: u32 = 4;
pub const DRM_MODE_PAGE_FLIP_TARGET_RELATIVE: u32 = 8;
pub const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;
pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub const DRM_MODE_OBJECT_ENCODER: u32 = 0xe0e0_e0e0;
pub const DRM_MODE_OBJECT_MODE: u32 = 0xdede_dede;
pub const DRM_MODE_OBJECT_PROPERTY: u32 = 0xb0b0_b0b0;
pub const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;
pub const DRM_MODE_OBJECT_BLOB: u32 = 0xbbbb_bbbb;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
pub const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_PROP_BITMASK: u32 = 1 << 5;
pub const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
pub const DRM_MODE_PROP_SIGNED_RANGE: u32 = 2 << 6;
pub const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub type_: u32,
    pub name: [u8; DRM_DISPLAY_MODE_LEN],
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeModeInfo,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCrtcLut {
    pub crtc_id: u32,
    pub gamma_size: u32,
    pub red: u64,
    pub green: u64,
    pub blue: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeGetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeGetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeSetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_h: u32,
    pub src_w: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeGetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeGetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModePropertyEnum {
    pub value: u64,
    pub name: [u8; DRM_PROP_NAME_LEN],
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeGetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; DRM_PROP_NAME_LEN],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeConnectorSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub connector_id: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeObjSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeGetBlob {
    pub blob_id: u32,
    pub length: u32,
    pub data: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCreateBlob {
    pub data: u64,
    pub length: u32,
    pub blob_id: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeDestroyBlob {
    pub blob_id: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeFbCmd {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeFbDirtyCmd {
    pub fb_id: u32,
    pub flags: u32,
    pub color: u32,
    pub num_clips: u32,
    pub clips_ptr: u64,
}
pub const DRM_MODE_CURSOR_BO: u32 = 0x01;
pub const DRM_MODE_CURSOR_MOVE: u32 = 0x02;
pub const DRM_MODE_CURSOR_FLAGS: u32 = 0x03;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCursor {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCursor2 {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCrtcPageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeCreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeMapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmModeDestroyDumb {
    pub handle: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrmModeAtomic {
    pub flags: u32,
    pub count_objs: u32,
    pub objs_ptr: u64,
    pub count_props_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub reserved: u64,
    pub user_data: u64,
}
const _: () = assert!(core::mem::size_of::<DrmModeAtomic>() == 56);
pub const DRM_MODE_ATOMIC_FLAGS: u32 = DRM_MODE_ATOMIC_TEST_ONLY
    | DRM_MODE_ATOMIC_NONBLOCK
    | DRM_MODE_ATOMIC_ALLOW_MODESET
    | DRM_MODE_PAGE_FLIP_EVENT
    | DRM_MODE_PAGE_FLIP_ASYNC;
impl DrmModeAtomic {
    pub fn validate(self, max_objects: u32) -> Result<Self, DrmError> {
        if self.flags & !DRM_MODE_ATOMIC_FLAGS != 0
            || self.reserved != 0
            || self.count_objs == 0
            || self.count_objs > max_objects
            || (self.count_objs != 0
                && (self.objs_ptr == 0
                    || self.count_props_ptr == 0
                    || self.props_ptr == 0
                    || self.prop_values_ptr == 0))
        {
            Err(DrmError::InvalidFlags)
        } else {
            Ok(self)
        }
    }
}

pub const DRM_VBLANK_ABSOLUTE: u32 = 0;
pub const DRM_VBLANK_RELATIVE: u32 = 1;
pub const DRM_VBLANK_EVENT: u32 = 0x0400_0000;
pub const DRM_VBLANK_FLIP: u32 = 0x0800_0000;
pub const DRM_VBLANK_NEXTONMISS: u32 = 0x1000_0000;
pub const DRM_VBLANK_SECONDARY: u32 = 0x2000_0000;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmWaitVblankRequest {
    pub type_: u32,
    pub sequence: u32,
    pub signal: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmWaitVblankReply {
    pub type_: u32,
    pub sequence: u32,
    pub tval_sec: i64,
    pub tval_usec: i64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union DrmWaitVblank {
    pub request: DrmWaitVblankRequest,
    pub reply: DrmWaitVblankReply,
}

pub const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1;
pub const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE: u32 = 1;
pub const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE: u32 = 2;
pub const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1;
pub const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE: u32 = 2;
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1;
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 2;
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 4;
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE: u32 = 8;
pub const DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED: u32 = 1;
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrmSyncobjCreate {
    pub handle: u32,
    pub flags: u32,
}
const _: () = assert!(core::mem::size_of::<DrmSyncobjCreate>() == 8);
impl DrmSyncobjCreate {
    pub const fn validate(self) -> Result<bool, DrmError> {
        if self.handle != 0 || self.flags & !DRM_SYNCOBJ_CREATE_SIGNALED != 0 {
            Err(DrmError::InvalidFlags)
        } else {
            Ok(self.flags & DRM_SYNCOBJ_CREATE_SIGNALED != 0)
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrmSyncobjDestroy {
    pub handle: u32,
    pub pad: u32,
}
const _: () = assert!(core::mem::size_of::<DrmSyncobjDestroy>() == 8);
impl DrmSyncobjDestroy {
    pub const fn validate(self) -> Result<u32, DrmError> {
        if self.handle == 0 {
            Err(DrmError::InvalidHandle)
        } else {
            Ok(self.handle)
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSyncobjTransfer {
    pub src_handle: u32,
    pub dst_handle: u32,
    pub src_point: u64,
    pub dst_point: u64,
    pub flags: u32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSyncobjWait {
    pub handles: u64,
    pub timeout_nsec: i64,
    pub count_handles: u32,
    pub flags: u32,
    pub first_signaled: u32,
    pub pad: u32,
    pub deadline_nsec: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSyncobjTimelineWait {
    pub handles: u64,
    pub points: u64,
    pub timeout_nsec: i64,
    pub count_handles: u32,
    pub flags: u32,
    pub first_signaled: u32,
    pub pad: u32,
    pub deadline_nsec: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSyncobjEventfd {
    pub handle: u32,
    pub flags: u32,
    pub point: u64,
    pub fd: i32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSyncobjArray {
    pub handles: u64,
    pub count_handles: u32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmSyncobjTimelineArray {
    pub handles: u64,
    pub points: u64,
    pub count_handles: u32,
    pub flags: u32,
}

pub const DRM_VIRTGPU_MAP: u64 = 0x01;
pub const DRM_VIRTGPU_EXECBUFFER: u64 = 0x02;
pub const DRM_VIRTGPU_GETPARAM: u64 = 0x03;
pub const DRM_VIRTGPU_RESOURCE_CREATE: u64 = 0x04;
pub const DRM_VIRTGPU_RESOURCE_INFO: u64 = 0x05;
pub const DRM_VIRTGPU_TRANSFER_FROM_HOST: u64 = 0x06;
pub const DRM_VIRTGPU_TRANSFER_TO_HOST: u64 = 0x07;
pub const DRM_VIRTGPU_WAIT: u64 = 0x08;
pub const DRM_VIRTGPU_GET_CAPS: u64 = 0x09;
pub const DRM_VIRTGPU_RESOURCE_CREATE_BLOB: u64 = 0x0a;
pub const DRM_VIRTGPU_CONTEXT_INIT: u64 = 0x0b;
pub const VIRTGPU_EXECBUF_FENCE_FD_IN: u32 = 0x01;
pub const VIRTGPU_EXECBUF_FENCE_FD_OUT: u32 = 0x02;
pub const VIRTGPU_EXECBUF_RING_IDX: u32 = 0x04;
pub const VIRTGPU_EXECBUF_FLAGS: u32 =
    VIRTGPU_EXECBUF_FENCE_FD_IN | VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX;
pub const VIRTGPU_EXECBUF_SYNCOBJ_RESET: u32 = 0x01;
pub const VIRTGPU_EXECBUF_SYNCOBJ_FLAGS: u32 = VIRTGPU_EXECBUF_SYNCOBJ_RESET;
pub const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
pub const VIRTGPU_PARAM_CAPSET_QUERY_FIX: u64 = 2;
pub const VIRTGPU_PARAM_RESOURCE_BLOB: u64 = 3;
pub const VIRTGPU_PARAM_HOST_VISIBLE: u64 = 4;
pub const VIRTGPU_PARAM_CROSS_DEVICE: u64 = 5;
pub const VIRTGPU_PARAM_CONTEXT_INIT: u64 = 6;
pub const VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
pub const VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME: u64 = 8;
pub const VIRTGPU_WAIT_NOWAIT: u32 = 1;
pub const VIRTGPU_DRM_CAPSET_VIRGL: u32 = 1;
pub const VIRTGPU_DRM_CAPSET_VIRGL2: u32 = 2;
pub const VIRTGPU_DRM_CAPSET_GFXSTREAM_VULKAN: u32 = 3;
pub const VIRTGPU_DRM_CAPSET_VENUS: u32 = 4;
pub const VIRTGPU_DRM_CAPSET_CROSS_DOMAIN: u32 = 5;
pub const VIRTGPU_DRM_CAPSET_DRM: u32 = 6;
pub const VIRTGPU_BLOB_MEM_GUEST: u32 = 0x0001;
pub const VIRTGPU_BLOB_MEM_HOST3D: u32 = 0x0002;
pub const VIRTGPU_BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;
pub const VIRTGPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;
pub const VIRTGPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;
pub const VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 0x0004;
pub const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
pub const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;
pub const VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK: u64 = 0x0003;
pub const VIRTGPU_CONTEXT_PARAM_DEBUG_NAME: u64 = 0x0004;
pub const VIRTGPU_EVENT_FENCE_SIGNALED: u32 = 0x9000_0000;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuMap {
    pub offset: u64,
    pub handle: u32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuExecbufferSyncobj {
    pub handle: u32,
    pub flags: u32,
    pub point: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuExecbuffer {
    pub flags: u32,
    pub size: u32,
    pub command: u64,
    pub bo_handles: u64,
    pub num_bo_handles: u32,
    pub fence_fd: i32,
    pub ring_idx: u32,
    pub syncobj_stride: u32,
    pub num_in_syncobjs: u32,
    pub num_out_syncobjs: u32,
    pub in_syncobjs: u64,
    pub out_syncobjs: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuGetparam {
    pub param: u64,
    pub value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuResourceCreate {
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
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub stride: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuResourceInfo {
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub blob_mem: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpu3dBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpu3dTransfer {
    pub bo_handle: u32,
    pub box_: DrmVirtgpu3dBox,
    pub level: u32,
    pub offset: u32,
    pub stride: u32,
    pub layer_stride: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpu3dWait {
    pub handle: u32,
    pub flags: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuGetCaps {
    pub cap_set_id: u32,
    pub cap_set_ver: u32,
    pub addr: u64,
    pub size: u32,
    pub pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuResourceCreateBlob {
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u64,
    pub pad: u32,
    pub cmd_size: u32,
    pub cmd: u64,
    pub blob_id: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuContextSetParam {
    pub param: u64,
    pub value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DrmVirtgpuContextInit {
    pub num_params: u32,
    pub pad: u32,
    pub ctx_set_params: u64,
}

pub const DRM_IOCTL_VERSION: u64 = iowr::<DrmVersion>(0x00);
pub const DRM_IOCTL_GET_MAGIC: u64 = ioc(IOC_READ, 0x02, size_of::<DrmAuth>());
pub const DRM_IOCTL_SET_VERSION: u64 = iowr::<DrmSetVersion>(0x07);
pub const DRM_IOCTL_GEM_CLOSE: u64 = iow::<DrmGemClose>(0x09);
pub const DRM_IOCTL_GET_CAP: u64 = iowr::<DrmGetCap>(0x0c);
pub const DRM_IOCTL_SET_CLIENT_CAP: u64 = iow::<DrmSetClientCap>(0x0d);
pub const DRM_IOCTL_AUTH_MAGIC: u64 = iow::<DrmAuth>(0x11);
pub const DRM_IOCTL_SET_MASTER: u64 = io(0x1e);
pub const DRM_IOCTL_DROP_MASTER: u64 = io(0x1f);
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u64 = iowr::<DrmPrimeHandle>(0x2d);
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u64 = iowr::<DrmPrimeHandle>(0x2e);
pub const DRM_IOCTL_WAIT_VBLANK: u64 = iowr::<DrmWaitVblank>(0x3a);
pub const DRM_IOCTL_MODE_GETRESOURCES: u64 = iowr::<DrmModeCardRes>(0xa0);
pub const DRM_IOCTL_MODE_GETCRTC: u64 = iowr::<DrmModeCrtc>(0xa1);
pub const DRM_IOCTL_MODE_SETCRTC: u64 = iowr::<DrmModeCrtc>(0xa2);
pub const DRM_IOCTL_MODE_CURSOR: u64 = iowr::<DrmModeCursor>(0xa3);
pub const DRM_IOCTL_MODE_GETGAMMA: u64 = iowr::<DrmModeCrtcLut>(0xa4);
pub const DRM_IOCTL_MODE_SETGAMMA: u64 = iowr::<DrmModeCrtcLut>(0xa5);
pub const DRM_IOCTL_MODE_GETENCODER: u64 = iowr::<DrmModeGetEncoder>(0xa6);
pub const DRM_IOCTL_MODE_GETCONNECTOR: u64 = iowr::<DrmModeGetConnector>(0xa7);
pub const DRM_IOCTL_MODE_GETPROPERTY: u64 = iowr::<DrmModeGetProperty>(0xaa);
pub const DRM_IOCTL_MODE_SETPROPERTY: u64 = iowr::<DrmModeConnectorSetProperty>(0xab);
pub const DRM_IOCTL_MODE_GETPROPBLOB: u64 = iowr::<DrmModeGetBlob>(0xac);
pub const DRM_IOCTL_MODE_GETFB: u64 = iowr::<DrmModeFbCmd>(0xad);
pub const DRM_IOCTL_MODE_ADDFB: u64 = iowr::<DrmModeFbCmd>(0xae);
pub const DRM_IOCTL_MODE_RMFB: u64 = iowr::<u32>(0xaf);
pub const DRM_IOCTL_MODE_PAGE_FLIP: u64 = iowr::<DrmModeCrtcPageFlip>(0xb0);
pub const DRM_IOCTL_MODE_DIRTYFB: u64 = iowr::<DrmModeFbDirtyCmd>(0xb1);
pub const DRM_IOCTL_MODE_CREATE_DUMB: u64 = iowr::<DrmModeCreateDumb>(0xb2);
pub const DRM_IOCTL_MODE_MAP_DUMB: u64 = iowr::<DrmModeMapDumb>(0xb3);
pub const DRM_IOCTL_MODE_DESTROY_DUMB: u64 = iowr::<DrmModeDestroyDumb>(0xb4);
pub const DRM_IOCTL_MODE_GETPLANERESOURCES: u64 = iowr::<DrmModeGetPlaneRes>(0xb5);
pub const DRM_IOCTL_MODE_GETPLANE: u64 = iowr::<DrmModeGetPlane>(0xb6);
pub const DRM_IOCTL_MODE_SETPLANE: u64 = iowr::<DrmModeSetPlane>(0xb7);
pub const DRM_IOCTL_MODE_ADDFB2: u64 = iowr::<DrmModeFbCmd2>(0xb8);
pub const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u64 = iowr::<DrmModeObjGetProperties>(0xb9);
pub const DRM_IOCTL_MODE_OBJ_SETPROPERTY: u64 = iowr::<DrmModeObjSetProperty>(0xba);
pub const DRM_IOCTL_MODE_CURSOR2: u64 = iowr::<DrmModeCursor2>(0xbb);
pub const DRM_IOCTL_MODE_ATOMIC: u64 = iowr::<DrmModeAtomic>(0xbc);
pub const DRM_IOCTL_MODE_CREATEPROPBLOB: u64 = iowr::<DrmModeCreateBlob>(0xbd);
pub const DRM_IOCTL_MODE_DESTROYPROPBLOB: u64 = iowr::<DrmModeDestroyBlob>(0xbe);
pub const DRM_IOCTL_MODE_GETFB2: u64 = iowr::<DrmModeFbCmd2>(0xce);
pub const DRM_IOCTL_SYNCOBJ_CREATE: u64 = iowr::<DrmSyncobjCreate>(0xbf);
pub const DRM_IOCTL_SYNCOBJ_DESTROY: u64 = iowr::<DrmSyncobjDestroy>(0xc0);
pub const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: u64 = iowr::<DrmSyncobjHandle>(0xc1);
pub const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u64 = iowr::<DrmSyncobjHandle>(0xc2);
pub const DRM_IOCTL_SYNCOBJ_WAIT: u64 = iowr::<DrmSyncobjWait>(0xc3);
pub const DRM_IOCTL_SYNCOBJ_RESET: u64 = iowr::<DrmSyncobjArray>(0xc4);
pub const DRM_IOCTL_SYNCOBJ_SIGNAL: u64 = iowr::<DrmSyncobjArray>(0xc5);
pub const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: u64 = iowr::<DrmSyncobjTimelineWait>(0xca);
pub const DRM_IOCTL_SYNCOBJ_QUERY: u64 = iowr::<DrmSyncobjTimelineArray>(0xcb);
pub const DRM_IOCTL_SYNCOBJ_TRANSFER: u64 = iowr::<DrmSyncobjTransfer>(0xcc);
pub const DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL: u64 = iowr::<DrmSyncobjTimelineArray>(0xcd);
pub const DRM_IOCTL_SYNCOBJ_EVENTFD: u64 = iowr::<DrmSyncobjEventfd>(0xcf);
pub const DRM_IOCTL_VIRTGPU_MAP: u64 = iowr::<DrmVirtgpuMap>(0x41);
pub const DRM_IOCTL_VIRTGPU_EXECBUFFER: u64 = iowr::<DrmVirtgpuExecbuffer>(0x42);
pub const DRM_IOCTL_VIRTGPU_GETPARAM: u64 = iowr::<DrmVirtgpuGetparam>(0x43);
pub const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE: u64 = iowr::<DrmVirtgpuResourceCreate>(0x44);
pub const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: u64 = iowr::<DrmVirtgpuResourceInfo>(0x45);
pub const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: u64 = iowr::<DrmVirtgpu3dTransfer>(0x46);
pub const DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST: u64 = iowr::<DrmVirtgpu3dTransfer>(0x47);
pub const DRM_IOCTL_VIRTGPU_WAIT: u64 = iowr::<DrmVirtgpu3dWait>(0x48);
pub const DRM_IOCTL_VIRTGPU_GET_CAPS: u64 = iowr::<DrmVirtgpuGetCaps>(0x49);
pub const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB: u64 = iowr::<DrmVirtgpuResourceCreateBlob>(0x4a);
pub const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: u64 = iowr::<DrmVirtgpuContextInit>(0x4b);

macro_rules! layout {
    ($t:ty, $size:expr, $align:expr) => {
        const _: [(); $size] = [(); size_of::<$t>()];
        const _: [(); $align] = [(); align_of::<$t>()];
    };
}
// Values below are from tests/guest/graphics/drm-uapi-oracle.c, compiled
// against the native x86_64 Linux DRM headers.
layout!(DrmVersion, 64, 8);
layout!(DrmAuth, 4, 4);
layout!(DrmGetCap, 16, 8);
layout!(DrmSetClientCap, 16, 8);
layout!(DrmGemClose, 8, 4);
layout!(DrmPrimeHandle, 12, 4);
layout!(DrmSetVersion, 16, 4);
layout!(DrmModeModeInfo, 68, 4);
layout!(DrmModeCardRes, 64, 8);
layout!(DrmModeCrtc, 104, 8);
layout!(DrmModeCrtcLut, 32, 8);
layout!(DrmModeGetEncoder, 20, 4);
layout!(DrmModeGetConnector, 80, 8);
layout!(DrmModeSetPlane, 48, 4);
layout!(DrmModeGetPlane, 32, 8);
layout!(DrmModeGetPlaneRes, 16, 8);
layout!(DrmModePropertyEnum, 40, 8);
layout!(DrmModeGetProperty, 64, 8);
layout!(DrmModeConnectorSetProperty, 16, 8);
layout!(DrmModeObjGetProperties, 32, 8);
layout!(DrmModeObjSetProperty, 24, 8);
layout!(DrmModeGetBlob, 16, 8);
layout!(DrmModeCreateBlob, 16, 8);
layout!(DrmModeDestroyBlob, 4, 4);
layout!(DrmModeFbCmd, 28, 4);
layout!(DrmModeFbCmd2, 104, 8);
layout!(DrmModeFbDirtyCmd, 24, 8);
layout!(DrmModeCursor, 28, 4);
layout!(DrmModeCursor2, 36, 4);
layout!(DrmModeCrtcPageFlip, 24, 8);
layout!(DrmModeCreateDumb, 32, 8);
layout!(DrmModeMapDumb, 16, 8);
layout!(DrmModeDestroyDumb, 4, 4);
layout!(DrmModeAtomic, 56, 8);
layout!(DrmWaitVblankRequest, 16, 8);
layout!(DrmWaitVblankReply, 24, 8);
layout!(DrmWaitVblank, 24, 8);
layout!(DrmSyncobjCreate, 8, 4);
layout!(DrmSyncobjDestroy, 8, 4);
layout!(DrmSyncobjHandle, 16, 4);
layout!(DrmSyncobjTransfer, 32, 8);
layout!(DrmSyncobjWait, 40, 8);
layout!(DrmSyncobjTimelineWait, 48, 8);
layout!(DrmSyncobjEventfd, 24, 8);
layout!(DrmSyncobjArray, 16, 8);
layout!(DrmSyncobjTimelineArray, 24, 8);
layout!(DrmVirtgpuMap, 16, 8);
layout!(DrmVirtgpuExecbufferSyncobj, 16, 8);
layout!(DrmVirtgpuExecbuffer, 64, 8);
layout!(DrmVirtgpuGetparam, 16, 8);
layout!(DrmVirtgpuResourceCreate, 56, 4);
layout!(DrmVirtgpuResourceInfo, 16, 4);
layout!(DrmVirtgpu3dBox, 24, 4);
layout!(DrmVirtgpu3dTransfer, 44, 4);
layout!(DrmVirtgpu3dWait, 8, 4);
layout!(DrmVirtgpuGetCaps, 24, 8);
layout!(DrmVirtgpuResourceCreateBlob, 48, 8);
layout!(DrmVirtgpuContextSetParam, 16, 8);
layout!(DrmVirtgpuContextInit, 16, 8);

macro_rules! field_offset {
    ($t:ty, $field:ident, $offset:expr) => {
        const _: [(); $offset] = [(); offset_of!($t, $field)];
    };
}
field_offset!(DrmVersion, name_len, 16);
field_offset!(DrmVersion, name, 24);
field_offset!(DrmVersion, date_len, 32);
field_offset!(DrmVersion, date, 40);
field_offset!(DrmVersion, desc_len, 48);
field_offset!(DrmVersion, desc, 56);
field_offset!(DrmPrimeHandle, fd, 8);
field_offset!(DrmModeModeInfo, vrefresh, 24);
field_offset!(DrmModeModeInfo, flags, 28);
field_offset!(DrmModeModeInfo, type_, 32);
field_offset!(DrmModeModeInfo, name, 36);
field_offset!(DrmModeCardRes, count_fbs, 32);
field_offset!(DrmModeCardRes, min_width, 48);
field_offset!(DrmModeCardRes, max_height, 60);
field_offset!(DrmVirtgpuExecbufferSyncobj, flags, 4);
field_offset!(DrmVirtgpuExecbufferSyncobj, point, 8);
field_offset!(DrmVirtgpuResourceCreateBlob, size, 16);
field_offset!(DrmVirtgpuResourceCreateBlob, pad, 24);
field_offset!(DrmVirtgpuResourceCreateBlob, cmd, 32);
field_offset!(DrmVirtgpuResourceCreateBlob, blob_id, 40);
field_offset!(DrmVirtgpuContextInit, ctx_set_params, 8);
field_offset!(DrmModeCrtc, count_connectors, 8);
field_offset!(DrmModeCrtc, crtc_id, 12);
field_offset!(DrmModeCrtc, mode, 36);
field_offset!(DrmModeCrtcLut, red, 8);
field_offset!(DrmModeCrtcLut, blue, 24);
field_offset!(DrmModeGetConnector, count_modes, 32);
field_offset!(DrmModeGetConnector, connector_id, 48);
field_offset!(DrmModeGetConnector, pad, 76);
field_offset!(DrmModeSetPlane, crtc_x, 16);
field_offset!(DrmModeSetPlane, src_x, 32);
field_offset!(DrmModeSetPlane, src_w, 44);
field_offset!(DrmModeGetPlane, format_type_ptr, 24);
field_offset!(DrmModeGetPlaneRes, count_planes, 8);
field_offset!(DrmModePropertyEnum, name, 8);
field_offset!(DrmModeGetProperty, prop_id, 16);
field_offset!(DrmModeGetProperty, name, 24);
field_offset!(DrmModeGetProperty, count_values, 56);
field_offset!(DrmModeConnectorSetProperty, prop_id, 8);
field_offset!(DrmModeObjGetProperties, count_props, 16);
field_offset!(DrmModeObjGetProperties, obj_type, 24);
field_offset!(DrmModeObjSetProperty, prop_id, 8);
field_offset!(DrmModeObjSetProperty, obj_type, 16);
field_offset!(DrmModeGetBlob, data, 8);
field_offset!(DrmModeCreateBlob, length, 8);
field_offset!(DrmModeFbCmd, handle, 24);
field_offset!(DrmModeFbCmd2, handles, 20);
field_offset!(DrmModeFbCmd2, pitches, 36);
field_offset!(DrmModeFbCmd2, offsets, 52);
field_offset!(DrmModeFbCmd2, modifier, 72);
field_offset!(DrmModeFbDirtyCmd, clips_ptr, 16);
field_offset!(DrmModeCursor, handle, 24);
field_offset!(DrmModeCursor2, hot_x, 28);
field_offset!(DrmModeCrtcPageFlip, user_data, 16);
field_offset!(DrmModeCreateDumb, handle, 16);
field_offset!(DrmModeCreateDumb, size, 24);
field_offset!(DrmModeMapDumb, offset, 8);
field_offset!(DrmModeAtomic, objs_ptr, 8);
field_offset!(DrmModeAtomic, user_data, 48);
field_offset!(DrmWaitVblankRequest, signal, 8);
field_offset!(DrmWaitVblankReply, tval_sec, 8);
field_offset!(DrmSyncobjHandle, fd, 8);
field_offset!(DrmSyncobjTransfer, src_point, 8);
field_offset!(DrmSyncobjTransfer, flags, 24);
field_offset!(DrmSyncobjWait, timeout_nsec, 8);
field_offset!(DrmSyncobjWait, deadline_nsec, 32);
field_offset!(DrmSyncobjTimelineWait, points, 8);
field_offset!(DrmSyncobjTimelineWait, deadline_nsec, 40);
field_offset!(DrmSyncobjEventfd, point, 8);
field_offset!(DrmSyncobjEventfd, fd, 16);
field_offset!(DrmSyncobjArray, count_handles, 8);
field_offset!(DrmSyncobjTimelineArray, count_handles, 16);
field_offset!(DrmVirtgpuMap, handle, 8);
field_offset!(DrmVirtgpuExecbuffer, command, 8);
field_offset!(DrmVirtgpuExecbuffer, in_syncobjs, 48);
field_offset!(DrmVirtgpuResourceCreate, bo_handle, 40);
field_offset!(DrmVirtgpu3dTransfer, box_, 4);
field_offset!(DrmVirtgpuGetCaps, addr, 8);

#[cfg(test)]
mod uapi_tests {
    use super::*;
    macro_rules! linux_ioctl {
        ($ioctl:ident, $value:expr) => {
            assert_eq!($ioctl, $value, stringify!($ioctl));
        };
    }
    #[test]
    fn linux_ioctl_encodings() {
        linux_ioctl!(DRM_IOCTL_VERSION, 0xc040_6400);
        linux_ioctl!(DRM_IOCTL_GET_MAGIC, 0x8004_6402);
        linux_ioctl!(DRM_IOCTL_SET_VERSION, 0xc010_6407);
        linux_ioctl!(DRM_IOCTL_GEM_CLOSE, 0x4008_6409);
        linux_ioctl!(DRM_IOCTL_GET_CAP, 0xc010_640c);
        linux_ioctl!(DRM_IOCTL_SET_CLIENT_CAP, 0x4010_640d);
        linux_ioctl!(DRM_IOCTL_AUTH_MAGIC, 0x4004_6411);
        linux_ioctl!(DRM_IOCTL_SET_MASTER, 0x641e);
        linux_ioctl!(DRM_IOCTL_DROP_MASTER, 0x641f);
        linux_ioctl!(DRM_IOCTL_PRIME_HANDLE_TO_FD, 0xc00c_642d);
        linux_ioctl!(DRM_IOCTL_PRIME_FD_TO_HANDLE, 0xc00c_642e);
        linux_ioctl!(DRM_IOCTL_WAIT_VBLANK, 0xc018_643a);
        linux_ioctl!(DRM_IOCTL_MODE_GETRESOURCES, 0xc040_64a0);
        linux_ioctl!(DRM_IOCTL_MODE_GETCRTC, 0xc068_64a1);
        linux_ioctl!(DRM_IOCTL_MODE_SETCRTC, 0xc068_64a2);
        linux_ioctl!(DRM_IOCTL_MODE_CURSOR, 0xc01c_64a3);
        linux_ioctl!(DRM_IOCTL_MODE_GETGAMMA, 0xc020_64a4);
        linux_ioctl!(DRM_IOCTL_MODE_SETGAMMA, 0xc020_64a5);
        linux_ioctl!(DRM_IOCTL_MODE_GETENCODER, 0xc014_64a6);
        linux_ioctl!(DRM_IOCTL_MODE_GETCONNECTOR, 0xc050_64a7);
        linux_ioctl!(DRM_IOCTL_MODE_GETPROPERTY, 0xc040_64aa);
        linux_ioctl!(DRM_IOCTL_MODE_SETPROPERTY, 0xc010_64ab);
        linux_ioctl!(DRM_IOCTL_MODE_GETPROPBLOB, 0xc010_64ac);
        linux_ioctl!(DRM_IOCTL_MODE_GETFB, 0xc01c_64ad);
        linux_ioctl!(DRM_IOCTL_MODE_ADDFB, 0xc01c_64ae);
        linux_ioctl!(DRM_IOCTL_MODE_RMFB, 0xc004_64af);
        linux_ioctl!(DRM_IOCTL_MODE_PAGE_FLIP, 0xc018_64b0);
        linux_ioctl!(DRM_IOCTL_MODE_DIRTYFB, 0xc018_64b1);
        linux_ioctl!(DRM_IOCTL_MODE_CREATE_DUMB, 0xc020_64b2);
        linux_ioctl!(DRM_IOCTL_MODE_MAP_DUMB, 0xc010_64b3);
        linux_ioctl!(DRM_IOCTL_MODE_DESTROY_DUMB, 0xc004_64b4);
        linux_ioctl!(DRM_IOCTL_MODE_GETPLANERESOURCES, 0xc010_64b5);
        linux_ioctl!(DRM_IOCTL_MODE_GETPLANE, 0xc020_64b6);
        linux_ioctl!(DRM_IOCTL_MODE_SETPLANE, 0xc030_64b7);
        linux_ioctl!(DRM_IOCTL_MODE_ADDFB2, 0xc068_64b8);
        linux_ioctl!(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, 0xc020_64b9);
        linux_ioctl!(DRM_IOCTL_MODE_OBJ_SETPROPERTY, 0xc018_64ba);
        linux_ioctl!(DRM_IOCTL_MODE_CURSOR2, 0xc024_64bb);
        linux_ioctl!(DRM_IOCTL_MODE_ATOMIC, 0xc038_64bc);
        linux_ioctl!(DRM_IOCTL_MODE_CREATEPROPBLOB, 0xc010_64bd);
        linux_ioctl!(DRM_IOCTL_MODE_DESTROYPROPBLOB, 0xc004_64be);
        linux_ioctl!(DRM_IOCTL_MODE_GETFB2, 0xc068_64ce);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_CREATE, 0xc008_64bf);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_DESTROY, 0xc008_64c0);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, 0xc010_64c1);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, 0xc010_64c2);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_WAIT, 0xc028_64c3);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_RESET, 0xc010_64c4);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_SIGNAL, 0xc010_64c5);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, 0xc030_64ca);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_QUERY, 0xc018_64cb);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_TRANSFER, 0xc020_64cc);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, 0xc018_64cd);
        linux_ioctl!(DRM_IOCTL_SYNCOBJ_EVENTFD, 0xc018_64cf);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_MAP, 0xc010_6441);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_EXECBUFFER, 0xc040_6442);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_GETPARAM, 0xc010_6443);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, 0xc038_6444);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_RESOURCE_INFO, 0xc010_6445);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST, 0xc02c_6446);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST, 0xc02c_6447);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_WAIT, 0xc008_6448);
        linux_ioctl!(DRM_IOCTL_VIRTGPU_GET_CAPS, 0xc018_6449);
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmError {
    InvalidIoctl,
    InvalidSize,
    InvalidFlags,
    InvalidHandle,
    InvalidObject,
    PermissionDenied,
    Busy,
    NotMaster,
    NotFound,
    Overflow,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoctlDirection {
    None,
    Write,
    Read,
    ReadWrite,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedIoctl {
    pub direction: IoctlDirection,
    pub type_: u8,
    pub number: u8,
    pub size: u16,
}
impl DecodedIoctl {
    pub const fn decode(raw: u32) -> Self {
        let direction = match raw >> 30 {
            0 => IoctlDirection::None,
            1 => IoctlDirection::Write,
            2 => IoctlDirection::Read,
            _ => IoctlDirection::ReadWrite,
        };
        Self {
            direction,
            type_: ((raw >> 8) & 0xff) as u8,
            number: raw as u8,
            size: ((raw >> 16) & 0x3fff) as u16,
        }
    }
    pub const fn is_drm(self) -> bool {
        self.type_ == DRM_IOCTL_BASE
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmIoctl {
    Version,
    GetCap,
    SetClientCap,
    SetMaster,
    DropMaster,
    ModeAtomic,
    SyncobjCreate,
    SyncobjDestroy,
    SyncobjWait,
    RenderCommand(u8),
}
pub fn decode_ioctl(raw: u32) -> Result<DrmIoctl, DrmError> {
    let d = DecodedIoctl::decode(raw);
    if !d.is_drm() {
        return Err(DrmError::InvalidIoctl);
    }
    match d.number {
        0x00 => Ok(DrmIoctl::Version),
        0x0c => Ok(DrmIoctl::GetCap),
        0x0d => Ok(DrmIoctl::SetClientCap),
        0x1e => Ok(DrmIoctl::SetMaster),
        0x1f => Ok(DrmIoctl::DropMaster),
        0xbc => Ok(DrmIoctl::ModeAtomic),
        0xbf => Ok(DrmIoctl::SyncobjCreate),
        0xc0 => Ok(DrmIoctl::SyncobjDestroy),
        0xc3 => Ok(DrmIoctl::SyncobjWait),
        n if (DRM_COMMAND_BASE..DRM_COMMAND_END).contains(&n) => {
            Ok(DrmIoctl::RenderCommand(n - DRM_COMMAND_BASE))
        }
        _ => Err(DrmError::InvalidIoctl),
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ClientId(u64);
impl ClientId {
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ObjectId(u32);
impl ObjectId {
    pub const fn new(raw: u32) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientNode {
    Primary,
    Render,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientSnapshot {
    pub node: ClientNode,
    pub master: bool,
    pub authenticated: bool,
}
impl ClientSnapshot {
    pub fn can_render(self) -> bool {
        self.node == ClientNode::Render || self.authenticated || self.master
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmPlan<H> {
    AcquireMaster {
        client: ClientId,
    },
    DropMaster {
        client: ClientId,
    },
    AtomicCommit {
        client: ClientId,
        objects: H,
        test_only: bool,
    },
    CreateSyncobj {
        client: ClientId,
        signaled: bool,
    },
    DestroySyncobj {
        client: ClientId,
        object: H,
    },
    WaitSyncobj {
        client: ClientId,
        object: H,
    },
}
pub trait ObjectHandle: Copy + Eq {}
impl<T: Copy + Eq> ObjectHandle for T {}
pub trait ObjectResolver {
    type Handle: ObjectHandle;
    fn resolve(&self, id: ObjectId, kind: u32) -> Result<Self::Handle, DrmError>;
}
pub fn plan_set_master(client: ClientId, state: ClientSnapshot) -> Result<DrmPlan<()>, DrmError> {
    if state.node != ClientNode::Primary || state.master {
        Err(DrmError::PermissionDenied)
    } else {
        Ok(DrmPlan::AcquireMaster { client })
    }
}
pub fn plan_atomic<R: ObjectResolver>(
    client: ClientId,
    state: ClientSnapshot,
    request: DrmModeAtomic,
    object: ObjectId,
    kind: u32,
    resolver: &R,
) -> Result<DrmPlan<R::Handle>, DrmError> {
    if !state.master {
        return Err(DrmError::NotMaster);
    }
    let request = request.validate(4096)?;
    let handle = resolver.resolve(object, kind)?;
    Ok(DrmPlan::AtomicCommit {
        client,
        objects: handle,
        test_only: request.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0,
    })
}
pub fn plan_render<H: ObjectHandle>(
    client: ClientId,
    state: ClientSnapshot,
    handle: H,
) -> Result<DrmPlan<H>, DrmError> {
    if state.can_render() {
        Ok(DrmPlan::WaitSyncobj {
            client,
            object: handle,
        })
    } else {
        Err(DrmError::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone, Copy, Eq, PartialEq)]
    struct R;
    impl ObjectResolver for R {
        type Handle = u8;
        fn resolve(&self, _: ObjectId, _: u32) -> Result<u8, DrmError> {
            Ok(7)
        }
    }
    #[test]
    fn decode_and_layout_are_stable() {
        assert_eq!(
            decode_ioctl((DRM_IOCTL_BASE as u32) << 8 | 0xbf).unwrap(),
            DrmIoctl::SyncobjCreate
        );
        assert_eq!(core::mem::size_of::<DrmModeAtomic>(), 56);
    }
    #[test]
    fn decoder_does_not_admit_kms_as_render_command() {
        assert_eq!(
            decode_ioctl(DRM_IOCTL_VIRTGPU_MAP as u32),
            Ok(DrmIoctl::RenderCommand(1))
        );
        assert_eq!(
            decode_ioctl(DRM_IOCTL_MODE_GETRESOURCES as u32),
            Err(DrmError::InvalidIoctl)
        );
    }
    #[test]
    fn kms_requires_master_and_snapshots_handle() {
        let request = DrmModeAtomic {
            flags: DRM_MODE_ATOMIC_TEST_ONLY,
            count_objs: 1,
            objs_ptr: 1,
            count_props_ptr: 1,
            props_ptr: 1,
            prop_values_ptr: 1,
            reserved: 0,
            user_data: 0,
        };
        assert!(
            plan_atomic(
                ClientId::new(1).unwrap(),
                ClientSnapshot {
                    node: ClientNode::Primary,
                    master: false,
                    authenticated: false
                },
                request,
                ObjectId::new(1).unwrap(),
                DRM_MODE_OBJECT_CRTC,
                &R
            )
            .is_err()
        );
        let plan = plan_atomic(
            ClientId::new(1).unwrap(),
            ClientSnapshot {
                node: ClientNode::Primary,
                master: true,
                authenticated: false,
            },
            request,
            ObjectId::new(1).unwrap(),
            DRM_MODE_OBJECT_CRTC,
            &R,
        )
        .unwrap();
        assert!(matches!(
            plan,
            DrmPlan::AtomicCommit {
                objects: 7,
                test_only: true,
                ..
            }
        ));
    }
}
