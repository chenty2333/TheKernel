//! FreeBSD IGC base helpers shared by the I225 MAC and PHY code.
//!
//! Translated from FreeBSD `sys/dev/igc/igc_base.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright 2021 Intel Corp; Copyright 2021 Rubicon Communications, LLC (Netgate).

pub const SWFW_PHY0_SM: u16 = 0x02;
pub const SWFW_PHY1_SM: u16 = 0x04;
const RFCTL: u32 = 0x05008;
const MANC: u32 = 0x05820;
const RXDCTL0: u32 = 0x02828;
const RLPML: u32 = 0x05004;
const RCTL: u32 = 0x00100;
const ROC: u32 = 0x040ac;
const RNBC: u32 = 0x040a0;
const MPC: u32 = 0x04010;
const MTA: u32 = 0x05200;
const UTA: u32 = 0x0a000;
const RFCTL_IPV6_EX_DIS: u32 = 0x0001_0000;
const RFCTL_LEF: u32 = 0x0004_0000;
const MANC_RCV_TCO_EN: u32 = 0x0002_0000;
const RXDCTL_QUEUE_ENABLE: u32 = 0x0200_0000;
const RCTL_EN: u32 = 0x2;
const RCTL_SBP: u32 = 0x4;
const RCTL_LPE: u32 = 0x20;

