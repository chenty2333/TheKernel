//! FreeBSD IGC PHY operations and MDIC/MDIO algorithms.
//!
//! Translated from FreeBSD `sys/dev/igc/igc_phy.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright 2021 Intel Corp; Copyright 2021 Rubicon Communications, LLC (Netgate).

use super::{
    api::{IgcApiCallback, IgcHardware, IgcPhyOps},
    mac::FlowMode,
};

const MANC: u32 = 0x05820;
const MANC_BLK_PHY_RST_ON_IDE: u32 = 0x0000_0400;
const MDIC: u32 = 0x00020;
const MDIC_DATA_MASK: u32 = 0xffff;
const MDIC_REG_MASK: u32 = 0x001f_0000;
const MDIC_REG_SHIFT: u32 = 16;
const MDIC_PHY_SHIFT: u32 = 21;
const MDIC_OP_WRITE: u32 = 0x0400_0000;
const MDIC_OP_READ: u32 = 0x0800_0000;
const MDIC_READY: u32 = 0x1000_0000;
const MDIC_ERROR: u32 = 0x4000_0000;
const MAX_PHY_REG_ADDRESS: u32 = 0x1f;
const GEN_POLL_TIMEOUT: u32 = 64;
const PHY_ID1: u16 = 2;
const PHY_ID2: u16 = 3;
const PHY_REVISION_MASK: u16 = 0x000f;
const PHY_STATUS: u16 = 1;
const PHY_AUTONEG_ADV: u16 = 4;
const PHY_1000T_CTRL: u16 = 9;
const PHY_CONTROL: u16 = 0;
const MMD_DEVADDR_SHIFT: u32 = 16;
const ANEG_MULTIGBT_AN_CTRL: u16 = 0x20;
const STANDARD_AN_REG_MASK: u32 = 0x0d;
const ADV_10_HALF: u16 = 1;
const ADV_10_FULL: u16 = 2;
const ADV_100_HALF: u16 = 4;
const ADV_100_FULL: u16 = 8;
const ADV_1000_HALF: u16 = 0x10;
const ADV_1000_FULL: u16 = 0x20;
const ADV_2500_HALF: u16 = 0x40;
const ADV_2500_FULL: u16 = 0x80;
const ALL_SPEED_DUPLEX: u16 = 0x002f;
const ALL_NOT_GIG: u16 = 0x000f;
const ALL_10_SPEED: u16 = ADV_10_HALF | ADV_10_FULL;
const ALL_HALF_DUPLEX: u16 = ADV_10_HALF | ADV_100_HALF;
const ALL_100_SPEED: u16 = ADV_100_HALF | ADV_100_FULL;
const NWAY_10_HD: u16 = 0x20;
const NWAY_10_FD: u16 = 0x40;
const NWAY_100_HD: u16 = 0x80;
const NWAY_100_FD: u16 = 0x100;
const CR_1000T_HD: u16 = 0x100;
const CR_1000T_FD: u16 = 0x200;
const CR_2500T_FD: u16 = 0x80;
const NWAY_AR_ASM_DIR: u16 = 0x0800;
const NWAY_AR_PAUSE: u16 = 0x0400;
const MII_CR_FULL_DUPLEX: u16 = 0x0100;
const MII_CR_RESTART_AUTO_NEG: u16 = 0x0200;
const MII_CR_POWER_DOWN: u16 = 0x0800;
const MII_CR_AUTO_NEG_EN: u16 = 0x1000;
const MII_CR_SPEED_100: u16 = 0x2000;
const MII_CR_SPEED_1000: u16 = 0x0040;
const MII_SR_LINK_STATUS: u16 = 0x0004;
const MII_SR_AUTONEG_COMPLETE: u16 = 0x0020;
const COPPER_LINK_UP_LIMIT: u32 = 10;
const PHY_AUTO_NEG_LIMIT: u16 = 45;
const CTRL: u32 = 0;
const I225_PHPM: u32 = 0x0e14;
const CTRL_PHY_RST: u32 = 0x8000_0000;
const I225_PHPM_RST_COMPL: u32 = 0x0100;
const GPY_MMD_MASK: u32 = 0x03e0_0000;
const GPY_MMD_SHIFT: u32 = 21;
const GPY_REG_MASK: u32 = 0x001f_ffff;
const MMDAC: u16 = 0x0d;
const MMDAAD: u16 = 0x0e;
const MMDAC_FUNC_DATA: u16 = 0x4000;
const CTRL_FRCSPD: u32 = 0x800;
const CTRL_FRCDPX: u32 = 0x1000;
const CTRL_SPD_SEL: u32 = 0x300;
const CTRL_ASDE: u32 = 0x20;
const CTRL_FD: u32 = 1;
const CTRL_SPD_100: u32 = 0x100;
const CTRL_SPD_1000: u32 = 0x200;
const IGP02IGC_PHY_POWER_MGMT: u32 = 0x0c10;
const IGP02IGC_PM_D3_LPLU: u16 = 0x0004;
const IGP01IGC_PHY_PORT_CONFIG: u32 = 0x0010;
const IGP01IGC_PSCFR_SMART_SPEED: u16 = 0x0080;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhyError {
    Bounds,
    Timeout,
    Io,
    Config,
    ResetBlocked,
    Sync,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartSpeed {
    Default,
    On,
    Off,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhyState {
    pub address: u8,
    pub id: u32,
    pub revision: u32,
    pub autoneg_mask: u16,
    pub autoneg_advertised: u16,
    pub autoneg_wait_to_complete: bool,
    pub reset_delay_usec: u32,
    pub smart_speed: SmartSpeed,
    pub speed_downgraded: bool,
    pub is_i225: bool,
}
impl Default for PhyState {
    fn default() -> Self {
        Self {
            address: 0,
            id: 0,
            revision: 0,
            autoneg_mask: 0,
            autoneg_advertised: 0,
            autoneg_wait_to_complete: false,
            reset_delay_usec: 100,
            smart_speed: SmartSpeed::Default,
            speed_downgraded: false,
            is_i225: true,
        }
    }
}

pub trait IgcPhyIo {
    fn read(&mut self, reg: u32) -> u32;
    fn write(&mut self, reg: u32, value: u32);
    fn flush(&mut self);
    fn read_phy(&mut self, offset: u32) -> Result<u16, PhyError>;
    fn write_phy(&mut self, offset: u32, value: u16) -> Result<(), PhyError>;
    fn has_read_phy_callback(&self) -> bool;
    fn acquire_phy(&mut self) -> Result<(), PhyError>;
    fn release_phy(&mut self);
    fn delay_us(&mut self, us: u32);
    fn delay_us_irq(&mut self, us: u32);
    fn delay_ms(&mut self, ms: u32);
    fn mac_autoneg(&self) -> bool;
    fn flow_mode(&self) -> FlowMode;
    fn set_flow_mode(&mut self, mode: FlowMode);
    fn check_phy_reset_block(&mut self) -> Result<(), PhyError>;
    fn configure_collision_distance(&mut self);
    fn configure_flow_control(&mut self) -> Result<(), PhyError>;
    fn link_status(&mut self) -> Result<bool, PhyError>;
    fn force_speed_duplex(&mut self) -> Result<(), PhyError>;
    fn debug(&mut self, _message: &'static str) {}
}

// upstream: igc_phy.c igc_init_phy_ops_generic()
pub fn igc_init_phy_ops_generic(hw: &mut IgcHardware) {
    hw.phy_ops = IgcPhyOps {
        init_params: Some(IgcApiCallback::PhyNullOpsGeneric),
        acquire: Some(IgcApiCallback::PhyNullOpsGeneric),
        check_reset_block: Some(IgcApiCallback::PhyNullOpsGeneric),
        force_speed_duplex: Some(IgcApiCallback::PhyNullOpsGeneric),
        get_info: Some(IgcApiCallback::PhyNullOpsGeneric),
        set_page: Some(IgcApiCallback::PhyNullSetPage),
        read: Some(IgcApiCallback::PhyNullReadReg),
        read_locked: Some(IgcApiCallback::PhyNullReadReg),
        read_page: Some(IgcApiCallback::PhyNullReadReg),
        release: Some(IgcApiCallback::PhyNullGeneric),
        reset: Some(IgcApiCallback::PhyNullOpsGeneric),
        set_d0_lplu_state: Some(IgcApiCallback::PhyNullLplu),
        set_d3_lplu_state: Some(IgcApiCallback::PhyNullLplu),
        write: Some(IgcApiCallback::PhyNullWriteReg),
        write_locked: Some(IgcApiCallback::PhyNullWriteReg),
        write_page: Some(IgcApiCallback::PhyNullWriteReg),
        power_up: Some(IgcApiCallback::PhyNullGeneric),
        power_down: Some(IgcApiCallback::PhyNullGeneric),
    }
}
// upstream: igc_phy.c igc_null_set_page()
pub fn igc_null_set_page(_page: u16) -> Result<(), PhyError> {
    Ok(())
}
// upstream: igc_phy.c igc_null_read_reg()
pub fn igc_null_read_reg(_offset: u32) -> Result<u16, PhyError> {
    Ok(0)
}
// upstream: igc_phy.c igc_null_phy_generic()
pub fn igc_null_phy_generic() {}
// upstream: igc_phy.c igc_null_lplu_state()
pub fn igc_null_lplu_state(_active: bool) -> Result<(), PhyError> {
    Ok(())
}
// upstream: igc_phy.c igc_null_write_reg()
pub fn igc_null_write_reg(_offset: u32, _value: u16) -> Result<(), PhyError> {
    Ok(())
}
// upstream: igc_phy.c igc_check_reset_block_generic()
pub fn igc_check_reset_block_generic<I: IgcPhyIo>(io: &mut I) -> Result<(), PhyError> {
    if io.read(MANC) & MANC_BLK_PHY_RST_ON_IDE != 0 {
        Err(PhyError::ResetBlocked)
    } else {
        Ok(())
    }
}
// upstream: igc_phy.c igc_get_phy_id()
pub fn igc_get_phy_id<I: IgcPhyIo>(
    io: &mut I,
    phy: &mut PhyState,
    has_read_callback: bool,
) -> Result<(), PhyError> {
    if !has_read_callback {
        return Ok(());
    }
    let id1 = io.read_phy(u32::from(PHY_ID1))?;
    phy.id = u32::from(id1) << 16;
    io.delay_us(200);
    let id2 = io.read_phy(u32::from(PHY_ID2))?;
    phy.id |= u32::from(id2 & PHY_REVISION_MASK);
    phy.revision = u32::from(id2 & !PHY_REVISION_MASK);
    Ok(())
}
// upstream: igc_phy.c igc_read_phy_reg_mdic()
pub fn igc_read_phy_reg_mdic<I: IgcPhyIo>(
    io: &mut I,
    phy: &PhyState,
    offset: u32,
) -> Result<u16, PhyError> {
    if offset > MAX_PHY_REG_ADDRESS {
        return Err(PhyError::Bounds);
    }
    let command =
        (offset << MDIC_REG_SHIFT) | (u32::from(phy.address) << MDIC_PHY_SHIFT) | MDIC_OP_READ;
    io.write(MDIC, command);
    let mut mdic = 0;
    for _ in 0..GEN_POLL_TIMEOUT * 3 {
        io.delay_us_irq(50);
        mdic = io.read(MDIC);
        if mdic & MDIC_READY != 0 {
            break;
        }
    }
    if mdic & MDIC_READY == 0
        || mdic & MDIC_ERROR != 0
        || ((mdic & MDIC_REG_MASK) >> MDIC_REG_SHIFT) != offset
    {
        return Err(PhyError::Io);
    }
    Ok((mdic & MDIC_DATA_MASK) as u16)
}
// upstream: igc_phy.c igc_write_phy_reg_mdic()
pub fn igc_write_phy_reg_mdic<I: IgcPhyIo>(
    io: &mut I,
    phy: &PhyState,
    offset: u32,
    data: u16,
) -> Result<(), PhyError> {
    if offset > MAX_PHY_REG_ADDRESS {
        return Err(PhyError::Bounds);
    }
    let command = u32::from(data)
        | (offset << MDIC_REG_SHIFT)
        | (u32::from(phy.address) << MDIC_PHY_SHIFT)
        | MDIC_OP_WRITE;
    io.write(MDIC, command);
    let mut mdic = 0;
    for _ in 0..GEN_POLL_TIMEOUT * 3 {
        io.delay_us_irq(50);
        mdic = io.read(MDIC);
        if mdic & MDIC_READY != 0 {
            break;
        }
    }
    if mdic & MDIC_READY == 0
        || mdic & MDIC_ERROR != 0
        || ((mdic & MDIC_REG_MASK) >> MDIC_REG_SHIFT) != offset
    {
        return Err(PhyError::Io);
    }
    Ok(())
}

// upstream: igc_phy.c igc_phy_setup_autoneg()
pub fn igc_phy_setup_autoneg<I: IgcPhyIo>(io: &mut I, phy: &mut PhyState) -> Result<(), PhyError> {
    phy.autoneg_advertised &= phy.autoneg_mask;
    let mut adv = io.read_phy(u32::from(PHY_AUTONEG_ADV))?;
    let mut gigabit = 0;
    if phy.autoneg_mask & ADV_1000_FULL != 0 {
        gigabit = io.read_phy(u32::from(PHY_1000T_CTRL))?
    }
    let mut multigig = 0;
    if phy.autoneg_mask & ADV_2500_FULL != 0 {
        multigig = io.read_phy(
            (STANDARD_AN_REG_MASK << MMD_DEVADDR_SHIFT) | u32::from(ANEG_MULTIGBT_AN_CTRL),
        )?
    }
    adv &= !(NWAY_100_FD | NWAY_100_HD | NWAY_10_FD | NWAY_10_HD);
    gigabit &= !(CR_1000T_HD | CR_1000T_FD);
    for (advert, bit) in [
        (ADV_10_HALF, NWAY_10_HD),
        (ADV_10_FULL, NWAY_10_FD),
        (ADV_100_HALF, NWAY_100_HD),
        (ADV_100_FULL, NWAY_100_FD),
    ] {
        if phy.autoneg_advertised & advert != 0 {
            adv |= bit
        }
    }
    if phy.autoneg_advertised & ADV_1000_HALF != 0 {
        io.debug("Advertise 1000mb Half duplex request denied!")
    }
    if phy.autoneg_advertised & ADV_1000_FULL != 0 {
        gigabit |= CR_1000T_FD
    }
    if phy.autoneg_advertised & ADV_2500_HALF != 0 {
        io.debug("Advertise 2500mb Half duplex request denied!")
    }
    if phy.autoneg_advertised & ADV_2500_FULL != 0 {
        multigig |= CR_2500T_FD
    } else {
        multigig &= !CR_2500T_FD
    }
    match io.flow_mode() {
        FlowMode::None => adv &= !(NWAY_AR_ASM_DIR | NWAY_AR_PAUSE),
        FlowMode::RxPause => adv |= NWAY_AR_ASM_DIR | NWAY_AR_PAUSE,
        FlowMode::TxPause => {
            adv |= NWAY_AR_ASM_DIR;
            adv &= !NWAY_AR_PAUSE
        }
        FlowMode::Full => adv |= NWAY_AR_ASM_DIR | NWAY_AR_PAUSE,
        FlowMode::Default => return Err(PhyError::Config),
    }
    io.write_phy(u32::from(PHY_AUTONEG_ADV), adv)?;
    let mut ret_val = Ok(());
    if phy.autoneg_mask & ADV_1000_FULL != 0 {
        ret_val = io.write_phy(u32::from(PHY_1000T_CTRL), gigabit);
    }
    if phy.autoneg_mask & ADV_2500_FULL != 0 {
        ret_val = io.write_phy(
            (STANDARD_AN_REG_MASK << MMD_DEVADDR_SHIFT) | u32::from(ANEG_MULTIGBT_AN_CTRL),
            multigig,
        );
    }
    ret_val
}

// upstream: igc_phy.c igc_copper_link_autoneg()
pub fn igc_copper_link_autoneg<I: IgcPhyIo>(
    io: &mut I,
    phy: &mut PhyState,
    link_status: &mut bool,
) -> Result<(), PhyError> {
    phy.autoneg_advertised &= phy.autoneg_mask;
    if phy.autoneg_advertised == 0 {
        phy.autoneg_advertised = phy.autoneg_mask
    }
    igc_phy_setup_autoneg(io, phy)?;
    let mut ctrl = io.read_phy(u32::from(PHY_CONTROL))?;
    ctrl |= MII_CR_AUTO_NEG_EN | MII_CR_RESTART_AUTO_NEG;
    io.write_phy(u32::from(PHY_CONTROL), ctrl)?;
    if phy.autoneg_wait_to_complete {
        igc_wait_autoneg(io)?
    }
    *link_status = true;
    Ok(())
}

// upstream: igc_phy.c igc_setup_copper_link_generic()
pub fn igc_setup_copper_link_generic<I: IgcPhyIo>(
    io: &mut I,
    phy: &mut PhyState,
    link_status: &mut bool,
) -> Result<(), PhyError> {
    if io.mac_autoneg() {
        igc_copper_link_autoneg(io, phy, link_status)?
    } else {
        io.force_speed_duplex()?
    }
    let link = igc_phy_has_link_generic(io, COPPER_LINK_UP_LIMIT, 10)?;
    if link {
        io.configure_collision_distance();
        io.configure_flow_control()?;
    }
    Ok(())
}

// upstream: igc_phy.c igc_phy_force_speed_duplex_setup()
pub fn igc_phy_force_speed_duplex_setup<I: IgcPhyIo>(
    io: &mut I,
    forced_speed_duplex: u16,
    phy_control: &mut u16,
) {
    io.set_flow_mode(FlowMode::None);
    let mut ctrl = io.read(CTRL) | CTRL_FRCSPD | CTRL_FRCDPX;
    ctrl &= !CTRL_SPD_SEL;
    ctrl &= !CTRL_ASDE;
    *phy_control &= !MII_CR_AUTO_NEG_EN;
    if forced_speed_duplex & ALL_HALF_DUPLEX != 0 {
        ctrl &= !CTRL_FD;
        *phy_control &= !MII_CR_FULL_DUPLEX
    } else {
        ctrl |= CTRL_FD;
        *phy_control |= MII_CR_FULL_DUPLEX
    }
    if forced_speed_duplex & ALL_100_SPEED != 0 {
        ctrl |= CTRL_SPD_100;
        *phy_control |= MII_CR_SPEED_100;
        *phy_control &= !MII_CR_SPEED_1000
    } else {
        ctrl &= !(CTRL_SPD_1000 | CTRL_SPD_100);
        *phy_control &= !(MII_CR_SPEED_1000 | MII_CR_SPEED_100)
    }
    io.configure_collision_distance();
    io.write(CTRL, ctrl)
}

// upstream: igc_phy.c igc_set_d3_lplu_state_generic()
pub fn igc_set_d3_lplu_state_generic<I: IgcPhyIo>(
    io: &mut I,
    phy: &PhyState,
    active: bool,
) -> Result<(), PhyError> {
    if !io.has_read_phy_callback() {
        return Ok(());
    }
    let mut data = io.read_phy(IGP02IGC_PHY_POWER_MGMT)?;
    if !active {
        data &= !IGP02IGC_PM_D3_LPLU;
        io.write_phy(IGP02IGC_PHY_POWER_MGMT, data)?;
        match phy.smart_speed {
            SmartSpeed::On | SmartSpeed::Off => {
                let mut port = io.read_phy(IGP01IGC_PHY_PORT_CONFIG)?;
                if phy.smart_speed == SmartSpeed::On {
                    port |= IGP01IGC_PSCFR_SMART_SPEED
                } else {
                    port &= !IGP01IGC_PSCFR_SMART_SPEED
                }
                io.write_phy(IGP01IGC_PHY_PORT_CONFIG, port)?
            }
            SmartSpeed::Default => {}
        }
    } else if [ALL_SPEED_DUPLEX, ALL_NOT_GIG, ALL_10_SPEED].contains(&phy.autoneg_advertised) {
        data |= IGP02IGC_PM_D3_LPLU;
        io.write_phy(IGP02IGC_PHY_POWER_MGMT, data)?;
        let mut port = io.read_phy(IGP01IGC_PHY_PORT_CONFIG)?;
        port &= !IGP01IGC_PSCFR_SMART_SPEED;
        io.write_phy(IGP01IGC_PHY_PORT_CONFIG, port)?;
    }
    Ok(())
}
// upstream: igc_phy.c igc_check_downshift_generic()
pub fn igc_check_downshift_generic(phy: &mut PhyState) {
    phy.speed_downgraded = false;
}
// upstream: igc_phy.c igc_wait_autoneg()
pub fn igc_wait_autoneg<I: IgcPhyIo>(io: &mut I) -> Result<(), PhyError> {
    if !io.has_read_phy_callback() {
        return Ok(());
    }
    for _ in 0..PHY_AUTO_NEG_LIMIT {
        let _ = io.read_phy(u32::from(PHY_STATUS))?;
        let status = io.read_phy(u32::from(PHY_STATUS))?;
        if status & MII_SR_AUTONEG_COMPLETE != 0 {
            break;
        }
        io.delay_ms(100)
    }
    Ok(())
}
// upstream: igc_phy.c igc_phy_has_link_generic()
pub fn igc_phy_has_link_generic<I: IgcPhyIo>(
    io: &mut I,
    iterations: u32,
    usec_interval: u32,
) -> Result<bool, PhyError> {
    if !io.has_read_phy_callback() {
        return Ok(false);
    }
    let mut i = 0;
    for attempt in 0..iterations {
        i = attempt;
        let first = io.read_phy(u32::from(PHY_STATUS));
        if first.is_err() {
            if usec_interval >= 1000 {
                io.delay_ms(usec_interval / 1000)
            } else {
                io.delay_us(usec_interval)
            }
        }
        let status = io.read_phy(u32::from(PHY_STATUS))?;
        if status & MII_SR_LINK_STATUS != 0 {
            break;
        }
        if usec_interval >= 1000 {
            io.delay_ms(usec_interval / 1000)
        } else {
            io.delay_us(usec_interval)
        }
    }
    Ok(i < iterations)
}
// upstream: igc_phy.c igc_phy_hw_reset_generic()
pub fn igc_phy_hw_reset_generic<I: IgcPhyIo>(io: &mut I, phy: &PhyState) -> Result<(), PhyError> {
    if io.check_phy_reset_block().is_err() {
        return Ok(());
    }
    io.acquire_phy()?;
    let _phpm = io.read(I225_PHPM);
    let ctrl = io.read(CTRL);
    io.write(CTRL, ctrl | CTRL_PHY_RST);
    io.flush();
    io.delay_us(phy.reset_delay_usec);
    io.write(CTRL, ctrl);
    io.flush();
    io.delay_us(150);
    let mut done = false;
    for _ in 0..10_000 {
        let phpm = io.read(I225_PHPM);
        done = phpm & I225_PHPM_RST_COMPL != 0;
        io.delay_us(1);
        if done {
            break;
        }
    }
    if !done {
        io.debug("Timeout expired after a phy reset")
    }
    io.release_phy();
    Ok(())
}
// upstream: igc_phy.c igc_power_up_phy_copper()
pub fn igc_power_up_phy_copper<I: IgcPhyIo>(io: &mut I) -> Result<(), PhyError> {
    let ctrl = io.read_phy(u32::from(PHY_CONTROL))?;
    let _ = io.write_phy(u32::from(PHY_CONTROL), ctrl & !MII_CR_POWER_DOWN);
    io.delay_us(300);
    Ok(())
}
// upstream: igc_phy.c igc_power_down_phy_copper()
pub fn igc_power_down_phy_copper<I: IgcPhyIo>(io: &mut I) -> Result<(), PhyError> {
    let ctrl = io.read_phy(u32::from(PHY_CONTROL))?;
    let _ = io.write_phy(u32::from(PHY_CONTROL), ctrl | MII_CR_POWER_DOWN);
    io.delay_ms(1);
    Ok(())
}
// upstream: igc_phy.c igc_write_phy_reg_gpy()
pub fn igc_write_phy_reg_gpy<I: IgcPhyIo>(
    io: &mut I,
    phy: &PhyState,
    offset: u32,
    data: u16,
) -> Result<(), PhyError> {
    let dev = ((offset & GPY_MMD_MASK) >> GPY_MMD_SHIFT) as u8;
    let reg = offset & GPY_REG_MASK;
    if dev == 0 {
        io.acquire_phy()?;
        igc_write_phy_reg_mdic(io, phy, reg, data)?;
        io.release_phy();
        Ok(())
    } else {
        igc_write_xmdio_reg(io, reg as u16, dev, data)
    }
}
// upstream: igc_phy.c igc_read_phy_reg_gpy()
pub fn igc_read_phy_reg_gpy<I: IgcPhyIo>(
    io: &mut I,
    phy: &PhyState,
    offset: u32,
) -> Result<u16, PhyError> {
    let dev = ((offset & GPY_MMD_MASK) >> GPY_MMD_SHIFT) as u8;
    let reg = offset & GPY_REG_MASK;
    if dev == 0 {
        io.acquire_phy()?;
        let data = igc_read_phy_reg_mdic(io, phy, reg)?;
        io.release_phy();
        Ok(data)
    } else {
        igc_read_xmdio_reg(io, reg as u16, dev)
    }
}
fn access_xmdio<I: IgcPhyIo>(
    io: &mut I,
    address: u16,
    dev_addr: u8,
    read: bool,
    data: &mut u16,
) -> Result<(), PhyError> {
    io.write_phy(u32::from(MMDAC), u16::from(dev_addr))?;
    io.write_phy(u32::from(MMDAAD), address)?;
    io.write_phy(u32::from(MMDAC), MMDAC_FUNC_DATA | u16::from(dev_addr))?;
    if read {
        *data = io.read_phy(u32::from(MMDAAD))?
    } else {
        io.write_phy(u32::from(MMDAAD), *data)?
    }
    io.write_phy(u32::from(MMDAC), 0)?;
    Ok(())
}
// upstream: igc_phy.c __igc_access_xmdio_reg()
pub fn __igc_access_xmdio_reg<I: IgcPhyIo>(
    io: &mut I,
    address: u16,
    dev_addr: u8,
    read: bool,
    data: &mut u16,
) -> Result<(), PhyError> {
    access_xmdio(io, address, dev_addr, read, data)
}
// upstream: igc_phy.c igc_read_xmdio_reg()
pub fn igc_read_xmdio_reg<I: IgcPhyIo>(
    io: &mut I,
    address: u16,
    dev_addr: u8,
) -> Result<u16, PhyError> {
    let mut data = 0;
    __igc_access_xmdio_reg(io, address, dev_addr, true, &mut data)?;
    Ok(data)
}
// upstream: igc_phy.c igc_write_xmdio_reg()
pub fn igc_write_xmdio_reg<I: IgcPhyIo>(
    io: &mut I,
    address: u16,
    dev_addr: u8,
    data: u16,
) -> Result<(), PhyError> {
    let mut value = data;
    __igc_access_xmdio_reg(io, address, dev_addr, false, &mut value)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    struct Fake {
        regs: Vec<(u32, u32)>,
        phy: Vec<(u32, u16)>,
        writes: Vec<(u32, u32)>,
        phy_writes: Vec<(u32, u16)>,
        delays: Vec<u32>,
        flow: FlowMode,
        autoneg: bool,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                regs: Vec::new(),
                phy: Vec::new(),
                writes: Vec::new(),
                phy_writes: Vec::new(),
                delays: Vec::new(),
                flow: FlowMode::Default,
                autoneg: true,
            }
        }
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
        fn get_phy(&self, r: u32) -> u16 {
            self.phy.iter().rev().find(|v| v.0 == r).map_or(0, |v| v.1)
        }
        fn set_phy(&mut self, r: u32, v: u16) {
            if let Some(x) = self.phy.iter_mut().find(|x| x.0 == r) {
                x.1 = v
            } else {
                self.phy.push((r, v))
            }
        }
    }
    impl IgcPhyIo for Fake {
        fn read(&mut self, r: u32) -> u32 {
            if r == MDIC {
                let v = self.get(r);
                (v & MDIC_REG_MASK) | MDIC_READY | 0x55
            } else {
                self.get(r)
            }
        }
        fn write(&mut self, r: u32, v: u32) {
            self.writes.push((r, v));
            self.set(r, v)
        }
        fn flush(&mut self) {}
        fn read_phy(&mut self, r: u32) -> Result<u16, PhyError> {
            Ok(self.get_phy(r))
        }
        fn write_phy(&mut self, r: u32, v: u16) -> Result<(), PhyError> {
            self.phy_writes.push((r, v));
            self.set_phy(r, v);
            Ok(())
        }
        fn has_read_phy_callback(&self) -> bool {
            true
        }
        fn acquire_phy(&mut self) -> Result<(), PhyError> {
            Ok(())
        }
        fn release_phy(&mut self) {}
        fn delay_us(&mut self, v: u32) {
            self.delays.push(v)
        }
        fn delay_us_irq(&mut self, v: u32) {
            self.delays.push(v)
        }
        fn delay_ms(&mut self, v: u32) {
            self.delays.push(v * 1000)
        }
        fn mac_autoneg(&self) -> bool {
            self.autoneg
        }
        fn flow_mode(&self) -> FlowMode {
            self.flow
        }
        fn set_flow_mode(&mut self, v: FlowMode) {
            self.flow = v
        }
        fn check_phy_reset_block(&mut self) -> Result<(), PhyError> {
            Ok(())
        }
        fn configure_collision_distance(&mut self) {}
        fn configure_flow_control(&mut self) -> Result<(), PhyError> {
            Ok(())
        }
        fn link_status(&mut self) -> Result<bool, PhyError> {
            Ok(true)
        }
        fn force_speed_duplex(&mut self) -> Result<(), PhyError> {
            Ok(())
        }
    }
    #[test]
    fn mdic_encodes_phy_address_and_rejects_wrong_ready_or_offset() {
        let mut fake = Fake::default();
        let phy = PhyState {
            address: 3,
            ..PhyState::default()
        };
        fake.set(MDIC, 0x55);
        let command = (7 << MDIC_REG_SHIFT) | (3 << MDIC_PHY_SHIFT) | MDIC_OP_READ;
        assert_eq!(igc_read_phy_reg_mdic(&mut fake, &phy, 7), Ok(0x55));
        assert_eq!(fake.writes[0], (MDIC, command));
        assert_eq!(
            igc_read_phy_reg_mdic(&mut fake, &phy, 32),
            Err(PhyError::Bounds)
        );
    }
    #[test]
    fn autoneg_advertisement_rewrites_supported_modes_pause_and_mmd() {
        let mut fake = Fake {
            flow: FlowMode::Full,
            ..Fake::default()
        };
        fake.set_phy(u32::from(PHY_AUTONEG_ADV), 0xffff);
        fake.set_phy(u32::from(PHY_1000T_CTRL), 0xffff);
        fake.set_phy(
            (STANDARD_AN_REG_MASK << MMD_DEVADDR_SHIFT) | u32::from(ANEG_MULTIGBT_AN_CTRL),
            0xffff,
        );
        let mut phy = PhyState {
            autoneg_mask: ALL_SPEED_DUPLEX | ADV_2500_FULL,
            autoneg_advertised: ADV_10_FULL | ADV_100_FULL | ADV_1000_FULL | ADV_2500_FULL,
            ..PhyState::default()
        };
        igc_phy_setup_autoneg(&mut fake, &mut phy).unwrap();
        assert_eq!(phy.autoneg_advertised, 0xaa);
        assert_eq!(
            fake.get_phy(u32::from(PHY_AUTONEG_ADV))
                & (NWAY_10_FD | NWAY_100_FD | NWAY_AR_ASM_DIR | NWAY_AR_PAUSE),
            NWAY_10_FD | NWAY_100_FD | NWAY_AR_ASM_DIR | NWAY_AR_PAUSE
        );
        assert_eq!(
            fake.get_phy(u32::from(PHY_1000T_CTRL)) & CR_1000T_FD,
            CR_1000T_FD
        );
        assert_eq!(
            fake.get_phy(
                (STANDARD_AN_REG_MASK << MMD_DEVADDR_SHIFT) | u32::from(ANEG_MULTIGBT_AN_CTRL)
            ) & CR_2500T_FD,
            CR_2500T_FD
        );
    }
    #[test]
    fn sticky_link_status_is_double_read_and_downshift_and_lplu_behave() {
        let mut fake = Fake::default();
        fake.set_phy(
            u32::from(PHY_STATUS),
            MII_SR_LINK_STATUS | MII_SR_AUTONEG_COMPLETE,
        );
        assert!(igc_phy_has_link_generic(&mut fake, 1, 10).unwrap());
        assert_eq!(fake.delays, []);
        assert!(igc_wait_autoneg(&mut fake).is_ok());
        assert!(fake.delays.is_empty());
        let phy = PhyState {
            autoneg_advertised: ALL_NOT_GIG,
            smart_speed: SmartSpeed::On,
            ..PhyState::default()
        };
        igc_set_d3_lplu_state_generic(&mut fake, &phy, true).unwrap();
        assert_ne!(
            fake.get_phy(IGP02IGC_PHY_POWER_MGMT) & IGP02IGC_PM_D3_LPLU,
            0
        );
        assert_eq!(
            fake.get_phy(IGP01IGC_PHY_PORT_CONFIG) & IGP01IGC_PSCFR_SMART_SPEED,
            0
        );
        igc_set_d3_lplu_state_generic(&mut fake, &phy, false).unwrap();
        assert_eq!(
            fake.get_phy(IGP02IGC_PHY_POWER_MGMT) & IGP02IGC_PM_D3_LPLU,
            0
        );
        assert_ne!(
            fake.get_phy(IGP01IGC_PHY_PORT_CONFIG) & IGP01IGC_PSCFR_SMART_SPEED,
            0
        );
    }
}
