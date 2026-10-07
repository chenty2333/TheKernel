//! ADL-P/N HDMI audio handshake over the existing HDA playback owner.
//!
//! The display half never starts without the already-powered TC HDMI link,
//! VBT-selected source EDID, Pipe-A state and the fastboot power pin. The HDA
//! half receives that same validated ELD; disabling it must complete before a
//! caller may turn off the transcoder, TC port or audio powerwell.
use alloc::{format, string::String};

use intel_display::{
    Error as DisplayError, RegisterIo,
    audio::{self as i915_audio, AudioIo, Eld, EldError, Session},
    ddi::{self, DdiMode},
    device::Port,
    display::Pipe,
    dkl_phy::TcPort,
};
use spin::Mutex;

use super::{
    gmbus::PollTimer,
    power::{DC_STATE_DISABLE, DC_STATE_MASK},
    regs::{Meaning, Register, Registers},
};

const HSW_AUD_PIN_ELD_CP_VLD: u32 = 0x650c0;
const AUDIO_OUTPUT_ENABLE_A: u32 = 1 << 2;
const AUDIO_ELD_VALID_A: u32 = 1;
const UNOWNED_AUDIO_STATUS_MASK: u32 = AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A;

fn register(offset: u32, writable: bool) -> Register {
    if writable {
        Register::read_write("N305_HDMI_AUDIO", offset, Meaning::BringUp, None)
    } else {
        Register::read_only("N305_HDMI_AUDIO", offset, Meaning::BringUp, None)
    }
}

struct AudioRegisters<'a, R, T> {
    registers: &'a R,
    timer: &'a T,
    port: TcPort,
}

impl<R: Registers, T: PollTimer> RegisterIo for AudioRegisters<'_, R, T> {
    fn read32(&self, offset: u32) -> Result<u32, DisplayError> {
        self.registers
            .read(register(offset, false))
            .filter(|value| *value != u32::MAX)
            .ok_or(DisplayError::Unavailable(offset))
    }

    fn write32(&self, offset: u32, value: u32) -> Result<(), DisplayError> {
        if self.registers.write(register(offset, true), value) {
            Ok(())
        } else {
            Err(DisplayError::Unavailable(offset))
        }
    }
}

