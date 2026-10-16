// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Host tests for the sysfb / efidrm / format-helper ports: screen_info
//! derivation, format matching, validated sizes, fourcc list construction,
//! the XRGB8888 converters, and the efidrm memory-attribute selection.

use core::{ffi::c_void, mem::zeroed, ptr};

use tk_linux_kapi::config::{EINVAL, ENODEV, ENXIO, GFP_KERNEL};

use crate::{
    drm_format_helper_types::DrmFormatConvState,
    drm_format_helper_upstream::{
        drm_fb_clip_offset, drm_fb_xrgb8888_to_rgb565, drm_fb_xrgb8888_to_rgb888,
        drm_format_conv_state_init, drm_format_conv_state_release, drm_format_conv_state_reserve,
    },
    drm_fourcc_types::DrmFormatInfo,
    drm_fourcc_upstream::drm_format_info,
    drm_framebuffer_types::DrmFramebuffer,
    drm_rect_types::DrmRect,
    drm_rect_upstream::drm_rect_init,
    drm_sysfb_helper_types::{
        PIXEL_FORMAT_RGB565, PIXEL_FORMAT_XRGB8888, PixelFormat, Resource, ScreenInfo,
        VIDEO_CAPABILITY_64BIT_BASE, VIDEO_TYPE_EFI, VIDEO_TYPE_VGAC, pixel_format_equal,
    },
    drm_sysfb_modeset_upstream::{
        drm_sysfb_build_fourcc_list, drm_sysfb_get_blit_func, drm_sysfb_mode,
    },
    drm_sysfb_screen_info_upstream::{
        drm_sysfb_get_stride_si, drm_sysfb_get_visible_size_si, drm_sysfb_get_width_si,
        screen_info_pixel_format, screen_info_resources, screen_info_video_type,
    },
    drm_sysfb_upstream::{drm_sysfb_get_format, drm_sysfb_get_validated_int0},
    efidrm_upstream::{
        EFI_MEMORY_UC, EFI_MEMORY_WB, EFI_MEMORY_WC, EFI_MEMORY_WT, EFIDRM_FORMATS, EfiMemoryDesc,
        SysfbMapType, SysfbPlatformOps, efidrm_get_mem_flags, efidrm_probe_screen_info,
    },
    iosys_map_types::{IosysMap, IosysMapAnon0},
    uapi_drm_consts::{
        DRM_FORMAT_ARGB8888, DRM_FORMAT_RGB332, DRM_FORMAT_RGB565, DRM_FORMAT_RGB888,
        DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888, DRM_MODE_TYPE_DRIVER,
    },
};

const EFI_FB_BASE: u32 = 0xfd00_0000;
const EFI_FB_SIZE: u32 = 0x0080_0000; // 8 MiB

/// A 32-bit XRGB EFI framebuffer record, as the boot loader reports it.
fn efi_screen_info(width: u16, height: u16, linelen: u16) -> ScreenInfo {
    // SAFETY: ScreenInfo is plain integer data; all-zero means "no display".
    let mut si: ScreenInfo = unsafe { zeroed() };
    si.orig_video_isVGA = VIDEO_TYPE_EFI;
    si.lfb_width = width;
    si.lfb_height = height;
    si.lfb_depth = 32;
    si.lfb_base = EFI_FB_BASE;
    si.lfb_size = EFI_FB_SIZE;
    si.lfb_linelength = linelen;
    si.red_size = 8;
    si.red_pos = 16;
    si.green_size = 8;
    si.green_pos = 8;
    si.blue_size = 8;
    si.blue_pos = 0;
    si.rsvd_size = 8;
    si.rsvd_pos = 24;
    si
}

fn xrgb8888_format() -> *mut DrmFormatInfo {
    drm_format_info(DRM_FORMAT_XRGB8888) as *mut DrmFormatInfo
}

#[test]
fn screen_info_efi_derives_xrgb8888_pixel_format() {
    let si = efi_screen_info(640, 480, 2560);
    assert_eq!(screen_info_video_type(&si), VIDEO_TYPE_EFI);
    let mut pixel: PixelFormat = unsafe { zeroed() };
    assert_eq!(screen_info_pixel_format(&si, &mut pixel), 0);
    assert!(pixel_format_equal(&pixel, &PIXEL_FORMAT_XRGB8888));
    assert!(!pixel_format_equal(&pixel, &PIXEL_FORMAT_RGB565));
}

