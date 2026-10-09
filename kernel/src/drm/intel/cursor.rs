//! Source-backed register program builder for the restricted ADL-N cursor.
//!
//! This module is deliberately not registered by itself: the live native KMS
//! adapter must first own a cursor GGTT binding, preflight cursor WM/DDB state,
//! verify the saved register image, serialize this program with scanout/modeset,
//! and retain old pages through the source-required vblank retirement point.
//! A `CursorProgram` is only the i915 register sequence, not permission to issue
//! it or evidence that DMA has retired.

use alloc::vec::Vec;

use intel_display::intel_cursor_full as source;

const PIPE_A: u8 = 0;
const CURSOR_PLANE_ID: u8 = intel_display::intel_crtc_full::PLANE_CURSOR;
const ARGB64_BYTES: u64 = 64 * 64 * 4;
const DISPLAY_VERSION: u8 = 13;

/// The exact immutable framebuffer facts required by this one cursor profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorSurface {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: u32,
    pub modifier: u64,
    pub backing_size: u64,
    /// GGTT address, not a physical or CPU address.
    pub ggtt_address: u64,
}

/// The software arm-cache values used by Linux's `i9xx_cursor_update_arm()`.
/// The native adapter must compare the corresponding live MMIO values with
/// its tracked `CursorHardwareState` before constructing a plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorHardwareState {
    pub control: u32,
    pub base: u32,
    pub position: u32,
    pub fbc_control: u32,
}