impl<R: Registers, T: PollTimer> AudioIo for AudioRegisters<'_, R, T> {
    fn audio_power_held(&self, pipe: Pipe) -> Result<(), DisplayError> {
        if pipe != Pipe::A {
            return Err(DisplayError::Refused);
        }
        // Display-13's XELPD power map puts AUDIO_PLAYBACK in PW_2 and audio
        // MMIO behind DC_off. The fastboot owner holds the PW_2 driver request;
        // DC-state admission keeps the MMIO domain accessible. Do not wake either.
        let pw2 = self.read32(0x45404)?;
        let pw2_mask = 2 << 2;
        if pw2 & (pw2_mask | (pw2_mask >> 1)) != pw2_mask | (pw2_mask >> 1)
            || self.read32(0x45504)? & DC_STATE_MASK != DC_STATE_DISABLE
        {
            return Err(DisplayError::Refused);
        }
        if self.read32(0x70008)? & (3 << 30) != 3 << 30 {
            return Err(DisplayError::Refused);
        }
        let port = match self.port {
            TcPort::Tc1 => Port::Tc1,
            TcPort::Tc2 => Port::Tc2,
            TcPort::Tc3 | TcPort::Tc4 => return Err(DisplayError::Refused),
        };
        let function = ddi::decode_function_control(self.read32(0x60400)?);
        if !function.enabled || function.port != Some(port) || function.mode != DdiMode::Hdmi {
            return Err(DisplayError::Refused);
        }
        Ok(())
    }

    fn wait_vblanks(&self, pipe: Pipe, count: u8) -> Result<(), DisplayError> {
        if pipe != Pipe::A || count == 0 {
            return Err(DisplayError::Refused);
        }
        for _ in 0..count {
            let initial = self.read32(0x70040)?;
            let start = self.timer.now_micros();
            let mut observed = false;
            for _ in 0..1_000_000 {
                self.audio_power_held(pipe)?;
                if self.read32(0x70040)? != initial {
                    observed = true;
                    break;
                }
                if self.timer.now_micros().saturating_sub(start) > 150_000 {
                    break;
                }
                self.timer.pause();
            }
            if !observed {
                return Err(DisplayError::Refused);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LinkAudioStatus {
    Enabled,
    Unavailable(&'static str),
}

struct ActiveAudio {
    port: TcPort,
    pixel_clock_khz: u32,
    eld: Eld,
    session: Session,
}

static ACTIVE: Mutex<Option<ActiveAudio>> = Mutex::new(None);
static QUARANTINED: Mutex<bool> = Mutex::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnownedAudioState {
    Inactive,
    Active,
    Unknown,
}

fn classify_unowned_audio_status(status: Option<u32>) -> UnownedAudioState {
    match status {
        Some(value) if value & UNOWNED_AUDIO_STATUS_MASK == 0 => UnownedAudioState::Inactive,
        Some(_) => UnownedAudioState::Active,
        None => UnownedAudioState::Unknown,
    }
}

fn unowned_audio_state(io: &impl AudioIo) -> UnownedAudioState {
    if io.audio_power_held(Pipe::A).is_err() {
        return UnownedAudioState::Unknown;
    }
    // A clear status is only a read-only statement about the display-side
    // presence/ELD handshake; it is not evidence that arbitrary HDA DMA stopped.
    classify_unowned_audio_status(io.read32(HSW_AUD_PIN_ELD_CP_VLD).ok())
}

fn active_audio_matches(
    active_port: TcPort,
    active_clock_khz: u32,
    active_eld: &Eld,
    port: TcPort,
    pixel_clock_khz: u32,
    eld: &Eld,
) -> bool {
    active_port == port && active_clock_khz == pixel_clock_khz && active_eld == eld
}

fn hda_port(port: TcPort) -> Option<u8> {
    // i915 encoder port numbers: PORT_D/TC1=3 and PORT_E/TC2=4. The HDA side
    // additionally validates the corresponding ADL-P digital HDMI widget path.
    match port {
        TcPort::Tc1 => Some(3),
        TcPort::Tc2 => Some(4),
        TcPort::Tc3 | TcPort::Tc4 => None,
    }
}

fn edid_eld(edid: &[u8]) -> Result<Eld, &'static str> {
    i915_audio::build_eld(edid).map_err(|error| match error {
        EldError::InvalidEdid | EldError::InvalidCta => "sink EDID audio blocks are invalid",
        EldError::Truncated => "source EDID did not contain every declared block",
        EldError::NoAudio => "sink EDID contains no CTA Audio SAD",
        EldError::UnsupportedPcm => "sink lacks stereo 48-kHz 16-bit LPCM",
        EldError::TooManySad => "sink ELD exceeds the bounded HDA capability buffer",
    })
}

/// Publish ELD only after the caller has twice-stable source-port/pipe readout,
/// completed its scanout proof and retained the TC/PW2 power pin. Sink or HDA
/// capability gaps leave display modesetting untouched and audio disabled.
pub(super) fn after_link_enabled<R: Registers, T: PollTimer>(
    registers: &R,
    timer: &T,
    port: TcPort,
    pixel_clock_khz: u32,
    edid: &[u8],
) -> Result<LinkAudioStatus, String> {
    if *QUARANTINED.lock() {
        return Err(String::from(
            "HDMI audio state is quarantined; retain display link and power pin",
        ));
    }
    let next_eld = edid_eld(edid);
    if let Ok(eld) = next_eld {
        let same = ACTIVE.lock().as_ref().is_some_and(|active| {
            active_audio_matches(
                active.port,
                active.pixel_clock_khz,
                &active.eld,
                port,
                pixel_clock_khz,
                &eld,
            )
        });
        if same {
            return Ok(LinkAudioStatus::Enabled);
        }
    }
    let previous_port = ACTIVE.lock().as_ref().map(|active| active.port);
    if let Some(previous_port) = previous_port {
        before_link_disable(registers, timer, previous_port)?;
    }
    let Some(hda_port) = hda_port(port) else {
        return Ok(LinkAudioStatus::Unavailable(
            "only TC1/TC2 HDMI is admitted",
        ));
    };
    let eld = match next_eld {
        Ok(eld) => eld,
        Err(reason) => return Ok(LinkAudioStatus::Unavailable(reason)),
    };
    let io = AudioRegisters {
        registers,
        timer,
        port,
    };
    if io.audio_power_held(Pipe::A).is_err() {
        return Ok(LinkAudioStatus::Unavailable(
            "audio power/link proof is not held",
        ));
    }
    let session = match Session::enable(&io, Pipe::A, pixel_clock_khz, &eld) {
        Ok(session) => session,
        Err(i915_audio::AudioError::UnsupportedClock(_)) => {
            return Ok(LinkAudioStatus::Unavailable(
                "HDMI audio clock is not in the DDI table",
            ));
        }
        Err(i915_audio::AudioError::AlreadyEnabled) => {
            // The preexisting status has no Session owner, so do not infer the
            // HDA DMA state or let a later clock/link transition overwrite it.
            *QUARANTINED.lock() = true;
            return Ok(LinkAudioStatus::Unavailable(
                "unowned HDMI audio activity is present; retain link and power pin",
            ));
        }
        Err(i915_audio::AudioError::UnsupportedPipe)
        | Err(i915_audio::AudioError::InvalidSinkEld) => {
            return Ok(LinkAudioStatus::Unavailable(
                "audio power, sink format or existing hardware state is not admitted",
            ));
        }
        Err(i915_audio::AudioError::RestoreFailed) => {
            *QUARANTINED.lock() = true;
            return Err(String::from(
                "display audio register rollback is unverified; retain link and power pin",
            ));
        }
    };
    // i915's DDI sequence raises presence detect, waits one vblank, then
    // invalidates hardware ELD before `pin_eld_notify` delivers the ELD to HDA.
    if let Err(error) = axdriver::sound::set_display_eld(hda_port, Some(eld.as_bytes())) {
        let hda_route_unknown = !matches!(
            error,
            axdriver::prelude::DevError::Unsupported | axdriver::prelude::DevError::InvalidParam
        );
        let mut session = session;
        if let Err(disable_error) = session.disable(&io) {
            *QUARANTINED.lock() = true;
            return Err(format!(
                "HDA handshake failed ({error:?}) and display audio rollback is unverified: \
                 {disable_error:?}"
            ));
        }
        if hda_route_unknown {
            *QUARANTINED.lock() = true;
            return Err(format!(
                "HDA route transition is uncertain ({error:?}); retain display link and power pin"
            ));
        }
        return Ok(LinkAudioStatus::Unavailable(
            "no matching supported HDA HDMI codec route",
        ));
    }
    *ACTIVE.lock() = Some(ActiveAudio {
        port,
        pixel_clock_khz,
        eld,
        session,
    });
    Ok(LinkAudioStatus::Enabled)
}

/// Disable audio and retire HDA before the caller disables the transcoder,
/// TC port or power reference. A failed proof keeps the display link powered.
pub(super) fn before_link_disable<R: Registers, T: PollTimer>(
    registers: &R,
    timer: &T,
    port: TcPort,
) -> Result<(), String> {
    if *QUARANTINED.lock() {
        return Err(String::from(
            "HDMI audio state is quarantined; retain display link and power pin",
        ));
    }
    let Some(active_port) = ACTIVE.lock().as_ref().map(|active| active.port) else {
        let io = AudioRegisters {
            registers,
            timer,
            port,
        };
        return match unowned_audio_state(&io) {
            UnownedAudioState::Inactive => Ok(()),
            UnownedAudioState::Active => {
                *QUARANTINED.lock() = true;
                Err(String::from(
                    "unowned HDMI audio is active; refusing link disable without an HDA \
                     retirement proof",
                ))
            }
            UnownedAudioState::Unknown => {
                *QUARANTINED.lock() = true;
                Err(String::from(
                    "unowned HDMI audio state is unreadable; retain link and power pin",
                ))
            }
        };
    };
    if active_port != port {
        return Ok(());
    }
    let Some(hda_port) = hda_port(port) else {
        *QUARANTINED.lock() = true;
        return Err(String::from(
            "unsupported HDA display port; retain link and power pin",
        ));
    };
    let mut active = ACTIVE.lock();
    let Some(mut state) = active.take() else {
        return Ok(());
    };
    let io = AudioRegisters {
        registers,
        timer,
        port,
    };
    if let Err(error) = state.session.disable(&io) {
        *active = Some(state);
        *QUARANTINED.lock() = true;
        return Err(format!(
            "HDMI audio disable did not retire cleanly; retain link and power pin: {error:?}"
        ));
    }
    if let Err(error) = axdriver::sound::set_display_eld(hda_port, None) {
        *QUARANTINED.lock() = true;
        *active = Some(state);
        return Err(format!(
            "HDA HDMI route shutdown failed before link disable: {error:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{vec, vec::Vec};
    use core::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct RegisterModel {
        values: RefCell<BTreeMap<u32, u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
    }

    impl RegisterModel {
        fn active_link(pin: Option<u32>) -> Self {
            let mut values = BTreeMap::from([
                (0x45404, 0x0c),
                (0x45504, DC_STATE_DISABLE),
                (0x70008, 3 << 30),
                (0x60400, (1 << 31) | (4 << 27)),
            ]);
            if let Some(pin) = pin {
                values.insert(HSW_AUD_PIN_ELD_CP_VLD, pin);
            }
            Self {
                values: RefCell::new(values),
                writes: RefCell::new(Vec::new()),
            }
        }

        fn without_power_proof() -> Self {
            let model = Self::active_link(Some(0));
            model.values.borrow_mut().remove(&0x45404);
            model
        }
    }

    impl Registers for RegisterModel {
        fn read(&self, register: Register) -> Option<u32> {
            self.values.borrow().get(&register.offset()).copied()
        }

        fn read64(&self, _register: Register) -> Option<u64> {
            None
        }

        fn write(&self, register: Register, value: u32) -> bool {
            self.writes.borrow_mut().push((register.offset(), value));
            self.values.borrow_mut().insert(register.offset(), value);
            true
        }
    }

    struct Timer;
    impl PollTimer for Timer {
        fn now_micros(&self) -> u64 {
            0
        }
        fn pause(&self) {}
    }

    /// Give this test an empty audio lifecycle and restore process-global
    /// state even when an assertion unwinds. Fastboot TC model tests share the
    /// same statics, so callers must hold `scheduler_test_context()` first.
    struct AudioStateGuard {
        active: Option<ActiveAudio>,
        quarantined: bool,
    }

    impl AudioStateGuard {
        fn isolate() -> Self {
            let active = ACTIVE.lock().take();
            let quarantined = core::mem::replace(&mut *QUARANTINED.lock(), false);
            Self {
                active,
                quarantined,
            }
        }
    }

    impl Drop for AudioStateGuard {
        fn drop(&mut self) {
            *ACTIVE.lock() = self.active.take();
            *QUARANTINED.lock() = self.quarantined;
        }
    }

    fn test_eld() -> Eld {
        let mut edid = vec![0u8; 256];
        edid[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
        edid[126] = 1;
        edid[127] = 0u8.wrapping_sub(
            edid[..127]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        );
        let cta = &mut edid[128..];
        cta[0] = 2;
        cta[1] = 3;
        cta[2] = 8;
        cta[4] = (1 << 5) | 3;
        cta[5..8].copy_from_slice(&[0x09, 0x04, 0x01]);
        cta[127] = 0u8.wrapping_sub(
            cta[..127]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        );
        i915_audio::build_eld(&edid).unwrap()
    }

    #[test]
    fn unowned_link_disable_requires_readable_inactive_hardware_audio() {
        let _context = crate::test_support::scheduler_test_context();
        let _audio_state = AudioStateGuard::isolate();
        let timer = Timer;

        let idle = RegisterModel::active_link(Some(0x40));
        assert!(before_link_disable(&idle, &timer, TcPort::Tc1).is_ok());
        assert!(!*QUARANTINED.lock());
        assert!(idle.writes.borrow().is_empty());

        for status in [AUDIO_OUTPUT_ENABLE_A, AUDIO_ELD_VALID_A] {
            *QUARANTINED.lock() = false;
            let active = RegisterModel::active_link(Some(0x40 | status));
            assert!(before_link_disable(&active, &timer, TcPort::Tc1).is_err());
            assert!(*QUARANTINED.lock());
            assert!(active.writes.borrow().is_empty());
        }

        for unknown in [
            RegisterModel::active_link(None),
            RegisterModel::without_power_proof(),
        ] {
            *QUARANTINED.lock() = false;
            assert!(before_link_disable(&unknown, &timer, TcPort::Tc1).is_err());
            assert!(*QUARANTINED.lock());
            assert!(unknown.writes.borrow().is_empty());
        }
    }

    #[test]
    fn active_audio_idempotence_includes_the_pixel_clock() {
        let eld = test_eld();
        assert!(active_audio_matches(
            TcPort::Tc1,
            148_500,
            &eld,
            TcPort::Tc1,
            148_500,
            &eld,
        ));
        assert!(!active_audio_matches(
            TcPort::Tc1,
            148_500,
            &eld,
            TcPort::Tc1,
            297_000,
            &eld,
        ));
        assert!(!active_audio_matches(
            TcPort::Tc1,
            148_500,
            &eld,
            TcPort::Tc2,
            148_500,
            &eld,
        ));
    }
}
