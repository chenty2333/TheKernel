//! Intel I225-family operations translated from `sys/dev/igc/igc_i225.c`.
//!
//! FreeBSD commit `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright 2021 Intel Corp; Copyright 2021 Rubicon Communications, LLC.

use super::api::{IgcApiCallback, IgcHardware, IgcMediaType, IgcNvmType, IgcPhyType};

const EECD_ADDR_BITS: u32 = 0x0000_0400;
const EECD_SIZE_EX_MASK: u32 = 0x0000_7800;
const EECD_SIZE_EX_SHIFT: u32 = 11;
const NVM_WORD_SIZE_BASE_SHIFT: u32 = 6;
const RAR_ENTRIES_BASE: u16 = 16;
const AUTONEG_ADVERTISE_SPEED_DEFAULT_2500: u32 = 0x2f;
const SWSM: u32 = 0x05b50;
const SW_FW_SYNC: u32 = 0x05b5c;
const SWSM_SMBI: u32 = 0x1;
const SWSM_SWESMBI: u32 = 0x2;
const SWFW_EEP_SM: u16 = 0x1;
const CTRL: u32 = 0x00000;
const I225_PHPM: u32 = 0x0e14;
const CTRL_SLU: u32 = 0x40;
const CTRL_FRCSPD: u32 = 0x800;
const CTRL_FRCDPX: u32 = 0x1000;
const PHPM_GO_LINKD: u32 = 0x0000_0001;
const SRWR: u32 = 0x12018;
const EERD_EEWR_MAX_COUNT: usize = 512;
const NVM_RW_ADDR_SHIFT: u32 = 2;
const NVM_RW_REG_DATA: u32 = 16;
const NVM_RW_REG_DONE: u32 = 2;
const NVM_RW_REG_START: u32 = 1;
const NVM_CHECKSUM_REG: u16 = 0x003f;
const NVM_SUM: u16 = 0xbaba;
const EECD: u32 = 0x00010;
const EECD_FLASH_DETECTED: u32 = 0x0008_0000;
const EECD_FLUPD: u32 = 0x0080_0000;
const EECD_FLUDONE: u32 = 0x0400_0000;
const EECD_SEC1VAL: u32 = 0x0200_0000;
const FLSECU: u32 = 0x12114;
const FLSECU_BLK_SW_ACCESS: u32 = 0x4;
const FWSM: u32 = 0x05b54;
const FWSM_FW_VALID: u32 = 0x8000;
const FLSWCTL: u32 = 0x12048;
const FLSWDATA: u32 = 0x1204c;
const FLSWCNT: u32 = 0x12050;
const FLSWCTL_DONE: u32 = 0x4000_0000;
const FLSWCTL_CMDV: u32 = 0x1000_0000;
const ERASE_CMD_OPCODE: u32 = 0x0200_0000;
const WRITE_CMD_OPCODE: u32 = 0x0100_0000;
const SHADOW_RAM_SIZE: u32 = 4096;
const NVM_GRANT_ATTEMPTS: u32 = 1000;
const FLUDONE_ATTEMPTS: u32 = 20_000;
const I225_PHPM_DIS_1000: u32 = 0x0040;
const I225_PHPM_DIS_2500: u32 = 0x0800;
const I225_PHPM_DIS_100_D3: u32 = 0x0200;
const I225_PHPM_DIS_1000_D3: u32 = 0x0008;
const I225_PHPM_DIS_2500_D3: u32 = 0x1000;
const IMC: u32 = 0x000d8;
const RCTL: u32 = 0x00100;
const TCTL: u32 = 0x00400;
const ICR: u32 = 0x000c0;
const CTRL_DEV_RST: u32 = 0x2000_0000;
const TCTL_PSP: u32 = 0x8;
const LTRC: u32 = 0x001a0;
const EEE_SU: u32 = 0x00e34;
const RXPBS: u32 = 0x02404;
const DMACR: u32 = 0x02508;
const LTRMINV: u32 = 0x05bb0;
const LTRMAXV: u32 = 0x05bb4;
const DMACR_DMAC_EN: u32 = 0x8000_0000;
const DMACR_DMACTHR_MASK: u32 = 0x00ff_0000;
const LTRC_EEEMS_EN: u32 = 0x20;
const TW_SYSTEM_100_MASK: u32 = 0x0000_ff00;
const TW_SYSTEM_100_SHIFT: u32 = 8;
const TW_SYSTEM_1000_MASK: u32 = 0xff;
const LTRV_MASK: u32 = 0x3ff;
const LTR_SCALE_SHIFT: u32 = 10;
const LTR_SCALE_1024: u32 = 2;
const LTR_SCALE_32768: u32 = 3;
const LTR_LSNP_REQ: u32 = 0x8000;
const RX_BUFFER_SIZE_MASK: u32 = 0x3f;
const IPCNFG: u32 = 0x00e38;
const EEER: u32 = 0x00e30;
const IPCNFG_EEE_2500: u32 = 0x10;
const IPCNFG_EEE_1000: u32 = 0x8;
const IPCNFG_EEE_100: u32 = 0x4;
const EEER_TX_LPI_EN: u32 = 0x0001_0000;
const EEER_RX_LPI_EN: u32 = 0x0002_0000;
const EEER_LPI_FC: u32 = 0x0004_0000;
const EEE_SU_LPI_CLK_STP: u32 = 0x0080_0000;

