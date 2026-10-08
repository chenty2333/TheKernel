// SPDX-License-Identifier: MIT
// Copyright (c) 2016 Intel Corporation
//
// Permission to use, copy, modify, distribute, and sell this software and its
// documentation for any purpose is hereby granted without fee, provided that
// the above copyright notice appear in all copies and that both that copyright
// notice and this permission notice appear in supporting documentation, and
// that the name of the copyright holders not be used in advertising or
// publicity pertaining to distribution of the software without specific,
// written prior permission. The copyright holders make no representations
// about the suitability of this software for any purpose. It is provided "as
// is" without express or implied warranty.
//
// THE COPYRIGHT HOLDERS DISCLAIM ALL WARRANTIES WITH REGARD TO THIS SOFTWARE,
// INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS, IN NO
// EVENT SHALL THE COPYRIGHT HOLDERS BE LIABLE FOR ANY SPECIAL, INDIRECT OR
// CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE,
// DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
// TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
// OF THIS SOFTWARE.

//! Function-level Rust translation of Linux v7.2.3 `drm_connector.c`.
//!
//! Connector policy, property definitions, EDID/status/mode selection, and
//! atomic-state algorithms remain here. DRM object/property registration,
//! usercopy, locking, and EDID/blob reference operations are explicit hooks.

extern crate alloc;
use alloc::{string::{String, ToString}, vec::Vec};
use core::cmp;

pub type DeviceId = u64;
pub type ConnectorId = u32;
pub type EncoderId = u32;
pub type PropertyId = u32;
pub type BlobId = u32;
pub type FwnodeId = u64;
pub type UserPtr = u64;
pub type KResult<T> = Result<T, i32>;

pub const EACCES: i32 = -13;
pub const EFAULT: i32 = -14;
pub const EINVAL: i32 = -22;
pub const ENODEV: i32 = -19;
pub const ENOENT: i32 = -2;
pub const ENOMEM: i32 = -12;
pub const EOPNOTSUPP: i32 = -95;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_0000;
pub const DRM_MODE_MATCH_TIMINGS: u32 = 1 << 0;
pub const DRM_MODE_MATCH_CLOCK: u32 = 1 << 1;
pub const DRM_MODE_MATCH_FLAGS: u32 = 1 << 2;
pub const DRM_MODE_MATCH_3D_FLAGS: u32 = 1 << 3;
pub const DRM_MODE_MATCH_ASPECT_RATIO: u32 = 1 << 4;
pub const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
pub const DRM_MODE_FLAG_DBLSCAN: u32 = 1 << 5;
pub const DRM_MODE_FLAG_3D_MASK: u32 = 0x1f << 14;
pub const DRM_MODE_FLAG_PIC_AR_MASK: u32 = 0x0f << 19;
pub const DRM_MODE_COLORIMETRY_COUNT: usize = 16;
pub const DRM_MODE_TV_MODE_MAX: usize = 8;
pub const DRM_CONNECTOR_HDMI_VENDOR_LEN: usize = 8;
pub const DRM_CONNECTOR_HDMI_PRODUCT_LEN: usize = 16;

pub const DRM_MODE_CONNECTOR_UNKNOWN: u32 = 0;
pub const DRM_MODE_CONNECTOR_VGA: u32 = 1;
pub const DRM_MODE_CONNECTOR_DVII: u32 = 2;
pub const DRM_MODE_CONNECTOR_DVID: u32 = 3;
pub const DRM_MODE_CONNECTOR_DVIA: u32 = 4;
pub const DRM_MODE_CONNECTOR_COMPOSITE: u32 = 5;
pub const DRM_MODE_CONNECTOR_SVIDEO: u32 = 6;
pub const DRM_MODE_CONNECTOR_LVDS: u32 = 7;
pub const DRM_MODE_CONNECTOR_COMPONENT: u32 = 8;
pub const DRM_MODE_CONNECTOR_9PINDIN: u32 = 9;
pub const DRM_MODE_CONNECTOR_DISPLAYPORT: u32 = 10;
pub const DRM_MODE_CONNECTOR_HDMIA: u32 = 11;
pub const DRM_MODE_CONNECTOR_HDMIB: u32 = 12;
pub const DRM_MODE_CONNECTOR_TV: u32 = 13;
pub const DRM_MODE_CONNECTOR_EDP: u32 = 14;
pub const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub const DRM_MODE_CONNECTOR_DSI: u32 = 16;
pub const DRM_MODE_CONNECTOR_DPI: u32 = 17;
pub const DRM_MODE_CONNECTOR_WRITEBACK: u32 = 18;
pub const DRM_MODE_CONNECTOR_SPI: u32 = 19;
pub const DRM_MODE_CONNECTOR_USB: u32 = 20;

pub const DRM_MODE_PANEL_ORIENTATION_UNKNOWN: u32 = u32::MAX;
pub const DRM_MODE_PANEL_ORIENTATION_NORMAL: u32 = 0;
pub const DRM_MODE_PANEL_ORIENTATION_BOTTOM_UP: u32 = 1;
pub const DRM_MODE_PANEL_ORIENTATION_LEFT_UP: u32 = 2;
pub const DRM_MODE_PANEL_ORIENTATION_RIGHT_UP: u32 = 3;
pub const DRM_MODE_COLORIMETRY_DEFAULT: u32 = 0;
pub const DRM_MODE_COLORIMETRY_SMPTE_170M_YCC: u32 = 1;
pub const DRM_MODE_COLORIMETRY_BT709_YCC: u32 = 2;
pub const DRM_MODE_COLORIMETRY_XVYCC_601: u32 = 3;
pub const DRM_MODE_COLORIMETRY_XVYCC_709: u32 = 4;
pub const DRM_MODE_COLORIMETRY_SYCC_601: u32 = 5;
pub const DRM_MODE_COLORIMETRY_OPYCC_601: u32 = 6;
pub const DRM_MODE_COLORIMETRY_OPRGB: u32 = 7;
pub const DRM_MODE_COLORIMETRY_BT2020_CYCC: u32 = 8;
pub const DRM_MODE_COLORIMETRY_BT2020_RGB: u32 = 9;
pub const DRM_MODE_COLORIMETRY_BT2020_YCC: u32 = 10;
pub const DRM_MODE_COLORIMETRY_DCI_P3_RGB_D65: u32 = 11;
pub const DRM_MODE_COLORIMETRY_DCI_P3_RGB_THEATER: u32 = 12;
pub const DRM_MODE_COLORIMETRY_RGB_WIDE_FIXED: u32 = 13;
pub const DRM_MODE_COLORIMETRY_RGB_WIDE_FLOAT: u32 = 14;
pub const DRM_MODE_COLORIMETRY_BT601_YCC: u32 = 15;
pub const DRM_HDMI_BROADCAST_RGB_AUTO: u32 = 0;
pub const DRM_HDMI_BROADCAST_RGB_FULL: u32 = 1;
pub const DRM_HDMI_BROADCAST_RGB_LIMITED: u32 = 2;
pub const DRM_OUTPUT_COLOR_FORMAT_RGB444: u32 = 0;
pub const DRM_OUTPUT_COLOR_FORMAT_YCBCR420: u32 = 3;
pub const DRM_OUTPUT_COLOR_FORMAT_YCBCR422: u32 = 2;
pub const DRM_OUTPUT_COLOR_FORMAT_YCBCR444: u32 = 1;
pub const DRM_MODE_CONTENT_TYPE_NO_DATA: u32 = 0;
pub const PRIVACY_SCREEN_DISABLED: u64 = 0;
pub const PRIVACY_SCREEN_ENABLED: u64 = 1;
pub const PRIVACY_SCREEN_DISABLED_LOCKED: u64 = 2;
pub const PRIVACY_SCREEN_ENABLED_LOCKED: u64 = 3;

