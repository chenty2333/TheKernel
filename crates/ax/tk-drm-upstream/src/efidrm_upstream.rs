use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, zeroed},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use tk_linux_kapi::{
    config::{EINVAL, ENODEV, ENXIO},
    print::DrmLogLevel,
};

use crate::{
    drm_client_setup_upstream::drm_client_setup,
    drm_connector_types::{DRM_MODE_PANEL_ORIENTATION_UNKNOWN, DrmConnector, DrmConnectorFuncs},
    drm_connector_upstream::{
        drm_connector_attach_edid_property, drm_connector_attach_encoder, drm_connector_cleanup,
        drm_connector_init, drm_connector_set_panel_orientation_with_quirk,
    },
    drm_crtc_types::{DrmCrtc, DrmCrtcFuncs},
    drm_crtc_upstream::{drm_crtc_cleanup, drm_crtc_init_with_planes, drm_crtc_mask},
    drm_device_types::DrmDevice,
    drm_drv_types::DrmDriver,
    drm_drv_upstream::{__devm_drm_dev_alloc, drm_dev_put, drm_dev_register, drm_dev_unplug},
    drm_edid_upstream::drm_edid_header_is_valid,
    drm_encoder_types::{DrmEncoder, DrmEncoderFuncs},
    drm_encoder_upstream::{drm_encoder_cleanup, drm_encoder_init},
    drm_fbdev_shmem_upstream::drm_fbdev_shmem_driver_fbdev_probe,
    drm_fourcc_types::DrmFormatInfo,
    drm_gem_shmem_helper_upstream::{drm_gem_shmem_dumb_create, drm_gem_shmem_prime_import_no_map},
    drm_gem_types::DrmGemObject,
    drm_mode_config_types::DrmModeConfigFuncs,
    drm_mode_config_upstream::{drm_mode_config_reset, drmm_mode_config_init},
    drm_mode_object_upstream::{DRIVER_ATOMIC, DRIVER_MODESET},
    drm_modeset_helper_upstream::{drm_mode_config_helper_resume, drm_mode_config_helper_suspend},
    drm_modeset_helper_vtables_types::{
        DrmConnectorHelperFuncs, DrmCrtcHelperFuncs, DrmPlaneHelperFuncs,
    },
    drm_plane_types::{DRM_PLANE_TYPE_PRIMARY, DrmPlane, DrmPlaneFuncs},
    drm_plane_upstream::{
        drm_plane_cleanup, drm_plane_enable_fb_damage_clips, drm_universal_plane_init,
    },
    drm_print_upstream::drm_dev_printk,
    drm_sysfb_helper_types::{
        DRM_SYSFB_PLANE_FORMAT_MODIFIERS, DRM_SYSFB_PLANE_NFORMATS, DrmSysfbDevice, DrmSysfbFormat,
        PIXEL_FORMAT_RGB565, PIXEL_FORMAT_RGB888, PIXEL_FORMAT_XBGR8888, PIXEL_FORMAT_XRGB1555,
        PIXEL_FORMAT_XRGB8888, PIXEL_FORMAT_XRGB2101010, PixelFormat, Resource, ScreenInfo,
        VIDEO_TYPE_EFI,
    },
    drm_sysfb_modeset_upstream::{
        DRM_SYSFB_CONNECTOR_FUNCS, DRM_SYSFB_CONNECTOR_HELPER_FUNCS, DRM_SYSFB_CRTC_FUNCS,
        DRM_SYSFB_CRTC_HELPER_FUNCS, DRM_SYSFB_MODE_CONFIG_FUNCS, DRM_SYSFB_PLANE_FUNCS,
        DRM_SYSFB_PLANE_HELPER_FUNCS, drm_connector_helper_add, drm_crtc_helper_add,
        drm_plane_helper_add, drm_sysfb_build_fourcc_list, drm_sysfb_mode,
    },
    drm_sysfb_screen_info_upstream::{
        drm_sysfb_get_height_si, drm_sysfb_get_memory_si, drm_sysfb_get_stride_si,
        drm_sysfb_get_visible_size_si, drm_sysfb_get_width_si, screen_info_pixel_format,
        screen_info_video_type,
    },
    drm_sysfb_upstream::{drm_sysfb_get_format, drm_warn},
    iosys_map_types::IosysMap,
    uapi_drm_consts::{
        DRM_FORMAT_RGB565, DRM_FORMAT_RGB888, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB1555,
        DRM_FORMAT_XRGB8888, DRM_FORMAT_XRGB2101010, DRM_MODE_CONNECTOR_Unknown,
        DRM_MODE_ENCODER_NONE,
    },
};

