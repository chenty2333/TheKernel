//! Intel e1000 common API device-family selection.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_api.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

/// MAC generation chosen by `e1000_set_mac_type()` from the PCI device ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000MacType {
    I82542,
    I82543,
    I82544,
    I82540,
    I82545,
    I82545Rev3,
    I82546,
    I82546Rev3,
    I82541,
    I82541Rev2,
    I82547,
    I82547Rev2,
    I82571,
    I82572,
    I82573,
    I82574,
    I82583,
    I80003Es2lan,
    Ich8Lan,
    Ich9Lan,
    Ich10Lan,
    PchLan,
    Pch2Lan,
    PchLpt,
    PchSpt,
    PchCnp,
    PchTgp,
    PchAdp,
    PchMtp,
    PchPtp,
    PchNvp,
    I82575,
    I82576,
    I82580,
    I350,
    I210,
    I211,
    VfAdapt,
    VfAdaptI350,
    I354,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000InitFamily {
    I82542,
    I82543,
    I82540,
    I82541,
    I82571,
    I80003Es2lan,
    Ich8Lan,
    I82575,
    I210,
    Vf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000ApiError {
    Config,
    MacInit,
    Unsupported,
}

pub type E1000ApiResult<T = ()> = Result<T, E1000ApiError>;

/// Implemented by the hardware context that receives the shared API calls.
/// The option-valued initializers represent nullable C function pointers.
pub trait E1000ApiHardware {
    fn registers_mapped(&self) -> bool;
    fn set_mac_type(&mut self, mac_type: E1000MacType);
    fn init_generic_ops(&mut self);
    fn init_family_ops(&mut self, family: E1000InitFamily) -> E1000ApiResult;
    fn init_mac_params(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn init_nvm_params(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn init_phy_params(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn init_mbx_params(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn get_bus_info(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn clear_vfta(&mut self) -> Option<()> {
        None
    }
    fn write_vfta(&mut self, _offset: u32, _value: u32) -> Option<()> {
        None
    }
    fn update_mc_addr_list(&mut self, _addresses: &[u8], _count: u32) -> Option<()> {
        None
    }
    fn force_mac_fc_generic(&mut self) -> E1000ApiResult {
        Err(E1000ApiError::Unsupported)
    }
    fn check_for_link(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn check_mng_mode(&mut self) -> Option<bool> {
        None
    }
    fn mng_write_dhcp_info_generic(&mut self, _buffer: &[u8]) -> E1000ApiResult {
        Err(E1000ApiError::Unsupported)
    }
    fn reset_hw(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn init_hw(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn setup_link(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn get_link_up_info(&mut self) -> Option<E1000ApiResult<(u16, u16)>> {
        None
    }
    fn setup_led(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn cleanup_led(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn blink_led(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn id_led_init(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn led_on(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn led_off(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn reset_adaptive_generic(&mut self) {}
    fn update_adaptive_generic(&mut self) {}
    fn disable_pcie_master_generic(&mut self) -> E1000ApiResult {
        Err(E1000ApiError::Unsupported)
    }
    fn config_collision_dist(&mut self) -> Option<()> {
        None
    }
    fn rar_set(&mut self, _address: [u8; 6], _index: u32) -> Option<E1000ApiResult> {
        None
    }
    fn validate_mdi_setting(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn hash_mc_addr_generic(&mut self, _address: [u8; 6]) -> u32 {
        0
    }
    fn enable_tx_pkt_filtering_generic(&mut self) -> bool {
        false
    }
    fn mng_host_if_write_generic(&mut self, _buffer: &[u8], _offset: u16) -> E1000ApiResult<u8> {
        Err(E1000ApiError::Unsupported)
    }
    fn mng_write_cmd_header_generic(&mut self, _header: &[u8]) -> E1000ApiResult {
        Err(E1000ApiError::Unsupported)
    }
    fn mng_enable_host_if_generic(&mut self) -> E1000ApiResult {
        Err(E1000ApiError::Unsupported)
    }
    fn set_obff_timer(&mut self, _itr: u32) -> Option<E1000ApiResult> {
        None
    }
    fn check_reset_block(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn read_phy_reg(&mut self, _offset: u32) -> Option<E1000ApiResult<u16>> {
        None
    }
    fn write_phy_reg(&mut self, _offset: u32, _value: u16) -> Option<E1000ApiResult> {
        None
    }
    fn release_phy(&mut self) -> Option<()> {
        None
    }
    fn acquire_phy(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn cfg_on_link_up(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn read_kmrn_reg_generic(&mut self, _offset: u32) -> E1000ApiResult<u16> {
        Err(E1000ApiError::Unsupported)
    }
    fn write_kmrn_reg_generic(&mut self, _offset: u32, _value: u16) -> E1000ApiResult {
        Err(E1000ApiError::Unsupported)
    }
    fn get_cable_length(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn get_phy_info(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn phy_hw_reset(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn phy_commit(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn set_d0_lplu_state(&mut self, _active: bool) -> Option<E1000ApiResult> {
        None
    }
    fn set_d3_lplu_state(&mut self, _active: bool) -> Option<E1000ApiResult> {
        None
    }
    fn read_mac_addr(&mut self) -> Option<E1000ApiResult<[u8; 6]>> {
        None
    }
    fn read_mac_addr_generic(&mut self) -> E1000ApiResult<[u8; 6]> {
        Err(E1000ApiError::Unsupported)
    }
    fn read_pba_string_generic(&mut self) -> E1000ApiResult<alloc::vec::Vec<u8>> {
        Err(E1000ApiError::Unsupported)
    }
    fn read_pba_length_generic(&mut self) -> E1000ApiResult<u32> {
        Err(E1000ApiError::Unsupported)
    }
    fn read_pba_num_generic(&mut self) -> E1000ApiResult<u32> {
        Err(E1000ApiError::Unsupported)
    }
    fn validate_nvm_checksum(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn update_nvm_checksum(&mut self) -> Option<E1000ApiResult> {
        None
    }
    fn reload_nvm(&mut self) -> Option<()> {
        None
    }
    fn read_nvm(
        &mut self,
        _offset: u16,
        _words: u16,
    ) -> Option<E1000ApiResult<alloc::vec::Vec<u16>>> {
        None
    }
    fn write_nvm(&mut self, _offset: u16, _words: &[u16]) -> Option<E1000ApiResult> {
        None
    }
    fn write_8bit_ctrl_reg_generic(
        &mut self,
        _register: u32,
        _offset: u32,
        _value: u8,
    ) -> E1000ApiResult {
        Err(E1000ApiError::Unsupported)
    }
    fn power_up_phy(&mut self) -> Option<()> {
        None
    }
    fn power_down_phy(&mut self) -> Option<()> {
        None
    }
    fn power_up_fiber_serdes_link(&mut self) -> Option<()> {
        None
    }
    fn shutdown_fiber_serdes_link(&mut self) -> Option<()> {
        None
    }
}

// upstream: e1000_api.c e1000_init_mac_params()
pub fn init_mac_params<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.init_mac_params().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_init_nvm_params()
pub fn init_nvm_params<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.init_nvm_params().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_init_phy_params()
pub fn init_phy_params<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.init_phy_params().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_init_mbx_params()
pub fn init_mbx_params<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.init_mbx_params().unwrap_or(Err(E1000ApiError::Config))
}

fn init_family(mac_type: E1000MacType) -> E1000InitFamily {
    use E1000InitFamily as Family;
    use E1000MacType as Mac;
    match mac_type {
        Mac::I82542 => Family::I82542,
        Mac::I82543 | Mac::I82544 => Family::I82543,
        Mac::I82540 | Mac::I82545 | Mac::I82545Rev3 | Mac::I82546 | Mac::I82546Rev3 => {
            Family::I82540
        }
        Mac::I82541 | Mac::I82541Rev2 | Mac::I82547 | Mac::I82547Rev2 => Family::I82541,
        Mac::I82571 | Mac::I82572 | Mac::I82573 | Mac::I82574 | Mac::I82583 => Family::I82571,
        Mac::I80003Es2lan => Family::I80003Es2lan,
        Mac::Ich8Lan
        | Mac::Ich9Lan
        | Mac::Ich10Lan
        | Mac::PchLan
        | Mac::Pch2Lan
        | Mac::PchLpt
        | Mac::PchSpt
        | Mac::PchCnp
        | Mac::PchTgp
        | Mac::PchAdp
        | Mac::PchMtp
        | Mac::PchPtp
        | Mac::PchNvp => Family::Ich8Lan,
        Mac::I82575 | Mac::I82576 | Mac::I82580 | Mac::I350 | Mac::I354 => Family::I82575,
        Mac::I210 | Mac::I211 => Family::I210,
        Mac::VfAdapt | Mac::VfAdaptI350 => Family::Vf,
    }
}

// upstream: e1000_api.c e1000_setup_init_funcs()
pub fn setup_init_funcs<H: E1000ApiHardware>(
    hw: &mut H,
    device_id: u16,
    init_device: bool,
) -> E1000ApiResult {
    let mac_type = set_mac_type(device_id).map_err(|_| E1000ApiError::MacInit)?;
    hw.set_mac_type(mac_type);
    if !hw.registers_mapped() {
        return Err(E1000ApiError::Config);
    }
    hw.init_generic_ops();
    hw.init_family_ops(init_family(mac_type))?;
    if init_device {
        init_mac_params(hw)?;
        init_nvm_params(hw)?;
        init_phy_params(hw)?;
        init_mbx_params(hw)?;
    }
    Ok(())
}

impl E1000MacType {
    /// Whether this family uses the 82575+ advanced queue register banks.
    pub const fn igb_advanced_queues(self) -> bool {
        matches!(
            self,
            Self::I82575
                | Self::I82576
                | Self::I82580
                | Self::I350
                | Self::I210
                | Self::I211
                | Self::I354
                | Self::VfAdapt
                | Self::VfAdaptI350
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownE1000Device(pub u16);

// upstream: e1000_api.c e1000_set_mac_type()
const MAC_TYPES: &[(u16, E1000MacType)] = &[
    (0x0438, E1000MacType::I82580),       // E1000_DEV_ID_DH89XXCC_SGMII
    (0x043a, E1000MacType::I82580),       // E1000_DEV_ID_DH89XXCC_SERDES
    (0x043c, E1000MacType::I82580),       // E1000_DEV_ID_DH89XXCC_BACKPLANE
    (0x0440, E1000MacType::I82580),       // E1000_DEV_ID_DH89XXCC_SFP
    (0x0d4c, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CMP_I219_LM11
    (0x0d4d, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CMP_I219_V11
    (0x0d4e, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CMP_I219_LM10
    (0x0d4f, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CMP_I219_V10
    (0x0d53, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_CMP_I219_LM12
    (0x0d55, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_CMP_I219_V12
    (0x0dc5, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_RPL_I219_LM23
    (0x0dc6, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_RPL_I219_V23
    (0x0dc7, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_RPL_I219_LM22
    (0x0dc8, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_RPL_I219_V22
    (0x1000, E1000MacType::I82542),       // E1000_DEV_ID_82542
    (0x1001, E1000MacType::I82543),       // E1000_DEV_ID_82543GC_FIBER
    (0x1004, E1000MacType::I82543),       // E1000_DEV_ID_82543GC_COPPER
    (0x1008, E1000MacType::I82544),       // E1000_DEV_ID_82544EI_COPPER
    (0x1009, E1000MacType::I82544),       // E1000_DEV_ID_82544EI_FIBER
    (0x100c, E1000MacType::I82544),       // E1000_DEV_ID_82544GC_COPPER
    (0x100d, E1000MacType::I82544),       // E1000_DEV_ID_82544GC_LOM
    (0x100e, E1000MacType::I82540),       // E1000_DEV_ID_82540EM
    (0x100f, E1000MacType::I82545),       // E1000_DEV_ID_82545EM_COPPER
    (0x1010, E1000MacType::I82546),       // E1000_DEV_ID_82546EB_COPPER
    (0x1011, E1000MacType::I82545),       // E1000_DEV_ID_82545EM_FIBER
    (0x1012, E1000MacType::I82546),       // E1000_DEV_ID_82546EB_FIBER
    (0x1013, E1000MacType::I82541),       // E1000_DEV_ID_82541EI
    (0x1014, E1000MacType::I82541),       // E1000_DEV_ID_82541ER_LOM
    (0x1015, E1000MacType::I82540),       // E1000_DEV_ID_82540EM_LOM
    (0x1016, E1000MacType::I82540),       // E1000_DEV_ID_82540EP_LOM
    (0x1017, E1000MacType::I82540),       // E1000_DEV_ID_82540EP
    (0x1018, E1000MacType::I82541),       // E1000_DEV_ID_82541EI_MOBILE
    (0x1019, E1000MacType::I82547),       // E1000_DEV_ID_82547EI
    (0x101a, E1000MacType::I82547),       // E1000_DEV_ID_82547EI_MOBILE
    (0x101d, E1000MacType::I82546),       // E1000_DEV_ID_82546EB_QUAD_COPPER
    (0x101e, E1000MacType::I82540),       // E1000_DEV_ID_82540EP_LP
    (0x1026, E1000MacType::I82545Rev3),   // E1000_DEV_ID_82545GM_COPPER
    (0x1027, E1000MacType::I82545Rev3),   // E1000_DEV_ID_82545GM_FIBER
    (0x1028, E1000MacType::I82545Rev3),   // E1000_DEV_ID_82545GM_SERDES
    (0x1049, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_IGP_M_AMT
    (0x104a, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_IGP_AMT
    (0x104b, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_IGP_C
    (0x104c, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_IFE
    (0x104d, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_IGP_M
    (0x105e, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_COPPER
    (0x105f, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_FIBER
    (0x1060, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_SERDES
    (0x1075, E1000MacType::I82547Rev2),   // E1000_DEV_ID_82547GI
    (0x1076, E1000MacType::I82541Rev2),   // E1000_DEV_ID_82541GI
    (0x1077, E1000MacType::I82541Rev2),   // E1000_DEV_ID_82541GI_MOBILE
    (0x1078, E1000MacType::I82541Rev2),   // E1000_DEV_ID_82541ER
    (0x1079, E1000MacType::I82546Rev3),   // E1000_DEV_ID_82546GB_COPPER
    (0x107a, E1000MacType::I82546Rev3),   // E1000_DEV_ID_82546GB_FIBER
    (0x107b, E1000MacType::I82546Rev3),   // E1000_DEV_ID_82546GB_SERDES
    (0x107c, E1000MacType::I82541Rev2),   // E1000_DEV_ID_82541GI_LF
    (0x107d, E1000MacType::I82572),       // E1000_DEV_ID_82572EI_COPPER
    (0x107e, E1000MacType::I82572),       // E1000_DEV_ID_82572EI_FIBER
    (0x107f, E1000MacType::I82572),       // E1000_DEV_ID_82572EI_SERDES
    (0x108a, E1000MacType::I82546Rev3),   // E1000_DEV_ID_82546GB_PCIE
    (0x108b, E1000MacType::I82573),       // E1000_DEV_ID_82573E
    (0x108c, E1000MacType::I82573),       // E1000_DEV_ID_82573E_IAMT
    (0x1096, E1000MacType::I80003Es2lan), // E1000_DEV_ID_80003ES2LAN_COPPER_DPT
    (0x1098, E1000MacType::I80003Es2lan), // E1000_DEV_ID_80003ES2LAN_SERDES_DPT
    (0x1099, E1000MacType::I82546Rev3),   // E1000_DEV_ID_82546GB_QUAD_COPPER
    (0x109a, E1000MacType::I82573),       // E1000_DEV_ID_82573L
    (0x10a4, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_QUAD_COPPER
    (0x10a5, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_QUAD_FIBER
    (0x10a7, E1000MacType::I82575),       // E1000_DEV_ID_82575EB_COPPER
    (0x10a9, E1000MacType::I82575),       // E1000_DEV_ID_82575EB_FIBER_SERDES
    (0x10b5, E1000MacType::I82546Rev3),   // E1000_DEV_ID_82546GB_QUAD_COPPER_KSP3
    (0x10b9, E1000MacType::I82572),       // E1000_DEV_ID_82572EI
    (0x10ba, E1000MacType::I80003Es2lan), // E1000_DEV_ID_80003ES2LAN_COPPER_SPT
    (0x10bb, E1000MacType::I80003Es2lan), // E1000_DEV_ID_80003ES2LAN_SERDES_SPT
    (0x10bc, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_QUAD_COPPER_LP
    (0x10bd, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IGP_AMT
    (0x10bf, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IGP_M
    (0x10c0, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IFE
    (0x10c2, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IFE_G
    (0x10c3, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IFE_GT
    (0x10c4, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_IFE_GT
    (0x10c5, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_IFE_G
    (0x10c9, E1000MacType::I82576),       // E1000_DEV_ID_82576
    (0x10ca, E1000MacType::VfAdapt),      // E1000_DEV_ID_82576_VF
    (0x10cb, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IGP_M_V
    (0x10cc, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH10_R_BM_LM
    (0x10cd, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH10_R_BM_LF
    (0x10ce, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH10_R_BM_V
    (0x10d3, E1000MacType::I82574),       // E1000_DEV_ID_82574L
    (0x10d5, E1000MacType::I82571),       // E1000_DEV_ID_82571PT_QUAD_COPPER
    (0x10d6, E1000MacType::I82575),       // E1000_DEV_ID_82575GB_QUAD_COPPER
    (0x10d9, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_SERDES_DUAL
    (0x10da, E1000MacType::I82571),       // E1000_DEV_ID_82571EB_SERDES_QUAD
    (0x10de, E1000MacType::Ich10Lan),     // E1000_DEV_ID_ICH10_D_BM_LM
    (0x10df, E1000MacType::Ich10Lan),     // E1000_DEV_ID_ICH10_D_BM_LF
    (0x10e5, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_BM
    (0x10e6, E1000MacType::I82576),       // E1000_DEV_ID_82576_FIBER
    (0x10e7, E1000MacType::I82576),       // E1000_DEV_ID_82576_SERDES
    (0x10e8, E1000MacType::I82576),       // E1000_DEV_ID_82576_QUAD_COPPER
    (0x10ea, E1000MacType::PchLan),       // E1000_DEV_ID_PCH_M_HV_LM
    (0x10eb, E1000MacType::PchLan),       // E1000_DEV_ID_PCH_M_HV_LC
    (0x10ef, E1000MacType::PchLan),       // E1000_DEV_ID_PCH_D_HV_DM
    (0x10f0, E1000MacType::PchLan),       // E1000_DEV_ID_PCH_D_HV_DC
    (0x10f5, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IGP_M_AMT
    (0x10f6, E1000MacType::I82574),       // E1000_DEV_ID_82574LA
    (0x1501, E1000MacType::Ich8Lan),      // E1000_DEV_ID_ICH8_82567V_3
    (0x1502, E1000MacType::Pch2Lan),      // E1000_DEV_ID_PCH2_LV_LM
    (0x1503, E1000MacType::Pch2Lan),      // E1000_DEV_ID_PCH2_LV_V
    (0x150a, E1000MacType::I82576),       // E1000_DEV_ID_82576_NS
    (0x150c, E1000MacType::I82583),       // E1000_DEV_ID_82583V
    (0x150d, E1000MacType::I82576),       // E1000_DEV_ID_82576_SERDES_QUAD
    (0x150e, E1000MacType::I82580),       // E1000_DEV_ID_82580_COPPER
    (0x150f, E1000MacType::I82580),       // E1000_DEV_ID_82580_FIBER
    (0x1510, E1000MacType::I82580),       // E1000_DEV_ID_82580_SERDES
    (0x1511, E1000MacType::I82580),       // E1000_DEV_ID_82580_SGMII
    (0x1516, E1000MacType::I82580),       // E1000_DEV_ID_82580_COPPER_DUAL
    (0x1518, E1000MacType::I82576),       // E1000_DEV_ID_82576_NS_SERDES
    (0x1520, E1000MacType::VfAdaptI350),  // E1000_DEV_ID_I350_VF
    (0x1521, E1000MacType::I350),         // E1000_DEV_ID_I350_COPPER
    (0x1522, E1000MacType::I350),         // E1000_DEV_ID_I350_FIBER
    (0x1523, E1000MacType::I350),         // E1000_DEV_ID_I350_SERDES
    (0x1524, E1000MacType::I350),         // E1000_DEV_ID_I350_SGMII
    (0x1525, E1000MacType::Ich10Lan),     // E1000_DEV_ID_ICH10_D_BM_V
    (0x1526, E1000MacType::I82576),       // E1000_DEV_ID_82576_QUAD_COPPER_ET2
    (0x1527, E1000MacType::I82580),       // E1000_DEV_ID_82580_QUAD_FIBER
    (0x152d, E1000MacType::VfAdapt),      // E1000_DEV_ID_82576_VF_HV
    (0x152f, E1000MacType::VfAdaptI350),  // E1000_DEV_ID_I350_VF_HV
    (0x1533, E1000MacType::I210),         // E1000_DEV_ID_I210_COPPER
    (0x1534, E1000MacType::I210),         // E1000_DEV_ID_I210_COPPER_OEM1
    (0x1535, E1000MacType::I210),         // E1000_DEV_ID_I210_COPPER_IT
    (0x1536, E1000MacType::I210),         // E1000_DEV_ID_I210_FIBER
    (0x1537, E1000MacType::I210),         // E1000_DEV_ID_I210_SERDES
    (0x1538, E1000MacType::I210),         // E1000_DEV_ID_I210_SGMII
    (0x1539, E1000MacType::I211),         // E1000_DEV_ID_I211_COPPER
    (0x153a, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_LPT_I217_LM
    (0x153b, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_LPT_I217_V
    (0x1546, E1000MacType::I350),         // E1000_DEV_ID_I350_DA4
    (0x1559, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_LPTLP_I218_V
    (0x155a, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_LPTLP_I218_LM
    (0x156f, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_LM
    (0x1570, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_V
    (0x157b, E1000MacType::I210),         // E1000_DEV_ID_I210_COPPER_FLASHLESS
    (0x157c, E1000MacType::I210),         // E1000_DEV_ID_I210_SERDES_FLASHLESS
    (0x15a0, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_I218_LM2
    (0x15a1, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_I218_V2
    (0x15a2, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_I218_LM3
    (0x15a3, E1000MacType::PchLpt),       // E1000_DEV_ID_PCH_I218_V3
    (0x15b7, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_LM2
    (0x15b8, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_V2
    (0x15b9, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_LBG_I219_LM3
    (0x15bb, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CNP_I219_LM7
    (0x15bc, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CNP_I219_V7
    (0x15bd, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CNP_I219_LM6
    (0x15be, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_CNP_I219_V6
    (0x15d6, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_V5
    (0x15d7, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_LM4
    (0x15d8, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_V4
    (0x15df, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_ICP_I219_LM8
    (0x15e0, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_ICP_I219_V8
    (0x15e1, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_ICP_I219_LM9
    (0x15e2, E1000MacType::PchCnp),       // E1000_DEV_ID_PCH_ICP_I219_V9
    (0x15e3, E1000MacType::PchSpt),       // E1000_DEV_ID_PCH_SPT_I219_LM5
    (0x15f4, E1000MacType::PchTgp),       // E1000_DEV_ID_PCH_TGP_I219_LM15
    (0x15f5, E1000MacType::PchTgp),       // E1000_DEV_ID_PCH_TGP_I219_V15
    (0x15f6, E1000MacType::I210),         // E1000_DEV_ID_I210_SGMII_FLASHLESS
    (0x15f9, E1000MacType::PchTgp),       // E1000_DEV_ID_PCH_TGP_I219_LM14
    (0x15fa, E1000MacType::PchTgp),       // E1000_DEV_ID_PCH_TGP_I219_V14
    (0x15fb, E1000MacType::PchTgp),       // E1000_DEV_ID_PCH_TGP_I219_LM13
    (0x15fc, E1000MacType::PchTgp),       // E1000_DEV_ID_PCH_TGP_I219_V13
    (0x1a1c, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_ADL_I219_LM17
    (0x1a1d, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_ADL_I219_V17
    (0x1a1e, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_ADL_I219_LM16
    (0x1a1f, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_ADL_I219_V16
    (0x1f40, E1000MacType::I354),         // E1000_DEV_ID_I354_BACKPLANE_1GBPS
    (0x1f41, E1000MacType::I354),         // E1000_DEV_ID_I354_SGMII
    (0x1f45, E1000MacType::I354),         // E1000_DEV_ID_I354_BACKPLANE_2_5GBPS
    (0x294c, E1000MacType::Ich9Lan),      // E1000_DEV_ID_ICH9_IGP_C
    (0x550a, E1000MacType::PchMtp),       // E1000_DEV_ID_PCH_MTP_I219_LM18
    (0x550b, E1000MacType::PchMtp),       // E1000_DEV_ID_PCH_MTP_I219_V18
    (0x550c, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_ADL_I219_LM19
    (0x550d, E1000MacType::PchAdp),       // E1000_DEV_ID_PCH_ADL_I219_V19
    (0x550e, E1000MacType::PchMtp),       // E1000_DEV_ID_PCH_LNL_I219_LM20
    (0x550f, E1000MacType::PchMtp),       // E1000_DEV_ID_PCH_LNL_I219_V20
    (0x5510, E1000MacType::PchMtp),       // E1000_DEV_ID_PCH_LNL_I219_LM21
    (0x5511, E1000MacType::PchMtp),       // E1000_DEV_ID_PCH_LNL_I219_V21
    (0x57a0, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_ARL_I219_LM24
    (0x57a1, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_ARL_I219_V24
    (0x57b3, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_PTP_I219_LM25
    (0x57b4, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_PTP_I219_V25
    (0x57b5, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_PTP_I219_LM26
    (0x57b6, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_PTP_I219_V26
    (0x57b7, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_PTP_I219_LM27
    (0x57b8, E1000MacType::PchPtp),       // E1000_DEV_ID_PCH_PTP_I219_V27
    (0x57b9, E1000MacType::PchNvp),       // E1000_DEV_ID_PCH_NVL_I219_LM29
    (0x57ba, E1000MacType::PchNvp),       // E1000_DEV_ID_PCH_NVL_I219_V29
];

/// Select the shared MAC generation for one Intel PCI device identifier.
pub fn set_mac_type(device_id: u16) -> Result<E1000MacType, UnknownE1000Device> {
    MAC_TYPES
        .iter()
        .find_map(|(id, mac_type)| (*id == device_id).then_some(*mac_type))
        .ok_or(UnknownE1000Device(device_id))
}

// upstream: e1000_api.c e1000_get_bus_info()
pub fn get_bus_info<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.get_bus_info().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_clear_vfta()
pub fn clear_vfta<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.clear_vfta();
}

// upstream: e1000_api.c e1000_write_vfta()
pub fn write_vfta<H: E1000ApiHardware>(hw: &mut H, offset: u32, value: u32) {
    let _ = hw.write_vfta(offset, value);
}

// upstream: e1000_api.c e1000_update_mc_addr_list()
pub fn update_mc_addr_list<H: E1000ApiHardware>(hw: &mut H, addresses: &[u8], count: u32) {
    let _ = hw.update_mc_addr_list(addresses, count);
}

// upstream: e1000_api.c e1000_force_mac_fc()
pub fn force_mac_fc<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.force_mac_fc_generic()
}

// upstream: e1000_api.c e1000_check_for_link()
pub fn check_for_link<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.check_for_link().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_check_mng_mode()
pub fn check_mng_mode<H: E1000ApiHardware>(hw: &mut H) -> bool {
    hw.check_mng_mode().unwrap_or(false)
}

// upstream: e1000_api.c e1000_mng_write_dhcp_info()
pub fn mng_write_dhcp_info<H: E1000ApiHardware>(hw: &mut H, buffer: &[u8]) -> E1000ApiResult {
    hw.mng_write_dhcp_info_generic(buffer)
}

// upstream: e1000_api.c e1000_reset_hw()
pub fn reset_hw<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.reset_hw().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_init_hw()
pub fn init_hw<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.init_hw().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_setup_link()
pub fn setup_link<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.setup_link().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_get_speed_and_duplex()
pub fn get_speed_and_duplex<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult<(u16, u16)> {
    hw.get_link_up_info().unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_setup_led()
pub fn setup_led<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.setup_led().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_cleanup_led()
pub fn cleanup_led<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.cleanup_led().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_blink_led()
pub fn blink_led<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.blink_led().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_id_led_init()
pub fn id_led_init<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.id_led_init().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_led_on()
pub fn led_on<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.led_on().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_led_off()
pub fn led_off<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.led_off().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_reset_adaptive()
pub fn reset_adaptive<H: E1000ApiHardware>(hw: &mut H) {
    hw.reset_adaptive_generic();
}

// upstream: e1000_api.c e1000_update_adaptive()
pub fn update_adaptive<H: E1000ApiHardware>(hw: &mut H) {
    hw.update_adaptive_generic();
}

// upstream: e1000_api.c e1000_disable_pcie_master()
pub fn disable_pcie_master<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.disable_pcie_master_generic()
}

// upstream: e1000_api.c e1000_config_collision_dist()
pub fn config_collision_dist<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.config_collision_dist();
}

// upstream: e1000_api.c e1000_rar_set()
pub fn rar_set<H: E1000ApiHardware>(hw: &mut H, address: [u8; 6], index: u32) -> E1000ApiResult {
    hw.rar_set(address, index).unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_validate_mdi_setting()
pub fn validate_mdi_setting<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.validate_mdi_setting().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_hash_mc_addr()
pub fn hash_mc_addr<H: E1000ApiHardware>(hw: &mut H, address: [u8; 6]) -> u32 {
    hw.hash_mc_addr_generic(address)
}

// upstream: e1000_api.c e1000_enable_tx_pkt_filtering()
pub fn enable_tx_pkt_filtering<H: E1000ApiHardware>(hw: &mut H) -> bool {
    hw.enable_tx_pkt_filtering_generic()
}

// upstream: e1000_api.c e1000_mng_host_if_write()
pub fn mng_host_if_write<H: E1000ApiHardware>(
    hw: &mut H,
    buffer: &[u8],
    offset: u16,
) -> E1000ApiResult<u8> {
    hw.mng_host_if_write_generic(buffer, offset)
}

// upstream: e1000_api.c e1000_mng_write_cmd_header()
pub fn mng_write_cmd_header<H: E1000ApiHardware>(hw: &mut H, header: &[u8]) -> E1000ApiResult {
    hw.mng_write_cmd_header_generic(header)
}

// upstream: e1000_api.c e1000_mng_enable_host_if()
pub fn mng_enable_host_if<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.mng_enable_host_if_generic()
}

// upstream: e1000_api.c e1000_set_obff_timer()
pub fn set_obff_timer<H: E1000ApiHardware>(hw: &mut H, itr: u32) -> E1000ApiResult {
    hw.set_obff_timer(itr).unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_check_reset_block()
pub fn check_reset_block<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.check_reset_block().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_read_phy_reg()
pub fn read_phy_reg<H: E1000ApiHardware>(hw: &mut H, offset: u32) -> E1000ApiResult<Option<u16>> {
    hw.read_phy_reg(offset)
        .map(|result| result.map(Some))
        .unwrap_or(Ok(None))
}

// upstream: e1000_api.c e1000_write_phy_reg()
pub fn write_phy_reg<H: E1000ApiHardware>(hw: &mut H, offset: u32, value: u16) -> E1000ApiResult {
    hw.write_phy_reg(offset, value).unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_release_phy()
pub fn release_phy<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.release_phy();
}

// upstream: e1000_api.c e1000_acquire_phy()
pub fn acquire_phy<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.acquire_phy().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_cfg_on_link_up()
pub fn cfg_on_link_up<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.cfg_on_link_up().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_read_kmrn_reg()
pub fn read_kmrn_reg<H: E1000ApiHardware>(hw: &mut H, offset: u32) -> E1000ApiResult<u16> {
    hw.read_kmrn_reg_generic(offset)
}

// upstream: e1000_api.c e1000_write_kmrn_reg()
pub fn write_kmrn_reg<H: E1000ApiHardware>(hw: &mut H, offset: u32, value: u16) -> E1000ApiResult {
    hw.write_kmrn_reg_generic(offset, value)
}

// upstream: e1000_api.c e1000_get_cable_length()
pub fn get_cable_length<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.get_cable_length().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_get_phy_info()
pub fn get_phy_info<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.get_phy_info().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_phy_hw_reset()
pub fn phy_hw_reset<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.phy_hw_reset().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_phy_commit()
pub fn phy_commit<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.phy_commit().unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_set_d0_lplu_state()
pub fn set_d0_lplu_state<H: E1000ApiHardware>(hw: &mut H, active: bool) -> E1000ApiResult {
    hw.set_d0_lplu_state(active).unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_set_d3_lplu_state()
pub fn set_d3_lplu_state<H: E1000ApiHardware>(hw: &mut H, active: bool) -> E1000ApiResult {
    hw.set_d3_lplu_state(active).unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_read_mac_addr()
pub fn read_mac_addr<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult<[u8; 6]> {
    hw.read_mac_addr()
        .unwrap_or_else(|| hw.read_mac_addr_generic())
}

// upstream: e1000_api.c e1000_read_pba_string()
pub fn read_pba_string<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult<alloc::vec::Vec<u8>> {
    hw.read_pba_string_generic()
}

// upstream: e1000_api.c e1000_read_pba_length()
pub fn read_pba_length<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult<u32> {
    hw.read_pba_length_generic()
}

// upstream: e1000_api.c e1000_read_pba_num()
pub fn read_pba_num<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult<u32> {
    hw.read_pba_num_generic()
}

// upstream: e1000_api.c e1000_validate_nvm_checksum()
pub fn validate_nvm_checksum<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.validate_nvm_checksum()
        .unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_update_nvm_checksum()
pub fn update_nvm_checksum<H: E1000ApiHardware>(hw: &mut H) -> E1000ApiResult {
    hw.update_nvm_checksum()
        .unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_reload_nvm()
pub fn reload_nvm<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.reload_nvm();
}

// upstream: e1000_api.c e1000_read_nvm()
pub fn read_nvm<H: E1000ApiHardware>(
    hw: &mut H,
    offset: u16,
    words: u16,
) -> E1000ApiResult<alloc::vec::Vec<u16>> {
    hw.read_nvm(offset, words)
        .unwrap_or(Err(E1000ApiError::Config))
}

// upstream: e1000_api.c e1000_write_nvm()
pub fn write_nvm<H: E1000ApiHardware>(hw: &mut H, offset: u16, words: &[u16]) -> E1000ApiResult {
    hw.write_nvm(offset, words).unwrap_or(Ok(()))
}

// upstream: e1000_api.c e1000_write_8bit_ctrl_reg()
pub fn write_8bit_ctrl_reg<H: E1000ApiHardware>(
    hw: &mut H,
    register: u32,
    offset: u32,
    value: u8,
) -> E1000ApiResult {
    hw.write_8bit_ctrl_reg_generic(register, offset, value)
}

// upstream: e1000_api.c e1000_power_up_phy()
pub fn power_up_phy<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.power_up_phy();
    let _ = setup_link(hw);
}

// upstream: e1000_api.c e1000_power_down_phy()
pub fn power_down_phy<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.power_down_phy();
}

// upstream: e1000_api.c e1000_power_up_fiber_serdes_link()
pub fn power_up_fiber_serdes_link<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.power_up_fiber_serdes_link();
}

// upstream: e1000_api.c e1000_shutdown_fiber_serdes_link()
pub fn shutdown_fiber_serdes_link<H: E1000ApiHardware>(hw: &mut H) {
    let _ = hw.shutdown_fiber_serdes_link();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockHardware {
        mac_type: Option<E1000MacType>,
        events: alloc::vec::Vec<&'static str>,
        mapped: bool,
    }

    impl E1000ApiHardware for MockHardware {
        fn registers_mapped(&self) -> bool {
            self.mapped
        }
        fn set_mac_type(&mut self, mac_type: E1000MacType) {
            self.mac_type = Some(mac_type);
        }
        fn init_generic_ops(&mut self) {
            self.events.push("generic");
        }
        fn init_family_ops(&mut self, family: E1000InitFamily) -> E1000ApiResult {
            let event = match family {
                E1000InitFamily::I82540 => "82540",
                _ => "other-family",
            };
            self.events.push(event);
            Ok(())
        }
        fn init_mac_params(&mut self) -> Option<E1000ApiResult> {
            self.events.push("mac");
            Some(Ok(()))
        }
        fn init_nvm_params(&mut self) -> Option<E1000ApiResult> {
            self.events.push("nvm");
            Some(Ok(()))
        }
        fn init_phy_params(&mut self) -> Option<E1000ApiResult> {
            self.events.push("phy");
            Some(Ok(()))
        }
        fn init_mbx_params(&mut self) -> Option<E1000ApiResult> {
            self.events.push("mbx");
            Some(Ok(()))
        }
    }

    #[test]
    fn device_id_dispatch_matches_shared_mac_families() {
        assert_eq!(set_mac_type(0x100e), Ok(E1000MacType::I82540));
        assert_eq!(set_mac_type(0x10c9), Ok(E1000MacType::I82576));
        assert_eq!(set_mac_type(0x1502), Ok(E1000MacType::Pch2Lan));
        assert_eq!(set_mac_type(0x1533), Ok(E1000MacType::I210));
        assert!(set_mac_type(0xdead).is_err());
    }

    #[test]
    fn igb_queue_family_matches_82575_and_newer_types() {
        assert!(E1000MacType::I82575.igb_advanced_queues());
        assert!(E1000MacType::I82576.igb_advanced_queues());
        assert!(!E1000MacType::I82574.igb_advanced_queues());
    }

    #[test]
    fn setup_init_funcs_preserves_mapping_and_initializer_order() {
        let mut hw = MockHardware {
            mapped: true,
            ..MockHardware::default()
        };
        setup_init_funcs(&mut hw, 0x100e, false).unwrap();
        assert_eq!(hw.mac_type, Some(E1000MacType::I82540));
        assert_eq!(hw.events, ["generic", "82540"]);

        hw.events.clear();
        setup_init_funcs(&mut hw, 0x100e, true).unwrap();
        assert_eq!(hw.events, ["generic", "82540", "mac", "nvm", "phy", "mbx"]);
    }

    #[test]
    fn setup_init_funcs_fails_closed_without_mapping_or_mac_type() {
        let mut unmapped = MockHardware::default();
        assert_eq!(
            setup_init_funcs(&mut unmapped, 0x100e, false),
            Err(E1000ApiError::Config)
        );
        let mut mapped = MockHardware {
            mapped: true,
            ..MockHardware::default()
        };
        assert_eq!(
            setup_init_funcs(&mut mapped, 0xdead, false),
            Err(E1000ApiError::MacInit)
        );
    }

    #[test]
    fn api_wrappers_preserve_source_optional_pointer_defaults() {
        let mut hw = MockHardware::default();
        assert_eq!(check_for_link(&mut hw), Err(E1000ApiError::Config));
        assert_eq!(read_phy_reg(&mut hw, 1), Ok(None));
        assert_eq!(write_nvm(&mut hw, 0, &[0x1234]), Ok(()));
        assert_eq!(update_nvm_checksum(&mut hw), Err(E1000ApiError::Config));
        assert_eq!(setup_led(&mut hw), Ok(()));
        assert!(!check_mng_mode(&mut hw));
    }
}