const CONNECTOR_TYPE_NAMES: [&str; 21] = [
    "Unknown",
    "VGA",
    "DVI-I",
    "DVI-D",
    "DVI-A",
    "Composite",
    "SVIDEO",
    "LVDS",
    "Component",
    "DIN",
    "DP",
    "HDMI-A",
    "HDMI-B",
    "TV",
    "eDP",
    "Virtual",
    "DSI",
    "DPI",
    "Writeback",
    "SPI",
    "USB",
];
const SUBPIXEL_NAMES: [&str; 6] = [
    "Unknown",
    "Horizontal RGB",
    "Horizontal BGR",
    "Vertical RGB",
    "Vertical BGR",
    "None",
];
const CONNECTOR_STATUS_UNKNOWN: i32 = 0;
const CONNECTOR_STATUS_CONNECTED: i32 = 1;
const CONNECTOR_STATUS_DISCONNECTED: i32 = 2;
const CONNECTOR_FORCE_UNSPECIFIED: u32 = 0;
const CONNECTOR_FORCE_OFF: u32 = 1;
const CONNECTOR_FORCE_ON: u32 = 2;
const CONNECTOR_FORCE_ON_DIGITAL: u32 = 3;
const DRM_MODE_SCALE_NONE: u32 = 0;
const DRM_MODE_SCALE_FULLSCREEN: u32 = 1;
const DRM_MODE_SCALE_CENTER: u32 = 2;
const DRM_MODE_SCALE_ASPECT: u32 = 3;
const DRM_MODE_PICTURE_ASPECT_NONE: u32 = 0;
const DRM_MODE_PICTURE_ASPECT_4_3: u32 = 1;
const DRM_MODE_PICTURE_ASPECT_16_9: u32 = 2;
const DRM_MODE_CONTENT_TYPE_GRAPHICS: u32 = 1;
const DRM_MODE_CONTENT_TYPE_PHOTO: u32 = 2;
const DRM_MODE_CONTENT_TYPE_CINEMA: u32 = 3;
const DRM_MODE_CONTENT_TYPE_GAME: u32 = 4;
const DRM_MODE_SUBCONNECTOR_UNKNOWN: u32 = 0;
const DRM_MODE_SUBCONNECTOR_DVID: u32 = 3;
const DRM_MODE_SUBCONNECTOR_DVIA: u32 = 4;
const DRM_MODE_SUBCONNECTOR_COMPOSITE: u32 = 5;
const DRM_MODE_SUBCONNECTOR_SVIDEO: u32 = 6;
const DRM_MODE_SUBCONNECTOR_COMPONENT: u32 = 8;
const DRM_MODE_SUBCONNECTOR_SCART: u32 = 9;
const DRM_MODE_SUBCONNECTOR_VGA: u32 = 1;
const DRM_MODE_SUBCONNECTOR_HDMIA: u32 = 11;
const DRM_MODE_SUBCONNECTOR_DISPLAYPORT: u32 = 10;
const DRM_MODE_SUBCONNECTOR_WIRELESS: u32 = 18;
const DRM_MODE_SUBCONNECTOR_NATIVE: u32 = 15;
const DRM_MODE_TV_MODE_NTSC: u32 = 0;
const DRM_MODE_TV_MODE_NTSC_443: u32 = 1;
const DRM_MODE_TV_MODE_NTSC_J: u32 = 2;
const DRM_MODE_TV_MODE_PAL: u32 = 3;
const DRM_MODE_TV_MODE_PAL_M: u32 = 4;
const DRM_MODE_TV_MODE_PAL_N: u32 = 5;
const DRM_MODE_TV_MODE_SECAM: u32 = 6;
const DRM_MODE_TV_MODE_MONOCHROME: u32 = 7;
const DRM_MODE_PANEL_TYPE_UNKNOWN: u32 = 0;
const DRM_MODE_PANEL_TYPE_OLED: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationState {
    Initializing,
    Registered,
    Unregistered,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectorStatus {
    Unknown,
    Connected,
    Disconnected,
    Other(i32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelOrientation {
    Unknown,
    Normal,
    BottomUp,
    LeftUp,
    RightUp,
    Other(u32),
}
impl Default for RegistrationState {
    fn default() -> Self {
        Self::Initializing
    }
}
impl Default for ConnectorStatus {
    fn default() -> Self {
        Self::Unknown
    }
}
impl Default for PanelOrientation {
    fn default() -> Self {
        Self::Unknown
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyKind {
    Blob,
    Bool,
    Range { min: u64, max: u64 },
    Enum,
    Bitmask,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumValue {
    pub value: u32,
    pub name: &'static str,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertySpec {
    pub name: &'static str,
    pub kind: PropertyKind,
    pub flags: u32,
    pub enums: Vec<EnumValue>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorProperties {
    pub edid: Option<PropertyId>,
    pub dpms: Option<PropertyId>,
    pub path: Option<PropertyId>,
    pub tile: Option<PropertyId>,
    pub link_status: Option<PropertyId>,
    pub panel_type: Option<PropertyId>,
    pub non_desktop: Option<PropertyId>,
    pub hdr_output_metadata: Option<PropertyId>,
    pub dp_subconnector: Option<PropertyId>,
    pub crtc_id: Option<PropertyId>,
    pub dvi_i_select: Option<PropertyId>,
    pub dvi_i_subconnector: Option<PropertyId>,
    pub tv_select_subconnector: Option<PropertyId>,
    pub tv_subconnector: Option<PropertyId>,
    pub content_type: Option<PropertyId>,
    pub tv_left: Option<PropertyId>,
    pub tv_right: Option<PropertyId>,
    pub tv_top: Option<PropertyId>,
    pub tv_bottom: Option<PropertyId>,
    pub legacy_tv_mode: Option<PropertyId>,
    pub tv_mode: Option<PropertyId>,
    pub tv_brightness: Option<PropertyId>,
    pub tv_contrast: Option<PropertyId>,
    pub tv_flicker_reduction: Option<PropertyId>,
    pub tv_overscan: Option<PropertyId>,
    pub tv_saturation: Option<PropertyId>,
    pub tv_hue: Option<PropertyId>,
    pub scaling_mode: Option<PropertyId>,
    pub aspect_ratio: Option<PropertyId>,
    pub suggested_x: Option<PropertyId>,
    pub suggested_y: Option<PropertyId>,
    pub vrr_capable: Option<PropertyId>,
    pub max_bpc: Option<PropertyId>,
    pub broadcast_rgb: Option<PropertyId>,
    pub colorspace: Option<PropertyId>,
    pub panel_orientation: Option<PropertyId>,
    pub privacy_sw: Option<PropertyId>,
    pub privacy_hw: Option<PropertyId>,
    pub panel_type_property: Option<PropertyId>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorState {
    pub link_status: u64,
    pub max_requested_bpc: u32,
    pub max_bpc: u32,
    pub privacy_screen_sw_state: u64,
    pub hdr_metadata: Option<Vec<u8>>,
    pub best_encoder: Option<EncoderId>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayInfo {
    pub width_mm: u32,
    pub height_mm: u32,
    pub subpixel_order: u32,
    pub panel_orientation: PanelOrientation,
    pub source_physical_address: u16,
    pub bus_formats: Vec<u32>,
    pub vics: Vec<u8>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayMode {
    pub name: String,
    pub status: i32,
    pub clock: u32,
    pub hdisplay: u32,
    pub hsync_start: u32,
    pub hsync_end: u32,
    pub htotal: u32,
    pub hskew: u32,
    pub vdisplay: u32,
    pub vsync_start: u32,
    pub vsync_end: u32,
    pub vtotal: u32,
    pub vscan: u32,
    pub flags: u32,
    pub mode_type: u32,
    pub aspect_ratio: u32,
    pub stereo_3d: bool,
    pub expose_to_userspace: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connector {
    pub id: ConnectorId,
    pub name: String,
    pub dev: DeviceId,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub index: u32,
    pub registration_state: RegistrationState,
    pub status: ConnectorStatus,
    pub force: u32,
    pub possible_encoders: u32,
    pub current_encoder: Option<EncoderId>,
    pub state: Option<ConnectorState>,
    pub display_info: DisplayInfo,
    pub edid_blob: Option<BlobId>,
    pub path_blob: Option<BlobId>,
    pub tile_blob: Option<BlobId>,
    pub edid_epoch_counter: u64,
    pub has_tile: bool,
    pub tile_group: Option<TileGroup>,
    pub tile_is_single_monitor: u32,
    pub num_h_tile: u32,
    pub num_v_tile: u32,
    pub tile_h_loc: u32,
    pub tile_v_loc: u32,
    pub tile_h_size: u32,
    pub tile_v_size: u32,
    pub ddc: Option<u64>,
    pub fwnode: Option<FwnodeId>,
    pub secondary_fwnode: Option<FwnodeId>,
    pub cec_physical_address: Option<u16>,
    pub privacy_screen: Option<u64>,
    pub privacy_screen_sw_property: Option<PropertyId>,
    pub privacy_screen_hw_property: Option<PropertyId>,
    pub vrr_capable_property: Option<PropertyId>,
    pub max_bpc_property: Option<PropertyId>,
    pub broadcast_rgb_property: Option<PropertyId>,
    pub colorspace_property: Option<PropertyId>,
    pub scaling_mode_property: Option<PropertyId>,
    pub modes: Vec<DisplayMode>,
    pub probed_modes: Vec<DisplayMode>,
    pub bus_formats: Vec<u32>,
    pub vics: Vec<u8>,
    pub hdmi_vendor: String,
    pub hdmi_product: String,
    pub hdmi_supported_formats: u64,
    pub hdmi_max_bpc: u32,
    pub hdmi_audio_device: Option<u64>,
    pub hdmi_callbacks_valid: bool,
    pub ycbcr_420_allowed: bool,
    pub atomic_callbacks_valid: bool,
    pub legacy_destroy_callback: bool,
    pub has_reset_callback: bool,
    pub has_late_register_callback: bool,
    pub has_early_unregister_callback: bool,
    pub has_oob_hotplug_callback: bool,
    pub panel_type: u32,
    pub non_desktop: bool,
    pub connected_to_crtc: bool,
    pub registration_on_global_list: bool,
    pub connector_list_member: bool,
    pub atomic_mode: bool,
    pub cmdline: Option<CmdlineMode>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CmdlineMode {
    pub force: u32,
    pub panel_orientation: PanelOrientation,
    pub name: String,
    pub xres: u32,
    pub yres: u32,
    pub refresh: u32,
    pub refresh_specified: bool,
    pub reduced_blanking: bool,
    pub margins: bool,
    pub interlace: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorDevice {
    pub id: DeviceId,
    pub registered: bool,
    pub modeset_supported: bool,
    pub atomic_modeset: bool,
    pub driver_atomic: bool,
    pub properties: ConnectorProperties,
    pub connectors: Vec<ConnectorId>,
    pub max_width: u32,
    pub max_height: u32,
    pub connector_id_limit: u32,
    pub next_connector_id: u32,
    pub next_type_id: u32,
    pub free_connector_ids: Vec<u32>,
    pub free_type_ids: Vec<(u32, u32)>,
    pub connectors_count: u32,
    pub global_connector_order: Vec<ConnectorId>,
    pub tile_groups: Vec<TileGroup>,
    pub next_tile_id: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Encoder {
    pub id: EncoderId,
    pub index: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TileGroup {
    pub id: u32,
    pub topology_id: [u8; 8],
    pub refcount: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorIterator {
    pub dev: DeviceId,
    pub cursor: usize,
    pub current: Option<ConnectorId>,
    pub held: Vec<ConnectorId>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectorFile {
    pub id: u64,
    pub stereo_allowed: bool,
    pub aspect_ratio_allowed: bool,
    pub atomic: bool,
    pub plane_color_pipeline: bool,
    pub is_master: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModeInfo {
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
    pub mode_type: u32,
    pub name: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GetConnectorArgs {
    pub connector_id: ConnectorId,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub encoder_id: EncoderId,
    pub count_encoders: u32,
    pub encoders_ptr: UserPtr,
    pub count_modes: u32,
    pub modes_ptr: UserPtr,
    pub count_props: u32,
    pub props_ptr: UserPtr,
    pub prop_values_ptr: UserPtr,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub connection: i32,
}
/// Kernel-boundary operations. Defaults are inert/failing; a live DRM adapter
/// supplies object lifetimes, ID allocators, locks, usercopy, sysfs, EDID/blob
/// references, privacy-screen providers, and property storage.
pub trait ConnectorUapiIo {
    fn create_property(&mut self, _dev: DeviceId, _spec: &PropertySpec) -> Option<PropertyId> {
        None
    }
    fn destroy_property(&mut self, _dev: DeviceId, _property: PropertyId) {}
    fn attach_property(&mut self, _connector: ConnectorId, _property: PropertyId, _value: u64) {}
    fn set_property_value(&mut self, _connector: ConnectorId, _property: PropertyId, _value: u64) {}
    fn replace_blob(
        &mut self,
        _connector: ConnectorId,
        _old: &mut Option<BlobId>,
        _property: Option<PropertyId>,
        _bytes: Option<&[u8]>,
    ) -> KResult<()> {
        Err(EOPNOTSUPP)
    }
    fn alloc_connector_object(&mut self, _dev: DeviceId, _connector: ConnectorId) -> KResult<()> {
        Ok(())
    }
    fn unregister_connector_object(&mut self, _dev: DeviceId, _connector: ConnectorId) {}
    fn alloc_connector_index(&mut self, _dev: DeviceId, _max: u32) -> KResult<u32> {
        Err(ENOMEM)
    }
    fn free_connector_index(&mut self, _dev: DeviceId, _index: u32) {}
    fn alloc_type_id(&mut self, _kind: u32, _minimum: u32) -> KResult<u32> {
        Err(ENOMEM)
    }
    fn free_type_id(&mut self, _kind: u32, _id: u32) {}
    fn cmdline_mode(&mut self, _name: &str) -> Option<CmdlineMode> {
        None
    }
    fn parse_cmdline_mode(&mut self, _name: &str, _mode: &mut CmdlineMode) -> bool {
        false
    }
    fn panel_orientation_quirk(&mut self, _width: i32, _height: i32) -> PanelOrientation {
        PanelOrientation::Unknown
    }
    fn init_connector_mutexes(&mut self, _connector: ConnectorId) {}
    fn destroy_connector_mutexes(&mut self, _connector: ConnectorId) {}
    fn register_sysfs(&mut self, _connector: ConnectorId) -> KResult<()> {
        Ok(())
    }
    fn unregister_sysfs(&mut self, _connector: ConnectorId) {}
    fn register_sysfs_late(&mut self, _connector: ConnectorId) -> KResult<()> {
        Ok(())
    }
    fn unregister_sysfs_early(&mut self, _connector: ConnectorId) {}
    fn register_debugfs(&mut self, _connector: ConnectorId) {}
    fn unregister_debugfs(&mut self, _connector: ConnectorId) {}
    fn connector_late_register(&mut self, _connector: ConnectorId) -> KResult<()> {
        Ok(())
    }
    fn connector_early_unregister(&mut self, _connector: ConnectorId) {}
    fn object_register(&mut self, _dev: DeviceId, _connector: ConnectorId) {}
    fn hotplug_event(&mut self, _connector: ConnectorId) {}
    fn sysfs_property_event(&mut self, _connector: ConnectorId, _property: Option<PropertyId>) {}
    fn lock(&mut self, _name: &'static str) {}
    fn unlock(&mut self, _name: &'static str) {}
    fn warn(&mut self, _message: &'static str) {}
    fn kref_get_unless_zero(&mut self, _connector: ConnectorId) -> bool {
        true
    }
    fn connector_put(&mut self, _connector: ConnectorId) {}
    fn connector_put_and_test(&mut self, connector: ConnectorId) -> bool {
        self.connector_put(connector);
        false
    }
    fn free_connector(&mut self, _connector: ConnectorId) {}
    fn get_edid_blob(&mut self, _connector: ConnectorId) -> Option<Vec<u8>> {
        None
    }
    fn get_encoder_id(&mut self, _connector: ConnectorId, encoder: EncoderId) -> EncoderId {
        encoder
    }
    fn encoder_id_by_index(&mut self, _dev: DeviceId, _index: u32) -> Option<EncoderId> {
        None
    }
    fn copy_encoder_to_user(&mut self, _ptr: UserPtr, _index: usize, _encoder: EncoderId) -> bool {
        true
    }
    fn copy_mode_to_user(&mut self, _ptr: UserPtr, _index: usize, _mode: &ModeInfo) -> bool {
        true
    }
    fn copy_properties_to_user(
        &mut self,
        _connector: ConnectorId,
        _atomic: bool,
        _plane_color_pipeline: bool,
        _props_ptr: UserPtr,
        _values_ptr: UserPtr,
        _count: &mut u32,
    ) -> KResult<()> {
        Ok(())
    }
    fn get_tile_ref(&mut self, _group: &TileGroup) -> bool {
        true
    }
    fn privacy_screen_state(&mut self, _provider: u64) -> (u64, u64) {
        (PRIVACY_SCREEN_DISABLED, PRIVACY_SCREEN_DISABLED)
    }
    fn set_privacy_screen_state(&mut self, _provider: u64, _state: u64) -> KResult<()> {
        Ok(())
    }
    fn set_legacy_property(
        &mut self,
        _connector: ConnectorId,
        _property: PropertyId,
        _value: u64,
    ) -> KResult<()> {
        Err(EINVAL)
    }
    fn set_property_ioctl(
        &mut self,
        _dev: DeviceId,
        _connector: ConnectorId,
        _property: PropertyId,
        _value: u64,
        _file: u64,
    ) -> KResult<()> {
        Err(EINVAL)
    }
    fn probe_connector(&mut self, _connector: ConnectorId, _max_width: u32, _max_height: u32) {}
    fn fill_modes(&mut self, connector: &mut Connector, max_width: u32, max_height: u32) {
        self.probe_connector(connector.id, max_width, max_height)
    }
    fn connector_lookup(
        &mut self,
        _dev: DeviceId,
        _connector: ConnectorId,
        _file: u64,
    ) -> Option<Connector> {
        None
    }
    fn connector_store(&mut self, _connector: &Connector) {}
    fn fwnode_get(&mut self, _fwnode: FwnodeId) -> Option<ConnectorId> {
        None
    }
    fn fwnode_put(&mut self, _fwnode: FwnodeId) {}
    fn out_of_band_hotplug(&mut self, _connector: ConnectorId, _status: ConnectorStatus) {}
    fn mode_destroy(&mut self, _dev: DeviceId, _mode: &DisplayMode) {}
    fn tile_group_remove(&mut self, _dev: DeviceId, _id: u32) {}
    fn schedule_connector_free(&mut self, _connector: ConnectorId) {}
    fn drm_managed_cleanup_action(
        &mut self,
        _dev: DeviceId,
        _connector: ConnectorId,
    ) -> KResult<()> {
        Ok(())
    }
    fn hdmi_reset_connector(&mut self, _connector: ConnectorId) {}
    fn panel_orientation(&mut self, _panel: u64) -> PanelOrientation {
        PanelOrientation::Unknown
    }
    fn cec_phys_addr_invalidate(&mut self, _connector: ConnectorId) {}
    fn cec_phys_addr_set(&mut self, _connector: ConnectorId, _address: u16) {}
    fn dpms(&mut self, _connector: ConnectorId, _value: i32) -> KResult<()> {
        Err(EINVAL)
    }
    fn drm_mode_tile_group_free(&mut self, _group: &TileGroup) {}
    fn privacy_screen_put(&mut self, _provider: u64) {}
    fn privacy_screen_register_notifier(&mut self, _connector: ConnectorId, _provider: u64) {}
    fn privacy_screen_unregister_notifier(&mut self, _connector: ConnectorId, _provider: u64) {}
    fn hdmi_audio_unregister(&mut self, _device: Option<u64>) {}
    fn destroy_atomic_state(&mut self, _connector: ConnectorId) {}
    fn device_hotplug_event(&mut self, _dev: DeviceId) {}
    fn sysfs_connector_add_late(&mut self, _connector: ConnectorId) -> KResult<()> {
        Ok(())
    }
    fn sysfs_connector_hotplug_event(&mut self, _connector: ConnectorId) {}
    fn sysfs_connector_remove_early(&mut self, _connector: ConnectorId) {}
    fn sysfs_connector_remove(&mut self, _connector: ConnectorId) {}
    fn sysfs_connector_add(&mut self, _connector: ConnectorId) -> KResult<()> {
        Ok(())
    }
    fn privacy_screen_changed(&mut self, _connector: ConnectorId) {}
    fn property_create_enum(
        &mut self,
        dev: DeviceId,
        name: &'static str,
        flags: u32,
        values: &[EnumValue],
    ) -> Option<PropertyId> {
        self.create_property(dev, &enum_property(name, flags, values))
    }
    fn property_add_enum(
        &mut self,
        _property: PropertyId,
        _value: u32,
        _name: &str,
    ) -> KResult<()> {
        Ok(())
    }
    fn object_property_set(&mut self, connector: ConnectorId, property: PropertyId, value: u64) {
        self.set_property_value(connector, property, value)
    }
    fn current_master(&mut self, _file: u64) -> bool {
        false
    }
    fn tile_group_insert(&mut self, _dev: DeviceId, _group: &TileGroup) -> KResult<u32> {
        Err(ENOMEM)
    }
    fn init_type_id_allocator(&mut self, _kind: u32) {}
    fn destroy_type_id_allocator(&mut self, _kind: u32) {}
}

fn enum_property(name: &'static str, flags: u32, values: &[EnumValue]) -> PropertySpec {
    PropertySpec {
        name,
        kind: PropertyKind::Enum,
        flags,
        enums: values.to_vec(),
    }
}
fn blob_property(name: &'static str, flags: u32) -> PropertySpec {
    PropertySpec {
        name,
        kind: PropertyKind::Blob,
        flags,
        enums: Vec::new(),
    }
}
fn range_property(name: &'static str, flags: u32, min: u64, max: u64) -> PropertySpec {
    PropertySpec {
        name,
        kind: PropertyKind::Range { min, max },
        flags,
        enums: Vec::new(),
    }
}
fn bool_property(name: &'static str, flags: u32) -> PropertySpec {
    PropertySpec {
        name,
        kind: PropertyKind::Bool,
        flags,
        enums: Vec::new(),
    }
}
fn enum_values(values: &[(u32, &'static str)]) -> Vec<EnumValue> {
    values
        .iter()
        .map(|&(value, name)| EnumValue { value, name })
        .collect()
}

// upstream: drm_connector.c drm_connector_ida_init()
pub fn drm_connector_ida_init<I: ConnectorUapiIo>(io: &mut I) {
    for kind in 0..CONNECTOR_TYPE_NAMES.len() as u32 {
        io.init_type_id_allocator(kind);
    }
}

// upstream: drm_connector.c drm_connector_ida_destroy()
pub fn drm_connector_ida_destroy<I: ConnectorUapiIo>(io: &mut I) {
    for kind in 0..CONNECTOR_TYPE_NAMES.len() as u32 {
        io.destroy_type_id_allocator(kind);
    }
}

// upstream: drm_connector.c drm_get_connector_type_name()
pub fn drm_get_connector_type_name(kind: u32) -> Option<&'static str> {
    CONNECTOR_TYPE_NAMES.get(kind as usize).copied()
}

// upstream: drm_connector.c drm_connector_get_cmdline_mode()
pub fn drm_connector_get_cmdline_mode<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) {
    let mut mode = match io.cmdline_mode(&connector.name) {
        Some(mode) => mode,
        None => return,
    };
    if !io.parse_cmdline_mode(&connector.name, &mut mode) {
        return;
    }
    if mode.force != CONNECTOR_FORCE_UNSPECIFIED {
        connector.force = mode.force;
    }
    if mode.panel_orientation != PanelOrientation::Unknown {
        let _ = drm_connector_set_panel_orientation(io, dev, connector, mode.panel_orientation);
    }
    connector.cmdline = Some(mode);
}

// upstream: drm_connector.c drm_connector_free()
pub fn drm_connector_free<I: ConnectorUapiIo>(io: &mut I, connector: &Connector) {
    io.unregister_connector_object(connector.dev, connector.id);
    io.free_connector(connector.id);
}

// upstream: drm_connector.c drm_connector_free_work_fn()
pub fn drm_connector_free_work_fn<I: ConnectorUapiIo>(io: &mut I, freed: &[Connector]) {
    for connector in freed {
        drm_connector_free(io, connector);
    }
}

// upstream: drm_connector.c drm_connector_init_only()
pub fn drm_connector_init_only<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    connector_type: u32,
    ddc: Option<u64>,
) -> KResult<()> {
    if connector_type as usize >= CONNECTOR_TYPE_NAMES.len() {
        return Err(EINVAL);
    }
    io.alloc_connector_object(dev.id, connector.id)?;
    let index = match io.alloc_connector_index(dev.id, 31) {
        Ok(index) => index,
        Err(error) => {
            io.unregister_connector_object(dev.id, connector.id);
            return Err(error);
        }
    };
    let type_id = match io.alloc_type_id(connector_type, 1) {
        Ok(id) => id,
        Err(error) => {
            io.free_connector_index(dev.id, index);
            io.unregister_connector_object(dev.id, connector.id);
            return Err(error);
        }
    };
    let type_name = CONNECTOR_TYPE_NAMES[connector_type as usize];
    connector.dev = dev.id;
    connector.index = index;
    connector.connector_type = connector_type;
    connector.connector_type_id = type_id;
    connector.name = alloc::format!("{type_name}-{type_id}");
    connector.ddc = ddc;
    connector.status = ConnectorStatus::Unknown;
    connector.registration_state = RegistrationState::Initializing;
    connector.edid_blob = None;
    connector.edid_epoch_counter = 0;
    connector.tile_blob = None;
    connector.modes.clear();
    connector.probed_modes.clear();
    connector.display_info.panel_orientation = PanelOrientation::Unknown;
    connector.atomic_mode = dev.driver_atomic;
    if dev.driver_atomic && !connector.atomic_callbacks_valid {
        io.warn("atomic connector requires atomic destroy and duplicate-state callbacks");
    }
    io.init_connector_mutexes(connector.id);
    drm_connector_get_cmdline_mode(io, dev, connector);
    if connector_type != DRM_MODE_CONNECTOR_VIRTUAL
        && connector_type != DRM_MODE_CONNECTOR_WRITEBACK
    {
        drm_connector_attach_edid_property(io, dev, connector);
    }
    for (property, value) in [
        (dev.properties.dpms, 0),
        (dev.properties.link_status, 0),
        (dev.properties.non_desktop, 0),
        (dev.properties.tile, 0),
    ] {
        if let Some(property) = property {
            io.attach_property(connector.id, property, value);
        }
    }
    if dev.driver_atomic {
        if let Some(property) = dev.properties.crtc_id {
            io.attach_property(connector.id, property, 0);
        }
    }
    connector.connector_list_member = false;
    Ok(())
}

// upstream: drm_connector.c drm_connector_add()
pub fn drm_connector_add<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) {
    if connector.connector_list_member {
        io.warn("connector was already on connector_list");
        return;
    }
    io.lock("mode_config.connector_list_lock");
    dev.connectors.push(connector.id);
    dev.connectors_count += 1;
    connector.connector_list_member = true;
    io.unlock("mode_config.connector_list_lock");
}

// upstream: drm_connector.c drm_connector_remove()
pub fn drm_connector_remove<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) {
    if !connector.connector_list_member {
        return;
    }
    io.lock("mode_config.connector_list_lock");
    if let Some(index) = dev.connectors.iter().position(|&id| id == connector.id) {
        dev.connectors.remove(index);
        dev.connectors_count = dev.connectors_count.saturating_sub(1);
    }
    connector.connector_list_member = false;
    io.unlock("mode_config.connector_list_lock");
}

// upstream: drm_connector.c drm_connector_init_and_add()
pub fn drm_connector_init_and_add<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    connector_type: u32,
    ddc: Option<u64>,
) -> KResult<()> {
    drm_connector_init_only(io, dev, connector, connector_type, ddc)?;
    drm_connector_add(io, dev, connector);
    Ok(())
}

// upstream: drm_connector.c drm_connector_init()
pub fn drm_connector_init<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    connector_type: u32,
) -> KResult<()> {
    if !connector.legacy_destroy_callback {
        return Err(EINVAL);
    }
    drm_connector_init_and_add(io, dev, connector, connector_type, None)
}

// upstream: drm_connector.c drm_connector_dynamic_init()
pub fn drm_connector_dynamic_init<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    connector_type: u32,
    ddc: Option<u64>,
) -> KResult<()> {
    if !connector.legacy_destroy_callback {
        return Err(EINVAL);
    }
    drm_connector_init_only(io, dev, connector, connector_type, ddc)
}

// upstream: drm_connector.c drm_connector_init_with_ddc()
pub fn drm_connector_init_with_ddc<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    connector_type: u32,
    ddc: Option<u64>,
) -> KResult<()> {
    if !connector.legacy_destroy_callback {
        return Err(EINVAL);
    }
    drm_connector_init_and_add(io, dev, connector, connector_type, ddc)
}

// upstream: drm_connector.c drm_connector_cleanup_action()
pub fn drm_connector_cleanup_action<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) {
    drm_connector_cleanup(io, dev, connector);
}

// upstream: drm_connector.c drmm_connector_init()
pub fn drmm_connector_init<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    connector_type: u32,
    ddc: Option<u64>,
) -> KResult<()> {
    if connector.legacy_destroy_callback {
        return Err(EINVAL);
    }
    drm_connector_init_and_add(io, dev, connector, connector_type, ddc)?;
    if let Err(error) = io.drm_managed_cleanup_action(dev.id, connector.id) {
        drm_connector_cleanup(io, dev, connector);
        return Err(error);
    }
    Ok(())
}

// upstream: drm_connector.c drmm_connector_hdmi_init()
pub fn drmm_connector_hdmi_init<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    vendor: &str,
    product: &str,
    connector_type: u32,
    supported_formats: u64,
    max_bpc: u32,
    funcs_valid: bool,
    ddc: Option<u64>,
) -> KResult<()> {
    if vendor.len() > DRM_CONNECTOR_HDMI_VENDOR_LEN
        || product.len() > DRM_CONNECTOR_HDMI_PRODUCT_LEN
        || !(connector_type == DRM_MODE_CONNECTOR_HDMIA
            || connector_type == DRM_MODE_CONNECTOR_HDMIB)
        || supported_formats == 0
        || supported_formats & (1 << DRM_OUTPUT_COLOR_FORMAT_RGB444) == 0
        || connector.ycbcr_420_allowed
            != (supported_formats & (1 << DRM_OUTPUT_COLOR_FORMAT_YCBCR420) != 0)
        || !matches!(max_bpc, 8 | 10 | 12)
        || !funcs_valid
    {
        return Err(EINVAL);
    }
    drmm_connector_init(io, dev, connector, connector_type, ddc)?;
    connector.hdmi_supported_formats = supported_formats;
    connector.hdmi_vendor = vendor.to_string();
    connector.hdmi_product = product.to_string();
    if connector.has_reset_callback {
        io.hdmi_reset_connector(connector.id);
        if connector.state.is_none() {
            connector.state = Some(ConnectorState::default());
        }
    }
    drm_connector_attach_max_bpc_property(io, dev, connector, 8, max_bpc as i32)?;
    connector.hdmi_max_bpc = max_bpc;
    if max_bpc > 8 {
        drm_connector_attach_hdr_output_metadata_property(io, dev, connector);
    }
    connector.hdmi_callbacks_valid = funcs_valid;
    Ok(())
}

// upstream: drm_connector.c drm_connector_attach_edid_property()
pub fn drm_connector_attach_edid_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &Connector,
) {
    if let Some(property) = dev.properties.edid {
        io.attach_property(connector.id, property, 0);
    }
}

// upstream: drm_connector.c drm_connector_attach_encoder()
pub fn drm_connector_attach_encoder(connector: &mut Connector, encoder: &Encoder) -> KResult<()> {
    if connector.current_encoder.is_some() {
        return Err(EINVAL);
    }
    connector.possible_encoders |= 1u32.checked_shl(encoder.index).unwrap_or(0);
    Ok(())
}

// upstream: drm_connector.c drm_connector_has_possible_encoder()
pub fn drm_connector_has_possible_encoder(connector: &Connector, encoder: &Encoder) -> bool {
    connector.possible_encoders & 1u32.checked_shl(encoder.index).unwrap_or(0) != 0
}

// upstream: drm_connector.c drm_mode_remove()
pub fn drm_mode_remove<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &mut Connector,
    probed: bool,
    index: usize,
) {
    let modes = if probed {
        &mut connector.probed_modes
    } else {
        &mut connector.modes
    };
    if index < modes.len() {
        let mode = modes.remove(index);
        io.mode_destroy(connector.dev, &mode);
    }
}

// upstream: drm_connector.c drm_connector_cec_phys_addr_invalidate()
pub fn drm_connector_cec_phys_addr_invalidate<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &mut Connector,
    has_cec_invalidate: bool,
) {
    io.lock("connector.cec.mutex");
    if has_cec_invalidate {
        io.cec_phys_addr_invalidate(connector.id);
    }
    io.unlock("connector.cec.mutex");
}

// upstream: drm_connector.c drm_connector_cec_phys_addr_set()
pub fn drm_connector_cec_phys_addr_set<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &mut Connector,
    has_cec_set: bool,
) {
    io.lock("connector.cec.mutex");
    let address = connector.display_info.source_physical_address;
    if has_cec_set {
        connector.cec_physical_address = Some(address);
        io.cec_phys_addr_set(connector.id, address);
    }
    io.unlock("connector.cec.mutex");
}

// upstream: drm_connector.c drm_connector_cleanup()
pub fn drm_connector_cleanup<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) {
    if connector.registration_state == RegistrationState::Registered {
        drm_connector_unregister(io, dev, connector);
    }
    io.hdmi_audio_unregister(connector.hdmi_audio_device.take());
    if let Some(provider) = connector.privacy_screen.take() {
        io.privacy_screen_put(provider);
    }
    if let Some(group) = connector.tile_group.take() {
        drm_mode_put_tile_group(io, dev, group);
    }
    while !connector.probed_modes.is_empty() {
        drm_mode_remove(io, connector, true, 0);
    }
    while !connector.modes.is_empty() {
        drm_mode_remove(io, connector, false, 0);
    }
    io.free_type_id(connector.connector_type, connector.connector_type_id);
    io.free_connector_index(dev.id, connector.index);
    connector.display_info.bus_formats.clear();
    connector.display_info.vics.clear();
    connector.bus_formats.clear();
    connector.vics.clear();
    io.unregister_connector_object(dev.id, connector.id);
    connector.name.clear();
    if let Some(fwnode) = connector.fwnode.take() {
        io.fwnode_put(fwnode);
    }
    connector.secondary_fwnode = None;
    drm_connector_remove(io, dev, connector);
    if connector.state.is_some() {
        if connector.atomic_callbacks_valid {
            io.destroy_atomic_state(connector.id);
        } else {
            io.warn("connector state has no atomic destroy callback");
        }
    }
    io.destroy_connector_mutexes(connector.id);
    let was_registered = dev.registered;
    *connector = Connector::default();
    if was_registered {
        io.device_hotplug_event(dev.id);
    }
}

// upstream: drm_connector.c drm_connector_register()
pub fn drm_connector_register<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) -> KResult<()> {
    if !dev.registered {
        return Ok(());
    }
    io.lock("connector.mutex");
    if connector.registration_state != RegistrationState::Initializing {
        io.unlock("connector.mutex");
        return Ok(());
    }
    if let Err(error) = io.sysfs_connector_add(connector.id) {
        io.unlock("connector.mutex");
        return Err(error);
    }
    io.register_debugfs(connector.id);
    if connector.has_late_register_callback {
        if let Err(error) = io.connector_late_register(connector.id) {
            io.unregister_debugfs(connector.id);
            io.sysfs_connector_remove(connector.id);
            io.unlock("connector.mutex");
            return Err(error);
        }
    }
    if let Err(error) = io.sysfs_connector_add_late(connector.id) {
        if connector.has_early_unregister_callback {
            io.connector_early_unregister(connector.id);
        }
        io.unregister_debugfs(connector.id);
        io.sysfs_connector_remove(connector.id);
        io.unlock("connector.mutex");
        return Err(error);
    }
    io.object_register(dev.id, connector.id);
    connector.registration_state = RegistrationState::Registered;
    io.sysfs_connector_hotplug_event(connector.id);
    if let Some(provider) = connector.privacy_screen {
        io.privacy_screen_register_notifier(connector.id, provider);
    }
    io.lock("connector.global_list");
    if !dev.global_connector_order.contains(&connector.id) {
        dev.global_connector_order.push(connector.id);
    }
    connector.registration_on_global_list = true;
    io.unlock("connector.global_list");
    io.unlock("connector.mutex");
    Ok(())
}

// upstream: drm_connector.c drm_connector_dynamic_register()
pub fn drm_connector_dynamic_register<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) -> KResult<()> {
    if !connector.legacy_destroy_callback {
        return Err(EINVAL);
    }
    drm_connector_add(io, dev, connector);
    drm_connector_register(io, dev, connector)
}

// upstream: drm_connector.c drm_connector_unregister()
pub fn drm_connector_unregister<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
) {
    io.lock("connector.mutex");
    if connector.registration_state != RegistrationState::Registered {
        io.unlock("connector.mutex");
        return;
    }
    io.lock("connector.global_list");
    if let Some(index) = dev
        .global_connector_order
        .iter()
        .position(|&id| id == connector.id)
    {
        dev.global_connector_order.remove(index);
    }
    connector.registration_on_global_list = false;
    io.unlock("connector.global_list");
    if let Some(provider) = connector.privacy_screen {
        io.privacy_screen_unregister_notifier(connector.id, provider);
    }
    io.sysfs_connector_remove_early(connector.id);
    if connector.has_early_unregister_callback {
        io.connector_early_unregister(connector.id);
    }
    io.unregister_debugfs(connector.id);
    io.sysfs_connector_remove(connector.id);
    connector.registration_state = RegistrationState::Unregistered;
    io.unlock("connector.mutex");
}

// upstream: drm_connector.c drm_connector_unregister_all()
pub fn drm_connector_unregister_all<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connectors: &mut [Connector],
) {
    for id in dev.connectors.clone() {
        if let Some(connector) = connectors.iter_mut().find(|connector| connector.id == id) {
            drm_connector_unregister(io, dev, connector);
        }
    }
}

// upstream: drm_connector.c drm_connector_register_all()
pub fn drm_connector_register_all<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connectors: &mut [Connector],
) -> KResult<()> {
    let ids = dev.connectors.clone();
    for id in ids {
        let result = connectors
            .iter_mut()
            .find(|connector| connector.id == id)
            .map(|connector| drm_connector_register(io, dev, connector))
            .unwrap_or(Ok(()));
        if let Err(error) = result {
            drm_connector_unregister_all(io, dev, connectors);
            return Err(error);
        }
    }
    Ok(())
}

// upstream: drm_connector.c drm_get_connector_status_name()
pub fn drm_get_connector_status_name(status: ConnectorStatus) -> &'static str {
    match status {
        ConnectorStatus::Connected => "connected",
        ConnectorStatus::Disconnected => "disconnected",
        _ => "unknown",
    }
}

// upstream: drm_connector.c drm_get_connector_force_name()
pub fn drm_get_connector_force_name(force: u32) -> &'static str {
    match force {
        CONNECTOR_FORCE_UNSPECIFIED => "unspecified",
        CONNECTOR_FORCE_OFF => "off",
        CONNECTOR_FORCE_ON => "on",
        CONNECTOR_FORCE_ON_DIGITAL => "digital",
        _ => "unknown",
    }
}

