//! FreeBSD IGC generic MAC operations and register algorithms.
//!
//! Translated from FreeBSD `sys/dev/igc/igc_mac.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright 2021 Intel Corp; Copyright 2021 Rubicon Communications, LLC (Netgate).

use alloc::vec::Vec;

use super::api::{IgcApiCallback, IgcHardware, IgcMacOps};

const MANC: u32 = 0x05820;
const MANC_RCV_TCO_EN: u32 = 0x0002_0000;
const VFTA: u32 = 0x05600;
const RAL0: u32 = 0x05400;
const RAH0: u32 = 0x05404;
const RAH_AV: u32 = 0x8000_0000;
const VLAN_FILTER_TBL_SIZE: u32 = 128;
const ALT_MAC_PTR: u16 = 0x37;
const NVM_COMPAT: u16 = 3;
const ALT_MAC_LAN1_OFFSET: u16 = 3;
const CTRL: u32 = 0;
const TCTL: u32 = 0x00400;
const CRCERRS: u32 = 0x04000;
const RXERRC: u32 = 0x0400c;
const MPC: u32 = 0x04010;
const SCC: u32 = 0x04014;
const ECOL: u32 = 0x04018;
const MCC: u32 = 0x0401c;
const LATECOL: u32 = 0x04020;
const COLC: u32 = 0x04028;
const RERC: u32 = 0x0402c;
const DC: u32 = 0x04030;
const RLEC: u32 = 0x04040;
const XONRXC: u32 = 0x04048;
const XONTXC: u32 = 0x0404c;
const XOFFRXC: u32 = 0x04050;
const XOFFTXC: u32 = 0x04054;
const FCRUC: u32 = 0x04058;
const GPRC: u32 = 0x04074;
const BPRC: u32 = 0x04078;
const MPRC: u32 = 0x0407c;
const GPTC: u32 = 0x04080;
const GORCL: u32 = 0x04088;
const GORCH: u32 = 0x0408c;
const GOTCL: u32 = 0x04090;
const GOTCH: u32 = 0x04094;
const RNBC: u32 = 0x040a0;
const RUC: u32 = 0x040a4;
const RFC: u32 = 0x040a8;
const ROC: u32 = 0x040ac;
const RJC: u32 = 0x040b0;
const TORL: u32 = 0x040c0;
const TORH: u32 = 0x040c4;
const TOTL: u32 = 0x040c8;
const TOTH: u32 = 0x040cc;
const TPR: u32 = 0x040d0;
const TPT: u32 = 0x040d4;
const MPTC: u32 = 0x040f0;
const BPTC: u32 = 0x040f4;
const TLPIC: u32 = 0x04148;
const RLPIC: u32 = 0x0414c;
const RXDMTC: u32 = 0x04120;
const CTRL_TFCE: u32 = 0x1000_0000;
const CTRL_RFCE: u32 = 0x0800_0000;
const FCRTL_XONE: u32 = 0x8000_0000;
const TCTL_COLD: u32 = 0x003f_0000;
const COLD_SHIFT: u32 = 12;
const COLLISION_DISTANCE: u32 = 63;
const FCT: u32 = 0x00030;
const FCAH: u32 = 0x00034;
const FCAL: u32 = 0x00028;
const FCTTV: u32 = 0x00170;
const FCRTL: u32 = 0x02160;
const FCRTH: u32 = 0x02168;
const FLOW_CONTROL_TYPE: u32 = 0x8808;
const FLOW_CONTROL_ADDRESS_HIGH: u32 = 0x100;
const FLOW_CONTROL_ADDRESS_LOW: u32 = 0x00c2_8001;
const STATUS: u32 = 0x00008;
const EECD: u32 = 0x00010;
const SWSM: u32 = 0x05b50;
const CTRL_GIO_MASTER_DISABLE: u32 = 0x4;
const STATUS_GIO_MASTER_ENABLE: u32 = 0x0008_0000;
const EECD_AUTO_RD: u32 = 0x200;
const SWSM_SMBI: u32 = 1;
const SWSM_SWESMBI: u32 = 2;
const STATUS_SPEED_100: u32 = 0x40;
const STATUS_SPEED_1000: u32 = 0x80;
const STATUS_SPEED_2500: u32 = 0x0040_0000;
const STATUS_FD: u32 = 1;
const PHY_STATUS: u16 = 1;
const PHY_AUTONEG_ADV: u16 = 4;
const PHY_LP_ABILITY: u16 = 5;
const MII_SR_AUTONEG_COMPLETE: u16 = 0x20;
const NWAY_AR_PAUSE: u16 = 0x0400;
const NWAY_AR_ASM_DIR: u16 = 0x0800;
const NWAY_LPAR_PAUSE: u16 = 0x0400;
const NWAY_LPAR_ASM_DIR: u16 = 0x0800;