// upstream: drivers/gpu/drm/sysfb/drm_sysfb_helper.h EDID_LENGTH (include/drm/drm_edid.h)
const EDID_LENGTH: usize = 128;

// upstream: drivers/gpu/drm/sysfb/efidrm.c (local driver constants)
const DRIVER_NAME: &[u8] = b"efidrm\0";
const DRIVER_DESC: &[u8] = b"DRM driver for EFI platform devices\0";
const DRIVER_MAJOR: i32 = 1;
const DRIVER_MINOR: i32 = 0;
// upstream: include/drm/drm_drv.h DRIVER_GEM
const DRIVER_GEM: u32 = 1 << 0;

// upstream: include/drm/drm_gem_atomic_helper.h DRM_SHADOW_PLANE_MAX_WIDTH / _HEIGHT
const DRM_SHADOW_PLANE_MAX_WIDTH: u64 = 4096;
const DRM_SHADOW_PLANE_MAX_HEIGHT: u64 = 4096;

// upstream: include/linux/efi.h
pub const EFI_MEMORY_UC: u64 = 1 << 0;
pub const EFI_MEMORY_WC: u64 = 1 << 1;
pub const EFI_MEMORY_WT: u64 = 1 << 2;
pub const EFI_MEMORY_WB: u64 = 1 << 3;
const EFI_PAGE_SHIFT: u32 = 12;

/// The part of `efi_memory_desc_t` that efidrm reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EfiMemoryDesc {
    pub phys_addr: u64,
    pub num_pages: u64,
    pub attribute: u64,
}

/// Memory type requested from [`SysfbPlatformOps::map_io`]; mirrors the
/// `devm_ioremap_wc` / `devm_ioremap` / `devm_memremap(MEMREMAP_WT|WB)` calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysfbMapType {
    WriteCombine,
    Uncached,
    WriteThrough,
    WriteBack,
}

/// Platform services efidrm takes from outside DRM. See the module header.
pub struct SysfbPlatformOps {
    /// Claim the firmware aperture. Returns 0 or a negative errno.
    pub acquire_aperture: unsafe fn(base: u64, size: u64) -> i32,
    /// Reserve the framebuffer range. `false` is not fatal.
    pub request_mem_region: unsafe fn(base: u64, size: u64, name: &str) -> bool,
    /// Map the framebuffer with the given memory type.
    pub map_io: unsafe fn(base: u64, size: u64, kind: SysfbMapType) -> Result<*mut c_void, i32>,
    /// `efi_enabled(EFI_MEMMAP)`.
    pub efi_memmap_enabled: fn() -> bool,
    /// The EFI memory descriptor covering `phys`, if any.
    pub efi_mem_desc_lookup: fn(phys: u64) -> Option<EfiMemoryDesc>,
}

static SYSFB_PLATFORM_OPS: AtomicPtr<SysfbPlatformOps> = AtomicPtr::new(ptr::null_mut());

/// Installs the process-global sysfb platform provider. One-shot.
pub fn install_sysfb_platform_ops(ops: &'static SysfbPlatformOps) -> Result<(), &'static str> {
    SYSFB_PLATFORM_OPS
        .compare_exchange(
            ptr::null_mut(),
            ptr::from_ref(ops).cast_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| "sysfb platform ops already installed")
}

fn sysfb_platform_ops() -> Option<&'static SysfbPlatformOps> {
    let ops = SYSFB_PLATFORM_OPS.load(Ordering::Acquire);
    // SAFETY: install_sysfb_platform_ops publishes only a static table.
    unsafe { ops.as_ref() }
}

