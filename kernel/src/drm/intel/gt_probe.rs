//! Read-only Gen12 forcewake/engine observation; never acquire forcewake.
use alloc::{format, string::String};

use super::regs::RegisterWindow;
const ACK_GT: u32 = 0x130044;
const ACK_RENDER: u32 = 0xd84;
const ACK_VDBOX0: u32 = 0xd50;
const ACK_VEBOX0: u32 = 0xd70;
const MEDIA_DISABLE: u32 = 0x9140;
const BCS_CTL: u32 = 0x2203c;
/// Deliberately has no write operation, unlike the modeset register interface.
trait ReadOnly {
    fn read(&self, offset: u32) -> Option<u32>;
}
impl ReadOnly for RegisterWindow {
    fn read(&self, offset: u32) -> Option<u32> {
        if ![
            ACK_GT,
            ACK_RENDER,
            ACK_VDBOX0,
            ACK_VEBOX0,
            MEDIA_DISABLE,
            BCS_CTL,
        ]
        .contains(&offset)
            || offset as usize + 4 > self.len()
        {
            return None;
        }
        // SAFETY: aligned allowlisted dword in the live mapped BAR. Always-on
        // ACKs need no wake; observe() only attempts GT-gated reads if firmware
        // already holds GT wake. It never claims ownership or wakes the GT.
        let value = unsafe { ((self.base() + offset as usize) as *const u32).read_volatile() };
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        (value != u32::MAX).then_some(value)
    }
}
#[derive(Debug)]
struct Observation {
    gt: Option<u32>,
    render: Option<u32>,
    vdbox: Option<u32>,
    vebox: Option<u32>,
    media_disable: Option<u32>,
    bcs_control: Option<u32>,
    stable: bool,
}
fn observe(bus: &impl ReadOnly) -> Observation {
    let gt = bus.read(ACK_GT);
    let mut result = Observation {
        gt,
        render: bus.read(ACK_RENDER),
        vdbox: bus.read(ACK_VDBOX0),
        vebox: bus.read(ACK_VEBOX0),
        media_disable: None,
        bcs_control: None,
        stable: false,
    };
    if gt.is_some_and(|value| value & 1 != 0) {
        result.media_disable = bus.read(MEDIA_DISABLE);
        result.bcs_control = bus.read(BCS_CTL);
        result.stable = bus.read(ACK_GT).is_some_and(|value| value & 1 != 0);
        if !result.stable {
            result.media_disable = None;
            result.bcs_control = None;
        }
    }
    result
}
pub(crate) fn report(window: RegisterWindow) -> String {
    let observation = observe(&window);
    format!(
        "intel-gt: READ ONLY; ACK_GT={:?} ACK_RENDER={:?} ACK_VDBOX0={:?} ACK_VEBOX0={:?}; \
         media-engine-disable={:?} BCS0_CTL={:?}; firmware-wake-sample-stable={}; absent values \
         mean unavailable, NOT absent engines. No forcewake/engine writes; 未在硬件上验证\n",
        observation.gt,
        observation.render,
        observation.vdbox,
        observation.vebox,
        observation.media_disable,
        observation.bcs_control,
        observation.stable
    )
}
#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeMap, vec, vec::Vec};
    use core::cell::RefCell;

    use super::*;
    struct Fake {
        reads: RefCell<Vec<u32>>,
        values: BTreeMap<u32, u32>,
        acknowledgments: RefCell<Vec<u32>>,
    }
    impl ReadOnly for Fake {
        fn read(&self, offset: u32) -> Option<u32> {
            self.reads.borrow_mut().push(offset);
            if offset == ACK_GT {
                return Some(self.acknowledgments.borrow_mut().remove(0));
            }
            self.values.get(&offset).copied()
        }
    }
    fn fake(acks: Vec<u32>) -> Fake {
        Fake {
            reads: RefCell::new(Vec::new()),
            values: [(MEDIA_DISABLE, 0x10002), (BCS_CTL, 0x1001)].into(),
            acknowledgments: RefCell::new(acks),
        }
    }
    #[test]
    fn asleep_gt_is_never_woken_or_read() {
        let bus = fake(vec![0]);
        let state = observe(&bus);
        assert_eq!(
            *bus.reads.borrow(),
            vec![ACK_GT, ACK_RENDER, ACK_VDBOX0, ACK_VEBOX0]
        );
        assert!(state.bcs_control.is_none());
    }
    #[test]
    fn awake_observations_are_raw_not_a_submission() {
        let bus = fake(vec![1, 1]);
        let state = observe(&bus);
        assert!(state.stable);
        assert_eq!(state.media_disable, Some(0x10002));
        assert_eq!(state.bcs_control, Some(0x1001));
    }
    #[test]
    fn losing_firmware_wake_invalidates_engine_values() {
        let bus = fake(vec![1, 0]);
        let state = observe(&bus);
        assert!(!state.stable);
        assert!(state.bcs_control.is_none());
    }
    #[test]
    fn bounded_window_and_read_only_bytes() {
        let mut memory = vec![0u32; 0x140000 / 4];
        memory[ACK_GT as usize / 4] = 1;
        memory[BCS_CTL as usize / 4] = 0x801;
        let before = memory.clone();
        // SAFETY: owned aligned buffer remains live through observation.
        let window =
            unsafe { RegisterWindow::from_mapped(memory.as_mut_ptr() as usize, memory.len() * 4) };
        assert_eq!(observe(&window).bcs_control, Some(0x801));
        assert!(ReadOnly::read(&window, 0x22030).is_none());
        assert_eq!(memory, before);
    }
}