pub trait IgcMacIo {
    fn read(&mut self, reg: u32) -> u32;
    fn write(&mut self, reg: u32, value: u32);
    fn write_flush(&mut self);
    fn read_nvm_word(&mut self, offset: u16) -> Result<u16, MacError>;
    fn nvm_word_size(&self) -> u32;
    fn mac_type_i225(&self) -> bool;
    fn check_reset_block(&mut self) -> Result<bool, MacError>;
    fn rar_set(&mut self, address: [u8; 6], index: u32) -> Result<(), MacError>;
    fn check_link(&mut self) -> Result<bool, MacError>;
    fn check_downshift(&mut self);
    fn setup_physical_interface(&mut self) -> Result<(), MacError>;
    fn phy_read(&mut self, reg: u16) -> Result<u16, MacError>;
    fn get_speed_duplex(&mut self) -> Result<(u16, u16), MacError>;
    fn acquire_hw_semaphore(&mut self) -> Result<(), MacError>;
    fn put_hw_semaphore(&mut self);
    fn auto_read_done(&mut self) -> Result<(), MacError>;
    fn disable_pcie_master(&mut self) -> Result<(), MacError>;
    fn delay_us(&mut self, us: u32);
    fn delay_ms(&mut self, ms: u32);
    fn debug(&mut self, _message: &'static str) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacError {
    Bounds,
    Io,
    Timeout,
    Config,
    Sync,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowMode {
    None,
    RxPause,
    TxPause,
    Full,
    Default,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowControl {
    pub requested: FlowMode,
    pub current: FlowMode,
    pub pause_time: u16,
    pub low_water: u32,
    pub high_water: u32,
    pub send_xon: bool,
}
impl Default for FlowControl {
    fn default() -> Self {
        Self {
            requested: FlowMode::Default,
            current: FlowMode::Default,
            pause_time: 0x0680,
            low_water: 0,
            high_water: 0,
            send_xon: false,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacState {
    pub address: [u8; 6],
    pub permanent_address: [u8; 6],
    pub asf_firmware_present: bool,
    pub rar_entry_count: u16,
    pub mta_register_count: u16,
    pub uta_register_count: u16,
    pub mc_filter_type: u8,
    pub mta_shadow: Vec<u32>,
    pub get_link_status: bool,
    pub autoneg: bool,
    pub flow: FlowControl,
}
impl Default for MacState {
    fn default() -> Self {
        Self {
            address: [0; 6],
            permanent_address: [0; 6],
            asf_firmware_present: false,
            rar_entry_count: 0,
            mta_register_count: 0,
            uta_register_count: 0,
            mc_filter_type: 0,
            mta_shadow: Vec::new(),
            get_link_status: false,
            autoneg: true,
            flow: FlowControl::default(),
        }
    }
}

// upstream: igc_mac.c igc_init_mac_ops_generic()
pub fn igc_init_mac_ops_generic(hw: &mut IgcHardware) {
    hw.mac_ops = IgcMacOps {
        init_params: Some(IgcApiCallback::MacNullOpsGeneric),
        check_for_link: None,
        clear_vfta: None,
        get_bus_info: None,
        get_link_up_info: None,
        update_mc_addr_list: None,
        reset_hw: None,
        init_hw: None,
        setup_link: None,
        setup_physical_interface: None,
        write_vfta: None,
        config_collision_dist: Some(IgcApiCallback::MacCollisionDistGeneric),
        rar_set: Some(IgcApiCallback::MacRarSetGeneric),
        read_mac_addr: None,
        validate_mdi_setting: None,
    };
}
// upstream: igc_mac.c igc_null_ops_generic()
pub fn igc_null_ops_generic() -> Result<(), MacError> {
    Ok(())
}
// upstream: igc_mac.c igc_null_mac_generic()
pub fn igc_null_mac_generic() {}
// upstream: igc_mac.c igc_null_link_info()
pub fn igc_null_link_info() -> Result<(u16, u16), MacError> {
    Ok((0, 0))
}
// upstream: igc_mac.c igc_null_mng_mode()
pub fn igc_null_mng_mode() -> bool {
    false
}
// upstream: igc_mac.c igc_enable_mng_pass_thru()
pub fn igc_enable_mng_pass_thru<I: IgcMacIo>(io: &mut I, asf_firmware_present: bool) -> bool {
    asf_firmware_present && io.read(MANC) & MANC_RCV_TCO_EN != 0
}
// upstream: igc_mac.c igc_null_update_mc()
pub fn igc_null_update_mc() {}
// upstream: igc_mac.c igc_null_write_vfta()
pub fn igc_null_write_vfta() {}
// upstream: igc_mac.c igc_null_rar_set()
pub fn igc_null_rar_set() -> Result<(), MacError> {
    Ok(())
}
// upstream: igc_mac.c igc_set_lan_id_single_port()
pub fn igc_set_lan_id_single_port(bus_function: &mut u8) {
    *bus_function = 0;
}

// upstream: igc_mac.c igc_clear_vfta_generic()
pub fn igc_clear_vfta_generic<I: IgcMacIo>(io: &mut I) {
    for offset in 0..VLAN_FILTER_TBL_SIZE {
        io.write(VFTA + offset * 4, 0);
        io.write_flush();
    }
}
// upstream: igc_mac.c igc_write_vfta_generic()
pub fn igc_write_vfta_generic<I: IgcMacIo>(io: &mut I, offset: u32, value: u32) {
    io.write(VFTA + offset * 4, value);
    io.write_flush();
}
// upstream: igc_mac.c igc_init_rx_addrs_generic()
pub fn igc_init_rx_addrs_generic<I: IgcMacIo>(
    io: &mut I,
    address: [u8; 6],
    rar_count: u16,
) -> Result<(), MacError> {
    io.rar_set(address, 0)?;
    for index in 1..rar_count {
        io.rar_set([0; 6], u32::from(index))?;
    }
    Ok(())
}
// upstream: igc_mac.c igc_check_alt_mac_addr_generic()
pub fn igc_check_alt_mac_addr_generic<I: IgcMacIo>(
    io: &mut I,
    bus_function: u8,
) -> Result<(), MacError> {
    let _compat = io.read_nvm_word(NVM_COMPAT)?;
    let mut offset = io.read_nvm_word(ALT_MAC_PTR)?;
    if offset == u16::MAX || offset == 0 {
        return Ok(());
    }
    if bus_function == 1 {
        offset = offset.wrapping_add(ALT_MAC_LAN1_OFFSET)
    }
    let mut address = [0u8; 6];
    for i in (0..6).step_by(2) {
        let word = io.read_nvm_word(offset.wrapping_add((i / 2) as u16))?;
        address[i] = (word & 0xff) as u8;
        address[i + 1] = (word >> 8) as u8;
    }
    if address[0] & 1 != 0 {
        io.debug("Ignoring Alternate Mac Address with MC bit set");
        return Ok(());
    }
    io.rar_set(address, 0)
}
// upstream: igc_mac.c igc_rar_set_generic()
pub fn igc_rar_set_generic<I: IgcMacIo>(
    io: &mut I,
    address: [u8; 6],
    index: u32,
) -> Result<(), MacError> {
    let rar_low = u32::from(address[0])
        | (u32::from(address[1]) << 8)
        | (u32::from(address[2]) << 16)
        | (u32::from(address[3]) << 24);
    let mut rar_high = u32::from(address[4]) | (u32::from(address[5]) << 8);
    if rar_low != 0 || rar_high != 0 {
        rar_high |= RAH_AV;
    }
    io.write(RAL0 + index * 8, rar_low);
    io.write_flush();
    io.write(RAH0 + index * 8, rar_high);
    io.write_flush();
    Ok(())
}
// upstream: igc_mac.c igc_hash_mc_addr_generic()
pub fn igc_hash_mc_addr_generic(
    mc_addr: [u8; 6],
    mta_register_count: u16,
    mc_filter_type: u8,
) -> u32 {
    let hash_mask = (u32::from(mta_register_count) * 32) - 1;
    let mut bit_shift = 1u8;
    while bit_shift < 4 && hash_mask >> bit_shift != 0xff {
        bit_shift += 1;
    }
    match mc_filter_type {
        1 => bit_shift += 1,
        2 => bit_shift += 2,
        3 => bit_shift += 4,
        _ => {}
    }
    let mut hash_value = u32::from(mc_addr[4]);
    hash_value >>= 8 - bit_shift;
    hash_value |= u32::from(mc_addr[5]) << bit_shift;
    hash_value &= hash_mask;
    hash_value
}
// upstream: igc_mac.c igc_update_mc_addr_list_generic()
pub fn igc_update_mc_addr_list_generic<I: IgcMacIo>(
    io: &mut I,
    state: &mut MacState,
    packed_addresses: &[u8],
    count: u32,
) -> Result<(), MacError> {
    if state.mta_shadow.len() < usize::from(state.mta_register_count)
        || packed_addresses.len() < count as usize * 6
    {
        return Err(MacError::Bounds);
    }
    state.mta_shadow[..usize::from(state.mta_register_count)].fill(0);
    for address in packed_addresses.chunks_exact(6).take(count as usize) {
        let address: [u8; 6] = address.try_into().unwrap();
        let hash =
            igc_hash_mc_addr_generic(address, state.mta_register_count, state.mc_filter_type);
        let register = ((hash >> 5) & u32::from(state.mta_register_count - 1)) as usize;
        let bit = hash & 0x1f;
        state.mta_shadow[register] |= 1u32 << bit;
    }
    for index in (0..state.mta_register_count).rev() {
        io.write(
            0x05200 + u32::from(index) * 4,
            state.mta_shadow[usize::from(index)],
        );
    }
    io.write_flush();
    Ok(())
}
// upstream: igc_mac.c igc_clear_hw_cntrs_base_generic()
pub fn igc_clear_hw_cntrs_base_generic<I: IgcMacIo>(io: &mut I) {
    const COUNTERS: [u32; 40] = [
        CRCERRS, RXERRC, MPC, SCC, ECOL, MCC, LATECOL, COLC, RERC, DC, RLEC, XONRXC, XONTXC,
        XOFFRXC, XOFFTXC, FCRUC, GPRC, BPRC, MPRC, GPTC, GORCL, GORCH, GOTCL, GOTCH, RNBC, RUC,
        RFC, ROC, RJC, TORL, TORH, TOTL, TOTH, TPR, TPT, MPTC, BPTC, TLPIC, RLPIC, RXDMTC,
    ];
    for register in COUNTERS {
        let _ = io.read(register);
    }
}

// upstream: igc_mac.c igc_check_for_copper_link_generic()
pub fn igc_check_for_copper_link_generic<I: IgcMacIo>(
    io: &mut I,
    state: &mut MacState,
) -> Result<(), MacError> {
    if !state.get_link_status {
        return Ok(());
    }
    let link = io.check_link()?;
    if !link {
        return Ok(());
    }
    state.get_link_status = false;
    io.check_downshift();
    if !state.autoneg {
        return Err(MacError::Config);
    }
    igc_config_collision_dist_generic(io);
    igc_config_fc_after_link_up_generic(io, state)
}
// upstream: igc_mac.c igc_setup_link_generic()
pub fn igc_setup_link_generic<I: IgcMacIo>(
    io: &mut I,
    state: &mut MacState,
) -> Result<(), MacError> {
    if io.check_reset_block()? {
        return Ok(());
    }
    if state.flow.requested == FlowMode::Default {
        state.flow.requested = FlowMode::Full
    }
    state.flow.current = state.flow.requested;
    io.setup_physical_interface()?;
    io.write(FCT, FLOW_CONTROL_TYPE);
    io.write(FCAH, FLOW_CONTROL_ADDRESS_HIGH);
    io.write(FCAL, FLOW_CONTROL_ADDRESS_LOW);
    io.write(FCTTV, u32::from(state.flow.pause_time));
    igc_set_fc_watermarks_generic(io, state)
}
// upstream: igc_mac.c igc_config_collision_dist_generic()
pub fn igc_config_collision_dist_generic<I: IgcMacIo>(io: &mut I) {
    let mut v = io.read(TCTL);
    v &= !TCTL_COLD;
    v |= COLLISION_DISTANCE << COLD_SHIFT;
    io.write(TCTL, v);
    io.write_flush();
}
// upstream: igc_mac.c igc_set_fc_watermarks_generic()
pub fn igc_set_fc_watermarks_generic<I: IgcMacIo>(
    io: &mut I,
    state: &MacState,
) -> Result<(), MacError> {
    let (mut low, mut high) = (0, 0);
    if matches!(state.flow.current, FlowMode::TxPause | FlowMode::Full) {
        low = state.flow.low_water;
        if state.flow.send_xon {
            low |= FCRTL_XONE
        }
        high = state.flow.high_water
    }
    io.write(FCRTL, low);
    io.write(FCRTH, high);
    Ok(())
}
// upstream: igc_mac.c igc_force_mac_fc_generic()
pub fn igc_force_mac_fc_generic<I: IgcMacIo>(io: &mut I, state: &MacState) -> Result<(), MacError> {
    let mut ctrl = io.read(CTRL);
    match state.flow.current {
        FlowMode::None => ctrl &= !(CTRL_TFCE | CTRL_RFCE),
        FlowMode::RxPause => {
            ctrl &= !CTRL_TFCE;
            ctrl |= CTRL_RFCE
        }
        FlowMode::TxPause => {
            ctrl &= !CTRL_RFCE;
            ctrl |= CTRL_TFCE
        }
        FlowMode::Full => ctrl |= CTRL_TFCE | CTRL_RFCE,
        FlowMode::Default => return Err(MacError::Config),
    }
    io.write(CTRL, ctrl);
    Ok(())
}
// upstream: igc_mac.c igc_config_fc_after_link_up_generic()
pub fn igc_config_fc_after_link_up_generic<I: IgcMacIo>(
    io: &mut I,
    state: &mut MacState,
) -> Result<(), MacError> {
    if !state.autoneg {
        return Ok(());
    }
    let _ = io.phy_read(PHY_STATUS)?;
    let status = io.phy_read(PHY_STATUS)?;
    if status & MII_SR_AUTONEG_COMPLETE == 0 {
        return Ok(());
    }
    let local = io.phy_read(PHY_AUTONEG_ADV)?;
    let partner = io.phy_read(PHY_LP_ABILITY)?;
    state.flow.current = if local & NWAY_AR_PAUSE != 0 && partner & NWAY_LPAR_PAUSE != 0 {
        if state.flow.requested == FlowMode::Full {
            FlowMode::Full
        } else {
            FlowMode::RxPause
        }
    } else if local & NWAY_AR_PAUSE == 0
        && local & NWAY_AR_ASM_DIR != 0
        && partner & NWAY_LPAR_PAUSE != 0
        && partner & NWAY_LPAR_ASM_DIR != 0
    {
        FlowMode::TxPause
    } else if local & NWAY_AR_PAUSE != 0
        && local & NWAY_AR_ASM_DIR != 0
        && partner & NWAY_LPAR_PAUSE == 0
        && partner & NWAY_LPAR_ASM_DIR != 0
    {
        FlowMode::RxPause
    } else {
        FlowMode::None
    };
    let (_speed, duplex) = io.get_speed_duplex()?;
    if duplex == 0 {
        state.flow.current = FlowMode::None
    }
    igc_force_mac_fc_generic(io, state)
}
// upstream: igc_mac.c igc_get_speed_and_duplex_copper_generic()
pub fn igc_get_speed_and_duplex_copper_generic<I: IgcMacIo>(io: &mut I) -> (u16, u16) {
    let s = io.read(STATUS);
    let speed = if s & STATUS_SPEED_1000 != 0 {
        if io.mac_type_i225() && s & STATUS_SPEED_2500 != 0 {
            2500
        } else {
            1000
        }
    } else if s & STATUS_SPEED_100 != 0 {
        100
    } else {
        10
    };
    (speed, if s & STATUS_FD != 0 { 1 } else { 0 })
}
// upstream: igc_mac.c igc_get_hw_semaphore_generic()
pub fn igc_get_hw_semaphore_generic<I: IgcMacIo>(io: &mut I) -> Result<(), MacError> {
    let timeout = io.nvm_word_size() + 1;
    let mut i = 0;
    while i < timeout {
        if io.read(SWSM) & SWSM_SMBI == 0 {
            break;
        }
        io.delay_us(50);
        i += 1
    }
    if i == timeout {
        return Err(MacError::Sync);
    }
    for attempt in 0..timeout {
        let v = io.read(SWSM);
        io.write(SWSM, v | SWSM_SWESMBI);
        if io.read(SWSM) & SWSM_SWESMBI != 0 {
            return Ok(());
        }
        io.delay_us(50);
        if attempt + 1 == timeout {
            io.put_hw_semaphore();
            return Err(MacError::Sync);
        }
    }
    Err(MacError::Sync)
}
// upstream: igc_mac.c igc_put_hw_semaphore_generic()
pub fn igc_put_hw_semaphore_generic<I: IgcMacIo>(io: &mut I) {
    let v = io.read(SWSM) & !(SWSM_SMBI | SWSM_SWESMBI);
    io.write(SWSM, v)
}
// upstream: igc_mac.c igc_get_auto_rd_done_generic()
pub fn igc_get_auto_rd_done_generic<I: IgcMacIo>(io: &mut I) -> Result<(), MacError> {
    for _ in 0..10 {
        if io.read(EECD) & EECD_AUTO_RD != 0 {
            return Ok(());
        }
        io.delay_ms(1)
    }
    Err(MacError::Timeout)
}
// upstream: igc_mac.c igc_disable_pcie_master_generic()
pub fn igc_disable_pcie_master_generic<I: IgcMacIo>(io: &mut I) -> Result<(), MacError> {
    let ctrl = io.read(CTRL) | CTRL_GIO_MASTER_DISABLE;
    io.write(CTRL, ctrl);
    for _ in 0..800 {
        if io.read(STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
            return Ok(());
        }
        io.delay_us(100)
    }
    Err(MacError::Timeout)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    #[derive(Default)]
    struct Fake {
        regs: Vec<(u32, u32)>,
        writes: Vec<(u32, u32)>,
        nvm: Vec<u16>,
        rars: Vec<([u8; 6], u32)>,
        flushes: usize,
        reads: Vec<u32>,
        phy: Vec<u16>,
        downshifts: usize,
        setup_count: usize,
    }
    impl Fake {
        fn get(&self, r: u32) -> u32 {
            self.regs.iter().rev().find(|v| v.0 == r).map_or(0, |v| v.1)
        }
        fn set(&mut self, r: u32, v: u32) {
            if let Some(x) = self.regs.iter_mut().find(|x| x.0 == r) {
                x.1 = v
            } else {
                self.regs.push((r, v))
            }
        }
    }
    impl IgcMacIo for Fake {
        fn read(&mut self, r: u32) -> u32 {
            self.reads.push(r);
            self.get(r)
        }
        fn write(&mut self, r: u32, v: u32) {
            self.writes.push((r, v));
            self.set(r, v)
        }
        fn write_flush(&mut self) {
            self.flushes += 1
        }
        fn read_nvm_word(&mut self, o: u16) -> Result<u16, MacError> {
            self.nvm
                .get(usize::from(o))
                .copied()
                .ok_or(MacError::Bounds)
        }
        fn nvm_word_size(&self) -> u32 {
            64
        }
        fn mac_type_i225(&self) -> bool {
            true
        }
        fn check_reset_block(&mut self) -> Result<bool, MacError> {
            Ok(false)
        }
        fn rar_set(&mut self, a: [u8; 6], i: u32) -> Result<(), MacError> {
            self.rars.push((a, i));
            Ok(())
        }
        fn check_link(&mut self) -> Result<bool, MacError> {
            Ok(true)
        }
        fn check_downshift(&mut self) {
            self.downshifts += 1
        }
        fn setup_physical_interface(&mut self) -> Result<(), MacError> {
            self.setup_count += 1;
            Ok(())
        }
        fn phy_read(&mut self, _: u16) -> Result<u16, MacError> {
            if self.phy.is_empty() {
                Ok(0)
            } else {
                Ok(self.phy.remove(0))
            }
        }
        fn get_speed_duplex(&mut self) -> Result<(u16, u16), MacError> {
            Ok((1000, 1))
        }
        fn acquire_hw_semaphore(&mut self) -> Result<(), MacError> {
            Ok(())
        }
        fn put_hw_semaphore(&mut self) {}
        fn auto_read_done(&mut self) -> Result<(), MacError> {
            Ok(())
        }
        fn disable_pcie_master(&mut self) -> Result<(), MacError> {
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {}
        fn delay_ms(&mut self, _: u32) {}
    }
    #[test]
    fn rar_vlan_and_counter_helpers_keep_write_and_clear_order() {
        let mut f = Fake::default();
        igc_rar_set_generic(&mut f, [1, 2, 3, 4, 5, 6], 0).unwrap();
        assert_eq!(f.writes, [(RAL0, 0x0403_0201), (RAH0, RAH_AV | 0x0605)]);
        assert_eq!(f.flushes, 2);
        f.writes.clear();
        igc_clear_vfta_generic(&mut f);
        assert_eq!(f.writes.len(), 128);
        assert_eq!(f.flushes, 130);
        f.reads.clear();
        igc_clear_hw_cntrs_base_generic(&mut f);
        assert_eq!(f.reads.len(), 40);
        assert_eq!(f.reads[0], CRCERRS);
        assert_eq!(*f.reads.last().unwrap(), RXDMTC);
    }
    #[test]
    fn multicast_hash_and_shadow_write_match_generic_algorithm() {
        let addr = [1, 0xaa, 0, 0x12, 0x34, 0x56];
        assert_eq!(igc_hash_mc_addr_generic(addr, 128, 0), 0x563);
        let mut state = MacState {
            mta_register_count: 128,
            mta_shadow: vec![0; 128],
            ..MacState::default()
        };
        let mut f = Fake::default();
        igc_update_mc_addr_list_generic(&mut f, &mut state, &addr, 1).unwrap();
        assert_eq!(state.mta_shadow[43], 1 << 3);
        assert_eq!(f.writes.first().unwrap().0, 0x05200 + 127 * 4);
        assert_eq!(f.writes.last().unwrap().0, 0x05200);
        assert_eq!(f.flushes, 1);
    }
    #[test]
    fn alternate_mac_uses_second_lane_offset_and_ignores_multicast_address() {
        let mut f = Fake {
            nvm: vec![0; 128],
            ..Fake::default()
        };
        f.nvm[usize::from(ALT_MAC_PTR)] = 0x37;
        f.nvm[0x3a] = 0x0302;
        f.nvm[0x3b] = 0x0504;
        f.nvm[0x3c] = 0x0706;
        igc_check_alt_mac_addr_generic(&mut f, 1).unwrap();
        assert_eq!(f.rars, [([2, 3, 4, 5, 6, 7], 0)]);
        f.rars.clear();
        f.nvm[0x3a] = 0x0303;
        igc_check_alt_mac_addr_generic(&mut f, 1).unwrap();
        assert!(f.rars.is_empty());
    }

    #[test]
    fn link_fc_negotiation_register_setup_and_generic_timeouts_follow_source() {
        let mut io = Fake {
            phy: vec![0, MII_SR_AUTONEG_COMPLETE, NWAY_AR_PAUSE, NWAY_LPAR_PAUSE],
            ..Fake::default()
        };
        let mut state = MacState {
            get_link_status: true,
            flow: FlowControl {
                requested: FlowMode::Full,
                current: FlowMode::Full,
                ..FlowControl::default()
            },
            ..MacState::default()
        };
        igc_check_for_copper_link_generic(&mut io, &mut state).unwrap();
        assert!(!state.get_link_status);
        assert_eq!(io.downshifts, 1);
        assert!(io.get(TCTL) & TCTL_COLD != 0);
        assert_eq!(
            io.get(CTRL) & (CTRL_TFCE | CTRL_RFCE),
            CTRL_TFCE | CTRL_RFCE
        );
        state.flow = FlowControl {
            requested: FlowMode::Default,
            current: FlowMode::Default,
            pause_time: 0x1234,
            low_water: 0x100,
            high_water: 0x200,
            send_xon: true,
        };
        igc_setup_link_generic(&mut io, &mut state).unwrap();
        assert_eq!(state.flow.current, FlowMode::Full);
        assert_eq!(io.setup_count, 1);
        assert_eq!(io.get(FCTTV), 0x1234);
        assert_eq!(io.get(FCRTL), 0x8000_0100);
        assert_eq!(io.get(FCRTH), 0x200);
        assert_eq!(igc_get_speed_and_duplex_copper_generic(&mut io), (10, 0));
        io.set(STATUS, STATUS_SPEED_1000 | STATUS_SPEED_2500 | STATUS_FD);
        assert_eq!(igc_get_speed_and_duplex_copper_generic(&mut io), (2500, 1));
        io.set(EECD, EECD_AUTO_RD);
        assert!(igc_get_auto_rd_done_generic(&mut io).is_ok());
        assert!(igc_disable_pcie_master_generic(&mut io).is_ok());
        assert_eq!(
            io.get(CTRL) & CTRL_GIO_MASTER_DISABLE,
            CTRL_GIO_MASTER_DISABLE
        );
    }
}