/// Format table from `efidrm_get_format_si()`.
pub static EFIDRM_FORMATS: [DrmSysfbFormat; 6] = [
    DrmSysfbFormat {
        pixel: PIXEL_FORMAT_XRGB1555,
        fourcc: DRM_FORMAT_XRGB1555,
    },
    DrmSysfbFormat {
        pixel: PIXEL_FORMAT_RGB565,
        fourcc: DRM_FORMAT_RGB565,
    },
    DrmSysfbFormat {
        pixel: PIXEL_FORMAT_RGB888,
        fourcc: DRM_FORMAT_RGB888,
    },
    DrmSysfbFormat {
        pixel: PIXEL_FORMAT_XRGB8888,
        fourcc: DRM_FORMAT_XRGB8888,
    },
    DrmSysfbFormat {
        pixel: PIXEL_FORMAT_XBGR8888,
        fourcc: DRM_FORMAT_XBGR8888,
    },
    DrmSysfbFormat {
        pixel: PIXEL_FORMAT_XRGB2101010,
        fourcc: DRM_FORMAT_XRGB2101010,
    },
];

/// Device layout: the embedded sysfb device followed by the modeset objects.
#[repr(C)]
pub struct EfidrmDevice {
    pub sysfb: DrmSysfbDevice,
    /// upstream: `u32 formats[DRM_SYSFB_PLANE_NFORMATS(1)]`
    pub formats: [u32; DRM_SYSFB_PLANE_NFORMATS(1)],
    pub primary_plane: DrmPlane,
    pub crtc: DrmCrtc,
    pub encoder: DrmEncoder,
    pub connector: DrmConnector,
}

// ---------------------------------------------------------------------------
// Vtables: DRM_SYSFB_*_FUNCS plus the efidrm `.destroy` members
// ---------------------------------------------------------------------------

static EFIDRM_PRIMARY_PLANE_HELPER_FUNCS: DrmPlaneHelperFuncs = DRM_SYSFB_PLANE_HELPER_FUNCS();

const fn efidrm_plane_funcs() -> DrmPlaneFuncs {
    let mut f = DRM_SYSFB_PLANE_FUNCS();
    f.destroy = Some(drm_plane_cleanup);
    f
}
static EFIDRM_PRIMARY_PLANE_FUNCS: DrmPlaneFuncs = efidrm_plane_funcs();

static EFIDRM_CRTC_HELPER_FUNCS: DrmCrtcHelperFuncs = DRM_SYSFB_CRTC_HELPER_FUNCS();

const fn efidrm_crtc_funcs() -> DrmCrtcFuncs {
    let mut f = DRM_SYSFB_CRTC_FUNCS();
    f.destroy = Some(drm_crtc_cleanup);
    f
}
static EFIDRM_CRTC_FUNCS: DrmCrtcFuncs = efidrm_crtc_funcs();

const fn efidrm_encoder_funcs() -> DrmEncoderFuncs {
    // SAFETY: plain Option<fn> slots; all-zero means unset.
    let mut f: DrmEncoderFuncs = unsafe { zeroed() };
    f.destroy = Some(drm_encoder_cleanup);
    f
}
static EFIDRM_ENCODER_FUNCS: DrmEncoderFuncs = efidrm_encoder_funcs();

static EFIDRM_CONNECTOR_HELPER_FUNCS: DrmConnectorHelperFuncs = DRM_SYSFB_CONNECTOR_HELPER_FUNCS();

const fn efidrm_connector_funcs() -> DrmConnectorFuncs {
    let mut f = DRM_SYSFB_CONNECTOR_FUNCS();
    f.destroy = Some(drm_connector_cleanup);
    f
}
static EFIDRM_CONNECTOR_FUNCS: DrmConnectorFuncs = efidrm_connector_funcs();

