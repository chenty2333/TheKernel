//! Checked projection from the kernel's committed DRM KMS snapshot into the
//! source i915 pipe/plane transition consumed by the Native pipe callbacks.
//!
//! This is intentionally a value-only adapter: it performs no MMIO, never
//! derives a GGTT address, and admits only the existing single-Crtc / single
//! primary-plane Pipe-A RGB-linear contract. The caller resolves the exact
//! framebuffer-bound scanout and supplies it together with its DRM FB id.

use intel_display::intel_display_modeset_full::{
    OutputFormat, PipeState, PipeTransition, PlaneTransition, Timing,
};

use super::{
    native_modeset_ops::NativePlaneState,
    pipe::{self, PlaneScanout},
};
use crate::drm::{
    atomic::{self, State},
    modes::Mode,
    property,
};

const PIPE_A: u8 = 0;
const PRIMARY_PLANE_INDEX: usize = 0;
const DRM_FORMAT_XRGB8888: u32 = property::FORMAT_XRGB8888;
const DRM_FORMAT_RGB565: u32 = 0x3631_4752;

/// The KMS owner's exact resolved view of one primary framebuffer.
///
/// `scanout` is present only when the owner has a live, pinned and translated
/// GGTT binding. A non-visible old state still supplies its framebuffer id,
/// mode, format and stride, but does not need a scanout address to disable the
/// old plane. A visible new state always requires `scanout`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ResolvedPrimary {
    pub(super) framebuffer_id: u32,
    pub(super) mode: Mode,
    /// Native plane-format FourCC (currently XRGB8888 or RGB565).
    pub(super) pixel_format: u32,
    /// DRM modifier; only linear is supported by this projection.
    pub(super) modifier: u64,
    /// Framebuffer layout metadata from the exact KMS framebuffer object.
    pub(super) framebuffer_width: u32,
    pub(super) framebuffer_height: u32,
    /// This writer programs PLANE_OFFSET=0, so nonzero framebuffer byte
    /// offsets are deliberately refused instead of shifting the GGTT base.
    pub(super) framebuffer_offset: u64,
    pub(super) stride_bytes: u32,
    /// Exact allocation and GGTT information from the framebuffer owner.
    pub(super) scanout: Option<PlaneScanout>,
}

/// Source pipe and primary-plane values for one actual KMS old/new pair.
///
/// The resolved full modes are retained beside the source transition because
/// source `Timing` does not represent sync polarity, mode flags, or provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NativeAtomicKmsProjection {
    pub(super) transition: PipeTransition,
    pub(super) planes: [NativePlaneState; 1],
    pub(super) old_mode: Option<Mode>,
    pub(super) new_mode: Option<Mode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativeKmsProjectionError {
    InvalidCrtc,
    UnsupportedRoute,
    UnsupportedCursor,
    UnsupportedColor,
    UnsupportedDamage,
    UnsupportedDpms,
    MissingResolvedPrimary { new_state: bool },
    UnexpectedResolvedPrimary { new_state: bool },
    FramebufferMismatch { expected: u32, resolved: u32 },
    ModeMismatch,
    UnsupportedMode,
    UnsupportedFormat(u32),
    UnsupportedModifier(u64),
    InvalidStride(u32),
    InvalidPlaneGeometry,
    MissingScanout,
    ScanoutMismatch,
    InvalidSurface,
}