#[test]
fn screen_info_text_mode_is_not_a_framebuffer() {
    let mut si = efi_screen_info(640, 480, 2560);
    si.orig_video_isVGA = 0;
    assert_eq!(screen_info_video_type(&si), 0);
    let mut pixel: PixelFormat = unsafe { zeroed() };
    assert!(screen_info_pixel_format(&si, &mut pixel) < 0);
    // VGA colour text (not a linear framebuffer) has no pixel format either.
    si.orig_video_isVGA = VIDEO_TYPE_VGAC;
    assert!(screen_info_pixel_format(&si, &mut pixel) < 0);
}

#[test]
fn efi_format_table_matches_xrgb8888_only_for_xrgb8888_layout() {
    let si = efi_screen_info(640, 480, 2560);
    let mut pixel: PixelFormat = unsafe { zeroed() };
    assert_eq!(screen_info_pixel_format(&si, &mut pixel), 0);
    let info = drm_sysfb_get_format(ptr::null(), &EFIDRM_FORMATS, &pixel);
    assert!(!info.is_null());
    assert_eq!(unsafe { (*info).format }, DRM_FORMAT_XRGB8888);

    // A 8-bit indexed layout matches no efidrm format and yields NULL.
    let mut indexed = pixel;
    indexed.indexed = true;
    indexed.bits_per_pixel = 8;
    assert!(drm_sysfb_get_format(ptr::null(), &EFIDRM_FORMATS, &indexed).is_null());
}

#[test]
fn screen_info_resources_reports_efi_linear_framebuffer() {
    let si = efi_screen_info(640, 480, 2560);
    let mut res = [Resource {
        start: 0,
        end: 0,
        name: ptr::null(),
        flags: 0,
    }];
    assert_eq!(screen_info_resources(&si, &mut res), 1);
    assert_eq!(res[0].start, EFI_FB_BASE as u64);
    assert_eq!(res[0].end, EFI_FB_BASE as u64 + EFI_FB_SIZE as u64 - 1);
}

#[test]
fn screen_info_resources_honours_64bit_base() {
    let mut si = efi_screen_info(640, 480, 2560);
    si.capabilities = VIDEO_CAPABILITY_64BIT_BASE;
    si.ext_lfb_base = 0x1;
    let mut res = [Resource {
        start: 0,
        end: 0,
        name: ptr::null(),
        flags: 0,
    }];
    assert_eq!(screen_info_resources(&si, &mut res), 1);
    assert_eq!(res[0].start, 0x1_fd00_0000);
}

#[test]
fn validated_int0_rejects_zero_and_out_of_range() {
    assert_eq!(
        drm_sysfb_get_validated_int0(ptr::null(), "width", 0, u16::MAX as u32),
        -EINVAL
    );
    assert_eq!(
        drm_sysfb_get_validated_int0(ptr::null(), "width", 1024, u16::MAX as u32),
        1024
    );
    assert_eq!(
        drm_sysfb_get_validated_int0(ptr::null(), "width", 70_000, u16::MAX as u32),
        -EINVAL
    );
}

#[test]
fn width_and_stride_and_visible_size_validate_pitch() {
    let si = efi_screen_info(640, 480, 2560);
    let fmt = xrgb8888_format();
    assert_eq!(drm_sysfb_get_width_si(ptr::null(), &si), 640);

    let size = EFI_FB_SIZE as u64;
    let stride = drm_sysfb_get_stride_si(ptr::null(), &si, fmt, 640, 480, size);
    assert_eq!(stride, 2560);
    let vsize = drm_sysfb_get_visible_size_si(ptr::null(), &si, 480, stride as u32, size);
    assert_eq!(vsize, 480 * 2560);

    // Visible size larger than the firmware resource is rejected.
    assert_eq!(
        drm_sysfb_get_visible_size_si(ptr::null(), &si, 480, 2560, 1000),
        -(EINVAL as i64)
    );
    // A zero line length falls back to the minimum pitch of the format (640 * 4).
    let si0 = efi_screen_info(640, 480, 0);
    assert_eq!(
        drm_sysfb_get_stride_si(ptr::null(), &si0, fmt, 640, 480, size),
        2560
    );
    // A stride that cannot fit the firmware resource is rejected.
    assert_eq!(
        drm_sysfb_get_stride_si(ptr::null(), &si, fmt, 640, 480, 1000),
        -EINVAL
    );
}

