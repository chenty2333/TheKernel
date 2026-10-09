//! Intel 82542 first-generation MAC adaptation.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_82542.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    mac::E1000MediaType,
    nvm::{E1000NvmAccess, E1000NvmConfig, E1000NvmType},
    osdep::E1000RegisterIo,
    registers::*,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacParams82542 {
    pub media: E1000MediaType,
    pub mta_count: u16,
    pub rar_count: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitPointers82542 {
    pub mac: bool,
    pub nvm: bool,
    pub phy: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BusInfo82542 {
    pub pci: bool,
    pub speed_known: bool,
    pub width_known: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowControl82542 {
    None,
    RxPause,
    TxPause,
    Full,
    Default,
}

pub trait PciMwi82542 {
    fn clear_mwi(&mut self) -> DevResult;
    fn set_mwi(&mut self) -> DevResult;
}
pub trait InitOps82542 {
    fn clear_vfta(&mut self) -> DevResult;
    fn init_rx_addrs(&mut self, rar_count: u16) -> DevResult;
    fn clear_counters(&mut self) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn setup_physical_interface(&mut self) -> DevResult;
    fn set_default_flow_control(&mut self, requested: &mut FlowControl82542) -> DevResult;
    fn set_flow_control_watermarks(&mut self) -> DevResult;
}

/// upstream: e1000_82542.c e1000_init_phy_params_82542()
pub const fn init_phy_params_82542() -> Option<u32> {
    None
}

/// upstream: e1000_82542.c e1000_init_nvm_params_82542()
pub const fn init_nvm_params_82542() -> E1000NvmConfig {
    E1000NvmConfig {
        kind: E1000NvmType::Microwire,
        word_size: 64,
        delay_usec: 50,
        opcode_bits: 3,
        address_bits: 6,
        page_size: 1,
    }
}

/// upstream: e1000_82542.c e1000_init_mac_params_82542()
pub const fn init_mac_params_82542() -> MacParams82542 {
    MacParams82542 {
        media: E1000MediaType::Fiber,
        mta_count: 128,
        rar_count: E1000_RAR_ENTRIES as u16,
    }
}

/// upstream: e1000_82542.c e1000_init_function_pointers_82542()
pub const fn init_function_pointers_82542() -> InitPointers82542 {
    InitPointers82542 {
        mac: true,
        nvm: true,
        phy: true,
    }
}

/// upstream: e1000_82542.c e1000_get_bus_info_82542()
pub const fn get_bus_info_82542() -> BusInfo82542 {
    BusInfo82542 {
        pci: true,
        speed_known: false,
        width_known: false,
    }
}

/// upstream: e1000_82542.c e1000_reset_hw_82542()
pub fn reset_hw_82542<I: E1000RegisterIo, P: PciMwi82542, R: FnMut() -> DevResult>(
    io: &mut I,
    pci: &mut P,
    revision: u8,
    pci_command: u16,
    mut reload_nvm: R,
) -> DevResult {
    const REVISION_2: u8 = 2;
    const CMD_MEM_WRT_INVALIDATE: u16 = 0x0010;
    if revision == REVISION_2 {
        pci.clear_mwi()?;
    }
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    let ctrl = io.read_register(E1000_CTRL)?;
    io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    reload_nvm()?;
    io.delay_us(2_000);
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    if revision == REVISION_2 && pci_command & CMD_MEM_WRT_INVALIDATE != 0 {
        pci.set_mwi()?;
    }
    Ok(())
}

/// upstream: e1000_82542.c e1000_init_hw_82542()
pub fn init_hw_82542<I: E1000RegisterIo, P: PciMwi82542, O: InitOps82542>(
    io: &mut I,
    pci: &mut P,
    ops: &mut O,
    revision: u8,
    pci_command: u16,
    dma_fairness: bool,
    mta_count: u16,
    rar_count: u16,
) -> DevResult {
    const REVISION_2: u8 = 2;
    const CMD_MEM_WRT_INVALIDATE: u16 = 0x0010;
    io.write_register(E1000_VET, 0)?;
    ops.clear_vfta()?;
    if revision == REVISION_2 {
        pci.clear_mwi()?;
        io.write_register(E1000_RCTL, E1000_RCTL_RST)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(5_000);
    }
    ops.init_rx_addrs(rar_count)?;
    if revision == REVISION_2 {
        io.write_register(E1000_RCTL, 0)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(1_000);
        if pci_command & CMD_MEM_WRT_INVALIDATE != 0 {
            pci.set_mwi()?;
        }
    }
    for index in 0..mta_count {
        io.write_register(E1000_MTA + u32::from(index) * 4, 0)?;
    }
    if dma_fairness {
        let ctrl = io.read_register(E1000_CTRL)?;
        io.write_register(E1000_CTRL, ctrl | E1000_CTRL_PRIOR)?;
    }
    let link = ops.setup_link();
    ops.clear_counters()?;
    link
}

/// upstream: e1000_82542.c e1000_setup_link_82542()
pub fn setup_link_82542<I: E1000RegisterIo, O: InitOps82542>(
    io: &mut I,
    ops: &mut O,
    revision: u8,
    report_tx_early: bool,
    requested: &mut FlowControl82542,
    current: &mut FlowControl82542,
    pause_time: u32,
) -> DevResult {
    if *requested == FlowControl82542::Default {
        ops.set_default_flow_control(requested)?;
    }
    if revision == 2 {
        *requested = match *requested {
            FlowControl82542::TxPause => FlowControl82542::None,
            FlowControl82542::Full => FlowControl82542::RxPause,
            x => x,
        };
    }
    if report_tx_early {
        *requested = match *requested {
            FlowControl82542::RxPause => FlowControl82542::None,
            FlowControl82542::Full => FlowControl82542::TxPause,
            x => x,
        };
    }
    *current = *requested;
    ops.setup_physical_interface()?;
    io.write_register(E1000_FCAL, 0x00c2_8001)?;
    io.write_register(E1000_FCAH, 0x0000_0100)?;
    io.write_register(E1000_FCT, 0x0000_8808)?;
    io.write_register(E1000_FCTTV, pause_time)?;
    ops.set_flow_control_watermarks()
}

/// upstream: e1000_82542.c e1000_led_on_82542()
pub fn led_on_82542<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SWDPIN0 | E1000_CTRL_SWDPIO0;
    io.write_register(E1000_CTRL, ctrl)
}

/// upstream: e1000_82542.c e1000_led_off_82542()
pub fn led_off_82542<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let ctrl = (io.read_register(E1000_CTRL)? & !E1000_CTRL_SWDPIN0) | E1000_CTRL_SWDPIO0;
    io.write_register(E1000_CTRL, ctrl)
}

/// upstream: e1000_82542.c e1000_rar_set_82542()
pub fn rar_set_82542<I: E1000RegisterIo>(io: &mut I, addr: [u8; 6], index: u32) -> DevResult {
    let low = u32::from(addr[0])
        | (u32::from(addr[1]) << 8)
        | (u32::from(addr[2]) << 16)
        | (u32::from(addr[3]) << 24);
    let mut high = u32::from(addr[4]) | (u32::from(addr[5]) << 8);
    if low != 0 || high != 0 {
        high |= E1000_RAH_AV;
    }
    io.write_register(E1000_RA + (index << 3), low)?;
    io.write_register(E1000_RA + (index << 3) + 4, high)
}

/// upstream: e1000_82542.c e1000_translate_register_82542()
pub const fn translate_register_82542(reg: u32) -> u32 {
    match reg {
        0x05400 => 0x00040,
        0x02820 => 0x00108,
        0x02800 => 0x00110,
        0x02804 => 0x00114,
        0x02808 => 0x00118,
        0x02810 => 0x00120,
        0x02818 => 0x00128,
        0x02900 => 0x00138,
        0x02904 => 0x0013c,
        0x02908 => 0x00140,
        0x02910 => 0x00148,
        0x02918 => 0x00150,
        0x02168 => 0x00160,
        0x02160 => 0x00168,
        0x05200 => 0x00200,
        0x03800 => 0x00420,
        0x03804 => 0x00424,
        0x03808 => 0x00428,
        0x03810 => 0x00430,
        0x03818 => 0x00438,
        0x03820 => 0x00440,
        0x05600 => 0x00600,
        0x03410 => 0x08010,
        0x03418 => 0x08018,
        _ => reg,
    }
}

/// upstream: e1000_82542.c e1000_clear_hw_cntrs_82542()
pub fn clear_hw_cntrs_82542<I: E1000RegisterIo, B: FnMut() -> DevResult>(
    io: &mut I,
    mut clear_base: B,
) -> DevResult {
    clear_base()?;
    for reg in [
        0x405c, 0x4060, 0x4064, 0x4068, 0x406c, 0x4070, 0x40d8, 0x40dc, 0x40e0, 0x40e4, 0x40e8,
        0x40ec,
    ] {
        let _ = io.read_register(reg)?;
    }
    Ok(())
}

/// upstream: e1000_82542.c e1000_read_mac_addr_82542()
pub fn read_mac_addr_82542<N: E1000NvmAccess>(nvm: &mut N) -> DevResult<[u8; 6]> {
    let mut addr = [0u8; 6];
    for i in (0..6).step_by(2) {
        let word = nvm
            .read_nvm_words((i / 2) as u16, 1)?
            .first()
            .copied()
            .ok_or(DevError::Io)?;
        addr[i] = word as u8;
        addr[i + 1] = (word >> 8) as u8;
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Io {
        regs: alloc::vec::Vec<(u32, u32)>,
        writes: alloc::vec::Vec<(u32, u32)>,
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, r: u32) -> DevResult<u32> {
            Ok(self
                .regs
                .iter()
                .find(|(x, _)| *x == r)
                .map(|(_, v)| *v)
                .unwrap_or(0))
        }
        fn write_register(&mut self, r: u32, v: u32) -> DevResult {
            self.writes.push((r, v));
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {}
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    struct Ops;
    impl InitOps82542 for Ops {
        fn clear_vfta(&mut self) -> DevResult {
            Ok(())
        }
        fn init_rx_addrs(&mut self, _: u16) -> DevResult {
            Ok(())
        }
        fn clear_counters(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_link(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_physical_interface(&mut self) -> DevResult {
            Ok(())
        }
        fn set_default_flow_control(&mut self, fc: &mut FlowControl82542) -> DevResult {
            *fc = FlowControl82542::Full;
            Ok(())
        }
        fn set_flow_control_watermarks(&mut self) -> DevResult {
            Ok(())
        }
    }
    #[test]
    fn old_mac_register_aliases_rar_and_pause_quirks_match_source() {
        assert_eq!(translate_register_82542(0x02818), 0x00128);
        assert_eq!(translate_register_82542(0xdead), 0xdead);
        assert_eq!(init_nvm_params_82542().word_size, 64);
        assert_eq!(init_mac_params_82542().media, E1000MediaType::Fiber);
        let mut io = Io::default();
        rar_set_82542(&mut io, [1, 2, 3, 4, 5, 6], 2).unwrap();
        assert!(io.writes.contains(&(E1000_RA + 16, 0x0403_0201)));
        assert!(io.writes.contains(&(E1000_RA + 20, 0x8000_0605)));
        let mut req = FlowControl82542::Full;
        let mut cur = FlowControl82542::None;
        setup_link_82542(&mut io, &mut Ops, 2, false, &mut req, &mut cur, 0x1234).unwrap();
        assert_eq!(req, FlowControl82542::RxPause);
        assert_eq!(cur, req);
        assert!(io.writes.contains(&(E1000_FCAL, 0x00c2_8001)));
        assert!(io.writes.contains(&(E1000_FCTTV, 0x1234)));
    }
}