pub trait IgcBaseIo {
    fn read(&mut self, reg: u32) -> u32;
    fn write(&mut self, reg: u32, value: u32);
    fn delay_ms(&mut self, ms: u32);
    fn write_flush(&mut self);
    fn bus_function(&self) -> u8;
    fn acquire_swfw_sync(&mut self, mask: u16) -> i32;
    fn release_swfw_sync(&mut self, mask: u16);
    fn init_rx_addrs_generic(&mut self, rar_count: u16);
    fn setup_link(&mut self) -> i32;
    fn clear_hw_counters(&mut self);
    fn check_reset_block_installed(&self) -> bool;
    fn enable_mng_pass_thru(&mut self) -> bool;
    fn check_reset_block(&mut self) -> bool;
    fn power_down_phy_copper(&mut self);
    fn debug(&mut self, _message: &'static str) {}
}

// upstream: igc_base.c igc_acquire_phy_base()
pub fn igc_acquire_phy_base<I: IgcBaseIo>(io: &mut I) -> i32 {
    let mask = if io.bus_function() == 1 {
        SWFW_PHY1_SM
    } else {
        SWFW_PHY0_SM
    };
    io.acquire_swfw_sync(mask)
}

// upstream: igc_base.c igc_release_phy_base()
pub fn igc_release_phy_base<I: IgcBaseIo>(io: &mut I) {
    let mask = if io.bus_function() == 1 {
        SWFW_PHY1_SM
    } else {
        SWFW_PHY0_SM
    };
    io.release_swfw_sync(mask);
}

// upstream: igc_base.c igc_init_hw_base()
pub fn igc_init_hw_base<I: IgcBaseIo>(
    io: &mut I,
    rar_count: u16,
    mta_count: u16,
    uta_count: u16,
) -> i32 {
    io.init_rx_addrs_generic(rar_count);
    for i in 0..mta_count {
        io.write(MTA + u32::from(i) * 4, 0);
    }
    for i in 0..uta_count {
        io.write(UTA + u32::from(i) * 4, 0);
    }
    let result = io.setup_link();
    io.clear_hw_counters();
    result
}

// upstream: igc_base.c igc_power_down_phy_copper_base()
pub fn igc_power_down_phy_copper_base<I: IgcBaseIo>(io: &mut I) {
    if !io.check_reset_block_installed() {
        return;
    }
    if !(io.enable_mng_pass_thru() || io.check_reset_block()) {
        io.power_down_phy_copper();
    }
}

// upstream: igc_base.c igc_rx_fifo_flush_base()
pub fn igc_rx_fifo_flush_base<I: IgcBaseIo>(io: &mut I) {
    let rfctl = io.read(RFCTL) | RFCTL_IPV6_EX_DIS;
    io.write(RFCTL, rfctl);
    if io.read(MANC) & MANC_RCV_TCO_EN == 0 {
        return;
    }

    let mut rxdctl = [0u32; 4];
    for (i, saved) in rxdctl.iter_mut().enumerate() {
        let reg = RXDCTL0 + i as u32 * 0x100;
        *saved = io.read(reg);
        io.write(reg, *saved & !RXDCTL_QUEUE_ENABLE);
    }
    let mut queues_stopped = false;
    for _ in 0..10 {
        io.delay_ms(1);
        let mut rx_enabled = 0;
        for i in 0..4 {
            rx_enabled |= io.read(RXDCTL0 + i * 0x100);
        }
        if rx_enabled & RXDCTL_QUEUE_ENABLE == 0 {
            queues_stopped = true;
            break;
        }
    }
    if !queues_stopped {
        io.debug("Queue disable timed out after 10ms");
    }

    io.write(RFCTL, rfctl & !RFCTL_LEF);
    let rlpml = io.read(RLPML);
    io.write(RLPML, 0);
    let rctl = io.read(RCTL);
    let temp_rctl = (rctl & !(RCTL_EN | RCTL_SBP)) | RCTL_LPE;
    io.write(RCTL, temp_rctl);
    io.write(RCTL, temp_rctl | RCTL_EN);
    io.write_flush();
    io.delay_ms(2);
    for (i, saved) in rxdctl.iter().enumerate() {
        io.write(RXDCTL0 + i as u32 * 0x100, *saved);
    }
    io.write(RCTL, rctl);
    io.write_flush();
    io.write(RLPML, rlpml);
    io.write(RFCTL, rfctl);
    let _ = io.read(ROC);
    let _ = io.read(RNBC);
    let _ = io.read(MPC);
}

// upstream: igc_base.c igc_is_device_id_i225()
pub fn igc_is_device_id_i225(device_id: u16) -> bool {
    matches!(
        device_id,
        0x15f2 | 0x15f3 | 0x3100 | 0x15f8 | 0x3101 | 0x5502 | 0x0d9f | 0x15fd
    )
}

// upstream: igc_base.c igc_is_device_id_i226()
pub fn igc_is_device_id_i226(device_id: u16) -> bool {
    matches!(
        device_id,
        0x125b | 0x125c | 0x3102 | 0x125d | 0x5503 | 0x125f
    )
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    #[derive(Default)]
    struct Fake {
        regs: Vec<(u32, u32)>,
        f: u8,
        calls: Vec<&'static str>,
        result: i32,
    }
    impl Fake {
        fn get(&self, r: u32) -> u32 {
            self.regs.iter().rev().find(|x| x.0 == r).map_or(0, |x| x.1)
        }
        fn set(&mut self, r: u32, v: u32) {
            if let Some(x) = self.regs.iter_mut().find(|x| x.0 == r) {
                x.1 = v
            } else {
                self.regs.push((r, v))
            }
        }
    }
    impl IgcBaseIo for Fake {
        fn read(&mut self, r: u32) -> u32 {
            self.get(r)
        }
        fn write(&mut self, r: u32, v: u32) {
            self.set(r, v)
        }
        fn delay_ms(&mut self, _: u32) {}
        fn write_flush(&mut self) {}
        fn bus_function(&self) -> u8 {
            self.f
        }
        fn acquire_swfw_sync(&mut self, m: u16) -> i32 {
            self.calls.push(if m == SWFW_PHY1_SM {
                "acquire1"
            } else {
                "acquire0"
            });
            self.result
        }
        fn release_swfw_sync(&mut self, m: u16) {
            self.calls.push(if m == SWFW_PHY1_SM {
                "release1"
            } else {
                "release0"
            })
        }
        fn init_rx_addrs_generic(&mut self, _: u16) {
            self.calls.push("rar")
        }
        fn setup_link(&mut self) -> i32 {
            self.calls.push("link");
            self.result
        }
        fn clear_hw_counters(&mut self) {
            self.calls.push("counters")
        }
        fn check_reset_block_installed(&self) -> bool {
            true
        }
        fn enable_mng_pass_thru(&mut self) -> bool {
            false
        }
        fn check_reset_block(&mut self) -> bool {
            false
        }
        fn power_down_phy_copper(&mut self) {
            self.calls.push("power-down")
        }
    }
    #[test]
    fn phy_semaphore_mask_tracks_pci_function_and_init_order() {
        let mut f = Fake {
            f: 1,
            ..Fake::default()
        };
        assert_eq!(igc_acquire_phy_base(&mut f), 0);
        igc_release_phy_base(&mut f);
        assert_eq!(f.calls, ["acquire1", "release1"]);
    }
    #[test]
    fn init_clears_tables_then_links_then_reads_counters() {
        let mut f = Fake {
            result: 7,
            ..Fake::default()
        };
        assert_eq!(igc_init_hw_base(&mut f, 16, 2, 1), 7);
        assert_eq!(&f.calls[..], &["rar", "link", "counters"]);
        assert_eq!(f.get(MTA), 0);
        assert_eq!(f.get(MTA + 4), 0);
        assert_eq!(f.get(UTA), 0);
    }
    #[test]
    fn device_family_predicates_match_hardware_ids() {
        for id in [
            0x15f2, 0x15f3, 0x3100, 0x15f8, 0x3101, 0x5502, 0x0d9f, 0x15fd,
        ] {
            assert!(igc_is_device_id_i225(id));
        }
        for id in [0x125b, 0x125c, 0x3102, 0x125d, 0x5503, 0x125f] {
            assert!(igc_is_device_id_i226(id));
        }
        assert!(!igc_is_device_id_i225(0x125c));
    }
}
