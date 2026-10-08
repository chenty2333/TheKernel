//! Narrow projection boundary between translated i915 atomic state and the
//! existing, transactional N305 TC-HDMI modeset path.
//!
//! This is deliberately **not** a `ModesetOps` implementation.  The Linux
//! hooks in `intel_display_modeset_full` are fine grained, while the N305 path
//! owns its disable/program/enable/verify/rollback sequence as one transaction
//! (`tc_modeset::program`).  Reporting success from individual translated
//! callbacks would either skip hardware work or repeat MMIO.  Callers use this
//! module to validate/project the supported Pipe-A state and to identify which
//! translated hooks are already owned by that one TC transaction; every other
//! hook fails closed.

use intel_display::{
    dkl_phy::TcPort,
    intel_display_modeset_full as upstream,
    intel_display_modeset_full::{
        Action, AtomicState, DisplayCaps, EncoderKind, EncoderTransition, ModesetError,
        OutputFormat, PipeState, PipeTransition, PlaneTransition, Timing,
    },
};

use crate::drm::modes::{CTA_VIC_TIMINGS, Mode};

const PIPE_A: u8 = 0;
const PORT_TC1: u8 = 3;
const PORT_TC2: u8 = 4;
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const EOPNOTSUPP: i32 = -95;

/// An action already performed inside the existing TC-HDMI transaction.
///
/// This is a *do-not-dispatch-again* classification, not an `Ok(())` callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TcTransactionOwner {
    /// `tc_modeset::program` disables/re-enables the active DKL PLL.
    DklPllLifecycle,
    /// Its output half owns the TC encoder's pre-enable/enable/disable hooks.
    TcOutputLifecycle,
    /// Its pipe half computes and writes the supported timing/primary-WM state.
    PipeTimingAndPrimaryWatermarks,
    /// Its transcoder phase owns TRANSCONF for the admitted TC HDMI stream.
    TranscoderEnable,
}

/// Result of comparing one translated atomic hook with the current kernel path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActionDisposition {
    /// The action is already part of the indivisible TC transaction. Do not
    /// dispatch it separately, or the same hardware transition is duplicated.
    OwnedByTcTransaction(TcTransactionOwner),
}

/// Fail-closed classification for a hook emitted by translated modeset code.
///
/// The current TC transaction is a composite operation; it cannot safely
/// provide an individual callback for its sub-steps.  Hooks not listed below
/// have no semantically exact standalone N305 primitive and return
/// `ModesetError::Backend(-EOPNOTSUPP)` rather than pretending to succeed.
pub(crate) fn classify_action(
    action: Action,
    pipe: Option<u8>,
) -> Result<ActionDisposition, ModesetError> {
    if pipe.is_some_and(|pipe| pipe != PIPE_A) {
        return Err(unsupported());
    }

    let owner = match action {
        Action::EnableDpll | Action::DisableDpll => TcTransactionOwner::DklPllLifecycle,
        Action::EncodersPrePllEnable
        | Action::EncodersPreEnable
        | Action::EncodersEnable
        | Action::EncodersDisable
        | Action::EncodersPostDisable
        | Action::EncodersPostPllDisable
        | Action::EncoderPrePllHook
        | Action::EncoderPreEnableHook
        | Action::EncoderEnableHook
        | Action::EncoderDisableHook
        | Action::EncoderPostDisableHook
        | Action::EncoderPostPllDisableHook => TcTransactionOwner::TcOutputLifecycle,
        Action::SetPipeSourceSize
        | Action::SetPipeMisc
        | Action::SetTranscoderTimings
        | Action::WriteTranscoderHtotal
        | Action::WriteTranscoderHblank
        | Action::WriteTranscoderHsync
        | Action::WriteTranscoderVtotal
        | Action::WriteTranscoderVblank
        | Action::WriteTranscoderVsync
        | Action::InitialWatermarks => TcTransactionOwner::PipeTimingAndPrimaryWatermarks,
        Action::SetTransconf => TcTransactionOwner::TranscoderEnable,
        // These are intentionally not "close enough".  In particular the
        // current path has no exact per-hook equivalent for DMC pipe requests,
        // PFIT, color programming, linetime WM, pipe chicken, active-state
        // bookkeeping, wait-for-vblank, CDCLK, or shared-DPLL allocation.
        _ => return Err(unsupported()),
    };

    Ok(ActionDisposition::OwnedByTcTransaction(owner))
}