// upstream: drm_connector.c drm_connector_list_iter_begin()
pub fn drm_connector_list_iter_begin<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    iter: &mut ConnectorIterator,
) {
    iter.dev = dev;
    iter.cursor = 0;
    iter.current = None;
    iter.held.clear();
    io.lock("connector_list_iter.dep_map.shared_recursive");
}

// upstream: drm_connector.c __drm_connector_put_safe()
pub fn __drm_connector_put_safe<I: ConnectorUapiIo>(io: &mut I, connector: ConnectorId) {
    if io.connector_put_and_test(connector) {
        io.schedule_connector_free(connector);
    }
}

// upstream: drm_connector.c drm_connector_list_iter_next()
pub fn drm_connector_list_iter_next<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    iter: &mut ConnectorIterator,
) -> Option<ConnectorId> {
    io.lock("mode_config.connector_list_lock");
    let mut next = None;
    while iter.cursor < dev.connectors.len() {
        let id = dev.connectors[iter.cursor];
        iter.cursor += 1;
        if io.kref_get_unless_zero(id) {
            next = Some(id);
            break;
        }
    }
    if let Some(old) = iter.current.take() {
        __drm_connector_put_safe(io, old);
    }
    iter.current = next;
    if let Some(id) = next {
        iter.held.push(id);
    }
    io.unlock("mode_config.connector_list_lock");
    next
}