static EFIDRM_MODE_CONFIG_FUNCS: DrmModeConfigFuncs = DRM_SYSFB_MODE_CONFIG_FUNCS();

// ---------------------------------------------------------------------------
// DRM driver record
// ---------------------------------------------------------------------------

// The driver slot takes `void *dmabuf`; the shmem helper takes `struct dma_buf *`.
unsafe extern "C" fn efidrm_gem_prime_import(
    dev: *mut DrmDevice,
    dmabuf: *mut c_void,
) -> *mut DrmGemObject {
    drm_gem_shmem_prime_import_no_map(dev, dmabuf.cast())
}

// upstream: efidrm.c efidrm_driver (DRM_GEM_SHMEM_DRIVER_OPS and
// DRM_FBDEV_SHMEM_DRIVER_OPS expanded; .fops left null, see module header)
const fn efidrm_driver_def() -> DrmDriver {
    // SAFETY: DrmDriver is a record of Option<fn> slots and raw pointers; all-zero is valid.
    let mut d: DrmDriver = unsafe { zeroed() };
    d.dumb_create = Some(drm_gem_shmem_dumb_create);
    d.gem_prime_import = Some(efidrm_gem_prime_import);
    d.fbdev_probe = Some(drm_fbdev_shmem_driver_fbdev_probe);
    d.name = DRIVER_NAME.as_ptr().cast_mut().cast::<c_char>();
    d.desc = DRIVER_DESC.as_ptr().cast_mut().cast::<c_char>();
    d.major = DRIVER_MAJOR;
    d.minor = DRIVER_MINOR;
    d.driver_features = DRIVER_ATOMIC | DRIVER_GEM | DRIVER_MODESET;
    d
}

struct SyncDrmDriver(DrmDriver);
// SAFETY: the record is built at compile time and never mutated; its raw
// pointers reference static strings.
unsafe impl Sync for SyncDrmDriver {}

static EFIDRM_DRIVER: SyncDrmDriver = SyncDrmDriver(efidrm_driver_def());

// ---------------------------------------------------------------------------
// Logging and small helpers
// ---------------------------------------------------------------------------

// upstream: drm_print.c drm_err()
fn drm_err(dev: *const DrmDevice, text: &str) {
    drm_dev_printk(dev.cast(), DrmLogLevel::Error, text);
}

// upstream: include/linux/iosys-map.h iosys_map_set_vaddr_iomem()
unsafe fn iosys_map_set_vaddr_iomem(map: *mut IosysMap, vaddr: *mut c_void) {
    (*map)._anon_0.vaddr_iomem = vaddr;
    (*map).is_iomem = true;
}

// upstream: include/linux/iosys-map.h iosys_map_set_vaddr()
unsafe fn iosys_map_set_vaddr(map: *mut IosysMap, vaddr: *mut c_void) {
    (*map)._anon_0.vaddr = vaddr;
    (*map).is_iomem = false;
}

// upstream: efidrm.c efidrm_get_format_si()
pub(crate) unsafe fn efidrm_get_format_si(
    dev: *const DrmDevice,
    si: &ScreenInfo,
) -> *const DrmFormatInfo {
    let mut pixel: PixelFormat = zeroed();
    if screen_info_pixel_format(si, &mut pixel) != 0 {
        return ptr::null();
    }
    drm_sysfb_get_format(dev, &EFIDRM_FORMATS, &pixel)
}

