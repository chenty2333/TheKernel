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

/// upstream: igc_i225.c igc_init_nvm_params_i225()
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

/// upstream: igc_i225.c igc_init_mac_params_i225()
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

/// upstream: igc_i225.c igc_init_phy_params_i225()
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
}