/// Project the real DRM atomic old/new snapshots and exact framebuffer
/// resolutions into a one-pipe source transition. The primary engine index is
/// deliberately fixed to PLANE_1 (index zero); DRM object ids are never used
/// as register indices.
pub(super) fn project_atomic_kms(
    crtc_id: u32,
    old: State,
    new: State,
    old_primary: Option<ResolvedPrimary>,
    new_primary: Option<ResolvedPrimary>,
) -> Result<NativeAtomicKmsProjection, NativeKmsProjectionError> {
    if crtc_id == 0 {
        return Err(NativeKmsProjectionError::InvalidCrtc);
    }
    validate_state_capabilities(crtc_id, old)?;
    validate_state_capabilities(crtc_id, new)?;

    let old_visible = old.active;
    let new_visible = new.active;
    let old_mode = resolve_primary(crtc_id, old, old_primary, false)?;
    let new_mode = resolve_primary(crtc_id, new, new_primary, true)?;

    let old_timing = old_mode.map(mode_timing).unwrap_or_default();
    let new_timing = new_mode.map(mode_timing).unwrap_or_default();
    let old_pipe = pipe_state(old_visible, old_timing, old_visible as u32);
    let mut new_pipe = pipe_state(new_visible, new_timing, new_visible as u32);
    new_pipe.old_timing = old_timing;
    new_pipe.old_active_planes = old_visible as u32;
    new_pipe.needs_modeset = old_visible != new_visible || old_mode != new_mode;
    new_pipe.mode_changed = old_mode != new_mode;

    let old_plane = old_primary.map(|resolved| resolved_plane_state(old, resolved));
    let new_plane = new_primary.map(|resolved| resolved_plane_state(new, resolved));
    let plane_transition = PlaneTransition {
        pipe: PIPE_A,
        id: 0,
        uapi_plane_mask_bit: 1,
        old_visible,
        new_visible,
        old_fb_exists: old.fb != 0,
        new_fb_exists: new.fb != 0,
        old_format: old_plane
            .map(|plane| plane.transition.old_format)
            .unwrap_or(0),
        new_format: new_plane
            .map(|plane| plane.transition.new_format)
            .unwrap_or(0),
        old_modifier: old_plane
            .map(|plane| plane.transition.old_modifier)
            .unwrap_or(0),
        new_modifier: new_plane
            .map(|plane| plane.transition.new_modifier)
            .unwrap_or(0),
        old_mapping_stride: old_plane
            .map(|plane| plane.transition.old_mapping_stride)
            .unwrap_or(0),
        new_mapping_stride: new_plane
            .map(|plane| plane.transition.new_mapping_stride)
            .unwrap_or(0),
        old_rotation: 0,
        new_rotation: 0,
        old_src_rect: old_plane
            .map(|plane| plane.transition.old_src_rect)
            .unwrap_or([0; 4]),
        new_src_rect: new_plane
            .map(|plane| plane.transition.new_src_rect)
            .unwrap_or([0; 4]),
        old_dst_rect: old_plane
            .map(|plane| plane.transition.old_dst_rect)
            .unwrap_or([0; 4]),
        new_dst_rect: new_plane
            .map(|plane| plane.transition.new_dst_rect)
            .unwrap_or([0; 4]),
        old_alpha: u16::MAX,
        new_alpha: u16::MAX,
        ..PlaneTransition::default()
    };

    let plane = NativePlaneState {
        transition: plane_transition,
        engine_index: PRIMARY_PLANE_INDEX,
        scanout: new_primary.and_then(|primary| primary.scanout),
    };

    Ok(NativeAtomicKmsProjection {
        transition: PipeTransition {
            old: old_pipe,
            new: new_pipe,
        },
        planes: [plane],
        old_mode,
        new_mode,
    })
}

fn validate_state_capabilities(crtc_id: u32, state: State) -> Result<(), NativeKmsProjectionError> {
    if state.connector_crtc != 0 && state.connector_crtc != crtc_id
        || state.plane_crtc != 0 && state.plane_crtc != crtc_id
    {
        return Err(NativeKmsProjectionError::UnsupportedRoute);
    }
    if state.cursor_fb != 0 || state.cursor_crtc != 0 {
        return Err(NativeKmsProjectionError::UnsupportedCursor);
    }
    if state.gamma_lut_blob != 0 || state.degamma_lut_blob != 0 || state.ctm_blob != 0 {
        return Err(NativeKmsProjectionError::UnsupportedColor);
    }
    if state.damage_clips_blob != 0 {
        return Err(NativeKmsProjectionError::UnsupportedDamage);
    }
    if state.active && state.dpms != atomic::DPMS_ON {
        return Err(NativeKmsProjectionError::UnsupportedDpms);
    }
    if state.active {
        if state.connector_crtc != crtc_id || state.plane_crtc != crtc_id || state.fb == 0 {
            return Err(NativeKmsProjectionError::UnsupportedRoute);
        }
    } else if state.fb != 0 {
        return Err(NativeKmsProjectionError::UnsupportedRoute);
    }
    Ok(())
}