impl CursorHardwareState {
    /// Match `intel_cursor_plane_create()`'s initial software cache so the
    /// first update arms every register even if disabled firmware left stale
    /// values in the cursor block.
    pub const fn initial() -> Self {
        Self {
            control: u32::MAX,
            base: u32::MAX,
            position: 0,
            fbc_control: u32::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorWrite {
    pub offset: u32,
    pub value: u32,
}

/// A fully validated source-order register sequence. The caller must still
/// own the referenced pages and ensure its cursor watermark/DDB image was
/// computed for this active cursor before issuing these writes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CursorProgram {
    pub writes: Vec<CursorWrite>,
    pub next: CursorHardwareState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CursorPlanError {
    Invalid,
    Unsupported,
    NoMemory,
    MissingCursorWmDdb,
    UnsupportedRegister,
}

fn checked_wm_layout(wm: &source::CursorWm) -> Result<(), CursorPlanError> {
    if wm.levels != 6 {
        return Err(CursorPlanError::MissingCursorWmDdb);
    }
    Ok(())
}

fn checked_active_wm(wm: &source::CursorWm) -> Result<(), CursorPlanError> {
    checked_wm_layout(wm)?;
    let id = usize::from(CURSOR_PLANE_ID);
    let ddb = wm.ddb[id];
    // Do not manufacture a zero-size cursor allocation or accept the disabled
    // firmware defaults. This is a structural admission only: the caller must
    // supply WMs/DDB derived by a source-backed atomic projection, not merely
    // recopy firmware values and assume the new cursor fits.
    if ddb.end <= ddb.start || !wm.optimal[id][0].enable {
        return Err(CursorPlanError::MissingCursorWmDdb);
    }
    Ok(())
}

fn display() -> source::CursorDisplay {
    source::CursorDisplay {
        display_ver: DISPLAY_VERSION,
        // ADL-N is display version 13; the upstream cursor control applies
        // Wa_22012358565 to display 13 and reserves one arbiter slot.
        wa_22012358565: true,
        has_cur_fbc: true,
        ..source::CursorDisplay::default()
    }
}

/// Map only Pipe-A cursor registers used by the translated display-13 update
/// helper. Offsets follow Linux 7.2.3 `intel_cursor_regs.h`; unsupported pipes
/// and features are refused rather than mapped by guessed strides.
fn write_offset(reg: source::CursorReg) -> Result<u32, CursorPlanError> {
    use source::CursorReg;
    match reg {
        CursorReg::Control(PIPE_A) => Ok(0x70080),
        CursorReg::Base(PIPE_A) => Ok(0x70084),
        CursorReg::Position(PIPE_A) => Ok(0x70088),
        CursorReg::FbcControl(PIPE_A) => Ok(0x700a0),
        CursorReg::Wm(PIPE_A, level @ 0..=5) => Ok(0x70140 + u32::from(level) * 4),
        CursorReg::WmTransition(PIPE_A) => Ok(0x70168),
        CursorReg::WmSagv(PIPE_A) => Ok(0x70158),
        CursorReg::WmSagvTransition(PIPE_A) => Ok(0x7015c),
        CursorReg::BufCfg(PIPE_A) => Ok(0x7017c),
        // The native TC path excludes PSR/selective fetch; these registers are
        // intentionally not part of this minimal arm sequence.
        _ => Err(CursorPlanError::UnsupportedRegister),
    }
}

struct PlanIo {
    wm: source::CursorWm,
    writes: Vec<CursorWrite>,
    error: Option<CursorPlanError>,
}

impl PlanIo {
    fn new(wm: source::CursorWm) -> Result<Self, CursorPlanError> {
        let mut writes = Vec::new();
        writes
            .try_reserve_exact(16)
            .map_err(|_| CursorPlanError::NoMemory)?;
        Ok(Self {
            wm,
            writes,
            error: None,
        })
    }

    fn push(&mut self, reg: source::CursorReg, value: u32) {
        match write_offset(reg) {
            Ok(offset) => self.writes.push(CursorWrite { offset, value }),
            Err(error) => self.error = Some(error),
        }
    }
}

impl source::CursorIo for PlanIo {
    fn write(&mut self, reg: source::CursorReg, value: u32, _dsb: bool) {
        self.push(reg, value);
    }

    fn pipe_wm(&self, _pipe: u8) -> source::CursorWm {
        self.wm
    }

    fn fbc_has(&self, display: source::CursorDisplay) -> bool {
        display.has_cur_fbc
    }

    fn wa_22012358565(&self, display: source::CursorDisplay) -> bool {
        display.wa_22012358565
    }
}

fn crtc_state(width: u32, height: u32) -> Result<source::CursorCrtcState, CursorPlanError> {
    if width == 0 || height == 0 {
        return Err(CursorPlanError::Invalid);
    }
    let width = i32::try_from(width).map_err(|_| CursorPlanError::Invalid)?;
    let height = i32::try_from(height).map_err(|_| CursorPlanError::Invalid)?;
    Ok(source::CursorCrtcState {
        pipe: PIPE_A,
        active: true,
        pipe_src: source::Rect {
            x1: 0,
            y1: 0,
            x2: width,
            y2: height,
        },
        ..source::CursorCrtcState::default()
    })
}

fn checked_position(x: i32, y: i32, hot_x: u32, hot_y: u32) -> Result<(i32, i32), CursorPlanError> {
    if hot_x >= 64 || hot_y >= 64 {
        return Err(CursorPlanError::Invalid);
    }
    let x = x
        .checked_sub(i32::try_from(hot_x).map_err(|_| CursorPlanError::Invalid)?)
        .ok_or(CursorPlanError::Invalid)?;
    let y = y
        .checked_sub(i32::try_from(hot_y).map_err(|_| CursorPlanError::Invalid)?)
        .ok_or(CursorPlanError::Invalid)?;
    // The translated helper packs signed coordinates into 15-bit magnitudes.
    // Reject values it would otherwise silently truncate or wrap.
    if !(-0x7fff..=0x7fff).contains(&x) || !(-0x7fff..=0x7fff).contains(&y) {
        return Err(CursorPlanError::Invalid);
    }
    Ok((x, y))
}

fn validate_surface(surface: CursorSurface) -> Result<u32, CursorPlanError> {
    if surface.width != 64
        || surface.height != 64
        || surface.pitch != 64 * 4
        || surface.backing_size < ARGB64_BYTES
        || !source::intel_cursor_format_mod_supported(
            surface.modifier == source::DRM_FORMAT_MOD_LINEAR,
            surface.format,
        )
    {
        return Err(CursorPlanError::Unsupported);
    }
    let address = u32::try_from(surface.ggtt_address).map_err(|_| CursorPlanError::Unsupported)?;
    // The active VT-d cursor workaround is not a property of this pure
    // planner. Use the stronger upstream alignment unconditionally; the live
    // GGTT allocator is already aligned more strictly than this (256 KiB).
    let alignment = source::i9xx_cursor_min_alignment(true);
    if address == 0
        || address % alignment != 0
        || u64::from(address)
            .checked_add(ARGB64_BYTES)
            .is_none_or(|end| end > (1u64 << 32))
    {
        return Err(CursorPlanError::Unsupported);
    }
    Ok(address)
}

/// Prepare the Linux 7.2.3 display-13 arm sequence for the one supported
/// profile: pipe A, linear 64x64 ARGB8888, no scaling/rotation/PSR, and a
/// caller-supplied source-computed cursor WM/DDB image.
pub(super) fn plan_update(
    surface: CursorSurface,
    x: i32,
    y: i32,
    hot_x: u32,
    hot_y: u32,
    crtc_width: u32,
    crtc_height: u32,
    current: CursorHardwareState,
    wm: source::CursorWm,
) -> Result<CursorProgram, CursorPlanError> {
    let base = validate_surface(surface)?;
    checked_active_wm(&wm)?;
    let (x, y) = checked_position(x, y, hot_x, hot_y)?;
    let x2 = x.checked_add(64).ok_or(CursorPlanError::Invalid)?;
    let y2 = y.checked_add(64).ok_or(CursorPlanError::Invalid)?;
    let display = display();
    let crtc = crtc_state(crtc_width, crtc_height)?;
    let mut state = source::CursorPlaneState {
        pipe: PIPE_A,
        visible: true,
        src: source::Rect {
            x1: 0,
            y1: 0,
            x2: 64 << 16,
            y2: 64 << 16,
        },
        dst: source::Rect {
            x1: x,
            y1: y,
            x2,
            y2,
        },
        rotation: source::DRM_MODE_ROTATE_0,
        fb: Some(source::Framebuffer {
            modifier: surface.modifier,
            cpp0: 4,
            pitch0: surface.pitch,
            width: surface.width,
            height: surface.height,
        }),
        mapping_stride: surface.pitch,
        surf: base,
        ..source::CursorPlaneState::default()
    };
    let mut io = PlanIo::new(wm)?;
    if source::i9xx_check_cursor(
        &mut io,
        display,
        source::CursorModeConfig {
            cursor_width: 64,
            cursor_height: 64,
        },
        &crtc,
        &mut state,
    ) != 0
    {
        return Err(CursorPlanError::Invalid);
    }
    let mut plane = source::CursorPlane {
        pipe: PIPE_A,
        plane_id: CURSOR_PLANE_ID,
        display,
        cursor_base: current.base,
        cursor_size: current.fbc_control,
        cursor_cntl: current.control,
        ..source::CursorPlane::default()
    };
    source::i9xx_cursor_update_arm(&mut io, &mut plane, &crtc, Some(&state));
    if let Some(error) = io.error {
        return Err(error);
    }
    Ok(CursorProgram {
        writes: io.writes,
        next: CursorHardwareState {
            control: plane.cursor_cntl,
            base: plane.cursor_base,
            position: source::intel_cursor_position(&crtc, &state, false),
            fbc_control: plane.cursor_size,
        },
    })
}

/// Prepare the translated disable-arm sequence. The caller must retain the
/// currently bound cursor pages until at least the next real hardware vblank,
/// matching upstream's deferred `unpin_work` schedule.
pub(super) fn plan_disable(
    crtc_width: u32,
    crtc_height: u32,
    current: CursorHardwareState,
    wm: source::CursorWm,
) -> Result<CursorProgram, CursorPlanError> {
    checked_wm_layout(&wm)?;
    let display = display();
    let crtc = crtc_state(crtc_width, crtc_height)?;
    let mut io = PlanIo::new(wm)?;
    let mut plane = source::CursorPlane {
        pipe: PIPE_A,
        plane_id: CURSOR_PLANE_ID,
        display,
        cursor_base: current.base,
        cursor_size: current.fbc_control,
        cursor_cntl: current.control,
        ..source::CursorPlane::default()
    };
    source::i9xx_cursor_disable_arm(&mut io, &mut plane, &crtc);
    if let Some(error) = io.error {
        return Err(error);
    }
    Ok(CursorProgram {
        writes: io.writes,
        next: CursorHardwareState {
            control: plane.cursor_cntl,
            base: plane.cursor_base,
            position: 0,
            fbc_control: plane.cursor_size,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wm() -> source::CursorWm {
        let mut wm = source::CursorWm {
            levels: 6,
            ..source::CursorWm::default()
        };
        let id = usize::from(CURSOR_PLANE_ID);
        wm.optimal[id][0] = source::WmLevel {
            enable: true,
            blocks: 1,
            lines: 1,
            ..source::WmLevel::default()
        };
        wm.ddb[id] = source::DdbEntry { start: 1, end: 8 };
        wm
    }

    fn disabled_wm() -> source::CursorWm {
        source::CursorWm {
            levels: 6,
            ..source::CursorWm::default()
        }
    }

    fn surface() -> CursorSurface {
        CursorSurface {
            width: 64,
            height: 64,
            pitch: 256,
            format: source::DRM_FORMAT_ARGB8888,
            modifier: source::DRM_FORMAT_MOD_LINEAR,
            backing_size: ARGB64_BYTES,
            ggtt_address: 0x0040_0000,
        }
    }

    #[test]
    fn update_uses_source_order_and_hotspot_adjusted_signed_position() {
        let program = plan_update(
            surface(),
            2,
            5,
            4,
            8,
            1920,
            1080,
            CursorHardwareState::initial(),
            wm(),
        )
        .unwrap();
        let control = program
            .writes
            .iter()
            .position(|write| write.offset == 0x70080)
            .unwrap();
        let position = program
            .writes
            .iter()
            .position(|write| write.offset == 0x70088)
            .unwrap();
        let base = program
            .writes
            .iter()
            .position(|write| write.offset == 0x70084)
            .unwrap();
        assert!(control < position && position < base);
        assert_eq!(
            program.writes[position].value,
            (1 << 15) | 2 | ((3 << 16) | (1 << 31))
        );
        assert_eq!(program.writes[base].value, 0x0040_0000);
        assert_eq!(program.next.control & source::MCURSOR_MODE_MASK, 0x27);
    }

    #[test]
    fn plan_rejects_unsupported_layout_position_and_unallocated_wm() {
        let mut bad = surface();
        bad.format = intel_display::universal_plane::XRGB8888;
        assert_eq!(
            plan_update(
                bad,
                0,
                0,
                0,
                0,
                1920,
                1080,
                CursorHardwareState::initial(),
                wm()
            ),
            Err(CursorPlanError::Unsupported)
        );
        assert_eq!(
            plan_update(
                surface(),
                i32::MAX,
                0,
                0,
                0,
                1920,
                1080,
                CursorHardwareState::initial(),
                wm()
            ),
            Err(CursorPlanError::Invalid)
        );
        assert_eq!(
            plan_update(
                surface(),
                0,
                0,
                0,
                0,
                1920,
                1080,
                CursorHardwareState::initial(),
                source::CursorWm::default(),
            ),
            Err(CursorPlanError::MissingCursorWmDdb)
        );
        let mut weak_alignment = surface();
        weak_alignment.ggtt_address += 0x1000;
        assert_eq!(
            plan_update(
                weak_alignment,
                0,
                0,
                0,
                0,
                1920,
                1080,
                CursorHardwareState::initial(),
                wm(),
            ),
            Err(CursorPlanError::Unsupported)
        );
    }

    #[test]
    fn disable_uses_cursor_disable_helper() {
        let program = plan_disable(
            1920,
            1080,
            CursorHardwareState {
                control: 0x100_0027,
                base: 0x0040_0000,
                position: 0,
                fbc_control: 0,
            },
            wm(),
        )
        .unwrap();
        assert_eq!(program.next.control, 0);
        assert_eq!(program.next.base, 0);
        assert!(
            program
                .writes
                .iter()
                .any(|write| write.offset == 0x70080 && write.value == 0)
        );
        assert!(
            program
                .writes
                .iter()
                .any(|write| write.offset == 0x70084 && write.value == 0)
        );

        // Disabling may remove the cursor allocation in the target state;
        // source update-arm still writes that now-empty WM/DDB image.
        let disabled = plan_disable(1920, 1080, program.next, disabled_wm()).unwrap();
        assert!(
            disabled
                .writes
                .iter()
                .any(|write| write.offset == 0x7017c && write.value == 0)
        );
    }
}