// upstream: drm_connector.c drm_connector_list_iter_end()
pub fn drm_connector_list_iter_end<I: ConnectorUapiIo>(io: &mut I, iter: &mut ConnectorIterator) {
    if let Some(connector) = iter.current.take() {
        io.lock("mode_config.connector_list_lock");
        __drm_connector_put_safe(io, connector);
        io.unlock("mode_config.connector_list_lock");
    }
    iter.dev = 0;
    io.unlock("connector_list_iter.dep_map.shared_recursive");
}

// upstream: drm_connector.c drm_get_subpixel_order_name()
pub fn drm_get_subpixel_order_name(order: u32) -> Option<&'static str> {
    SUBPIXEL_NAMES.get(order as usize).copied()
}

// upstream: drm_connector.c drm_display_info_set_bus_formats()
pub fn drm_display_info_set_bus_formats(
    info: &mut DisplayInfo,
    formats: Option<&[u32]>,
    num_formats: usize,
) -> KResult<()> {
    if formats.is_none() && num_formats != 0 {
        return Err(EINVAL);
    }
    let new_formats = match formats {
        Some(formats) if num_formats != 0 => formats.get(..num_formats).ok_or(EINVAL)?.to_vec(),
        _ => Vec::new(),
    };
    info.bus_formats = new_formats;
    Ok(())
}