fn resolve_primary(
    crtc_id: u32,
    state: State,
    primary: Option<ResolvedPrimary>,
    new_state: bool,
) -> Result<Option<Mode>, NativeKmsProjectionError> {
    if !state.active {
        if primary.is_some() {
            return Err(NativeKmsProjectionError::UnexpectedResolvedPrimary { new_state });
        }
        return Ok(None);
    }
    let primary = primary.ok_or(NativeKmsProjectionError::MissingResolvedPrimary { new_state })?;
    if primary.framebuffer_id == 0 || primary.framebuffer_id != state.fb {
        return Err(NativeKmsProjectionError::FramebufferMismatch {
            expected: state.fb,
            resolved: primary.framebuffer_id,
        });
    }
    if state.connector_crtc != crtc_id || state.plane_crtc != crtc_id {
        return Err(NativeKmsProjectionError::UnsupportedRoute);
    }
    let kms_mode = state.mode.ok_or(NativeKmsProjectionError::ModeMismatch)?;
    if kms_mode.width != u32::from(primary.mode.hdisplay)
        || kms_mode.height != u32::from(primary.mode.vdisplay)
        || kms_mode.refresh_millihz != primary.mode.refresh_millihz()
    {
        return Err(NativeKmsProjectionError::ModeMismatch);
    }
    validate_full_mode(primary.mode)?;
    validate_primary(state, primary, new_state)?;
    Ok(Some(primary.mode))
}

fn validate_full_mode(mode: Mode) -> Result<(), NativeKmsProjectionError> {
    if mode.flags.is_empty()
        && mode.clock_khz != 0
        && mode.hdisplay != 0
        && mode.vdisplay != 0
        && mode.hdisplay < mode.hsync_start
        && mode.hsync_start < mode.hsync_end
        && mode.hsync_end <= mode.htotal
        && mode.vdisplay < mode.vsync_start
        && mode.vsync_start < mode.vsync_end
        && mode.vsync_end <= mode.vtotal
    {
        Ok(())
    } else {
        // This writer has no interlace/double-clock mode programming. Reject
        // both flags now, before a source program or any register write exists.
        Err(NativeKmsProjectionError::UnsupportedMode)
    }
}

fn validate_primary(
    state: State,
    primary: ResolvedPrimary,
    new_state: bool,
) -> Result<(), NativeKmsProjectionError> {
    if primary.modifier != 0 {
        return Err(NativeKmsProjectionError::UnsupportedModifier(
            primary.modifier,
        ));
    }
    let cpp: u64 = match primary.pixel_format {
        DRM_FORMAT_XRGB8888 => 4,
        DRM_FORMAT_RGB565 => 2,
        other => return Err(NativeKmsProjectionError::UnsupportedFormat(other)),
    };
    if primary.framebuffer_width != u32::from(primary.mode.hdisplay)
        || primary.framebuffer_height != u32::from(primary.mode.vdisplay)
        || primary.framebuffer_offset != 0
    {
        return Err(NativeKmsProjectionError::InvalidPlaneGeometry);
    }
    if primary.stride_bytes == 0
        || !primary
            .stride_bytes
            .is_multiple_of(pipe::PLANE_STRIDE_UNIT_BYTES)
        || primary.stride_bytes / pipe::PLANE_STRIDE_UNIT_BYTES > pipe::PLANE_STRIDE_MAX
        || u64::from(primary.stride_bytes) < u64::from(primary.mode.hdisplay) * cpp
    {
        return Err(NativeKmsProjectionError::InvalidStride(
            primary.stride_bytes,
        ));
    }
    validate_plane_rect(state, primary.mode)?;
    match primary.scanout {
        Some(scanout) => validate_scanout(primary, scanout),
        None if new_state && state.active => Err(NativeKmsProjectionError::MissingScanout),
        None => Ok(()),
    }
}