pub trait IgcI225NvmIo: IgcI225Io {
    fn read_nvm_eerd(&mut self, offset: u16, data: &mut [u16]) -> Result<(), I225NvmError>;
    fn validate_nvm_checksum_generic(&mut self) -> Result<(), I225NvmError>;
    fn update_flash_i225(&mut self) -> Result<(), I225NvmError>;
    fn poll_eerd_read_done(&mut self) -> Result<(), I225NvmError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I225NvmError {
    Bounds,
    Sync,
    Timeout,
    Hardware,
}

// upstream: igc_i225.c igc_read_nvm_srrd_i225()
pub fn igc_read_nvm_srrd_i225<I: IgcI225NvmIo>(
    io: &mut I,
    offset: u16,
    data: &mut [u16],
    words: u16,
) -> Result<(), I225NvmError> {
    if usize::from(words) > data.len() {
        return Err(I225NvmError::Bounds);
    }
    let mut i = 0usize;
    while i < usize::from(words) {
        let count = (usize::from(words) - i).min(EERD_EEWR_MAX_COUNT);
        if igc_acquire_nvm_i225(io).is_err() {
            return Err(I225NvmError::Sync);
        }
        let result = io.read_nvm_eerd(offset.wrapping_add(i as u16), &mut data[i..i + count]);
        igc_release_nvm_i225(io);
        result?;
        i += count;
    }
    Ok(())
}

// upstream: igc_i225.c __igc_write_nvm_srwr()
pub fn __igc_write_nvm_srwr<I: IgcI225Io>(
    io: &mut I,
    offset: u16,
    words: &[u16],
) -> Result<(), I225NvmError> {
    let nvm_size = io.nvm_word_size();
    if u32::from(offset) >= nvm_size
        || words.len() as u32 > nvm_size - u32::from(offset)
        || words.is_empty()
    {
        return Err(I225NvmError::Bounds);
    }
    for (i, data) in words.iter().enumerate() {
        let eewr = ((u32::from(offset) + i as u32) << NVM_RW_ADDR_SHIFT)
            | (u32::from(*data) << NVM_RW_REG_DATA)
            | NVM_RW_REG_START;
        io.write(SRWR, eewr);
        let mut done = false;
        for _ in 0..100_000 {
            if io.read(SRWR) & NVM_RW_REG_DONE != 0 {
                done = true;
                break;
            }
            io.delay_us(5);
        }
        if !done {
            return Err(I225NvmError::Timeout);
        }
    }
    Ok(())
}

// upstream: igc_i225.c igc_write_nvm_srwr_i225()
pub fn igc_write_nvm_srwr_i225<I: IgcI225NvmIo>(
    io: &mut I,
    offset: u16,
    words: &[u16],
) -> Result<(), I225NvmError> {
    let mut i = 0usize;
    while i < words.len() {
        let count = (words.len() - i).min(EERD_EEWR_MAX_COUNT);
        if igc_acquire_nvm_i225(io).is_err() {
            return Err(I225NvmError::Sync);
        }
        let result = __igc_write_nvm_srwr(io, offset.wrapping_add(i as u16), &words[i..i + count]);
        igc_release_nvm_i225(io);
        result?;
        i += count;
    }
    Ok(())
}

// upstream: igc_i225.c igc_validate_nvm_checksum_i225()
pub fn igc_validate_nvm_checksum_i225<I: IgcI225NvmIo>(io: &mut I) -> Result<(), I225NvmError> {
    if igc_acquire_nvm_i225(io).is_err() {
        return Err(I225NvmError::Sync);
    }
    let result = io.validate_nvm_checksum_generic();
    igc_release_nvm_i225(io);
    result
}

// upstream: igc_i225.c igc_update_nvm_checksum_i225()
pub fn igc_update_nvm_checksum_i225<I: IgcI225NvmIo>(io: &mut I) -> Result<(), I225NvmError> {
    let mut nvm_data = [0u16; 1];
    io.read_nvm_eerd(0, &mut nvm_data)?;
    if igc_acquire_nvm_i225(io).is_err() {
        return Err(I225NvmError::Sync);
    }
    let mut checksum = 0u16;
    for i in 0..NVM_CHECKSUM_REG {
        if let Err(error) = io.read_nvm_eerd(i, &mut nvm_data) {
            igc_release_nvm_i225(io);
            return Err(error);
        }
        checksum = checksum.wrapping_add(nvm_data[0]);
    }
    checksum = NVM_SUM.wrapping_sub(checksum);
    if let Err(error) = __igc_write_nvm_srwr(io, NVM_CHECKSUM_REG, &[checksum]) {
        igc_release_nvm_i225(io);
        return Err(error);
    }
    igc_release_nvm_i225(io);
    io.update_flash_i225()
}

// upstream: igc_i225.c igc_get_flash_presence_i225()
pub fn igc_get_flash_presence_i225<I: IgcI225Io>(io: &mut I) -> bool {
    io.read(EECD) & EECD_FLASH_DETECTED != 0
}

// upstream: igc_i225.c igc_set_flsw_flash_burst_counter_i225()
pub fn igc_set_flsw_flash_burst_counter_i225<I: IgcI225Io>(
    io: &mut I,
    burst_counter: u32,
) -> Result<(), I225NvmError> {
    if burst_counter < SHADOW_RAM_SIZE {
        io.write(FLSWCNT, burst_counter);
        Ok(())
    } else {
        Err(I225NvmError::Bounds)
    }
}

// upstream: igc_i225.c igc_write_erase_flash_command_i225()
pub fn igc_write_erase_flash_command_i225<I: IgcI225Io>(
    io: &mut I,
    opcode: u32,
    address: u32,
) -> Result<(), I225NvmError> {
    let mut flswctl = io.read(FLSWCTL);
    let mut timeout = NVM_GRANT_ATTEMPTS;
    while timeout != 0 {
        if flswctl & FLSWCTL_DONE != 0 {
            break;
        }
        io.delay_us(5);
        flswctl = io.read(FLSWCTL);
        timeout -= 1;
    }
    if timeout == 0 {
        return Err(I225NvmError::Timeout);
    }
    io.write(FLSWCTL, address | opcode);
    if io.read(FLSWCTL) & FLSWCTL_CMDV == 0 {
        return Err(I225NvmError::Bounds);
    }
    Ok(())
}

// upstream: igc_i225.c igc_pool_flash_update_done_i225()
pub fn igc_pool_flash_update_done_i225<I: IgcI225Io>(io: &mut I) -> Result<(), I225NvmError> {
    for _ in 0..FLUDONE_ATTEMPTS {
        if io.read(EECD) & EECD_FLUDONE != 0 {
            return Ok(());
        }
        io.delay_us(5);
    }
    Err(I225NvmError::Timeout)
}

// upstream: igc_i225.c igc_update_flash_i225()
pub fn igc_update_flash_i225<I: IgcI225NvmIo>(io: &mut I) -> Result<(), I225NvmError> {
    let block_sw_protect = io.read(FLSECU) & FLSECU_BLK_SW_ACCESS;
    let fw_valid = io.read(FWSM) & FWSM_FW_VALID;
    if fw_valid != 0 {
        igc_pool_flash_update_done_i225(io)?;
        let flup = io.read(EECD) | EECD_FLUPD;
        io.write(EECD, flup);
        return igc_pool_flash_update_done_i225(io);
    }
    if block_sw_protect == 0 {
        let base_address = if io.read(EECD) & EECD_SEC1VAL != 0 {
            0x1000
        } else {
            0
        };
        let erase = igc_write_erase_flash_command_i225(io, ERASE_CMD_OPCODE, base_address);
        // Preserve the source's `if (!ret_val) goto out` behavior after this
        // call, including its unusual success-path exit.
        if erase.is_ok() {
            return Ok(());
        }
        let mut current_offset = base_address as u16;
        for _ in 0..(SHADOW_RAM_SIZE / 2) {
            igc_set_flsw_flash_burst_counter_i225(io, 2)?;
            igc_write_erase_flash_command_i225(
                io,
                WRITE_CMD_OPCODE,
                2 * u32::from(current_offset),
            )?;
            let mut word = [0u16; 1];
            io.read_nvm_eerd(current_offset, &mut word)?;
            io.write(FLSWDATA, u32::from(word[0]));
            current_offset = current_offset.wrapping_add(1);
            io.poll_eerd_read_done()?;
            io.delay_us(1000);
        }
    }
    Ok(())
}

// upstream: igc_i225.c igc_set_d0_lplu_state_i225()
pub fn igc_set_d0_lplu_state_i225<I: IgcI225Io>(io: &mut I, active: bool) {
    let mut data = io.read(I225_PHPM);
    if active {
        data |= I225_PHPM_DIS_1000 | I225_PHPM_DIS_2500;
    } else {
        data &= !(I225_PHPM_DIS_1000 | I225_PHPM_DIS_2500);
    }
    io.write(I225_PHPM, data);
}

// upstream: igc_i225.c igc_set_d3_lplu_state_i225()
pub fn igc_set_d3_lplu_state_i225<I: IgcI225Io>(io: &mut I, active: bool) {
    let mut data = io.read(I225_PHPM);
    let mask = I225_PHPM_DIS_100_D3 | I225_PHPM_DIS_1000_D3 | I225_PHPM_DIS_2500_D3;
    if active {
        data |= mask;
    } else {
        data &= !mask;
    }
    io.write(I225_PHPM, data);
}

pub trait IgcI225ResetIo: IgcI225Io {
    fn disable_pcie_master_generic(&mut self) -> i32;
    fn get_auto_rd_done_generic(&mut self) -> i32;
    fn check_alt_mac_addr_generic(&mut self) -> i32;
}

// upstream: igc_i225.c igc_reset_hw_i225()
pub fn igc_reset_hw_i225<I: IgcI225ResetIo>(io: &mut I) -> i32 {
    let _ = io.disable_pcie_master_generic();
    io.write(IMC, u32::MAX);
    io.write(RCTL, 0);
    io.write(TCTL, TCTL_PSP);
    io.write_flush();
    io.delay_ms(10);
    let ctrl = io.read(CTRL);
    io.write(CTRL, ctrl | CTRL_DEV_RST);
    let _ = io.get_auto_rd_done_generic();
    io.write(IMC, u32::MAX);
    let _ = io.read(ICR);
    io.check_alt_mac_addr_generic()
}

// upstream: igc_i225.c igc_init_hw_i225()
pub fn igc_init_hw_i225<F: FnMut() -> i32>(mut base_init: F) -> i32 {
    base_init()
}

pub trait IgcI225LinkIo: IgcI225Io {
    fn get_link_status(&self) -> bool;
    fn set_get_link_status(&mut self, value: bool);
    fn phy_has_link(&mut self, iterations: u32, interval_ms: u32) -> Result<bool, i32>;
    fn check_downshift(&mut self);
    fn autoneg(&self) -> bool;
    fn config_collision_dist(&mut self);
    fn config_fc_after_link_up(&mut self) -> Result<(), i32>;
    fn get_speed_duplex(&mut self) -> (u16, u16);
    fn eee_disabled(&self) -> bool;
    fn mtu(&self) -> u32;
    fn debug(&mut self, _message: &'static str) {}
}

// upstream: igc_i225.c igc_set_ltr_i225()
pub fn igc_set_ltr_i225<I: IgcI225LinkIo>(io: &mut I, link: bool) -> i32 {
    if !link {
        return 0;
    }
    let (speed, _duplex) = io.get_speed_duplex();
    let mut tw_system = 0u32;
    if speed != 10 && !io.eee_disabled() {
        let ltrc = io.read(LTRC) | LTRC_EEEMS_EN;
        io.write(LTRC, ltrc);
        let eee_su = io.read(EEE_SU);
        tw_system = if speed == 100 {
            ((eee_su & TW_SYSTEM_100_MASK) >> TW_SYSTEM_100_SHIFT) * 500
        } else {
            (eee_su & TW_SYSTEM_1000_MASK) * 500
        };
    }
    let mut size = (io.read(RXPBS) & RX_BUFFER_SIZE_MASK) as i32;
    if io.read(DMACR) & DMACR_DMAC_EN != 0 {
        size -= ((io.read(DMACR) & DMACR_DMACTHR_MASK) >> 16) as i32;
        size *= 1024 * 8;
    } else {
        size *= 1024;
        size -= io.mtu() as i32;
        size *= 8;
    }
    if size < 0 {
        io.debug("invalid effective Rx buffer size");
        return -1;
    }
    let ltr_min = (1000 * size as u32) / u32::from(speed);
    let ltr_max = ltr_min + tw_system;
    let scale_min = if (ltr_min / 1024) < 1024 {
        LTR_SCALE_1024
    } else {
        LTR_SCALE_32768
    };
    let scale_max = if (ltr_max / 1024) < 1024 {
        LTR_SCALE_1024
    } else {
        LTR_SCALE_32768
    };
    let ltr_min = ltr_min
        / if scale_min == LTR_SCALE_1024 {
            1024
        } else {
            32768
        };
    let ltr_max = ltr_max
        / if scale_max == LTR_SCALE_1024 {
            1024
        } else {
            32768
        };
    let old_min = io.read(LTRMINV);
    if ltr_min != old_min & LTRV_MASK {
        io.write(
            LTRMINV,
            LTR_LSNP_REQ | ltr_min | (scale_min << LTR_SCALE_SHIFT),
        );
    }
    let old_max = io.read(LTRMAXV);
    if ltr_max != old_max & LTRV_MASK {
        // FreeBSD uses scale_min for the MAX register too; retain that source behavior.
        io.write(
            LTRMAXV,
            LTR_LSNP_REQ | ltr_max | (scale_min << LTR_SCALE_SHIFT),
        );
    }
    0
}

// upstream: igc_i225.c igc_check_for_link_i225()
pub fn igc_check_for_link_i225<I: IgcI225LinkIo>(io: &mut I) -> i32 {
    let mut ret_val = 0;
    let mut link = false;
    if io.get_link_status() {
        match io.phy_has_link(1, 0) {
            Ok(found) => link = found,
            Err(error) => ret_val = error,
        }
        if ret_val == 0 && link {
            match io.phy_has_link(1, 0) {
                Ok(found) => link = found,
                Err(error) => ret_val = error,
            }
        }
        if ret_val == 0 && link {
            io.set_get_link_status(false);
            io.check_downshift();
            if io.autoneg() {
                io.config_collision_dist();
                if io.config_fc_after_link_up().is_err() {
                    io.debug("error configuring flow control");
                }
            }
        }
    }
    ret_val = igc_set_ltr_i225(io, link);
    ret_val
}

// upstream: igc_i225.c igc_set_eee_i225()
pub fn igc_set_eee_i225<I: IgcI225Io>(
    io: &mut I,
    is_i225: bool,
    copper: bool,
    eee_disable: bool,
    adv2p5g: bool,
    adv1g: bool,
    adv100m: bool,
) {
    if !is_i225 || !copper {
        return;
    }
    let mut ipcnfg = io.read(IPCNFG);
    let mut eeer = io.read(EEER);
    if !eee_disable {
        let eee_su = io.read(EEE_SU);
        for (enabled, mask) in [
            (adv100m, IPCNFG_EEE_100),
            (adv1g, IPCNFG_EEE_1000),
            (adv2p5g, IPCNFG_EEE_2500),
        ] {
            if enabled {
                ipcnfg |= mask;
            } else {
                ipcnfg &= !mask;
            }
        }
        eeer |= EEER_TX_LPI_EN | EEER_RX_LPI_EN | EEER_LPI_FC;
        let _lpi_clock_stop = eee_su & EEE_SU_LPI_CLK_STP;
    } else {
        ipcnfg &= !(IPCNFG_EEE_2500 | IPCNFG_EEE_1000 | IPCNFG_EEE_100);
        eeer &= !(EEER_TX_LPI_EN | EEER_RX_LPI_EN | EEER_LPI_FC);
    }
    io.write(IPCNFG, ipcnfg);
    io.write(EEER, eeer);
    let _ = io.read(IPCNFG);
    let _ = io.read(EEER);
}

pub trait IgcI225Io {
    fn read(&mut self, reg: u32) -> u32;
    fn write(&mut self, reg: u32, value: u32);
    fn write_flush(&mut self);
    fn delay_us(&mut self, us: u32);
    fn delay_ms(&mut self, ms: u32);
    fn delay_ms_irq(&mut self, ms: u32);
    fn nvm_word_size(&self) -> u32;
    fn clear_semaphore_once(&mut self) -> bool;
    fn set_clear_semaphore_once(&mut self, value: bool);
    fn put_hw_semaphore_generic(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I225SyncError {
    Nvm,
    SwFwSync,
}

// upstream: igc_i225.c igc_get_hw_semaphore_i225()
pub fn igc_get_hw_semaphore_i225<I: IgcI225Io>(io: &mut I) -> Result<(), I225SyncError> {
    let timeout = io.nvm_word_size() + 1;
    let mut i = 0;
    while i < timeout {
        if io.read(SWSM) & SWSM_SMBI == 0 {
            break;
        }
        io.delay_us(50);
        i += 1;
    }
    if i == timeout {
        if io.clear_semaphore_once() {
            io.set_clear_semaphore_once(false);
            io.put_hw_semaphore_generic();
            i = 0;
            while i < timeout {
                if io.read(SWSM) & SWSM_SMBI == 0 {
                    break;
                }
                io.delay_us(50);
                i += 1;
            }
        }
        if i == timeout {
            return Err(I225SyncError::Nvm);
        }
    }
    for i in 0..timeout {
        let swsm = io.read(SWSM);
        io.write(SWSM, swsm | SWSM_SWESMBI);
        if io.read(SWSM) & SWSM_SWESMBI != 0 {
            return Ok(());
        }
        io.delay_us(50);
        if i + 1 == timeout {
            io.put_hw_semaphore_generic();
            return Err(I225SyncError::Nvm);
        }
    }
    Err(I225SyncError::Nvm)
}

// upstream: igc_i225.c igc_acquire_swfw_sync_i225()
pub fn igc_acquire_swfw_sync_i225<I: IgcI225Io>(
    io: &mut I,
    mask: u16,
) -> Result<(), I225SyncError> {
    let swmask = u32::from(mask);
    let fwmask = u32::from(mask) << 16;
    let mut i = 0;
    let mut swfw_sync = None;
    while i < 200 {
        igc_get_hw_semaphore_i225(io).map_err(|_| I225SyncError::SwFwSync)?;
        let value = io.read(SW_FW_SYNC);
        swfw_sync = Some(value);
        if value & (fwmask | swmask) == 0 {
            break;
        }
        io.put_hw_semaphore_generic();
        io.delay_ms_irq(5);
        i += 1;
    }
    if i == 200 {
        return Err(I225SyncError::SwFwSync);
    }
    let swfw_sync = swfw_sync.unwrap_or(0) | swmask;
    io.write(SW_FW_SYNC, swfw_sync);
    io.put_hw_semaphore_generic();
    Ok(())
}

// upstream: igc_i225.c igc_release_swfw_sync_i225()
pub fn igc_release_swfw_sync_i225<I: IgcI225Io>(io: &mut I, mask: u16) {
    while igc_get_hw_semaphore_i225(io).is_err() {}
    let swfw_sync = io.read(SW_FW_SYNC) & !u32::from(mask);
    io.write(SW_FW_SYNC, swfw_sync);
    io.put_hw_semaphore_generic();
}

// upstream: igc_i225.c igc_acquire_nvm_i225()
pub fn igc_acquire_nvm_i225<I: IgcI225Io>(io: &mut I) -> Result<(), I225SyncError> {
    igc_acquire_swfw_sync_i225(io, SWFW_EEP_SM)
}

// upstream: igc_i225.c igc_release_nvm_i225()
pub fn igc_release_nvm_i225<I: IgcI225Io>(io: &mut I) {
    igc_release_swfw_sync_i225(io, SWFW_EEP_SM)
}

// upstream: igc_i225.c igc_setup_copper_link_i225()
pub fn igc_setup_copper_link_i225<I: IgcI225Io, F: FnMut(&mut I) -> i32>(
    io: &mut I,
    mut generic_setup: F,
) -> i32 {
    let mut ctrl = io.read(CTRL);
    ctrl |= CTRL_SLU;
    ctrl &= !(CTRL_FRCSPD | CTRL_FRCDPX);
    io.write(CTRL, ctrl);
    let phpm = io.read(I225_PHPM) & !PHPM_GO_LINKD;
    io.write(I225_PHPM, phpm);
    generic_setup(io)
}

// upstream: igc_i225.c igc_init_nvm_params_i225()
pub fn init_nvm_params_i225(hw: &mut IgcHardware, eecd: u32, flash_present: bool) {
    let mut size = ((eecd & EECD_SIZE_EX_MASK) >> EECD_SIZE_EX_SHIFT) + NVM_WORD_SIZE_BASE_SHIFT;
    if size > 15 {
        size = 15;
    }
    hw.nvm_info.word_size = 1 << size;
    hw.nvm_info.opcode_bits = 8;
    hw.nvm_info.delay_usec = 1;
    hw.nvm_info.nvm_type = IgcNvmType::EepromSpi;
    hw.nvm_info.page_size = if eecd & EECD_ADDR_BITS != 0 { 32 } else { 8 };
    hw.nvm_info.address_bits = if eecd & EECD_ADDR_BITS != 0 { 16 } else { 8 };
    if hw.nvm_info.word_size == (1 << 15) {
        hw.nvm_info.page_size = 128;
    }
    hw.nvm_ops.acquire = Some(IgcApiCallback::NvmAcquireI225);
    hw.nvm_ops.release = Some(IgcApiCallback::NvmReleaseI225);
    if flash_present {
        hw.nvm_info.nvm_type = IgcNvmType::FlashHardware;
        hw.nvm_ops.read = Some(IgcApiCallback::NvmReadI225);
        hw.nvm_ops.write = Some(IgcApiCallback::NvmWriteI225);
        hw.nvm_ops.validate = Some(IgcApiCallback::NvmValidateI225);
        hw.nvm_ops.update = Some(IgcApiCallback::NvmUpdateI225);
    } else {
        hw.nvm_info.nvm_type = IgcNvmType::Invm;
        hw.nvm_ops.write = None;
        hw.nvm_ops.validate = None;
        hw.nvm_ops.update = None;
    }
}

// upstream: igc_i225.c igc_init_mac_params_i225()
pub fn init_mac_params_i225(hw: &mut IgcHardware) {
    super::api::init_generic_ops(hw);
    hw.mac_info.media_type = IgcMediaType::Copper;
    hw.phy_info.media_type = IgcMediaType::Copper;
    hw.mac_info.mta_register_count = 128;
    hw.mac_info.rar_entry_count = RAR_ENTRIES_BASE;
    hw.mac_ops.reset_hw = Some(IgcApiCallback::ResetHwI225);
    hw.mac_ops.init_hw = Some(IgcApiCallback::InitHwI225);
    hw.mac_ops.setup_link = Some(IgcApiCallback::SetupLinkI225);
    hw.mac_ops.check_for_link = Some(IgcApiCallback::CheckLinkI225);
    hw.mac_ops.get_link_up_info = Some(IgcApiCallback::GetLinkInfoI225);
    hw.mac_info.clear_semaphore_once = true;
    hw.mac_info.asf_firmware_present = true;
    hw.mac_ops.update_mc_addr_list = Some(IgcApiCallback::UpdateMcAddrListI225);
    hw.mac_ops.write_vfta = Some(IgcApiCallback::WriteVftaI225);
}

// upstream: igc_i225.c igc_init_phy_params_i225()
pub fn init_phy_params_i225<B: super::api::IgcApiBackend>(
    hw: &mut IgcHardware,
    backend: &mut B,
) -> axdriver_base::DevResult {
    if hw.phy_info.media_type != IgcMediaType::Copper {
        hw.phy_info.phy_type = IgcPhyType::None;
        return Ok(());
    }
    hw.phy_ops.power_up = Some(IgcApiCallback::PhyPowerUpI225);
    hw.phy_ops.power_down = Some(IgcApiCallback::PhyPowerDownI225);
    hw.phy_info.autoneg_mask = AUTONEG_ADVERTISE_SPEED_DEFAULT_2500;
    hw.phy_info.reset_delay_usec = 100;
    hw.phy_ops.acquire = Some(IgcApiCallback::PhyAcquireI225);
    hw.phy_ops.check_reset_block = Some(IgcApiCallback::PhyCheckResetBlockI225);
    hw.phy_ops.release = Some(IgcApiCallback::PhyReleaseI225);
    hw.phy_ops.reset = Some(IgcApiCallback::PhyResetI225);
    hw.phy_ops.read = Some(IgcApiCallback::PhyReadI225);
    hw.phy_ops.write = Some(IgcApiCallback::PhyWriteI225);
    backend.invoke(
        IgcApiCallback::PhyResetI225,
        super::api::IgcApiRequest::None,
    )?;
    let phy_id = backend.invoke(
        IgcApiCallback::GetPhyIdGeneric,
        super::api::IgcApiRequest::None,
    )?;
    if let super::api::IgcApiValue::U32(id) = phy_id {
        hw.phy_info.phy_id = id;
        hw.phy_info.phy_type = IgcPhyType::I225;
        Ok(())
    } else {
        Err(axdriver_base::DevError::Io)
    }
}

// upstream: igc_i225.c igc_init_function_pointers_i225()
pub fn init_function_pointers_i225(hw: &mut IgcHardware) {
    hw.mac_ops.init_params = Some(IgcApiCallback::MacInitParamsI225);
    hw.nvm_ops.init_params = Some(IgcApiCallback::NvmInitParamsI225);
    hw.phy_ops.init_params = Some(IgcApiCallback::PhyInitParamsI225);
}

#[cfg(test)]
mod tests {
    use axdriver_base::DevResult;

    use super::*;
    use crate::igc::api::{IgcApiRequest, IgcApiValue};

    #[derive(Default)]
    struct Api {
        calls: Vec<IgcApiCallback>,
    }
    impl super::super::api::IgcApiBackend for Api {
        fn invoke(
            &mut self,
            callback: IgcApiCallback,
            _request: IgcApiRequest,
        ) -> DevResult<IgcApiValue> {
            self.calls.push(callback);
            Ok(match callback {
                IgcApiCallback::GetPhyIdGeneric => IgcApiValue::U32(0x1234_5678),
                _ => IgcApiValue::Unit,
            })
        }
    }

    #[test]
    fn nvm_geometry_and_invm_write_policy_follow_eecd() {
        let mut hw = IgcHardware::new(0x15f3, true);
        init_nvm_params_i225(&mut hw, EECD_ADDR_BITS | (7 << EECD_SIZE_EX_SHIFT), false);
        assert_eq!(hw.nvm_info.word_size, 1 << 13);
        assert_eq!(hw.nvm_info.page_size, 32);
        assert_eq!(hw.nvm_info.address_bits, 16);
        assert_eq!(hw.nvm_info.nvm_type, IgcNvmType::Invm);
        assert_eq!(hw.nvm_ops.write, None);

        init_nvm_params_i225(&mut hw, EECD_ADDR_BITS | (15 << EECD_SIZE_EX_SHIFT), true);
        assert_eq!(hw.nvm_info.word_size, 1 << 15);
        assert_eq!(hw.nvm_info.page_size, 128);
        assert_eq!(hw.nvm_ops.read, Some(IgcApiCallback::NvmReadI225));
    }

    #[test]
    fn mac_and_phy_initializers_install_i225_operations_in_source_order() {
        let mut hw = IgcHardware::new(0x15f3, true);
        init_mac_params_i225(&mut hw);
        assert_eq!(hw.mac_info.mta_register_count, 128);
        assert_eq!(hw.mac_info.rar_entry_count, RAR_ENTRIES_BASE);
        assert!(hw.mac_info.clear_semaphore_once && hw.mac_info.asf_firmware_present);
        assert_eq!(
            hw.mac_ops.setup_physical_interface,
            Some(IgcApiCallback::SetupCopperLinkI225)
        );

        let mut backend = Api::default();
        init_phy_params_i225(&mut hw, &mut backend).unwrap();
        assert_eq!(
            backend.calls,
            [
                IgcApiCallback::PhyResetI225,
                IgcApiCallback::GetPhyIdGeneric
            ]
        );
        assert_eq!(hw.phy_info.phy_type, IgcPhyType::I225);
        assert_eq!(hw.phy_info.phy_id, 0x1234_5678);
    }

    #[derive(Default)]
    struct SemIo {
        regs: Vec<(u32, u32)>,
        word_size: u32,
        clear_once: bool,
        delays: Vec<u32>,
        puts: usize,
        nvm: Vec<u16>,
        flash_updates: usize,
        link_status: bool,
        link_found: bool,
        phy_checks: usize,
        autoneg_enabled: bool,
        speed: u16,
        mtu_bytes: u32,
        events: Vec<&'static str>,
    }
    impl SemIo {
        fn read_reg(&self, reg: u32) -> u32 {
            self.regs
                .iter()
                .rev()
                .find(|v| v.0 == reg)
                .map_or(0, |v| v.1)
        }
        fn write_reg(&mut self, reg: u32, val: u32) {
            if let Some(v) = self.regs.iter_mut().find(|v| v.0 == reg) {
                v.1 = val
            } else {
                self.regs.push((reg, val))
            }
        }
    }
    impl IgcI225Io for SemIo {
        fn read(&mut self, r: u32) -> u32 {
            self.read_reg(r)
                | if r == SRWR {
                    NVM_RW_REG_DONE
                } else if r == FLSWCTL {
                    FLSWCTL_DONE | FLSWCTL_CMDV
                } else {
                    0
                }
        }
        fn write(&mut self, r: u32, v: u32) {
            self.write_reg(r, v)
        }
        fn write_flush(&mut self) {}
        fn delay_us(&mut self, v: u32) {
            self.delays.push(v)
        }
        fn delay_ms(&mut self, v: u32) {
            self.delays.push(v * 1000)
        }
        fn delay_ms_irq(&mut self, v: u32) {
            self.delays.push(v * 1000)
        }
        fn nvm_word_size(&self) -> u32 {
            self.word_size
        }
        fn clear_semaphore_once(&mut self) -> bool {
            self.clear_once
        }
        fn set_clear_semaphore_once(&mut self, v: bool) {
            self.clear_once = v
        }
        fn put_hw_semaphore_generic(&mut self) {
            self.puts += 1;
            self.write_reg(SWSM, 0)
        }
    }
    impl IgcI225NvmIo for SemIo {
        fn read_nvm_eerd(&mut self, offset: u16, data: &mut [u16]) -> Result<(), I225NvmError> {
            let start = usize::from(offset);
            let Some(source) = self.nvm.get(start..start + data.len()) else {
                return Err(I225NvmError::Bounds);
            };
            data.copy_from_slice(source);
            Ok(())
        }
        fn validate_nvm_checksum_generic(&mut self) -> Result<(), I225NvmError> {
            Ok(())
        }
        fn update_flash_i225(&mut self) -> Result<(), I225NvmError> {
            self.flash_updates += 1;
            Ok(())
        }
        fn poll_eerd_read_done(&mut self) -> Result<(), I225NvmError> {
            Ok(())
        }
    }
    impl IgcI225ResetIo for SemIo {
        fn disable_pcie_master_generic(&mut self) -> i32 {
            self.events.push("disable-master");
            1
        }
        fn get_auto_rd_done_generic(&mut self) -> i32 {
            self.events.push("auto-read");
            1
        }
        fn check_alt_mac_addr_generic(&mut self) -> i32 {
            self.events.push("alt-mac");
            7
        }
    }
    impl IgcI225LinkIo for SemIo {
        fn get_link_status(&self) -> bool {
            self.link_status
        }
        fn set_get_link_status(&mut self, value: bool) {
            self.link_status = value;
            self.events.push("status");
        }
        fn phy_has_link(&mut self, _iterations: u32, _interval_ms: u32) -> Result<bool, i32> {
            self.phy_checks += 1;
            Ok(self.link_found)
        }
        fn check_downshift(&mut self) {
            self.events.push("downshift");
        }
        fn autoneg(&self) -> bool {
            self.autoneg_enabled
        }
        fn config_collision_dist(&mut self) {
            self.events.push("collision");
        }
        fn config_fc_after_link_up(&mut self) -> Result<(), i32> {
            self.events.push("flow-control");
            Ok(())
        }
        fn get_speed_duplex(&mut self) -> (u16, u16) {
            (self.speed, 1)
        }
        fn eee_disabled(&self) -> bool {
            true
        }
        fn mtu(&self) -> u32 {
            self.mtu_bytes
        }
    }
    #[test]
    fn swfw_access_sets_software_bit_and_release_clears_only_requested_bit() {
        let mut io = SemIo {
            word_size: 63,
            ..SemIo::default()
        };
        igc_acquire_nvm_i225(&mut io).unwrap();
        assert_eq!(io.read_reg(SW_FW_SYNC), u32::from(SWFW_EEP_SM));
        igc_release_nvm_i225(&mut io);
        assert_eq!(io.read_reg(SW_FW_SYNC), 0);
        assert_eq!(io.puts, 2);
    }

    #[test]
    fn shadow_nvm_operations_chunk_and_checksum_then_commit_flash() {
        let mut io = SemIo {
            word_size: 1024,
            nvm: (0..1024).map(|i| i as u16).collect(),
            ..SemIo::default()
        };
        let mut out = alloc::vec![0; 513];
        igc_read_nvm_srrd_i225(&mut io, 100, &mut out, 513).unwrap();
        assert_eq!(out[0], 100);
        assert_eq!(out[512], 612);
        igc_write_nvm_srwr_i225(&mut io, 4, &[0xabcd, 0x1234]).unwrap();
        assert_eq!((io.read_reg(SRWR) >> NVM_RW_ADDR_SHIFT) & 0x3fff, 5);
        igc_update_nvm_checksum_i225(&mut io).unwrap();
        assert_eq!(io.flash_updates, 1);
        let checksum = NVM_SUM.wrapping_sub(
            (0..NVM_CHECKSUM_REG)
                .map(|n| n as u16)
                .fold(0u16, u16::wrapping_add),
        );
        assert_eq!((io.read_reg(SRWR) >> NVM_RW_REG_DATA) as u16, checksum);
    }

    #[test]
    fn flash_register_helpers_apply_bounds_completion_and_lplu_masks() {
        let mut io = SemIo::default();
        assert!(igc_get_flash_presence_i225(&mut io) == false);
        io.write_reg(EECD, EECD_FLASH_DETECTED);
        assert!(igc_get_flash_presence_i225(&mut io));
        assert!(igc_set_flsw_flash_burst_counter_i225(&mut io, SHADOW_RAM_SIZE - 1).is_ok());
        assert!(igc_set_flsw_flash_burst_counter_i225(&mut io, SHADOW_RAM_SIZE).is_err());
        assert!(igc_write_erase_flash_command_i225(&mut io, WRITE_CMD_OPCODE, 8).is_ok());
        io.write_reg(I225_PHPM, 0);
        igc_set_d0_lplu_state_i225(&mut io, true);
        assert_eq!(
            io.read_reg(I225_PHPM),
            I225_PHPM_DIS_1000 | I225_PHPM_DIS_2500
        );
        io.write_reg(I225_PHPM, 0);
        igc_set_d3_lplu_state_i225(&mut io, true);
        assert_eq!(
            io.read_reg(I225_PHPM),
            I225_PHPM_DIS_100_D3 | I225_PHPM_DIS_1000_D3 | I225_PHPM_DIS_2500_D3
        );
    }

    #[test]
    fn reset_continues_after_nonfatal_master_and_auto_read_failures() {
        let mut io = SemIo::default();
        assert_eq!(igc_reset_hw_i225(&mut io), 7);
        assert_eq!(io.events, ["disable-master", "auto-read", "alt-mac"]);
        assert_eq!(io.read_reg(IMC), u32::MAX);
        assert_eq!(io.read_reg(TCTL), TCTL_PSP);
        assert!(io.read_reg(CTRL) & CTRL_DEV_RST != 0);
    }

    #[test]
    fn link_check_repeats_phy_check_then_runs_link_up_operations() {
        let mut io = SemIo {
            link_status: true,
            link_found: true,
            autoneg_enabled: true,
            speed: 1000,
            mtu_bytes: 1500,
            ..SemIo::default()
        };
        io.write_reg(RXPBS, 34);
        assert_eq!(igc_check_for_link_i225(&mut io), 0);
        assert_eq!(io.phy_checks, 2);
        assert!(!io.link_status);
        assert_eq!(
            &io.events[..],
            &["status", "downshift", "collision", "flow-control"]
        );
        assert_ne!(io.read_reg(LTRMINV), 0);
    }
}