/// A carefully bounded projection of one translated Pipe-A transition.
///
/// `old_mode` and `new_mode` are canonical CTA modes, not a reconstruction
/// from timing totals: translated `Timing` has no sync-polarity/provenance
/// fields, so noncanonical timings cannot be converted without losing data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct N305PipeAProjection {
    pub(crate) port: TcPort,
    pub(crate) old_mode: Option<Mode>,
    pub(crate) new_mode: Option<Mode>,
    pub(crate) old_enabled: bool,
    pub(crate) new_enabled: bool,
}

/// Convert the narrow Pipe-A/TC-HDMI subset that the existing transaction can
/// represent.  This only projects and validates state; it performs no MMIO,
/// power-domain acquisition, DPLL allocation, or callback dispatch.
pub(crate) fn project_pipe_a(
    state: &AtomicState,
    caps: DisplayCaps,
) -> Result<N305PipeAProjection, ModesetError> {
    if caps.display_version != 13 || caps.pipe_mask & 1 == 0 || !caps.has_ddi {
        return Err(unsupported());
    }
    if state.pipes.len() != 1
        || state.pipes[0].old.pipe != PIPE_A
        || state.pipes[0].new.pipe != PIPE_A
    {
        return Err(unsupported());
    }

    let transition = &state.pipes[0];
    validate_pipe_shape(&transition.old)?;
    validate_pipe_shape(&transition.new)?;

    let mut port = None;
    let mut encoder_count = 0usize;
    let mut has_old_encoder = false;
    let mut has_new_encoder = false;
    for encoder in &state.encoders {
        let touches_a = encoder.old_crtc == Some(PIPE_A) || encoder.new_crtc == Some(PIPE_A);
        if !touches_a {
            continue;
        }
        encoder_count += 1;
        has_old_encoder |= encoder.old_crtc == Some(PIPE_A);
        has_new_encoder |= encoder.new_crtc == Some(PIPE_A);
        if encoder.kind != EncoderKind::Hdmi || encoder.tbt_alt_mode || encoder.dedicated_external {
            return Err(unsupported());
        }
        let encoder_port = match encoder.port {
            PORT_TC1 => TcPort::Tc1,
            PORT_TC2 => TcPort::Tc2,
            _ => return Err(unsupported()),
        };
        if port.is_some_and(|previous| previous != encoder_port) {
            return Err(unsupported());
        }
        port = Some(encoder_port);
    }
    let port = port.ok_or_else(unsupported)?;
    if encoder_count != 1
        || (transition.old.hw_enable && !has_old_encoder)
        || (transition.old.pipe_active && !has_old_encoder)
        || (transition.new.hw_enable && !has_new_encoder)
        || (transition.new.pipe_active && !has_new_encoder)
    {
        return Err(unsupported());
    }

    if state.planes.len() != 1
        || state.planes.iter().any(|plane| plane.pipe != PIPE_A)
        || state.planes.iter().any(|plane| {
            plane.id != 0
                || plane.is_y_plane
                || plane.new_format != DRM_FORMAT_XRGB8888
                || plane.new_modifier != 0
                || plane.old_format != DRM_FORMAT_XRGB8888
                || plane.old_modifier != 0
                || !plane.old_visible
                || !plane.old_fb_exists
                || !plane.new_visible
                || !plane.new_fb_exists
                || plane.clear_color_plane
                || plane.new_decrypt
        })
    {
        return Err(unsupported());
    }

    let old_enabled = transition.old.hw_enable || transition.old.pipe_active;
    let new_enabled = transition.new.hw_enable || transition.new.pipe_active;
    let old_mode = if old_enabled {
        Some(canonical_mode(&transition.old.old_timing)?)
    } else {
        None
    };
    let new_mode = if new_enabled {
        Some(canonical_mode(&transition.new.new_timing)?)
    } else {
        None
    };

    Ok(N305PipeAProjection {
        port,
        old_mode,
        new_mode,
        old_enabled,
        new_enabled,
    })
}