fn validate_plane_rect(state: State, mode: Mode) -> Result<(), NativeKmsProjectionError> {
    let src_width = u32::from(mode.hdisplay)
        .checked_shl(16)
        .ok_or(NativeKmsProjectionError::InvalidPlaneGeometry)?;
    let src_height = u32::from(mode.vdisplay)
        .checked_shl(16)
        .ok_or(NativeKmsProjectionError::InvalidPlaneGeometry)?;
    if state.src_x != 0
        || state.src_y != 0
        || state.src_w != src_width
        || state.src_h != src_height
        || state.crtc_x != 0
        || state.crtc_y != 0
        || state.crtc_w != u32::from(mode.hdisplay)
        || state.crtc_h != u32::from(mode.vdisplay)
        || i32::try_from(state.src_w).is_err()
        || i32::try_from(state.src_h).is_err()
    {
        return Err(NativeKmsProjectionError::InvalidPlaneGeometry);
    }
    Ok(())
}

fn validate_scanout(
    primary: ResolvedPrimary,
    scanout: PlaneScanout,
) -> Result<(), NativeKmsProjectionError> {
    if scanout.plane_index != PRIMARY_PLANE_INDEX
        || scanout.pixel_format != primary.pixel_format
        || scanout.surface.stride_bytes != primary.stride_bytes
        || scanout.source_width != u32::from(primary.mode.hdisplay)
        || scanout.source_height != u32::from(primary.mode.vdisplay)
        || scanout.dst_x != 0
        || scanout.dst_y != 0
        || scanout.dst_width != u32::from(primary.mode.hdisplay)
        || scanout.dst_height != u32::from(primary.mode.vdisplay)
        || scanout.allocation_bytes == 0
    {
        return Err(NativeKmsProjectionError::ScanoutMismatch);
    }
    pipe::PlaneProgram::surface_field(scanout.surface.ggtt_address)
        .map_err(|_| NativeKmsProjectionError::InvalidSurface)?;
    let cpp = if primary.pixel_format == DRM_FORMAT_XRGB8888 {
        4u64
    } else {
        2u64
    };
    let row_bytes = u64::from(scanout.source_width)
        .checked_mul(cpp)
        .ok_or(NativeKmsProjectionError::InvalidPlaneGeometry)?;
    let required_bytes = u64::from(scanout.surface.stride_bytes)
        .checked_mul(u64::from(scanout.source_height))
        .ok_or(NativeKmsProjectionError::InvalidPlaneGeometry)?;
    if u64::from(scanout.surface.stride_bytes) < row_bytes
        || required_bytes > scanout.allocation_bytes
    {
        return Err(NativeKmsProjectionError::ScanoutMismatch);
    }
    Ok(())
}

fn resolved_plane_state(state: State, primary: ResolvedPrimary) -> NativePlaneState {
    let rect = plane_rect(state);
    let transition = PlaneTransition {
        pipe: PIPE_A,
        id: 0,
        uapi_plane_mask_bit: 1,
        old_visible: false,
        new_visible: false,
        old_fb_exists: false,
        new_fb_exists: false,
        old_format: primary.pixel_format,
        new_format: primary.pixel_format,
        old_modifier: primary.modifier,
        new_modifier: primary.modifier,
        old_mapping_stride: primary.stride_bytes,
        new_mapping_stride: primary.stride_bytes,
        old_rotation: 0,
        new_rotation: 0,
        old_src_rect: rect,
        new_src_rect: rect,
        old_dst_rect: destination_rect(state),
        new_dst_rect: destination_rect(state),
        old_alpha: u16::MAX,
        new_alpha: u16::MAX,
        ..PlaneTransition::default()
    };
    NativePlaneState {
        transition,
        engine_index: PRIMARY_PLANE_INDEX,
        scanout: primary.scanout,
    }
}

fn plane_rect(state: State) -> [i32; 4] {
    [
        state.src_x as i32,
        state.src_y as i32,
        state.src_x.saturating_add(state.src_w) as i32,
        state.src_y.saturating_add(state.src_h) as i32,
    ]
}

fn destination_rect(state: State) -> [i32; 4] {
    [
        state.crtc_x as i32,
        state.crtc_y as i32,
        state.crtc_x.saturating_add(state.crtc_w) as i32,
        state.crtc_y.saturating_add(state.crtc_h) as i32,
    ]
}

