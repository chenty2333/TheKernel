//! Read-only candidate inventory, NOT a complete hardware rollback image.
//! No restoration API is exposed to a production caller until register side
//! effects, GGTT ownership and actual firmware scanout recovery are covered.
use alloc::{format, string::String, vec::Vec};

use super::regs::{self, Register, Registers, Width};
#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) register: Register,
    pub(crate) value: Option<u64>,
}
#[derive(Clone)]
pub(crate) struct Snapshot {
    pub(crate) entries: Vec<Entry>,
}
impl Snapshot {
    pub(crate) fn capture(device: &impl Registers) -> Self {
        let mut inventory: Vec<Register> = regs::NAMED
            .iter()
            .copied()
            .chain(regs::POWER_AND_CLOCK_REGISTERS.iter().copied())
            .chain(regs::COMBO_PHYS.iter().flat_map(|phy| phy.all()))
            .chain(regs::table::ALL.iter().copied().flatten().copied())
            .filter(|register| register.required_quirk().is_none())
            .collect();
        inventory.sort_by_key(|register| register.offset());
        inventory.dedup_by_key(|register| register.offset());
        Self {
            entries: inventory
                .into_iter()
                .map(|register| Entry {
                    value: match register.width() {
                        Width::Bits32 => device.read(register).map(u64::from),
                        Width::Bits64 => device.read64(register),
                    },
                    register,
                })
                .collect(),
        }
    }
    pub(crate) fn permits_modeset(&self) -> bool {
        // A register read is not proof that a powered-down bank was captured.
        // Full firmware state and a hardware restoration sequence remain absent.
        false
    }
    pub(crate) fn render(&self) -> String {
        let mut text = String::from(
            "intel-firmware: candidate snapshot; NOT a complete rollback; modeset refused; \
             未在硬件上验证\n",
        );
        for entry in &self.entries {
            match entry.value {
                Some(value) => text.push_str(&format!(
                    "  {} [{:#x}]={value:#018x}\n",
                    entry.register.name(),
                    entry.register.offset()
                )),
                None => text.push_str(&format!("  {} unavailable\n", entry.register.name())),
            }
        }
        text
    }
}
#[cfg(test)]
mod tests {
    use super::{
        super::regs::{CDCLK_CTL, CDCLK_PLL_ENABLE, mock::MockRegisters},
        *,
    };
    #[test]
    fn candidate_capture_is_read_only_unique_and_fail_closed() {
        let device = MockRegisters::new();
        device.set(CDCLK_CTL, 0x12345678);
        device.set(CDCLK_PLL_ENABLE, 0x87654321);
        let state = Snapshot::capture(&device);
        assert!(device.writes().is_empty());
        assert!(!state.permits_modeset());
        assert!(
            state
                .entries
                .windows(2)
                .all(|pair| pair[0].register.offset() < pair[1].register.offset())
        );
        assert_eq!(
            state
                .entries
                .iter()
                .find(|e| e.register == CDCLK_CTL)
                .unwrap()
                .value,
            Some(0x12345678)
        );
        assert!(state.render().contains("NOT a complete rollback"));
    }
    #[test]
    fn missing_bank_never_becomes_permission_to_write() {
        let device = MockRegisters::new();
        device.hide(CDCLK_CTL);
        let state = Snapshot::capture(&device);
        assert!(state.entries.iter().any(|e| e.value.is_none()));
        assert!(!state.permits_modeset());
        assert!(device.writes().is_empty());
    }
    #[test]
    fn mock_journal_undoes_each_write_in_exact_reverse_order() {
        // This model checks sequencing/content only. It deliberately has no
        // production write entry point: real PLL/PHY/power registers are not RAM.
        let device = MockRegisters::new();
        device.set(CDCLK_CTL, 7);
        device.set(CDCLK_PLL_ENABLE, 9);
        let operations = [(CDCLK_CTL, 11), (CDCLK_PLL_ENABLE, 13), (CDCLK_CTL, 17)];
        for failure in 1..=operations.len() {
            let mut journal = Vec::new();
            for (register, value) in &operations[..failure] {
                journal.push((*register, device.read(*register).unwrap()));
                assert!(device.write(*register, *value));
            }
            let undo: Vec<_> = journal.into_iter().rev().collect();
            for (register, value) in &undo {
                assert!(device.write(*register, *value));
            }
            assert_eq!(device.read(CDCLK_CTL), Some(7));
            assert_eq!(device.read(CDCLK_PLL_ENABLE), Some(9));
            let writes = device.writes();
            let names: Vec<_> = writes[writes.len() - undo.len()..]
                .iter()
                .map(|(name, _)| *name)
                .collect();
            assert_eq!(
                names,
                operations[..failure]
                    .iter()
                    .rev()
                    .map(|(reg, _)| reg.name())
                    .collect::<Vec<_>>()
            );
        }
    }
}