// upstream: efidrm.c efidrm_get_mem_flags()
/// Derives the CPU mapping attributes for `[start, start + len)` from the EFI
/// memory map. Falls back to uncached or write-combined when the range is not
/// covered by a single descriptor.
pub(crate) fn efidrm_get_mem_flags(ops: &SysfbPlatformOps, start: u64, len: u64) -> u64 {
    let mut attribute = EFI_MEMORY_UC | EFI_MEMORY_WC | EFI_MEMORY_WT | EFI_MEMORY_WB;
    let mut mem_flags = EFI_MEMORY_WC | EFI_MEMORY_UC;
    let end = start.wrapping_add(len);

    if !(ops.efi_memmap_enabled)() {
        return mem_flags;
    }
    let Some(md) = (ops.efi_mem_desc_lookup)(start) else {
        return mem_flags;
    };

    let md_end = md.phys_addr.wrapping_add(md.num_pages << EFI_PAGE_SHIFT);
    if end > md_end {
        return mem_flags;
    }

    attribute &= md.attribute;
    if attribute != 0 {
        mem_flags |= EFI_MEMORY_WT | EFI_MEMORY_WB;
        mem_flags &= attribute;
    }
    mem_flags
}

// Maps an ioremap-style request; any failure is reported as -ENXIO, as efidrm does.
unsafe fn map_iomem(
    ops: &SysfbPlatformOps,
    start: u64,
    size: u64,
    kind: SysfbMapType,
) -> Result<*mut c_void, i32> {
    match (ops.map_io)(start, size, kind) {
        Ok(p) if !p.is_null() => Ok(p),
        _ => Err(-ENXIO),
    }
}

// upstream: efidrm.c devm_drm_dev_alloc(&pdev->dev, drv, struct efidrm_device, sysfb.dev)
unsafe fn devm_drm_dev_alloc(driver: *const DrmDriver) -> Result<*mut EfidrmDevice, i32> {
    let offset = offset_of!(EfidrmDevice, sysfb) + offset_of!(DrmSysfbDevice, dev);
    let p = __devm_drm_dev_alloc(
        ptr::null_mut(),
        driver,
        core::mem::size_of::<EfidrmDevice>(),
        offset,
    );
    let v = p as isize;
    if (-4095..0).contains(&v) {
        return Err(v as i32); // ERR_PTR
    }
    if p.is_null() {
        return Err(-EINVAL);
    }
    Ok(p.cast::<EfidrmDevice>())
}