/// Build a translated atomic-state projection from the real Native `present`
/// request and run the source helpers that are pure and semantically exact for
/// this path.  This is a preflight only: no `ModesetOps` hook, DPLL acquire,
/// power transition, register write, or source commit-tail is run here.  The
/// already-admitted TC transaction remains the sole owner of hardware writes.
pub(crate) fn preflight_native_mode_change(
    old_mode: Mode,
    new_mode: Mode,
    port: TcPort,
) -> Result<N305PipeAProjection, ModesetError> {
    let source_port = match port {
        TcPort::Tc1 => PORT_TC1,
        TcPort::Tc2 => PORT_TC2,
        TcPort::Tc3 | TcPort::Tc4 => return Err(unsupported()),
    };
    let caps = DisplayCaps {
        display_version: 13,
        display_version_x100: 1300,
        has_display: true,
        has_ddi: true,
        has_dpll_manager: true,
        pipe_mask: 1,
        port_mask: 1 << source_port,
        ..DisplayCaps::default()
    };
    let old = PipeState {
        pipe: PIPE_A,
        hw_enable: true,
        hw_active: true,
        uapi_enable: true,
        uapi_active: true,
        cpu_transcoder: PIPE_A,
        master_transcoder: u8::MAX,
        mst_master_transcoder: u8::MAX,
        pipe_bpp: 24,
        max_pipe_bpp: 24,
        old_timing: source_timing(old_mode),
        new_timing: source_timing(old_mode),
        output_format: OutputFormat::Rgb,
        sink_format: OutputFormat::Rgb,
        ..PipeState::default()
    };
    // Keep the old timing as both old/new in the old state: source comparisons
    // must not infer a change from a synthetic zero-valued default.
    let new = PipeState {
        pipe: PIPE_A,
        hw_enable: true,
        hw_active: true,
        uapi_enable: true,
        uapi_active: true,
        needs_modeset: true,
        mode_changed: true,
        cpu_transcoder: PIPE_A,
        master_transcoder: u8::MAX,
        mst_master_transcoder: u8::MAX,
        pipe_bpp: 24,
        max_pipe_bpp: 24,
        old_timing: source_timing(old_mode),
        new_timing: source_timing(new_mode),
        output_format: OutputFormat::Rgb,
        sink_format: OutputFormat::Rgb,
        ..PipeState::default()
    };
    let state = AtomicState {
        pipes: alloc::vec![PipeTransition { old, new }],
        planes: alloc::vec![PlaneTransition {
            pipe: PIPE_A,
            id: 0,
            old_visible: true,
            old_fb_exists: true,
            old_format: DRM_FORMAT_XRGB8888,
            old_modifier: 0,
            new_visible: true,
            new_fb_exists: true,
            new_format: DRM_FORMAT_XRGB8888,
            new_modifier: 0,
            ..PlaneTransition::default()
        }],
        encoders: alloc::vec![EncoderTransition {
            old_crtc: Some(PIPE_A),
            new_crtc: Some(PIPE_A),
            kind: EncoderKind::Hdmi,
            port: source_port,
            edid_bpc: 8,
            max_bpc: 8,
            max_requested_bpc: 8,
            has_best_encoder: true,
            ..EncoderTransition::default()
        }],
        modeset: true,
        ..AtomicState::default()
    };

    let projection = project_pipe_a(&state, caps)?;
    if !projection
        .old_mode
        .is_some_and(|projected| projected.same_timing(&old_mode))
        || !projection
            .new_mode
            .is_some_and(|projected| projected.same_timing(&new_mode))
    {
        return Err(unsupported());
    }

    // These are upstream pure calculations/identity checks.  Do not call
    // `intel_crtc_atomic_check` or `intel_atomic_check`: their DPLL, color,
    // scaler, WM, CDCLK, bandwidth and framework hooks are not all wired to
    // the current TC implementation and would require a real backend.
    if !upstream::check_digital_port_conflicts(&state, caps) {
        return Err(ModesetError::Invalid);
    }
    let phy =
        upstream::intel_port_to_phy(caps, i8::try_from(source_port).map_err(|_| unsupported())?);
    if !upstream::intel_phy_is_tc(caps, phy)
        || upstream::intel_port_to_tc(caps, i8::try_from(source_port).map_err(|_| unsupported())?)
            != port.index() as i8
    {
        return Err(unsupported());
    }
    let mut checked_new = state.pipes[0].new;
    upstream::compute_baseline_pipe_bpp(&state, &mut checked_new, caps)?;
    if checked_new.pipe_bpp != 24 || checked_new.max_pipe_bpp != 24 {
        return Err(unsupported());
    }

    Ok(projection)
}