#[test]
fn fixed_mode_derives_physical_size_at_96_dpi() {
    let mode = drm_sysfb_mode(640, 480, 0, 0);
    assert_eq!(mode.hdisplay, 640);
    assert_eq!(mode.vdisplay, 480);
    assert_eq!(mode.clock, 640 * 480 * 60 / 1000);
    assert_eq!(mode.width_mm, 169); // 640 * 254 / 960
    assert_eq!(mode.height_mm, 127); // 480 * 254 / 960
    assert_eq!(mode.r#type as u32, DRM_MODE_TYPE_DRIVER);
}

#[test]
fn fourcc_list_puts_native_first_and_drops_alpha_variants() {
    let mut out = [0u32; 4];
    let n = drm_sysfb_build_fourcc_list(
        ptr::null(),
        &[DRM_FORMAT_ARGB8888, DRM_FORMAT_RGB565],
        &mut out,
    );
    assert_eq!(n, 2);
    assert_eq!(out[0], DRM_FORMAT_XRGB8888);
    assert_eq!(out[1], DRM_FORMAT_RGB565);

    // With room for one entry only the first native format survives.
    let mut small = [0u32; 1];
    let n = drm_sysfb_build_fourcc_list(
        ptr::null(),
        &[DRM_FORMAT_ARGB8888, DRM_FORMAT_RGB565],
        &mut small,
    );
    assert_eq!(n, 1);
    assert_eq!(small[0], DRM_FORMAT_XRGB8888);
}

#[test]
fn blit_table_selects_xrgb8888_converters_only() {
    assert!(drm_sysfb_get_blit_func(DRM_FORMAT_XRGB8888, DRM_FORMAT_XRGB8888).is_some());
    assert!(drm_sysfb_get_blit_func(DRM_FORMAT_RGB565, DRM_FORMAT_XRGB8888).is_some());
    assert!(drm_sysfb_get_blit_func(DRM_FORMAT_RGB332, DRM_FORMAT_XRGB8888).is_some());
    // No converter from a non-XRGB8888 source to a different format.
    assert!(drm_sysfb_get_blit_func(DRM_FORMAT_RGB565, DRM_FORMAT_XBGR8888).is_none());
}

#[test]
fn clip_offset_is_row_times_pitch_plus_column_times_cpp() {
    let clip = {
        let mut r: DrmRect = unsafe { zeroed() };
        drm_rect_init(&mut r, 2, 3, 4, 1);
        r
    };
    let off = unsafe { drm_fb_clip_offset(100, xrgb8888_format(), &clip) };
    assert_eq!(off, 3 * 100 + 2 * 4);
}

/// Builds a single-plane framebuffer descriptor over `pitch`-strided pixels.
fn make_fb(fourcc_info: *mut DrmFormatInfo, pitch: u32) -> DrmFramebuffer {
    let mut fb: DrmFramebuffer = unsafe { zeroed() };
    fb.format = fourcc_info;
    fb.pitches[0] = pitch;
    fb
}

fn iomap(p: *mut u8) -> IosysMap {
    IosysMap {
        _anon_0: IosysMapAnon0 {
            vaddr: p.cast::<c_void>(),
        },
        is_iomem: false,
    }
}

#[test]
fn xrgb8888_to_rgb565_converts_primaries_and_white() {
    // Four XRGB8888 pixels: red, green, blue, white (little-endian u32 each).
    let src_px: [u32; 4] = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0x00ff_ffff];
    let mut src_bytes = [0u8; 16];
    for (i, px) in src_px.iter().enumerate() {
        src_bytes[i * 4..i * 4 + 4].copy_from_slice(&px.to_le_bytes());
    }

    let mut dst_bytes = [0u8; 8];
    let fb = make_fb(xrgb8888_format(), 16);
    let src_map = iomap(src_bytes.as_mut_ptr());
    let mut dst = iomap(dst_bytes.as_mut_ptr());
    let mut clip: DrmRect = unsafe { zeroed() };
    drm_rect_init(&mut clip, 0, 0, 4, 1);
    let mut state: DrmFormatConvState = unsafe { zeroed() };
    unsafe {
        drm_format_conv_state_init(&mut state);
        drm_fb_xrgb8888_to_rgb565(&mut dst, ptr::null(), &src_map, &fb, &clip, &mut state);
        drm_format_conv_state_release(&mut state);
    }

    let got: [u16; 4] = [
        u16::from_le_bytes([dst_bytes[0], dst_bytes[1]]),
        u16::from_le_bytes([dst_bytes[2], dst_bytes[3]]),
        u16::from_le_bytes([dst_bytes[4], dst_bytes[5]]),
        u16::from_le_bytes([dst_bytes[6], dst_bytes[7]]),
    ];
    assert_eq!(got, [0xf800, 0x07e0, 0x001f, 0xffff]);
}

