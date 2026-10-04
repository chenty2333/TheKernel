//! Read-only firmware scanout identity/progression contract for rollback.
//! MMIO progression is necessary, not proof of the picture at the monitor.
use alloc::{format, string::String, vec::Vec};

use super::{
    gmbus::PollTimer,
    regs::{Register, Registers, ddi as d, pipe as p},
};
const ENABLE: u32 = 1 << 31;
const RUNNING: u32 = 1 << 30;
const PIPE_REGS: [[Register; 10]; 4] = [
    [
        p::PIPECONF_A,
        p::PLANE_CTL_A,
        p::PLANE_SURF_A,
        p::PLANE_STRIDE_A,
        p::PLANE_SIZE_A,
        p::PLANE_OFFSET_A,
        p::PIPESRC_A,
        d::TRANS_DDI_FUNC_CTL_A,
        d::TRANS_CLK_SEL_A,
        p::PLANE_SURFLIVE_A,
    ],
    [
        p::PIPECONF_B,
        p::PLANE_CTL_B,
        p::PLANE_SURF_B,
        p::PLANE_STRIDE_B,
        p::PLANE_SIZE_B,
        p::PLANE_OFFSET_B,
        p::PIPESRC_B,
        d::TRANS_DDI_FUNC_CTL_B,
        d::TRANS_CLK_SEL_B,
        p::PLANE_SURFLIVE_B,
    ],
    [
        p::PIPECONF_C,
        p::PLANE_CTL_C,
        p::PLANE_SURF_C,
        p::PLANE_STRIDE_C,
        p::PLANE_SIZE_C,
        p::PLANE_OFFSET_C,
        p::PIPESRC_C,
        d::TRANS_DDI_FUNC_CTL_C,
        d::TRANS_CLK_SEL_C,
        p::PLANE_SURFLIVE_C,
    ],
    [
        p::PIPECONF_D,
        p::PLANE_CTL_D,
        p::PLANE_SURF_D,
        p::PLANE_STRIDE_D,
        p::PLANE_SIZE_D,
        p::PLANE_OFFSET_D,
        p::PIPESRC_D,
        d::TRANS_DDI_FUNC_CTL_D,
        d::TRANS_CLK_SEL_D,
        p::PLANE_SURFLIVE_D,
    ],
];
const DSL: [Register; 4] = [p::PIPEDSL_A, p::PIPEDSL_B, p::PIPEDSL_C, p::PIPEDSL_D];
#[derive(Clone)]
pub(crate) struct FirmwareScanout {
    pub(crate) enabled: [bool; 4],
    images: Vec<(usize, [u32; 10])>,
}
fn read(device: &impl Registers, register: Register) -> Result<u32, String> {
    device
        .read(register)
        .ok_or_else(|| format!("{} unavailable", register.name()))
}
impl FirmwareScanout {
    pub(crate) fn capture(device: &impl Registers) -> Result<Self, String> {
        let mut state = Self {
            enabled: [false; 4],
            images: Vec::new(),
        };
        state
            .images
            .try_reserve_exact(4)
            .map_err(|_| String::from("firmware identity allocation failed"))?;
        for (index, registers) in PIPE_REGS.iter().enumerate() {
            let conf = read(device, registers[0])?;
            if conf & (ENABLE | RUNNING) == 0 {
                continue;
            }
            if conf & (ENABLE | RUNNING) != ENABLE | RUNNING {
                return Err(format!(
                    "pipe {index} is transitioning, not stable firmware scanout"
                ));
            }
            state.enabled[index] = true;
            let mut image = [0; 10];
            for (slot, register) in image.iter_mut().zip(registers) {
                *slot = read(device, *register)?;
            }
            if image[1] & ENABLE == 0
                || image[7] & ENABLE == 0
                || (image[2] & 0xffff_f000) != (image[9] & 0xffff_f000)
            {
                return Err(format!(
                    "pipe {index} has no stable primary-plane/link identity"
                ));
            }
            state.images.push((index, image));
        }
        if state.images.is_empty() {
            return Err(String::from("no firmware primary scanout identified"));
        }
        Ok(state)
    }
    pub(crate) fn primary_a(&self) -> bool {
        self.enabled == [true, false, false, false]
    }
    pub(crate) fn value(&self, register: Register) -> Option<u32> {
        self.images.iter().find_map(|(index, image)| {
            PIPE_REGS[*index]
                .iter()
                .position(|r| *r == register)
                .map(|slot| image[slot])
        })
    }
    fn verify_identity(&self, device: &impl Registers) -> Result<(), String> {
        for (index, registers) in PIPE_REGS.iter().enumerate() {
            let actual = read(device, registers[0])? & (ENABLE | RUNNING);
            let expected = if self.enabled[index] {
                ENABLE | RUNNING
            } else {
                0
            };
            if actual != expected {
                return Err(format!("pipe {index} enable/running changed"));
            }
        }
        for (index, image) in &self.images {
            for (register, &saved) in PIPE_REGS[*index].iter().zip(image) {
                let actual = read(device, *register)?;
                if actual != saved {
                    return Err(format!(
                        "{} identity changed: {saved:#x} -> {actual:#x}",
                        register.name()
                    ));
                }
            }
        }
        Ok(())
    }
    /// Used before programming and after rollback, with the same original image.
    /// Requires multiple live advances while layout, link and surface stay equal.
    pub(crate) fn verify(
        &self,
        device: &impl Registers,
        timer: &impl PollTimer,
    ) -> Result<String, String> {
        self.verify_identity(device)?;
        let mut report = String::from(
            "firmware scanout MMIO identity/progression verified; monitor picture 未在硬件上验证",
        );
        for (index, _) in &self.images {
            let first = read(device, DSL[*index])? & 0xfffff;
            let mut last = first;
            let mut advances = 0;
            let start = timer.now_micros();
            let mut sample_at = start.saturating_add(500);
            for _ in 0..100_000 {
                let now = timer.now_micros();
                if now.saturating_sub(start) >= 50_000 {
                    break;
                }
                if now >= sample_at {
                    let line = read(device, DSL[*index])? & 0xfffff;
                    if line != last {
                        advances += 1;
                        last = line;
                    }
                    if advances >= 2 {
                        break;
                    }
                    sample_at = now.saturating_add(500);
                }
                timer.pause();
            }
            if advances < 2 {
                return Err(format!(
                    "pipe {index} scanline stalled at {last}; original={first}"
                ));
            }
            report.push_str(&format!(
                "; pipe {index} DSL {first}->{last}, advances={advances}"
            ));
        }
        self.verify_identity(device)?;
        Ok(report)
    }
}
#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::{super::regs::mock::MockRegisters, *};
    struct Timer(Cell<u64>);
    impl PollTimer for Timer {
        fn now_micros(&self) -> u64 {
            self.0.get()
        }
        fn pause(&self) {
            self.0.set(self.0.get() + 250);
        }
    }
    fn firmware() -> MockRegisters {
        let r = MockRegisters::new();
        r.set(p::PIPECONF_A, ENABLE | RUNNING);
        r.set(p::PLANE_CTL_A, ENABLE | 0x04000000);
        r.set(p::PLANE_SURF_A, 0x400000);
        r.set(p::PLANE_SURFLIVE_A, 0x400000);
        r.set(d::TRANS_DDI_FUNC_CTL_A, ENABLE | (1 << 27));
        r
    }
    #[test]
    fn read_only_identity_requires_live_progress_not_register_equality() {
        let r = firmware();
        let saved = FirmwareScanout::capture(&r).unwrap();
        assert!(saved.verify(&r, &Timer(Cell::new(0))).is_err());
        let line = Cell::new(0u32);
        r.on_read(p::PIPEDSL_A, move |_| {
            line.set((line.get() + 7) % 600);
            line.get()
        });
        assert!(saved.verify(&r, &Timer(Cell::new(0))).is_ok());
        assert!(r.writes().is_empty());
    }
    #[test]
    fn same_counter_with_wrong_surface_or_pitch_is_not_recovery() {
        let r = firmware();
        let saved = FirmwareScanout::capture(&r).unwrap();
        r.on_read(p::PIPEDSL_A, |v| v + 1);
        r.set(p::PLANE_SURFLIVE_A, 0x800000);
        assert!(saved.verify(&r, &Timer(Cell::new(0))).is_err());
        r.set(p::PLANE_SURFLIVE_A, 0x400000);
        r.set(p::PLANE_STRIDE_A, 999);
        assert!(saved.verify(&r, &Timer(Cell::new(0))).is_err());
        assert!(r.writes().is_empty());
    }
    #[test]
    fn missing_or_transitioning_pipe_and_extra_enabled_pipe_fail_closed() {
        let r = firmware();
        r.hide(p::PIPECONF_B);
        assert!(FirmwareScanout::capture(&r).is_err());
        let r = firmware();
        r.set(p::PIPECONF_A, ENABLE);
        assert!(FirmwareScanout::capture(&r).is_err());
        let r = firmware();
        let saved = FirmwareScanout::capture(&r).unwrap();
        r.set(p::PIPECONF_B, ENABLE | RUNNING);
        assert!(saved.verify(&r, &Timer(Cell::new(0))).is_err());
    }
}