fn validate_pipe_shape(pipe: &PipeState) -> Result<(), ModesetError> {
    if pipe.pipe != PIPE_A
        || pipe.is_joiner_secondary
        || pipe.joiner_pipes != 0
        || pipe.port_sync_mode
        || pipe.port_sync_master
        || pipe.port_sync_slave
        || pipe.mst_slave
        || pipe.has_dsc
        || pipe.transcoder_is_dsi
        || pipe.is_dsi_output
        || pipe.has_pch_encoder
        || pipe.has_dp_encoder
        || pipe.vrr_enabled
        || pipe.vrr_enabling
        || pipe.vrr_disabling
        || pipe.is_interlaced
        || pipe.double_wide
        || pipe.scaled_planes != 0
        || pipe.nv12_planes != 0
        || pipe.c8_planes != 0
        || pipe.color_update
        || pipe.color_mgmt_changed
        || pipe.uapi_color_mgmt_changed
        || pipe.is_yuv_output
        || pipe.output_format != OutputFormat::Rgb
        || pipe.sink_format != OutputFormat::Rgb
        || pipe.pipe_bpp != 24
        || pipe.max_pipe_bpp != 24
        || pipe.hdmi_scrambling
        || pipe.hdmi_high_tmds_clock_ratio
    {
        return Err(unsupported());
    }
    Ok(())
}

fn canonical_mode(timing: &Timing) -> Result<Mode, ModesetError> {
    for entry in CTA_VIC_TIMINGS {
        // This is the current driver's supported mode pair only: HDMI VIC 16
        // (1080p60) and VIC 95 (2160p30).  Polarity/provenance come from the
        // canonical table rather than guessed from the translated state.
        if !matches!(entry.vic, 16 | 95) {
            continue;
        }
        let mode = entry.mode;
        if timing.clock_khz == mode.clock_khz
            && timing.hdisplay == u32::from(mode.hdisplay)
            && timing.hsync_start == u32::from(mode.hsync_start)
            && timing.hsync_end == u32::from(mode.hsync_end)
            && timing.htotal == u32::from(mode.htotal)
            && timing.hblank_start == u32::from(mode.hdisplay)
            && timing.hblank_end == u32::from(mode.htotal)
            && timing.vdisplay == u32::from(mode.vdisplay)
            && timing.vsync_start == u32::from(mode.vsync_start)
            && timing.vsync_end == u32::from(mode.vsync_end)
            && timing.vtotal == u32::from(mode.vtotal)
            && timing.vblank_start == u32::from(mode.vdisplay)
            && timing.vblank_end == u32::from(mode.vtotal)
        {
            return Ok(mode);
        }
    }
    Err(unsupported())
}

fn source_timing(mode: Mode) -> Timing {
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

fn unsupported() -> ModesetError {
    ModesetError::Backend(EOPNOTSUPP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vic(vic: u8) -> Mode {
        CTA_VIC_TIMINGS
            .iter()
            .find(|entry| entry.vic == vic)
            .expect("listed CTA mode")
            .mode
    }

    #[test]
    fn native_preflight_preserves_the_admitted_vic_16_95_pair_on_tc1_tc2() {
        for (port, index) in [(TcPort::Tc1, 0), (TcPort::Tc2, 1)] {
            let projected = preflight_native_mode_change(vic(95), vic(16), port).unwrap();
            assert_eq!(projected.port, port);
            assert!(projected.old_mode.unwrap().same_timing(&vic(95)));
            assert!(projected.new_mode.unwrap().same_timing(&vic(16)));
            assert!(projected.old_enabled && projected.new_enabled);
            assert_eq!(port.index(), index);
        }
    }

    #[test]
    fn native_preflight_refuses_unrepresented_mode_or_tc_port() {
        assert_eq!(
            preflight_native_mode_change(vic(95), vic(4), TcPort::Tc1),
            Err(ModesetError::Backend(EOPNOTSUPP))
        );
        assert_eq!(
            preflight_native_mode_change(vic(95), vic(16), TcPort::Tc3),
            Err(ModesetError::Backend(EOPNOTSUPP))
        );
    }

    #[test]
    fn source_hooks_are_never_claimed_when_no_exact_mapping_exists() {
        assert_eq!(
            classify_action(Action::SetTransconf, Some(PIPE_A)),
            Ok(ActionDisposition::OwnedByTcTransaction(
                TcTransactionOwner::TranscoderEnable
            ))
        );
        assert_eq!(
            classify_action(Action::ColorModeset, Some(PIPE_A)),
            Err(ModesetError::Backend(EOPNOTSUPP))
        );
        assert_eq!(
            classify_action(Action::SetTransconf, Some(1)),
            Err(ModesetError::Backend(EOPNOTSUPP))
        );
    }
}