#[test]
fn xrgb8888_to_rgb888_packs_three_bytes_per_pixel() {
    let src_px: [u32; 2] = [0x0011_2233, 0x0044_5566];
    let mut src_bytes = [0u8; 8];
    for (i, px) in src_px.iter().enumerate() {
        src_bytes[i * 4..i * 4 + 4].copy_from_slice(&px.to_le_bytes());
    }
    let mut dst_bytes = [0u8; 6];
    let fb = make_fb(xrgb8888_format(), 8);
    let src_map = iomap(src_bytes.as_mut_ptr());
    let mut dst = iomap(dst_bytes.as_mut_ptr());
    let mut clip: DrmRect = unsafe { zeroed() };
    drm_rect_init(&mut clip, 0, 0, 2, 1);
    let mut state: DrmFormatConvState = unsafe { zeroed() };
    unsafe {
        drm_format_conv_state_init(&mut state);
        drm_fb_xrgb8888_to_rgb888(&mut dst, ptr::null(), &src_map, &fb, &clip, &mut state);
        drm_format_conv_state_release(&mut state);
    }
    // Pixel values are stored little-endian: low byte (blue) first.
    assert_eq!(dst_bytes, [0x33, 0x22, 0x11, 0x66, 0x55, 0x44]);
}

#[test]
fn conv_state_reserve_grows_and_reuses_storage() {
    let mut state: DrmFormatConvState = unsafe { zeroed() };
    unsafe {
        drm_format_conv_state_init(&mut state);
        let a = drm_format_conv_state_reserve(&mut state, 64, GFP_KERNEL);
        assert!(!a.is_null());
        assert_eq!(state.tmp.size, 64);
        // A smaller request reuses the existing block.
        let b = drm_format_conv_state_reserve(&mut state, 16, GFP_KERNEL);
        assert_eq!(a, b);
        drm_format_conv_state_release(&mut state);
        assert!(state.tmp.mem.is_null());
        assert_eq!(state.tmp.size, 0);
    }
}

// Mock platform provider, used only through the explicit ops argument (never installed).
unsafe fn mock_acquire(_base: u64, _size: u64) -> i32 {
    0
}
unsafe fn mock_request(_base: u64, _size: u64, _name: &str) -> bool {
    true
}
unsafe fn mock_map(_base: u64, _size: u64, _kind: SysfbMapType) -> Result<*mut c_void, i32> {
    Err(-ENXIO)
}
fn mock_efi_enabled() -> bool {
    true
}
fn mock_efi_lookup(phys: u64) -> Option<EfiMemoryDesc> {
    if phys == EFI_FB_BASE as u64 {
        Some(EfiMemoryDesc {
            phys_addr: EFI_FB_BASE as u64,
            num_pages: (EFI_FB_SIZE as u64) >> 12,
            attribute: EFI_MEMORY_WC | EFI_MEMORY_WB,
        })
    } else {
        None
    }
}
static MOCK_OPS_WC_WB: SysfbPlatformOps = SysfbPlatformOps {
    acquire_aperture: mock_acquire,
    request_mem_region: mock_request,
    map_io: mock_map,
    efi_memmap_enabled: mock_efi_enabled,
    efi_mem_desc_lookup: mock_efi_lookup,
};
static MOCK_OPS_NO_MEMMAP: SysfbPlatformOps = SysfbPlatformOps {
    acquire_aperture: mock_acquire,
    request_mem_region: mock_request,
    map_io: mock_map,
    efi_memmap_enabled: || false,
    efi_mem_desc_lookup: mock_efi_lookup,
};

#[test]
fn mem_flags_follow_efi_descriptor_attributes() {
    // Range fully covered by a WC|WB descriptor: the descriptor's attributes win.
    let flags = efidrm_get_mem_flags(&MOCK_OPS_WC_WB, EFI_FB_BASE as u64, EFI_FB_SIZE as u64);
    assert_eq!(flags, EFI_MEMORY_WC | EFI_MEMORY_WB);
    assert_ne!(flags & EFI_MEMORY_WC, 0);
    assert_eq!(flags & EFI_MEMORY_UC, 0);
    assert_eq!(flags & EFI_MEMORY_WT, 0);

    // Range that runs past the descriptor falls back to WC|UC.
    let flags = efidrm_get_mem_flags(
        &MOCK_OPS_WC_WB,
        EFI_FB_BASE as u64,
        EFI_FB_SIZE as u64 + 4096,
    );
    assert_eq!(flags, EFI_MEMORY_WC | EFI_MEMORY_UC);

    // No EFI memory map: WC|UC fallback.
    let flags = efidrm_get_mem_flags(&MOCK_OPS_NO_MEMMAP, EFI_FB_BASE as u64, EFI_FB_SIZE as u64);
    assert_eq!(flags, EFI_MEMORY_WC | EFI_MEMORY_UC);
}

#[test]
fn probe_without_installed_provider_fails_closed() {
    // No test installs a provider, so the probe must refuse before touching hardware.
    let si = efi_screen_info(640, 480, 2560);
    let res = unsafe { efidrm_probe_screen_info(&si, None) };
    assert_eq!(res.err(), Some(-ENODEV));
}