// upstream: efidrm.c efidrm_device_create() (hardware settings, memory, modesetting)
unsafe fn efidrm_device_init(
    efi: *mut EfidrmDevice,
    ops: &SysfbPlatformOps,
    si: &ScreenInfo,
    edid: Option<&'static [u8; EDID_LENGTH]>,
) -> Result<(), i32> {
    let sysfb: *mut DrmSysfbDevice = &mut (*efi).sysfb;
    let dev: *mut DrmDevice = &mut (*sysfb).dev;

    // Hardware settings
    let format = efidrm_get_format_si(dev, si);
    if format.is_null() {
        return Err(-EINVAL);
    }

    let width = drm_sysfb_get_width_si(dev, si);
    if width < 0 {
        return Err(width);
    }
    let height = drm_sysfb_get_height_si(dev, si);
    if height < 0 {
        return Err(height);
    }

    let mut resbuf = Resource {
        start: 0,
        end: 0,
        name: ptr::null(),
        flags: 0,
    };
    let (res_start, res_size) = match drm_sysfb_get_memory_si(dev, si, &mut resbuf) {
        Some(res) => (res.start, res.end - res.start + 1), // resource_size()
        None => return Err(-EINVAL),
    };

    let stride = drm_sysfb_get_stride_si(dev, si, format, width as u32, height as u32, res_size);
    if stride < 0 {
        return Err(stride);
    }
    let vsize = drm_sysfb_get_visible_size_si(dev, si, height as u32, stride as u32, res_size);
    if vsize < 0 {
        return Err(vsize as i32);
    }
    let vsize = vsize as u64;

    // #if defined(CONFIG_FIRMWARE_EDID): keep the firmware EDID only if its header is valid.
    if let Some(edid) = edid {
        if drm_edid_header_is_valid(edid.as_ptr().cast()) == 8 {
            (*sysfb).edid = edid.as_ptr();
        }
    }

    (*sysfb).fb_mode = drm_sysfb_mode(width as u32, height as u32, 0, 0);
    (*sysfb).fb_format = format;
    (*sysfb).fb_pitch = stride as u32;

    // Memory management
    let ret = (ops.acquire_aperture)(res_start, vsize);
    if ret != 0 {
        drm_err(dev, "could not acquire memory range\n");
        return Err(ret);
    }

    // On failure the whole firmware resource is mapped instead of the visible part.
    let mem_start = res_start;
    let mut mem_size = res_size;
    if (ops.request_mem_region)(res_start, vsize, "efidrm") {
        mem_size = vsize;
    } else {
        drm_warn(dev, "could not acquire memory region\n");
    }

    let mem_flags = efidrm_get_mem_flags(ops, res_start, vsize);
    if mem_flags & EFI_MEMORY_WC != 0 {
        let p = map_iomem(ops, mem_start, mem_size, SysfbMapType::WriteCombine)?;
        iosys_map_set_vaddr_iomem(&mut (*sysfb).fb_addr, p);
    } else if mem_flags & EFI_MEMORY_UC != 0 {
        let p = map_iomem(ops, mem_start, mem_size, SysfbMapType::Uncached)?;
        iosys_map_set_vaddr_iomem(&mut (*sysfb).fb_addr, p);
    } else if mem_flags & EFI_MEMORY_WT != 0 {
        let p = (ops.map_io)(mem_start, mem_size, SysfbMapType::WriteThrough)?;
        iosys_map_set_vaddr(&mut (*sysfb).fb_addr, p);
    } else if mem_flags & EFI_MEMORY_WB != 0 {
        let p = (ops.map_io)(mem_start, mem_size, SysfbMapType::WriteBack)?;
        iosys_map_set_vaddr(&mut (*sysfb).fb_addr, p);
    } else {
        drm_err(dev, "invalid mem_flags\n");
        return Err(-EINVAL);
    }

    // Modesetting
    let ret = drmm_mode_config_init(dev);
    if ret != 0 {
        return Err(ret);
    }

    let max_width = core::cmp::max(width as u64, DRM_SHADOW_PLANE_MAX_WIDTH) as u32;
    let max_height = core::cmp::max(height as u64, DRM_SHADOW_PLANE_MAX_HEIGHT) as u32;
    let mc = ptr::addr_of_mut!((*dev).mode_config);
    (*mc).min_width = width as u32;
    (*mc).max_width = max_width;
    (*mc).min_height = height as u32;
    (*mc).max_height = max_height;
    (*mc).preferred_depth = (*format).depth as u32;
    (*mc).funcs = ptr::addr_of!(EFIDRM_MODE_CONFIG_FUNCS).cast_mut();

    // Primary plane
    let nformats = drm_sysfb_build_fourcc_list(dev, &[(*format).format], &mut (*efi).formats);
    let primary_plane: *mut DrmPlane = &mut (*efi).primary_plane;
    let ret = drm_universal_plane_init(
        dev,
        primary_plane,
        0,
        ptr::addr_of!(EFIDRM_PRIMARY_PLANE_FUNCS),
        (*efi).formats.as_ptr(),
        nformats as u32,
        DRM_SYSFB_PLANE_FORMAT_MODIFIERS.as_ptr(),
        DRM_PLANE_TYPE_PRIMARY,
        ptr::null(),
    );
    if ret != 0 {
        return Err(ret);
    }
    drm_plane_helper_add(
        primary_plane,
        ptr::addr_of!(EFIDRM_PRIMARY_PLANE_HELPER_FUNCS),
    );
    drm_plane_enable_fb_damage_clips(primary_plane);

    // CRTC
    let crtc: *mut DrmCrtc = &mut (*efi).crtc;
    let ret = drm_crtc_init_with_planes(
        dev,
        crtc,
        primary_plane,
        ptr::null_mut(),
        ptr::addr_of!(EFIDRM_CRTC_FUNCS),
        ptr::null(),
    );
    if ret != 0 {
        return Err(ret);
    }
    drm_crtc_helper_add(crtc, ptr::addr_of!(EFIDRM_CRTC_HELPER_FUNCS));

    // Encoder
    let encoder: *mut DrmEncoder = &mut (*efi).encoder;
    let ret = drm_encoder_init(
        dev,
        encoder,
        ptr::addr_of!(EFIDRM_ENCODER_FUNCS),
        DRM_MODE_ENCODER_NONE as i32,
        ptr::null(),
    );
    if ret != 0 {
        return Err(ret);
    }
    (*encoder).possible_crtcs = drm_crtc_mask(crtc);

    // Connector
    let connector: *mut DrmConnector = &mut (*efi).connector;
    let ret = drm_connector_init(
        dev,
        connector,
        ptr::addr_of!(EFIDRM_CONNECTOR_FUNCS),
        DRM_MODE_CONNECTOR_Unknown as i32,
    );
    if ret != 0 {
        return Err(ret);
    }
    drm_connector_helper_add(connector, ptr::addr_of!(EFIDRM_CONNECTOR_HELPER_FUNCS));
    drm_connector_set_panel_orientation_with_quirk(
        connector,
        DRM_MODE_PANEL_ORIENTATION_UNKNOWN,
        width,
        height,
    );
    if !(*sysfb).edid.is_null() {
        drm_connector_attach_edid_property(connector);
    }
    let ret = drm_connector_attach_encoder(connector, encoder);
    if ret != 0 {
        return Err(ret);
    }

    drm_mode_config_reset(dev);
    Ok(())
}