// upstream: drm_connector.c DRM_ENUM_NAME_FN()
pub fn drm_get_tv_mode_name(mode: u32) -> &'static str {
    [
        "NTSC", "NTSC-443", "NTSC-J", "PAL", "PAL-M", "PAL-N", "SECAM", "Mono",
    ]
    .get(mode as usize)
    .copied()
    .unwrap_or("(unknown)")
}

// upstream: drm_connector.c drm_get_tv_mode_from_name()
pub fn drm_get_tv_mode_from_name(name: &str) -> KResult<u32> {
    for value in 0..DRM_MODE_TV_MODE_MAX as u32 {
        if drm_get_tv_mode_name(value) == name {
            return Ok(value);
        }
    }
    Err(EINVAL)
}

// upstream: drm_connector.c drm_get_colorspace_name()
pub fn drm_get_colorspace_name(colorspace: u32) -> &'static str {
    const NAMES: [&str; DRM_MODE_COLORIMETRY_COUNT] = [
        "Default",
        "SMPTE_170M_YCC",
        "BT709_YCC",
        "XVYCC_601",
        "XVYCC_709",
        "SYCC_601",
        "opYCC_601",
        "opRGB",
        "BT2020_CYCC",
        "BT2020_RGB",
        "BT2020_YCC",
        "DCI-P3_RGB_D65",
        "DCI-P3_RGB_Theater",
        "RGB_WIDE_FIXED",
        "RGB_WIDE_FLOAT",
        "BT601_YCC",
    ];
    NAMES.get(colorspace as usize).copied().unwrap_or("(null)")
}

// upstream: drm_connector.c drm_hdmi_connector_get_broadcast_rgb_name()
pub fn drm_hdmi_connector_get_broadcast_rgb_name(value: u32) -> Option<&'static str> {
    ["Automatic", "Full", "Limited 16:235"]
        .get(value as usize)
        .copied()
}

// upstream: drm_connector.c drm_hdmi_connector_get_output_format_name()
pub fn drm_hdmi_connector_get_output_format_name(format: u32) -> Option<&'static str> {
    ["RGB", "YUV 4:4:4", "YUV 4:2:2", "YUV 4:2:0"]
        .get(format as usize)
        .copied()
}

// upstream: drm_connector.c drm_connector_create_standard_properties()
pub fn drm_connector_create_standard_properties<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
) -> KResult<()> {
    let immutable = DRM_MODE_PROP_IMMUTABLE;
    dev.properties.edid = io.create_property(
        dev.id,
        &PropertySpec {
            name: "EDID",
            kind: PropertyKind::Blob,
            flags: DRM_MODE_PROP_BLOB | immutable,
            enums: Vec::new(),
        },
    );
    if dev.properties.edid.is_none() {
        return Err(ENOMEM);
    }
    let dpms = enum_values(&[(0, "On"), (1, "Standby"), (2, "Suspend"), (3, "Off")]);
    dev.properties.dpms = io.create_property(
        dev.id,
        &PropertySpec {
            name: "DPMS",
            kind: PropertyKind::Enum,
            flags: 0,
            enums: dpms,
        },
    );
    if dev.properties.dpms.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.path = io.create_property(
        dev.id,
        &PropertySpec {
            name: "PATH",
            kind: PropertyKind::Blob,
            flags: DRM_MODE_PROP_BLOB | immutable,
            enums: Vec::new(),
        },
    );
    if dev.properties.path.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tile = io.create_property(
        dev.id,
        &PropertySpec {
            name: "TILE",
            kind: PropertyKind::Blob,
            flags: DRM_MODE_PROP_BLOB | immutable,
            enums: Vec::new(),
        },
    );
    if dev.properties.tile.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.link_status = io.create_property(
        dev.id,
        &enum_property("link-status", 0, &enum_values(&[(0, "Good"), (1, "Bad")])),
    );
    if dev.properties.link_status.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.panel_type = io.create_property(
        dev.id,
        &enum_property(
            "panel_type",
            immutable,
            &enum_values(&[
                (DRM_MODE_PANEL_TYPE_UNKNOWN, "unknown"),
                (DRM_MODE_PANEL_TYPE_OLED, "OLED"),
            ]),
        ),
    );
    if dev.properties.panel_type.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.non_desktop =
        io.create_property(dev.id, &bool_property("non-desktop", immutable));
    if dev.properties.non_desktop.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.hdr_output_metadata = io.create_property(
        dev.id,
        &blob_property("HDR_OUTPUT_METADATA", DRM_MODE_PROP_BLOB),
    );
    if dev.properties.hdr_output_metadata.is_none() {
        return Err(ENOMEM);
    }
    Ok(())
}

// upstream: drm_connector.c drm_mode_create_dvi_i_properties()
pub fn drm_mode_create_dvi_i_properties<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
) -> KResult<()> {
    if dev.properties.dvi_i_select.is_some() {
        return Ok(());
    }
    dev.properties.dvi_i_select = io.property_create_enum(
        dev.id,
        "select subconnector",
        0,
        &enum_values(&[
            (0, "Automatic"),
            (DRM_MODE_SUBCONNECTOR_DVID, "DVI-D"),
            (DRM_MODE_SUBCONNECTOR_DVIA, "DVI-A"),
        ]),
    );
    dev.properties.dvi_i_subconnector = io.property_create_enum(
        dev.id,
        "subconnector",
        DRM_MODE_PROP_IMMUTABLE,
        &enum_values(&[
            (DRM_MODE_SUBCONNECTOR_UNKNOWN, "Unknown"),
            (DRM_MODE_SUBCONNECTOR_DVID, "DVI-D"),
            (DRM_MODE_SUBCONNECTOR_DVIA, "DVI-A"),
        ]),
    );
    Ok(())
}

// upstream: drm_connector.c drm_connector_attach_dp_subconnector_property()
pub fn drm_connector_attach_dp_subconnector_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &Connector,
) {
    if dev.properties.dp_subconnector.is_none() {
        dev.properties.dp_subconnector = io.property_create_enum(
            dev.id,
            "subconnector",
            DRM_MODE_PROP_IMMUTABLE,
            &enum_values(&[
                (DRM_MODE_SUBCONNECTOR_UNKNOWN, "Unknown"),
                (DRM_MODE_SUBCONNECTOR_VGA, "VGA"),
                (DRM_MODE_SUBCONNECTOR_DVID, "DVI-D"),
                (DRM_MODE_SUBCONNECTOR_HDMIA, "HDMI"),
                (DRM_MODE_SUBCONNECTOR_DISPLAYPORT, "DP"),
                (DRM_MODE_SUBCONNECTOR_WIRELESS, "Wireless"),
                (DRM_MODE_SUBCONNECTOR_NATIVE, "Native"),
            ]),
        );
    }
    if let Some(property) = dev.properties.dp_subconnector {
        io.attach_property(connector.id, property, DRM_MODE_SUBCONNECTOR_UNKNOWN as u64);
    }
}

// upstream: drm_connector.c drm_connector_attach_content_type_property()
pub fn drm_connector_attach_content_type_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &Connector,
) -> KResult<()> {
    if drm_mode_create_content_type_property(io, dev).is_ok() {
        if let Some(property) = dev.properties.content_type {
            io.attach_property(connector.id, property, DRM_MODE_CONTENT_TYPE_NO_DATA as u64);
        }
    }
    Ok(())
}

// upstream: drm_connector.c drm_connector_attach_tv_margin_properties()
pub fn drm_connector_attach_tv_margin_properties<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &Connector,
) {
    for property in [
        dev.properties.tv_left,
        dev.properties.tv_right,
        dev.properties.tv_top,
        dev.properties.tv_bottom,
    ] {
        if let Some(property) = property {
            io.attach_property(connector.id, property, 0);
        }
    }
}

// upstream: drm_connector.c drm_mode_create_tv_margin_properties()
pub fn drm_mode_create_tv_margin_properties<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
) -> KResult<()> {
    if dev.properties.tv_left.is_some() {
        return Ok(());
    }
    dev.properties.tv_left = io.create_property(dev.id, &range_property("left margin", 0, 0, 100));
    if dev.properties.tv_left.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_right =
        io.create_property(dev.id, &range_property("right margin", 0, 0, 100));
    if dev.properties.tv_right.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_top = io.create_property(dev.id, &range_property("top margin", 0, 0, 100));
    if dev.properties.tv_top.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_bottom =
        io.create_property(dev.id, &range_property("bottom margin", 0, 0, 100));
    if dev.properties.tv_bottom.is_none() {
        return Err(ENOMEM);
    }
    Ok(())
}