fn mode_timing(mode: Mode) -> Timing {
    Timing {
        hdisplay: u32::from(mode.hdisplay),
        hsync_start: u32::from(mode.hsync_start),
        hsync_end: u32::from(mode.hsync_end),
        htotal: u32::from(mode.htotal),
        hblank_start: u32::from(mode.hdisplay),
        hblank_end: u32::from(mode.htotal),
        vdisplay: u32::from(mode.vdisplay),
        vblank_start: u32::from(mode.vdisplay),
        vblank_end: u32::from(mode.vtotal),
        vsync_start: u32::from(mode.vsync_start),
        vsync_end: u32::from(mode.vsync_end),
        vtotal: u32::from(mode.vtotal),
        clock_khz: mode.clock_khz,
    }
}

fn pipe_state(active: bool, timing: Timing, planes: u32) -> PipeState {
    PipeState {
        pipe: PIPE_A,
        hw_enable: active,
        hw_active: active,
        uapi_enable: active,
        uapi_active: active,
        cpu_transcoder: PIPE_A,
        master_transcoder: u8::MAX,
        mst_master_transcoder: u8::MAX,
        pipe_active: active,
        pipe_bpp: 24,
        max_pipe_bpp: 24,
        output_format: OutputFormat::Rgb,
        sink_format: OutputFormat::Rgb,
        uapi_plane_mask: planes,
        enabled_planes: planes,
        active_planes: planes,
        old_active_planes: planes,
        old_timing: timing,
        new_timing: timing,
        pipe_mode: timing,
        uapi_mode: timing,
        hw_mode: timing,
        pipe_src_width: timing.hdisplay,
        pipe_src_height: timing.vdisplay,
        ..PipeState::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::{
        intel::{
            native_modeset_ops::{NativePipeDisableProgram, NativePipeProgram},
            pipe::WatermarkConfig,
        },
        modes::{CTA_VIC_TIMINGS, ModeFlags},
    };

    fn mode() -> Mode {
        CTA_VIC_TIMINGS
            .iter()
            .find(|entry| entry.vic == 16)
            .expect("CTA 1080p mode")
            .mode
    }

    fn kms_mode(mode: Mode) -> crate::drm::kms::Mode {
        crate::drm::kms::Mode {
            width: u32::from(mode.hdisplay),
            height: u32::from(mode.vdisplay),
            refresh_millihz: mode.refresh_millihz(),
        }
    }

    fn scanout(mode: Mode, address: u64) -> PlaneScanout {
        PlaneScanout {
            plane_index: 0,
            surface: pipe::PlaneSurface {
                ggtt_address: address,
                stride_bytes: 1920 * 4,
            },
            allocation_bytes: 1920 * 1080 * 4,
            pixel_format: DRM_FORMAT_XRGB8888,
            source_width: u32::from(mode.hdisplay),
            source_height: u32::from(mode.vdisplay),
            dst_x: 0,
            dst_y: 0,
            dst_width: u32::from(mode.hdisplay),
            dst_height: u32::from(mode.vdisplay),
        }
    }

    fn descriptor(mode: Mode, fb: u32, address: Option<u64>) -> ResolvedPrimary {
        ResolvedPrimary {
            framebuffer_id: fb,
            mode,
            pixel_format: DRM_FORMAT_XRGB8888,
            modifier: 0,
            framebuffer_width: u32::from(mode.hdisplay),
            framebuffer_height: u32::from(mode.vdisplay),
            framebuffer_offset: 0,
            stride_bytes: 1920 * 4,
            scanout: address.map(|address| scanout(mode, address)),
        }
    }

    fn visible_state(crtc: u32, fb: u32, mode: Mode) -> State {
        State {
            connector_crtc: crtc,
            plane_crtc: crtc,
            active: true,
            mode: Some(kms_mode(mode)),
            fb,
            src_w: u32::from(mode.hdisplay) << 16,
            src_h: u32::from(mode.vdisplay) << 16,
            crtc_w: u32::from(mode.hdisplay),
            crtc_h: u32::from(mode.vdisplay),
            ..State::default()
        }
    }

    fn wm() -> WatermarkConfig {
        WatermarkConfig {
            display_ver: 13,
            latencies: [2, 4, 6, 8, 14, 16, 0, 0],
            num_levels: 6,
            sagv_block_time_us: 0,
        }
    }

    #[test]
    fn drm_enable_projects_pipe_a_and_keeps_the_exact_full_mode() {
        let mode = mode();
        let old = State::default();
        let new = visible_state(41, 7, mode);
        let projected = project_atomic_kms(
            41,
            old,
            new,
            None,
            Some(descriptor(mode, 7, Some(0x0100_0000))),
        )
        .expect("complete state projects");

        assert_eq!(projected.transition.old.pipe, PIPE_A);
        assert!(!projected.transition.old.hw_active);
        assert!(projected.transition.new.hw_active);
        assert_eq!(projected.transition.new.old_timing, Timing::default());
        assert_eq!(projected.transition.new.new_timing, mode_timing(mode));
        assert_eq!(projected.transition.old.uapi_plane_mask, 0);
        assert_eq!(projected.transition.old.enabled_planes, 0);
        assert_eq!(projected.transition.old.active_planes, 0);
        assert_eq!(projected.transition.new.uapi_plane_mask, 1);
        assert_eq!(projected.transition.new.enabled_planes, 1);
        assert_eq!(projected.transition.new.active_planes, 1);
        assert!(projected.planes[0].transition.new_visible);
        assert_eq!(projected.planes[0].engine_index, 0);
        assert_eq!(projected.planes[0].transition.uapi_plane_mask_bit, 1);
        assert_eq!(
            projected.planes[0].transition.new_format,
            DRM_FORMAT_XRGB8888
        );
        assert_eq!(
            projected.planes[0].scanout,
            Some(scanout(mode, 0x0100_0000))
        );
        assert_eq!(projected.new_mode, Some(mode));

        let program = NativePipeProgram::from_source_with_mode(
            &projected.transition,
            &projected.planes,
            wm(),
            mode,
        )
        .expect("source program accepts exact resolved mode");
        assert_eq!(program.mode(), &mode);
        assert!(mode.hsync_positive);
        assert!(mode.vsync_positive);
    }

    #[test]
    fn active_to_disabled_projection_is_suitable_for_source_crtc_disable() {
        let mode = mode();
        let old = visible_state(41, 7, mode);
        let new = State {
            connector_crtc: 0,
            plane_crtc: 0,
            active: false,
            fb: 0,
            mode: None,
            ..State::default()
        };
        let projected = project_atomic_kms(41, old, new, Some(descriptor(mode, 7, None)), None)
            .expect("old FB identity and timing suffice to disable without GGTT");
        assert!(projected.transition.old.hw_active);
        assert!(!projected.transition.new.hw_active);
        assert_eq!(projected.transition.old.active_planes, 1);
        assert_eq!(projected.transition.old.uapi_plane_mask, 1);
        assert_eq!(projected.transition.old.enabled_planes, 1);
        assert!(projected.planes[0].transition.old_visible);
        assert!(!projected.planes[0].transition.new_visible);
        assert_eq!(projected.transition.new.uapi_plane_mask, 0);
        assert_eq!(projected.transition.new.enabled_planes, 0);
        assert_eq!(projected.transition.new.active_planes, 0);
        assert!(projected.planes[0].scanout.is_none());
        assert_eq!(projected.old_mode, Some(mode));
        NativePipeDisableProgram::from_source(&projected.transition, &projected.planes)
            .expect("source disable accepts complete old state without inventing an address");
    }

    #[test]
    fn source_program_requires_exact_uapi_and_hardware_plane_masks() {
        let mode = mode();
        let projected = project_atomic_kms(
            41,
            State::default(),
            visible_state(41, 7, mode),
            None,
            Some(descriptor(mode, 7, Some(0x0100_0000))),
        )
        .expect("atomic state projects");
        let mut bad_pipe_mask = projected.transition;
        bad_pipe_mask.new.uapi_plane_mask = 3;
        assert!(matches!(
            NativePipeProgram::from_source_with_mode(&bad_pipe_mask, &projected.planes, wm(), mode,),
            Err(
                super::super::native_modeset_ops::NativePipeProjectionError::PlaneMaskMismatch { .. }
            )
        ));

        let mut bad_plane_mask = projected.planes;
        bad_plane_mask[0].transition.uapi_plane_mask_bit = 2;
        assert!(matches!(
            NativePipeProgram::from_source_with_mode(
                &projected.transition,
                &bad_plane_mask,
                wm(),
                mode,
            ),
            Err(
                super::super::native_modeset_ops::NativePipeProjectionError::PlaneMaskMismatch { .. }
            )
        ));
    }

    #[test]
    fn incomplete_or_mismatched_framebuffer_resolution_fails_closed() {
        let mode = mode();
        let old = State::default();
        let new = visible_state(41, 7, mode);
        assert_eq!(
            project_atomic_kms(41, old, new, None, None).unwrap_err(),
            NativeKmsProjectionError::MissingResolvedPrimary { new_state: true }
        );
        assert!(matches!(
            project_atomic_kms(
                41,
                old,
                new,
                None,
                Some(descriptor(mode, 8, Some(0x0100_0000)))
            ),
            Err(NativeKmsProjectionError::FramebufferMismatch { .. })
        ));
        let live_old = visible_state(41, 6, mode);
        assert_eq!(
            project_atomic_kms(
                41,
                live_old,
                new,
                None,
                Some(descriptor(mode, 7, Some(0x0100_0000)))
            )
            .unwrap_err(),
            NativeKmsProjectionError::MissingResolvedPrimary { new_state: false }
        );
    }

    #[test]
    fn cursor_color_non_pipe_a_and_damage_state_are_rejected() {
        let mode = mode();
        let mut state = visible_state(41, 7, mode);
        state.cursor_fb = 9;
        assert_eq!(
            project_atomic_kms(
                41,
                State::default(),
                state,
                None,
                Some(descriptor(mode, 7, Some(0x0100_0000)))
            )
            .unwrap_err(),
            NativeKmsProjectionError::UnsupportedCursor
        );
        state.cursor_fb = 0;
        state.gamma_lut_blob = 5;
        assert_eq!(
            project_atomic_kms(
                41,
                State::default(),
                state,
                None,
                Some(descriptor(mode, 7, Some(0x0100_0000)))
            )
            .unwrap_err(),
            NativeKmsProjectionError::UnsupportedColor
        );
        state.gamma_lut_blob = 0;
        state.damage_clips_blob = 3;
        assert_eq!(
            project_atomic_kms(
                41,
                State::default(),
                state,
                None,
                Some(descriptor(mode, 7, Some(0x0100_0000)))
            )
            .unwrap_err(),
            NativeKmsProjectionError::UnsupportedDamage
        );
        state.damage_clips_blob = 0;
        state.connector_crtc = 42;
        assert_eq!(
            project_atomic_kms(
                41,
                State::default(),
                state,
                None,
                Some(descriptor(mode, 7, Some(0x0100_0000)))
            )
            .unwrap_err(),
            NativeKmsProjectionError::UnsupportedRoute
        );
    }

    #[test]
    fn malformed_rect_stride_format_modifier_and_surface_are_rejected() {
        let mode = mode();
        let old = State::default();
        let new = visible_state(41, 7, mode);
        let mut bad_rect = new;
        bad_rect.src_x = 1 << 16;
        assert_eq!(
            project_atomic_kms(
                41,
                old,
                bad_rect,
                None,
                Some(descriptor(mode, 7, Some(0x0100_0000)))
            )
            .unwrap_err(),
            NativeKmsProjectionError::InvalidPlaneGeometry
        );
        let mut bad_stride = descriptor(mode, 7, Some(0x0100_0000));
        bad_stride.stride_bytes = 1920 * 4 + 1;
        assert!(matches!(
            project_atomic_kms(41, old, new, None, Some(bad_stride)),
            Err(NativeKmsProjectionError::InvalidStride(_))
        ));
        let mut bad_format = descriptor(mode, 7, Some(0x0100_0000));
        bad_format.pixel_format = property::FORMAT_ARGB8888;
        assert_eq!(
            project_atomic_kms(41, old, new, None, Some(bad_format)).unwrap_err(),
            NativeKmsProjectionError::UnsupportedFormat(property::FORMAT_ARGB8888)
        );
        let mut bad_modifier = descriptor(mode, 7, Some(0x0100_0000));
        bad_modifier.modifier = 0x100;
        assert_eq!(
            project_atomic_kms(41, old, new, None, Some(bad_modifier)).unwrap_err(),
            NativeKmsProjectionError::UnsupportedModifier(0x100)
        );
        let mut bad_surface = descriptor(mode, 7, Some(0x0100_0000));
        bad_surface.scanout.as_mut().unwrap().surface.ggtt_address = 0;
        assert_eq!(
            project_atomic_kms(41, old, new, None, Some(bad_surface)).unwrap_err(),
            NativeKmsProjectionError::InvalidSurface
        );

        let mut bad_fb_width = descriptor(mode, 7, Some(0x0100_0000));
        bad_fb_width.framebuffer_width += 1;
        assert_eq!(
            project_atomic_kms(41, old, new, None, Some(bad_fb_width)).unwrap_err(),
            NativeKmsProjectionError::InvalidPlaneGeometry
        );
        let mut bad_fb_offset = descriptor(mode, 7, Some(0x0100_0000));
        bad_fb_offset.framebuffer_offset = 4;
        assert_eq!(
            project_atomic_kms(41, old, new, None, Some(bad_fb_offset)).unwrap_err(),
            NativeKmsProjectionError::InvalidPlaneGeometry
        );
    }

    #[test]
    fn source_builder_rejects_interlace_and_double_clock_before_writes() {
        let mode = mode();
        let projected = project_atomic_kms(
            41,
            State::default(),
            visible_state(41, 7, mode),
            None,
            Some(descriptor(mode, 7, Some(0x0100_0000))),
        )
        .expect("base state projects");
        for unsupported in [
            mode.with_flags(ModeFlags::INTERLACE),
            mode.with_doubled_clock(),
        ] {
            assert_eq!(
                NativePipeProgram::from_source_with_mode(
                    &projected.transition,
                    &projected.planes,
                    wm(),
                    unsupported,
                )
                .unwrap_err(),
                super::super::native_modeset_ops::NativePipeProjectionError::InvalidTiming
            );
        }
        let mut timing_mismatch = mode;
        timing_mismatch.hsync_start += 1;
        assert_eq!(
            NativePipeProgram::from_source_with_mode(
                &projected.transition,
                &projected.planes,
                wm(),
                timing_mismatch,
            )
            .unwrap_err(),
            super::super::native_modeset_ops::NativePipeProjectionError::InvalidTiming
        );
    }

    #[test]
    fn source_builder_preflights_watermarks_and_ddb_before_pipe_callbacks() {
        let mode = mode();
        let projected = project_atomic_kms(
            41,
            State::default(),
            visible_state(41, 7, mode),
            None,
            Some(descriptor(mode, 7, Some(0x0100_0000))),
        )
        .expect("atomic state projects");
        let impossible_latency = WatermarkConfig {
            // A zero latency is invalid for every watermark level in the
            // source builder; its sentinel minimum must be rejected.
            latencies: [0; intel_display::skl_watermark_full::WM_LEVELS],
            ..wm()
        };

        assert!(matches!(
            NativePipeProgram::from_source_with_mode(
                &projected.transition,
                &projected.planes,
                impossible_latency,
                mode,
            ),
            Err(super::super::native_modeset_ops::NativePipeProjectionError::DdbPlanning(_))
        ));
    }

    #[test]
    fn unsupported_mode_flags_are_rejected_at_the_drm_projection_boundary() {
        let base = mode();
        let old = State::default();
        let new = visible_state(41, 7, base);
        for unsupported in [
            base.with_flags(ModeFlags::INTERLACE),
            base.with_doubled_clock(),
        ] {
            let mut state = new;
            state.mode = Some(kms_mode(unsupported));
            assert_eq!(
                project_atomic_kms(
                    41,
                    old,
                    state,
                    None,
                    Some(descriptor(unsupported, 7, Some(0x0100_0000))),
                )
                .unwrap_err(),
                NativeKmsProjectionError::UnsupportedMode
            );
        }
    }
}