// upstream: efidrm.c efidrm_device_create()
unsafe fn efidrm_device_create(
    driver: *const DrmDriver,
    si: &ScreenInfo,
    edid: Option<&'static [u8; EDID_LENGTH]>,
) -> Result<*mut EfidrmDevice, i32> {
    // Fail closed: no platform provider means no way to claim the aperture or map it.
    let ops = sysfb_platform_ops().ok_or(-ENODEV)?;
    if screen_info_video_type(si) != VIDEO_TYPE_EFI {
        return Err(-ENODEV);
    }

    let efi = devm_drm_dev_alloc(driver)?;
    let dev: *mut DrmDevice = &mut (*efi).sysfb.dev;
    match efidrm_device_init(efi, ops, si, edid) {
        Ok(()) => Ok(efi),
        Err(e) => {
            // devm would release the device here.
            drm_dev_put(dev);
            Err(e)
        }
    }
}

/// The efidrm probe. See the module header for the provider contract.
///
/// # Safety
/// The installed [`SysfbPlatformOps`] must honour its contract, and the
/// caller must not probe the same firmware framebuffer twice.
pub unsafe fn efidrm_probe_screen_info(
    si: &ScreenInfo,
    edid: Option<&'static [u8; EDID_LENGTH]>,
) -> Result<*mut DrmDevice, i32> {
    let efi = efidrm_device_create(&EFIDRM_DRIVER.0, si, edid)?;
    let dev: *mut DrmDevice = &mut (*efi).sysfb.dev;

    let ret = drm_dev_register(dev, 0);
    if ret != 0 {
        drm_dev_put(dev);
        return Err(ret);
    }

    drm_client_setup(dev, (*efi).sysfb.fb_format);
    Ok(dev)
}

// upstream: efidrm.c efidrm_remove()
/// # Safety
/// `dev` must be a device returned by [`efidrm_probe_screen_info`].
pub unsafe fn efidrm_remove(dev: *mut DrmDevice) {
    drm_dev_unplug(dev);
}

// upstream: efidrm.c efidrm_pm_suspend()
/// # Safety
/// `dev` must be a live DRM device created by [`efidrm_probe_screen_info`].
pub unsafe fn efidrm_pm_suspend(dev: *mut DrmDevice) -> i32 {
    drm_mode_config_helper_suspend(dev)
}

// upstream: efidrm.c efidrm_pm_resume()
/// # Safety
/// `dev` must be a live DRM device created by [`efidrm_probe_screen_info`].
pub unsafe fn efidrm_pm_resume(dev: *mut DrmDevice) -> i32 {
    drm_mode_config_helper_resume(dev)
}