// upstream: drm_connector.c drm_mode_create_tv_properties_legacy()
pub fn drm_mode_create_tv_properties_legacy<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    modes: &[&str],
) -> KResult<()> {
    if dev.properties.tv_select_subconnector.is_some() {
        return Ok(());
    }
    // The DVI-I/TV selector values are installed before later range properties;
    // partial creation is retained on failure, matching the DRM property path.
    dev.properties.tv_select_subconnector = io.property_create_enum(
        dev.id,
        "select subconnector",
        0,
        &enum_values(&[
            (0, "Automatic"),
            (DRM_MODE_SUBCONNECTOR_COMPOSITE, "Composite"),
            (DRM_MODE_SUBCONNECTOR_SVIDEO, "SVIDEO"),
            (DRM_MODE_SUBCONNECTOR_COMPONENT, "Component"),
            (DRM_MODE_SUBCONNECTOR_SCART, "SCART"),
        ]),
    );
    if dev.properties.tv_select_subconnector.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_subconnector = io.property_create_enum(
        dev.id,
        "subconnector",
        DRM_MODE_PROP_IMMUTABLE,
        &enum_values(&[
            (DRM_MODE_SUBCONNECTOR_UNKNOWN, "Unknown"),
            (DRM_MODE_SUBCONNECTOR_COMPOSITE, "Composite"),
            (DRM_MODE_SUBCONNECTOR_SVIDEO, "SVIDEO"),
            (DRM_MODE_SUBCONNECTOR_COMPONENT, "Component"),
            (DRM_MODE_SUBCONNECTOR_SCART, "SCART"),
        ]),
    );
    if dev.properties.tv_subconnector.is_none() {
        return Err(ENOMEM);
    }
    drm_mode_create_tv_margin_properties(io, dev)?;
    if !modes.is_empty() {
        let property = io
            .create_property(
                dev.id,
                &PropertySpec {
                    name: "mode",
                    kind: PropertyKind::Enum,
                    flags: DRM_MODE_PROP_ENUM,
                    enums: Vec::new(),
                },
            )
            .ok_or(ENOMEM)?;
        dev.properties.legacy_tv_mode = Some(property);
        for (value, name) in modes.iter().enumerate() {
            let _ = io.property_add_enum(property, value as u32, name);
        }
    }
    let dev_id = dev.id;
    dev.properties.tv_brightness =
        io.create_property(dev_id, &range_property("brightness", 0, 0, 100));
    if dev.properties.tv_brightness.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_contrast = io.create_property(dev_id, &range_property("contrast", 0, 0, 100));
    if dev.properties.tv_contrast.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_flicker_reduction =
        io.create_property(dev_id, &range_property("flicker reduction", 0, 0, 100));
    if dev.properties.tv_flicker_reduction.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_overscan = io.create_property(dev_id, &range_property("overscan", 0, 0, 100));
    if dev.properties.tv_overscan.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_saturation =
        io.create_property(dev_id, &range_property("saturation", 0, 0, 100));
    if dev.properties.tv_saturation.is_none() {
        return Err(ENOMEM);
    }
    dev.properties.tv_hue = io.create_property(dev_id, &range_property("hue", 0, 0, 100));
    if dev.properties.tv_hue.is_none() {
        return Err(ENOMEM);
    }
    Ok(())
}

// upstream: drm_connector.c drm_mode_create_tv_properties()
pub fn drm_mode_create_tv_properties<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    supported_tv_modes: u32,
) -> KResult<()> {
    if dev.properties.tv_mode.is_some() {
        return Ok(());
    }
    let mut enums = Vec::new();
    for mode in 0..DRM_MODE_TV_MODE_MAX as u32 {
        if supported_tv_modes & (1 << mode) != 0 {
            enums.push(EnumValue {
                value: mode,
                name: drm_get_tv_mode_name(mode),
            });
        }
    }
    dev.properties.tv_mode = Some(
        io.create_property(
            dev.id,
            &PropertySpec {
                name: "TV mode",
                kind: PropertyKind::Enum,
                flags: 0,
                enums,
            },
        )
        .ok_or(ENOMEM)?,
    );
    drm_mode_create_tv_properties_legacy(io, dev, &[])
}

// upstream: drm_connector.c drm_mode_create_scaling_mode_property()
pub fn drm_mode_create_scaling_mode_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
) -> KResult<()> {
    if dev.properties.scaling_mode.is_some() {
        return Ok(());
    }
    dev.properties.scaling_mode = io.property_create_enum(
        dev.id,
        "scaling mode",
        0,
        &enum_values(&[
            (DRM_MODE_SCALE_NONE, "None"),
            (DRM_MODE_SCALE_FULLSCREEN, "Full"),
            (DRM_MODE_SCALE_CENTER, "Center"),
            (DRM_MODE_SCALE_ASPECT, "Full aspect"),
        ]),
    );
    Ok(())
}

// upstream: drm_connector.c drm_connector_attach_vrr_capable_property()
pub fn drm_connector_attach_vrr_capable_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &mut Connector,
) -> KResult<()> {
    if connector.vrr_capable_property.is_none() {
        let property = io
            .create_property(
                dev.id,
                &bool_property("vrr_capable", DRM_MODE_PROP_IMMUTABLE),
            )
            .ok_or(ENOMEM)?;
        connector.vrr_capable_property = Some(property);
        io.attach_property(connector.id, property, 0);
    }
    Ok(())
}

// upstream: drm_connector.c drm_connector_attach_scaling_mode_property()
pub fn drm_connector_attach_scaling_mode_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &mut Connector,
    mask: u32,
) -> KResult<()> {
    const VALID_MASK: u32 = (1 << 4) - 1;
    if mask.count_ones() < 2 || mask & !VALID_MASK != 0 {
        return Err(EINVAL);
    }
    let definitions = [(0, "None"), (1, "Full"), (2, "Center"), (3, "Full aspect")];
    let property = io
        .create_property(
            dev.id,
            &PropertySpec {
                name: "scaling mode",
                kind: PropertyKind::Enum,
                flags: DRM_MODE_PROP_ENUM,
                enums: Vec::new(),
            },
        )
        .ok_or(ENOMEM)?;
    for &(value, name) in &definitions {
        if mask & (1 << value) != 0 {
            if let Err(error) = io.property_add_enum(property, value, name) {
                io.destroy_property(dev.id, property);
                return Err(error);
            }
        }
    }
    io.attach_property(connector.id, property, 0);
    connector.scaling_mode_property = Some(property);
    Ok(())
}

// upstream: drm_connector.c drm_mode_create_aspect_ratio_property()
pub fn drm_mode_create_aspect_ratio_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
) -> KResult<()> {
    if dev.properties.aspect_ratio.is_some() {
        return Ok(());
    }
    dev.properties.aspect_ratio = io.property_create_enum(
        dev.id,
        "aspect ratio",
        0,
        &enum_values(&[
            (DRM_MODE_PICTURE_ASPECT_NONE, "Automatic"),
            (DRM_MODE_PICTURE_ASPECT_4_3, "4:3"),
            (DRM_MODE_PICTURE_ASPECT_16_9, "16:9"),
        ]),
    );
    if dev.properties.aspect_ratio.is_none() {
        return Err(ENOMEM);
    }
    Ok(())
}

const HDMI_COLORSPACES: u32 = (1 << DRM_MODE_COLORIMETRY_SMPTE_170M_YCC)
    | (1 << DRM_MODE_COLORIMETRY_BT709_YCC)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_709)
    | (1 << DRM_MODE_COLORIMETRY_SYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_OPYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_OPRGB)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_CYCC)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_RGB)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_YCC)
    | (1 << DRM_MODE_COLORIMETRY_DCI_P3_RGB_D65)
    | (1 << DRM_MODE_COLORIMETRY_DCI_P3_RGB_THEATER);
const DP_COLORSPACES: u32 = (1 << DRM_MODE_COLORIMETRY_RGB_WIDE_FIXED)
    | (1 << DRM_MODE_COLORIMETRY_RGB_WIDE_FLOAT)
    | (1 << DRM_MODE_COLORIMETRY_OPRGB)
    | (1 << DRM_MODE_COLORIMETRY_DCI_P3_RGB_D65)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_RGB)
    | (1 << DRM_MODE_COLORIMETRY_BT601_YCC)
    | (1 << DRM_MODE_COLORIMETRY_BT709_YCC)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_709)
    | (1 << DRM_MODE_COLORIMETRY_SYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_OPYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_CYCC)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_YCC);

// upstream: drm_connector.c drm_mode_create_colorspace_property()
pub fn drm_mode_create_colorspace_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    connector: &mut Connector,
    supported: u32,
) -> KResult<()> {
    if connector.colorspace_property.is_some() {
        return Ok(());
    }
    if supported == 0 {
        return Err(EINVAL);
    }
    if supported & !((1u32 << DRM_MODE_COLORIMETRY_COUNT) - 1) != 0 {
        return Err(EINVAL);
    }
    let colorspaces = supported | (1 << DRM_MODE_COLORIMETRY_DEFAULT);
    let enums = (0..DRM_MODE_COLORIMETRY_COUNT as u32)
        .filter(|&i| colorspaces & (1 << i) != 0)
        .map(|value| EnumValue {
            value,
            name: drm_get_colorspace_name(value),
        })
        .collect();
    connector.colorspace_property = io.create_property(
        dev,
        &PropertySpec {
            name: "Colorspace",
            kind: PropertyKind::Enum,
            flags: DRM_MODE_PROP_ENUM,
            enums,
        },
    );
    if connector.colorspace_property.is_none() {
        return Err(ENOMEM);
    }
    Ok(())
}

// upstream: drm_connector.c drm_mode_create_hdmi_colorspace_property()
pub fn drm_mode_create_hdmi_colorspace_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    connector: &mut Connector,
    supported: u32,
) -> KResult<()> {
    let colorspaces = if supported != 0 {
        supported & HDMI_COLORSPACES
    } else {
        HDMI_COLORSPACES
    };
    drm_mode_create_colorspace_property(io, dev, connector, colorspaces)
}

// upstream: drm_connector.c drm_mode_create_dp_colorspace_property()
pub fn drm_mode_create_dp_colorspace_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    connector: &mut Connector,
    supported: u32,
) -> KResult<()> {
    let colorspaces = if supported != 0 {
        supported & DP_COLORSPACES
    } else {
        DP_COLORSPACES
    };
    drm_mode_create_colorspace_property(io, dev, connector, colorspaces)
}

// upstream: drm_connector.c drm_mode_create_content_type_property()
pub fn drm_mode_create_content_type_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
) -> KResult<()> {
    if dev.properties.content_type.is_some() {
        return Ok(());
    }
    dev.properties.content_type = io.property_create_enum(
        dev.id,
        "content type",
        0,
        &enum_values(&[
            (DRM_MODE_CONTENT_TYPE_NO_DATA, "No Data"),
            (DRM_MODE_CONTENT_TYPE_GRAPHICS, "Graphics"),
            (DRM_MODE_CONTENT_TYPE_PHOTO, "Photo"),
            (DRM_MODE_CONTENT_TYPE_CINEMA, "Cinema"),
            (DRM_MODE_CONTENT_TYPE_GAME, "Game"),
        ]),
    );
    if dev.properties.content_type.is_none() {
        return Err(ENOMEM);
    }
    Ok(())
}

// upstream: drm_connector.c drm_mode_create_suggested_offset_properties()
pub fn drm_mode_create_suggested_offset_properties<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
) -> KResult<()> {
    if dev.properties.suggested_x.is_some() && dev.properties.suggested_y.is_some() {
        return Ok(());
    }
    dev.properties.suggested_x = io.create_property(
        dev.id,
        &range_property("suggested X", DRM_MODE_PROP_IMMUTABLE, 0, u32::MAX as u64),
    );
    dev.properties.suggested_y = io.create_property(
        dev.id,
        &range_property("suggested Y", DRM_MODE_PROP_IMMUTABLE, 0, u32::MAX as u64),
    );
    if dev.properties.suggested_x.is_none() || dev.properties.suggested_y.is_none() {
        return Err(ENOMEM);
    }
    Ok(())
}

// upstream: drm_connector.c drm_connector_set_path_property()
pub fn drm_connector_set_path_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &mut Connector,
    path: &str,
) -> KResult<()> {
    let mut bytes = path.as_bytes().to_vec();
    bytes.push(0);
    io.replace_blob(
        connector.id,
        &mut connector.path_blob,
        dev.properties.path,
        Some(&bytes),
    )
}

// upstream: drm_connector.c drm_connector_set_tile_property()
pub fn drm_connector_set_tile_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &mut Connector,
) -> KResult<()> {
    if !connector.has_tile {
        return io.replace_blob(
            connector.id,
            &mut connector.tile_blob,
            dev.properties.tile,
            None,
        );
    }
    let group_id = connector.tile_group.as_ref().map_or(0, |group| group.id);
    let text = alloc::format!(
        "{}:{}:{}:{}:{}:{}:{}:{}",
        group_id,
        connector.tile_is_single_monitor,
        connector.num_h_tile,
        connector.num_v_tile,
        connector.tile_h_loc,
        connector.tile_v_loc,
        connector.tile_h_size,
        connector.tile_v_size
    );
    let mut bytes = text.into_bytes();
    bytes.push(0);
    io.replace_blob(
        connector.id,
        &mut connector.tile_blob,
        dev.properties.tile,
        Some(&bytes),
    )
}

// upstream: drm_connector.c drm_connector_set_link_status_property()
pub fn drm_connector_set_link_status_property<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &mut Connector,
    value: u64,
) {
    io.lock("mode_config.connection_mutex");
    if let Some(state) = connector.state.as_mut() {
        state.link_status = value;
    }
    io.unlock("mode_config.connection_mutex");
}

// upstream: drm_connector.c drm_connector_attach_max_bpc_property()
pub fn drm_connector_attach_max_bpc_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &mut Connector,
    min: i32,
    max: i32,
) -> KResult<()> {
    let property = match connector.max_bpc_property {
        Some(property) => property,
        None => {
            let property = io
                .create_property(
                    dev.id,
                    &range_property("max bpc", 0, min as u64, max as u64),
                )
                .ok_or(ENOMEM)?;
            connector.max_bpc_property = Some(property);
            property
        }
    };
    io.attach_property(connector.id, property, max as u64);
    let state = connector.state.as_mut().ok_or(ENODEV)?;
    state.max_requested_bpc = max as u32;
    state.max_bpc = max as u32;
    Ok(())
}

// upstream: drm_connector.c drm_connector_attach_hdr_output_metadata_property()
pub fn drm_connector_attach_hdr_output_metadata_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &Connector,
) {
    if let Some(property) = dev.properties.hdr_output_metadata {
        io.attach_property(connector.id, property, 0);
    }
}

// upstream: drm_connector.c drm_connector_attach_broadcast_rgb_property()
pub fn drm_connector_attach_broadcast_rgb_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    connector: &mut Connector,
) -> KResult<()> {
    let property = match connector.broadcast_rgb_property {
        Some(property) => property,
        None => {
            let enums = enum_values(&[
                (DRM_HDMI_BROADCAST_RGB_AUTO, "Automatic"),
                (DRM_HDMI_BROADCAST_RGB_FULL, "Full"),
                (DRM_HDMI_BROADCAST_RGB_LIMITED, "Limited 16:235"),
            ]);
            let property = io
                .create_property(
                    dev,
                    &enum_property("Broadcast RGB", DRM_MODE_PROP_ENUM, &enums),
                )
                .ok_or(EINVAL)?;
            connector.broadcast_rgb_property = Some(property);
            property
        }
    };
    io.attach_property(connector.id, property, DRM_HDMI_BROADCAST_RGB_AUTO as u64);
    Ok(())
}

// upstream: drm_connector.c drm_connector_attach_colorspace_property()
pub fn drm_connector_attach_colorspace_property<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &Connector,
) {
    if let Some(property) = connector.colorspace_property {
        io.attach_property(connector.id, property, DRM_MODE_COLORIMETRY_DEFAULT as u64);
    }
}

// upstream: drm_connector.c drm_connector_atomic_hdr_metadata_equal()
pub fn drm_connector_atomic_hdr_metadata_equal(old: &ConnectorState, new: &ConnectorState) -> bool {
    match (&old.hdr_metadata, &new.hdr_metadata) {
        (None, None) => true,
        (Some(old), Some(new)) => old.len() == new.len() && old == new,
        _ => false,
    }
}

// upstream: drm_connector.c drm_connector_set_vrr_capable_property()
pub fn drm_connector_set_vrr_capable_property<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &Connector,
    capable: bool,
) {
    if let Some(property) = connector.vrr_capable_property {
        io.object_property_set(connector.id, property, u64::from(capable));
    }
}

// upstream: drm_connector.c drm_connector_set_panel_orientation()
pub fn drm_connector_set_panel_orientation<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    orientation: PanelOrientation,
) -> KResult<()> {
    if connector.display_info.panel_orientation != PanelOrientation::Unknown {
        return Ok(());
    }
    if orientation == PanelOrientation::Unknown {
        return Ok(());
    }
    connector.display_info.panel_orientation = orientation;
    if dev.properties.panel_orientation.is_none() {
        let enums = enum_values(&[
            (DRM_MODE_PANEL_ORIENTATION_NORMAL, "Normal"),
            (DRM_MODE_PANEL_ORIENTATION_BOTTOM_UP, "Upside Down"),
            (DRM_MODE_PANEL_ORIENTATION_LEFT_UP, "Left Side Up"),
            (DRM_MODE_PANEL_ORIENTATION_RIGHT_UP, "Right Side Up"),
        ]);
        dev.properties.panel_orientation =
            io.property_create_enum(dev.id, "panel orientation", DRM_MODE_PROP_IMMUTABLE, &enums);
        if dev.properties.panel_orientation.is_none() {
            return Err(ENOMEM);
        }
    }
    if let Some(property) = dev.properties.panel_orientation {
        io.attach_property(connector.id, property, orientation.as_u32() as u64);
    }
    Ok(())
}

impl PanelOrientation {
    fn as_u32(self) -> u32 {
        match self {
            Self::Unknown => DRM_MODE_PANEL_ORIENTATION_UNKNOWN,
            Self::Normal => DRM_MODE_PANEL_ORIENTATION_NORMAL,
            Self::BottomUp => DRM_MODE_PANEL_ORIENTATION_BOTTOM_UP,
            Self::LeftUp => DRM_MODE_PANEL_ORIENTATION_LEFT_UP,
            Self::RightUp => DRM_MODE_PANEL_ORIENTATION_RIGHT_UP,
            Self::Other(value) => value,
        }
    }
}

// upstream: drm_connector.c drm_connector_set_panel_orientation_with_quirk()
pub fn drm_connector_set_panel_orientation_with_quirk<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    mut orientation: PanelOrientation,
    width: i32,
    height: i32,
) -> KResult<()> {
    let quirk = io.panel_orientation_quirk(width, height);
    if quirk != PanelOrientation::Unknown {
        orientation = quirk;
    }
    drm_connector_set_panel_orientation(io, dev, connector, orientation)
}

// upstream: drm_connector.c drm_connector_set_orientation_from_panel()
pub fn drm_connector_set_orientation_from_panel<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    connector: &mut Connector,
    panel: Option<u64>,
) -> KResult<()> {
    let orientation = panel.map_or(PanelOrientation::Unknown, |panel| {
        io.panel_orientation(panel)
    });
    drm_connector_set_panel_orientation(io, dev, connector, orientation)
}

// upstream: drm_connector.c drm_connector_create_privacy_screen_properties()
pub fn drm_connector_create_privacy_screen_properties<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    connector: &mut Connector,
) {
    if connector.privacy_screen_sw_property.is_some() {
        return;
    }
    let values = enum_values(&[
        (PRIVACY_SCREEN_DISABLED as u32, "Disabled"),
        (PRIVACY_SCREEN_ENABLED as u32, "Enabled"),
        (PRIVACY_SCREEN_DISABLED_LOCKED as u32, "Disabled-locked"),
        (PRIVACY_SCREEN_ENABLED_LOCKED as u32, "Enabled-locked"),
    ]);
    connector.privacy_screen_sw_property = io.create_property(
        dev,
        &PropertySpec {
            name: "privacy-screen sw-state",
            kind: PropertyKind::Enum,
            flags: DRM_MODE_PROP_ENUM,
            enums: values[..2].to_vec(),
        },
    );
    connector.privacy_screen_hw_property = io.create_property(
        dev,
        &PropertySpec {
            name: "privacy-screen hw-state",
            kind: PropertyKind::Enum,
            flags: DRM_MODE_PROP_IMMUTABLE | DRM_MODE_PROP_ENUM,
            enums: values,
        },
    );
}

// upstream: drm_connector.c drm_connector_attach_privacy_screen_properties()
pub fn drm_connector_attach_privacy_screen_properties<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &Connector,
) {
    let property = match connector.privacy_screen_sw_property {
        Some(property) => property,
        None => return,
    };
    io.attach_property(connector.id, property, PRIVACY_SCREEN_DISABLED);
    if let Some(property) = connector.privacy_screen_hw_property {
        io.attach_property(connector.id, property, PRIVACY_SCREEN_DISABLED);
    }
}

// upstream: drm_connector.c drm_connector_update_privacy_screen_properties()
pub fn drm_connector_update_privacy_screen_properties<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &mut Connector,
    set_sw_state: bool,
) {
    if let Some(provider) = connector.privacy_screen {
        let (sw_state, hw_state) = io.privacy_screen_state(provider);
        if set_sw_state {
            if let Some(state) = connector.state.as_mut() {
                state.privacy_screen_sw_state = sw_state;
            }
        }
        if let Some(property) = connector.privacy_screen_hw_property {
            io.object_property_set(connector.id, property, hw_state);
        }
    }
}

// upstream: drm_connector.c drm_connector_privacy_screen_notifier()
pub fn drm_connector_privacy_screen_notifier<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &mut Connector,
) -> i32 {
    io.lock("mode_config.connection_mutex");
    drm_connector_update_privacy_screen_properties(io, connector, true);
    io.unlock("mode_config.connection_mutex");
    io.sysfs_property_event(connector.id, connector.privacy_screen_sw_property);
    io.sysfs_property_event(connector.id, connector.privacy_screen_hw_property);
    0
}

// upstream: drm_connector.c drm_connector_attach_privacy_screen_provider()
pub fn drm_connector_attach_privacy_screen_provider<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    connector: &mut Connector,
    provider: u64,
) {
    connector.privacy_screen = Some(provider);
    drm_connector_create_privacy_screen_properties(io, dev, connector);
    drm_connector_update_privacy_screen_properties(io, connector, true);
    drm_connector_attach_privacy_screen_properties(io, connector);
}

// upstream: drm_connector.c drm_connector_update_privacy_screen()
pub fn drm_connector_update_privacy_screen<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &mut Connector,
    state: &ConnectorState,
) {
    let provider = match connector.privacy_screen {
        Some(provider) => provider,
        None => return,
    };
    if io
        .set_privacy_screen_state(provider, state.privacy_screen_sw_state)
        .is_err()
    {
        return;
    }
    drm_connector_update_privacy_screen_properties(io, connector, false);
}

// upstream: drm_connector.c drm_connector_set_obj_prop()
pub fn drm_connector_set_obj_prop<I: ConnectorUapiIo>(
    io: &mut I,
    connector: &Connector,
    property: PropertyId,
    value: u64,
    dpms_property: Option<PropertyId>,
) -> KResult<()> {
    let result = if Some(property) == dpms_property {
        io.dpms(connector.id, value as i32)
    } else {
        io.set_legacy_property(connector.id, property, value)
    };
    if result.is_ok() {
        io.object_property_set(connector.id, property, value);
    }
    result
}

// upstream: drm_connector.c drm_connector_property_set_ioctl()
pub fn drm_connector_property_set_ioctl<I: ConnectorUapiIo>(
    io: &mut I,
    dev: DeviceId,
    connector: ConnectorId,
    property: PropertyId,
    value: u64,
    file: u64,
) -> KResult<()> {
    io.set_property_ioctl(dev, connector, property, value, file)
}

// upstream: drm_connector.c drm_connector_get_encoder()
pub fn drm_connector_get_encoder(connector: &Connector) -> Option<EncoderId> {
    match connector.state.as_ref() {
        Some(state) => state.best_encoder,
        None => connector.current_encoder,
    }
}

// upstream: drm_connector.c drm_mode_expose_to_userspace()
pub fn drm_mode_expose_to_userspace(
    mode: &DisplayMode,
    modes: &[DisplayMode],
    file: &ConnectorFile,
) -> bool {
    if !file.stereo_allowed && (mode.stereo_3d || mode.flags & DRM_MODE_FLAG_3D_MASK != 0) {
        return false;
    }
    if !file.aspect_ratio_allowed {
        for other in modes {
            if other.expose_to_userspace
                && connector_modes_match(
                    other,
                    mode,
                    DRM_MODE_MATCH_TIMINGS
                        | DRM_MODE_MATCH_CLOCK
                        | DRM_MODE_MATCH_FLAGS
                        | DRM_MODE_MATCH_3D_FLAGS,
                )
            {
                return false;
            }
        }
    }
    true
}

fn connector_modes_match(a: &DisplayMode, b: &DisplayMode, match_flags: u32) -> bool {
    if match_flags & DRM_MODE_MATCH_CLOCK != 0 && !connector_mode_match_clock(a.clock, b.clock) {
        return false;
    }
    if match_flags & DRM_MODE_MATCH_TIMINGS != 0
        && (
            a.hdisplay,
            a.hsync_start,
            a.hsync_end,
            a.htotal,
            a.hskew,
            a.vdisplay,
            a.vsync_start,
            a.vsync_end,
            a.vtotal,
            a.vscan,
        ) != (
            b.hdisplay,
            b.hsync_start,
            b.hsync_end,
            b.htotal,
            b.hskew,
            b.vdisplay,
            b.vsync_start,
            b.vsync_end,
            b.vtotal,
            b.vscan,
        )
    {
        return false;
    }
    if match_flags & DRM_MODE_MATCH_FLAGS != 0
        && (a.flags & !DRM_MODE_FLAG_3D_MASK) != (b.flags & !DRM_MODE_FLAG_3D_MASK)
    {
        return false;
    }
    if match_flags & DRM_MODE_MATCH_3D_FLAGS != 0
        && (a.flags & DRM_MODE_FLAG_3D_MASK) != (b.flags & DRM_MODE_FLAG_3D_MASK)
    {
        return false;
    }
    if match_flags & DRM_MODE_MATCH_ASPECT_RATIO != 0 && a.aspect_ratio != b.aspect_ratio {
        return false;
    }
    true
}

fn connector_mode_match_clock(a: u32, b: u32) -> bool {
    if a != 0 && b != 0 {
        1_000_000_000u64 / a as u64 == 1_000_000_000u64 / b as u64
    } else {
        a == b
    }
}

// upstream: drm_connector.c drm_mode_getconnector()
pub fn drm_mode_getconnector<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    args: &mut GetConnectorArgs,
    file: &ConnectorFile,
    encoders: &[Encoder],
) -> KResult<()> {
    if !dev.modeset_supported {
        return Err(EOPNOTSUPP);
    }
    let mut connector = io
        .connector_lookup(dev.id, args.connector_id, file.id)
        .ok_or(ENOENT)?;
    let encoder_count = connector.possible_encoders.count_ones();
    if args.count_encoders >= encoder_count && encoder_count != 0 {
        let mut copied = 0usize;
        for index in 0..32u32 {
            if connector.possible_encoders & (1u32 << index) == 0 {
                continue;
            }
            let encoder_id = encoders
                .iter()
                .find(|encoder| encoder.index == index)
                .map(|encoder| encoder.id)
                .or_else(|| io.encoder_id_by_index(dev.id, index))
                .unwrap_or(0);
            if !io.copy_encoder_to_user(args.encoders_ptr, copied, encoder_id) {
                io.connector_put(connector.id);
                return Err(EFAULT);
            }
            copied += 1;
        }
    }
    args.count_encoders = encoder_count;
    args.connector_id = connector.id;
    args.connector_type = connector.connector_type;
    args.connector_type_id = connector.connector_type_id;
    let is_current_master = io.current_master(file.id) || file.is_master;
    io.lock("mode_config.mutex");
    if args.count_modes == 0 {
        if is_current_master {
            io.fill_modes(&mut connector, dev.max_width, dev.max_height);
        }
    }
    args.mm_width = connector.display_info.width_mm;
    args.mm_height = connector.display_info.height_mm;
    args.subpixel = connector.display_info.subpixel_order;
    args.connection = match connector.status {
        ConnectorStatus::Connected => CONNECTOR_STATUS_CONNECTED,
        ConnectorStatus::Disconnected => CONNECTOR_STATUS_DISCONNECTED,
        _ => CONNECTOR_STATUS_UNKNOWN,
    };

    // The mode tags intentionally persist between the count and copy pass,
    // exactly as the C list walk does. Preserve earlier tags for aspect-ratio
    // duplicate suppression while marking the exposed subset.
    let mut mode_snapshot = connector.modes.clone();
    let mut mode_count = 0u32;
    for index in 0..mode_snapshot.len() {
        let current = mode_snapshot[index].clone();
        if connector.modes[index].expose_to_userspace {
            io.warn("mode exposure tag unexpectedly set before userspace enumeration");
        }
        if drm_mode_expose_to_userspace(&current, &mode_snapshot, file) {
            mode_snapshot[index].expose_to_userspace = true;
            connector.modes[index].expose_to_userspace = true;
            mode_count += 1;
        }
    }
    if args.count_modes >= mode_count && mode_count != 0 {
        let mut copied = 0usize;
        for index in 0..connector.modes.len() {
            if !connector.modes[index].expose_to_userspace {
                continue;
            }
            connector.modes[index].expose_to_userspace = false;
            let mode = &connector.modes[index];
            let mut info = ModeInfo {
                clock: mode.clock,
                hdisplay: mode.hdisplay as u16,
                hsync_start: mode.hsync_start as u16,
                hsync_end: mode.hsync_end as u16,
                htotal: mode.htotal as u16,
                hskew: mode.hskew as u16,
                vdisplay: mode.vdisplay as u16,
                vsync_start: mode.vsync_start as u16,
                vsync_end: mode.vsync_end as u16,
                vtotal: mode.vtotal as u16,
                vscan: mode.vscan as u16,
                vrefresh: connector_mode_vrefresh(mode),
                flags: mode.flags,
                mode_type: mode.mode_type,
                name: mode.name.clone(),
            };
            if !file.aspect_ratio_allowed {
                info.flags &= !DRM_MODE_FLAG_PIC_AR_MASK;
            }
            if !io.copy_mode_to_user(args.modes_ptr, copied, &info) {
                for remaining in index..connector.modes.len() {
                    connector.modes[remaining].expose_to_userspace = false;
                }
                io.unlock("mode_config.mutex");
                io.connector_store(&connector);
                io.connector_put(connector.id);
                return Err(EFAULT);
            }
            copied += 1;
        }
    } else {
        for mode in &mut connector.modes {
            mode.expose_to_userspace = false;
        }
    }
    args.count_modes = mode_count;
    io.unlock("mode_config.mutex");

    io.lock("mode_config.connection_mutex");
    args.encoder_id = drm_connector_get_encoder(&connector)
        .map(|encoder| io.get_encoder_id(connector.id, encoder))
        .unwrap_or(0);
    let property_result = io.copy_properties_to_user(
        connector.id,
        file.atomic,
        file.plane_color_pipeline,
        args.props_ptr,
        args.prop_values_ptr,
        &mut args.count_props,
    );
    io.unlock("mode_config.connection_mutex");
    io.connector_store(&connector);
    io.connector_put(connector.id);
    property_result
}

fn connector_mode_vrefresh(mode: &DisplayMode) -> u32 {
    if mode.htotal == 0 || mode.vtotal == 0 || mode.clock == 0 {
        return 0;
    }
    let mut numerator = match (mode.clock as u64).checked_mul(1000) {
        Some(value) => value,
        None => return 0,
    };
    let mut denominator = match (mode.htotal as u64).checked_mul(mode.vtotal as u64) {
        Some(value) => value,
        None => return 0,
    };
    if mode.flags & DRM_MODE_FLAG_INTERLACE != 0 {
        numerator = match numerator.checked_mul(2) {
            Some(value) => value,
            None => return 0,
        };
    }
    if mode.flags & DRM_MODE_FLAG_DBLSCAN != 0 {
        denominator = match denominator.checked_mul(2) {
            Some(value) => value,
            None => return 0,
        };
    }
    if mode.vscan > 1 {
        denominator = match denominator.checked_mul(mode.vscan as u64) {
            Some(value) => value,
            None => return 0,
        };
    }
    let rounded = match numerator.checked_add(denominator / 2) {
        Some(value) => value,
        None => return 0,
    };
    (rounded / denominator) as u32
}

// upstream: drm_connector.c drm_connector_find_by_fwnode()
pub fn drm_connector_find_by_fwnode<I: ConnectorUapiIo>(
    io: &mut I,
    connectors: &[Connector],
    fwnode: Option<FwnodeId>,
) -> KResult<ConnectorId> {
    let fwnode = fwnode.ok_or(ENODEV)?;
    io.lock("connector_list_lock");
    let found = connectors
        .iter()
        .find(|connector| {
            connector.registration_on_global_list
                && (connector.fwnode == Some(fwnode) || connector.secondary_fwnode == Some(fwnode))
                && io.kref_get_unless_zero(connector.id)
        })
        .map(|connector| connector.id);
    io.unlock("connector_list_lock");
    found.ok_or(ENODEV)
}

// upstream: drm_connector.c drm_connector_oob_hotplug_event()
pub fn drm_connector_oob_hotplug_event<I: ConnectorUapiIo>(
    io: &mut I,
    connectors: &[Connector],
    fwnode: Option<FwnodeId>,
    status: ConnectorStatus,
) {
    let connector = match drm_connector_find_by_fwnode(io, connectors, fwnode) {
        Ok(connector) => connector,
        Err(_) => return,
    };
    if connectors
        .iter()
        .any(|entry| entry.id == connector && entry.has_oob_hotplug_callback)
    {
        io.out_of_band_hotplug(connector, status);
    }
    io.connector_put(connector);
}

// upstream: drm_connector.c drm_tile_group_free()
pub fn drm_tile_group_free<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    group: TileGroup,
) {
    io.lock("mode_config.idr_mutex");
    if let Some(index) = dev
        .tile_groups
        .iter()
        .position(|entry| entry.id == group.id)
    {
        dev.tile_groups.remove(index);
    }
    io.tile_group_remove(dev.id, group.id);
    io.unlock("mode_config.idr_mutex");
    io.drm_mode_tile_group_free(&group);
}

// upstream: drm_connector.c drm_mode_put_tile_group()
pub fn drm_mode_put_tile_group<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    group: TileGroup,
) {
    let mut free = None;
    if let Some(stored) = dev
        .tile_groups
        .iter_mut()
        .find(|stored| stored.id == group.id)
    {
        if stored.refcount > 0 {
            stored.refcount -= 1;
        }
        if stored.refcount == 0 {
            free = Some(stored.clone());
        }
    }
    if let Some(group) = free {
        drm_tile_group_free(io, dev, group);
    }
}

// upstream: drm_connector.c drm_mode_get_tile_group()
pub fn drm_mode_get_tile_group<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    topology_id: &[u8; 9],
) -> Option<TileGroup> {
    io.lock("mode_config.idr_mutex");
    let group = dev
        .tile_groups
        .iter_mut()
        .find(|group| group.topology_id.as_slice() == &topology_id[..8]);
    let result = group.and_then(|group| {
        if group.refcount == 0 || !io.get_tile_ref(group) {
            return None;
        }
        group.refcount += 1;
        Some(group.clone())
    });
    io.unlock("mode_config.idr_mutex");
    result
}

// upstream: drm_connector.c drm_mode_create_tile_group()
pub fn drm_mode_create_tile_group<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &mut ConnectorDevice,
    topology_id: &[u8; 9],
) -> Option<TileGroup> {
    let mut group = TileGroup {
        id: 0,
        topology_id: topology_id[..8].try_into().ok()?,
        refcount: 1,
    };
    io.lock("mode_config.idr_mutex");
    let id = match io.tile_group_insert(dev.id, &group) {
        Ok(id) => id,
        Err(_) => {
            io.unlock("mode_config.idr_mutex");
            return None;
        }
    };
    group.id = id;
    dev.next_tile_id = cmp::max(dev.next_tile_id, id.saturating_add(1));
    dev.tile_groups.push(group.clone());
    io.unlock("mode_config.idr_mutex");
    Some(group)
}

// upstream: drm_connector.c drm_connector_attach_panel_type_property()
pub fn drm_connector_attach_panel_type_property<I: ConnectorUapiIo>(
    io: &mut I,
    dev: &ConnectorDevice,
    connector: &Connector,
) {
    if let Some(property) = dev.properties.panel_type {
        io.attach_property(connector.id, property, DRM_MODE_PANEL_TYPE_UNKNOWN as u64);
    }
}
