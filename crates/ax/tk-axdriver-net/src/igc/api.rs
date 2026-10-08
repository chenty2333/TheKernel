//! IGC shared API and function-table dispatch translated from FreeBSD.
//!
//! Source: `sys/dev/igc/igc_api.c` at FreeBSD commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright 2021 Intel Corp; Copyright 2021 Rubicon Communications, LLC.

use alloc::vec::Vec;

use axdriver_base::{DevError, DevResult};

use super::ids;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgcMacType {
    I225,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgcNvmType {
    EepromSpi,
    FlashHardware,
    Invm,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgcMediaType {
    Copper,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgcPhyType {
    None,
    I225,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IgcNvmInfo {
    pub word_size: u32,
    pub opcode_bits: u8,
    pub delay_usec: u32,
    pub page_size: u16,
    pub address_bits: u8,
    pub nvm_type: IgcNvmType,
}
impl Default for IgcNvmInfo {
    fn default() -> Self {
        Self {
            word_size: 0,
            opcode_bits: 0,
            delay_usec: 0,
            page_size: 0,
            address_bits: 0,
            nvm_type: IgcNvmType::EepromSpi,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IgcMacInfo {
    pub media_type: IgcMediaType,
    pub mta_register_count: u16,
    pub rar_entry_count: u16,
    pub clear_semaphore_once: bool,
    pub asf_firmware_present: bool,
}
impl Default for IgcMacInfo {
    fn default() -> Self {
        Self {
            media_type: IgcMediaType::Copper,
            mta_register_count: 0,
            rar_entry_count: 0,
            clear_semaphore_once: false,
            asf_firmware_present: false,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IgcPhyInfo {
    pub media_type: IgcMediaType,
    pub phy_type: IgcPhyType,
    pub autoneg_mask: u32,
    pub reset_delay_usec: u32,
    pub phy_id: u32,
}
impl Default for IgcPhyInfo {
    fn default() -> Self {
        Self {
            media_type: IgcMediaType::Copper,
            phy_type: IgcPhyType::None,
            autoneg_mask: 0,
            reset_delay_usec: 0,
            phy_id: 0,
        }
    }
}

/// Function pointer identity installed into a shared-code operation table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgcApiCallback {
    MacInitParamsGeneric,
    MacInitParamsI225,
    NvmInitParamsGeneric,
    NvmInitParamsI225,
    PhyInitParamsGeneric,
    PhyInitParamsI225,
    BusInfoGeneric,
    ClearVftaI225,
    WriteVftaI225,
    UpdateMcAddrListI225,
    CheckLinkI225,
    ResetHwI225,
    InitHwI225,
    SetupLinkI225,
    SetupCopperLinkI225,
    GetLinkInfoI225,
    DisablePcieMasterGeneric,
    CollisionDistI225,
    RarSetI225,
    ReadMacAddrGeneric,
    GetPhyIdGeneric,
    ValidateMdiI225,
    PhyAcquireI225,
    PhyCheckResetBlockI225,
    PhyForceSpeedDuplexI225,
    PhyGetInfoI225,
    PhySetPageI225,
    PhyReadI225,
    PhyReadLockedI225,
    PhyReadPageI225,
    PhyReleaseI225,
    PhyResetI225,
    PhySetD0LpluI225,
    PhySetD3LpluI225,
    PhyWriteI225,
    PhyWriteLockedI225,
    PhyWritePageI225,
    PhyPowerUpI225,
    PhyPowerDownI225,
    NvmAcquireI225,
    NvmReadI225,
    NvmReleaseI225,
    NvmReloadI225,
    NvmUpdateI225,
    NvmValidateI225,
    NvmWriteI225,
    ForceMacFcGeneric,
    HashMcAddrGeneric,
    ReadPbaStringGeneric,
    UpdateNvmChecksumGeneric,
    ReadPbaGeneric,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IgcApiRequest {
    None,
    Vfta {
        offset: u32,
        value: u32,
    },
    Multicast {
        packed_addresses: Vec<u8>,
        count: u32,
    },
    RarSet {
        address: [u8; 6],
        index: u32,
    },
    PhyRead {
        offset: u32,
    },
    PhyWrite {
        offset: u32,
        value: u16,
    },
    NvmRead {
        offset: u16,
        words: u16,
    },
    NvmWrite {
        offset: u16,
        words: Vec<u16>,
    },
    Lplu {
        active: bool,
    },
    PbaString {
        capacity: usize,
    },
    HashAddress {
        address: [u8; 6],
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IgcApiValue {
    Unit,
    U16(u16),
    U32(u32),
    Bool(bool),
    LinkInfo { speed_mbps: u16, full_duplex: bool },
    Bytes(Vec<u8>),
    Words(Vec<u16>),
}

/// Platform-specific realization of the selected I225 shared-code callback.
pub trait IgcApiBackend {
    fn invoke(
        &mut self,
        callback: IgcApiCallback,
        request: IgcApiRequest,
    ) -> DevResult<IgcApiValue>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IgcMacOps {
    pub init_params: Option<IgcApiCallback>,
    pub check_for_link: Option<IgcApiCallback>,
    pub clear_vfta: Option<IgcApiCallback>,
    pub get_bus_info: Option<IgcApiCallback>,
    pub get_link_up_info: Option<IgcApiCallback>,
    pub update_mc_addr_list: Option<IgcApiCallback>,
    pub reset_hw: Option<IgcApiCallback>,
    pub init_hw: Option<IgcApiCallback>,
    pub setup_link: Option<IgcApiCallback>,
    pub setup_physical_interface: Option<IgcApiCallback>,
    pub write_vfta: Option<IgcApiCallback>,
    pub config_collision_dist: Option<IgcApiCallback>,
    pub rar_set: Option<IgcApiCallback>,
    pub read_mac_addr: Option<IgcApiCallback>,
    pub validate_mdi_setting: Option<IgcApiCallback>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IgcNvmOps {
    pub init_params: Option<IgcApiCallback>,
    pub acquire: Option<IgcApiCallback>,
    pub read: Option<IgcApiCallback>,
    pub release: Option<IgcApiCallback>,
    pub reload: Option<IgcApiCallback>,
    pub update: Option<IgcApiCallback>,
    pub validate: Option<IgcApiCallback>,
    pub write: Option<IgcApiCallback>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IgcPhyOps {
    pub init_params: Option<IgcApiCallback>,
    pub acquire: Option<IgcApiCallback>,
    pub check_reset_block: Option<IgcApiCallback>,
    pub force_speed_duplex: Option<IgcApiCallback>,
    pub get_info: Option<IgcApiCallback>,
    pub set_page: Option<IgcApiCallback>,
    pub read: Option<IgcApiCallback>,
    pub read_locked: Option<IgcApiCallback>,
    pub read_page: Option<IgcApiCallback>,
    pub release: Option<IgcApiCallback>,
    pub reset: Option<IgcApiCallback>,
    pub set_d0_lplu_state: Option<IgcApiCallback>,
    pub set_d3_lplu_state: Option<IgcApiCallback>,
    pub write: Option<IgcApiCallback>,
    pub write_locked: Option<IgcApiCallback>,
    pub write_page: Option<IgcApiCallback>,
    pub power_up: Option<IgcApiCallback>,
    pub power_down: Option<IgcApiCallback>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IgcHardware {
    pub device_id: u16,
    pub mac_type: Option<IgcMacType>,
    pub registers_mapped: bool,
    pub mac_ops: IgcMacOps,
    pub nvm_ops: IgcNvmOps,
    pub phy_ops: IgcPhyOps,
    pub nvm_info: IgcNvmInfo,
    pub mac_info: IgcMacInfo,
    pub phy_info: IgcPhyInfo,
}

impl IgcHardware {
    pub fn new(device_id: u16, registers_mapped: bool) -> Self {
        Self {
            device_id,
            mac_type: None,
            registers_mapped,
            mac_ops: IgcMacOps {
                init_params: None,
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
                config_collision_dist: None,
                rar_set: None,
                read_mac_addr: None,
                validate_mdi_setting: None,
            },
            nvm_ops: IgcNvmOps {
                init_params: None,
                acquire: None,
                read: None,
                release: None,
                reload: None,
                update: None,
                validate: None,
                write: None,
            },
            phy_ops: IgcPhyOps {
                init_params: None,
                acquire: None,
                check_reset_block: None,
                force_speed_duplex: None,
                get_info: None,
                set_page: None,
                read: None,
                read_locked: None,
                read_page: None,
                release: None,
                reset: None,
                set_d0_lplu_state: None,
                set_d3_lplu_state: None,
                write: None,
                write_locked: None,
                write_page: None,
                power_up: None,
                power_down: None,
            },
            nvm_info: IgcNvmInfo::default(),
            mac_info: IgcMacInfo::default(),
            phy_info: IgcPhyInfo::default(),
        }
    }
}

// upstream: igc_api.c igc_set_mac_type()
pub fn igc_set_mac_type(hw: &mut IgcHardware) -> DevResult<IgcMacType> {
    if ids::identify(ids::INTEL_VENDOR, hw.device_id).is_none() {
        return Err(DevError::Unsupported);
    }
    hw.mac_type = Some(IgcMacType::I225);
    Ok(IgcMacType::I225)
}

pub(super) fn init_generic_ops(hw: &mut IgcHardware) {
    hw.mac_ops = IgcMacOps {
        init_params: Some(IgcApiCallback::MacInitParamsGeneric),
        check_for_link: Some(IgcApiCallback::CheckLinkI225),
        clear_vfta: Some(IgcApiCallback::ClearVftaI225),
        get_bus_info: Some(IgcApiCallback::BusInfoGeneric),
        get_link_up_info: Some(IgcApiCallback::GetLinkInfoI225),
        update_mc_addr_list: Some(IgcApiCallback::UpdateMcAddrListI225),
        reset_hw: Some(IgcApiCallback::ResetHwI225),
        init_hw: Some(IgcApiCallback::InitHwI225),
        setup_link: Some(IgcApiCallback::SetupLinkI225),
        setup_physical_interface: Some(IgcApiCallback::SetupCopperLinkI225),
        write_vfta: Some(IgcApiCallback::WriteVftaI225),
        config_collision_dist: Some(IgcApiCallback::CollisionDistI225),
        rar_set: Some(IgcApiCallback::RarSetI225),
        read_mac_addr: Some(IgcApiCallback::ReadMacAddrGeneric),
        validate_mdi_setting: Some(IgcApiCallback::ValidateMdiI225),
    };
    hw.nvm_ops = IgcNvmOps {
        init_params: Some(IgcApiCallback::NvmInitParamsGeneric),
        acquire: Some(IgcApiCallback::NvmAcquireI225),
        read: Some(IgcApiCallback::NvmReadI225),
        release: Some(IgcApiCallback::NvmReleaseI225),
        reload: Some(IgcApiCallback::NvmReloadI225),
        update: Some(IgcApiCallback::NvmUpdateI225),
        validate: Some(IgcApiCallback::NvmValidateI225),
        write: Some(IgcApiCallback::NvmWriteI225),
    };
    hw.phy_ops = IgcPhyOps {
        init_params: Some(IgcApiCallback::PhyInitParamsGeneric),
        acquire: Some(IgcApiCallback::PhyAcquireI225),
        check_reset_block: Some(IgcApiCallback::PhyCheckResetBlockI225),
        force_speed_duplex: Some(IgcApiCallback::PhyForceSpeedDuplexI225),
        get_info: Some(IgcApiCallback::PhyGetInfoI225),
        set_page: Some(IgcApiCallback::PhySetPageI225),
        read: Some(IgcApiCallback::PhyReadI225),
        read_locked: Some(IgcApiCallback::PhyReadLockedI225),
        read_page: Some(IgcApiCallback::PhyReadPageI225),
        release: Some(IgcApiCallback::PhyReleaseI225),
        reset: Some(IgcApiCallback::PhyResetI225),
        set_d0_lplu_state: Some(IgcApiCallback::PhySetD0LpluI225),
        set_d3_lplu_state: Some(IgcApiCallback::PhySetD3LpluI225),
        write: Some(IgcApiCallback::PhyWriteI225),
        write_locked: Some(IgcApiCallback::PhyWriteLockedI225),
        write_page: Some(IgcApiCallback::PhyWritePageI225),
        power_up: Some(IgcApiCallback::PhyPowerUpI225),
        power_down: Some(IgcApiCallback::PhyPowerDownI225),
    };
}

// upstream: igc_api.c igc_init_mac_params()
pub fn igc_init_mac_params<B: IgcApiBackend>(hw: &IgcHardware, backend: &mut B) -> DevResult {
    let callback = hw.mac_ops.init_params.ok_or(DevError::Unsupported)?;
    expect_unit(backend.invoke(callback, IgcApiRequest::None)?)
}

// upstream: igc_api.c igc_init_nvm_params()
pub fn igc_init_nvm_params<B: IgcApiBackend>(hw: &IgcHardware, backend: &mut B) -> DevResult {
    let callback = hw.nvm_ops.init_params.ok_or(DevError::Unsupported)?;
    expect_unit(backend.invoke(callback, IgcApiRequest::None)?)
}

// upstream: igc_api.c igc_init_phy_params()
pub fn igc_init_phy_params<B: IgcApiBackend>(hw: &IgcHardware, backend: &mut B) -> DevResult {
    let callback = hw.phy_ops.init_params.ok_or(DevError::Unsupported)?;
    expect_unit(backend.invoke(callback, IgcApiRequest::None)?)
}

fn expect_unit(value: IgcApiValue) -> DevResult {
    if value == IgcApiValue::Unit {
        Ok(())
    } else {
        Err(DevError::Io)
    }
}

// upstream: igc_api.c igc_setup_init_funcs()
pub fn igc_setup_init_funcs<B: IgcApiBackend>(
    hw: &mut IgcHardware,
    backend: &mut B,
    init_device: bool,
) -> DevResult {
    igc_set_mac_type(hw)?;
    if !hw.registers_mapped {
        return Err(DevError::BadState);
    }
    init_generic_ops(hw);
    super::i225::init_function_pointers_i225(hw);
    if init_device {
        igc_init_mac_params(hw, backend)?;
        igc_init_nvm_params(hw, backend)?;
        igc_init_phy_params(hw, backend)?;
    }
    Ok(())
}

fn invoke_optional<B: IgcApiBackend>(
    b: &mut B,
    c: Option<IgcApiCallback>,
    r: IgcApiRequest,
) -> DevResult<IgcApiValue> {
    match c {
        Some(c) => b.invoke(c, r),
        None => Ok(IgcApiValue::Unit),
    }
}
fn invoke_required<B: IgcApiBackend>(
    b: &mut B,
    c: Option<IgcApiCallback>,
    r: IgcApiRequest,
) -> DevResult<IgcApiValue> {
    b.invoke(c.ok_or(DevError::Unsupported)?, r)
}
fn unit(v: IgcApiValue) -> DevResult {
    if v == IgcApiValue::Unit {
        Ok(())
    } else {
        Err(DevError::Io)
    }
}

// upstream: igc_api.c igc_get_bus_info()
pub fn igc_get_bus_info<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(
        b,
        h.mac_ops.get_bus_info,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_clear_vfta()
pub fn igc_clear_vfta<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(
        b,
        h.mac_ops.clear_vfta,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_write_vfta()
pub fn igc_write_vfta<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    offset: u32,
    value: u32,
) -> DevResult {
    unit(invoke_optional(
        b,
        h.mac_ops.write_vfta,
        IgcApiRequest::Vfta { offset, value },
    )?)
}
// upstream: igc_api.c igc_update_mc_addr_list()
pub fn igc_update_mc_addr_list<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    packed_addresses: Vec<u8>,
    count: u32,
) -> DevResult {
    unit(invoke_optional(
        b,
        h.mac_ops.update_mc_addr_list,
        IgcApiRequest::Multicast {
            packed_addresses,
            count,
        },
    )?)
}
// upstream: igc_api.c igc_force_mac_fc()
pub fn igc_force_mac_fc<B: IgcApiBackend>(b: &mut B) -> DevResult {
    unit(b.invoke(IgcApiCallback::ForceMacFcGeneric, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_check_for_link()
pub fn igc_check_for_link<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_required(
        b,
        h.mac_ops.check_for_link,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_reset_hw()
pub fn igc_reset_hw<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_required(b, h.mac_ops.reset_hw, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_init_hw()
pub fn igc_init_hw<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_required(b, h.mac_ops.init_hw, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_setup_link()
pub fn igc_setup_link<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_required(
        b,
        h.mac_ops.setup_link,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_get_speed_and_duplex()
pub fn igc_get_speed_and_duplex<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
) -> DevResult<(u16, bool)> {
    match invoke_required(b, h.mac_ops.get_link_up_info, IgcApiRequest::None)? {
        IgcApiValue::LinkInfo {
            speed_mbps,
            full_duplex,
        } => Ok((speed_mbps, full_duplex)),
        _ => Err(DevError::Io),
    }
}
// upstream: igc_api.c igc_disable_pcie_master()
pub fn igc_disable_pcie_master<B: IgcApiBackend>(b: &mut B) -> DevResult {
    unit(b.invoke(
        IgcApiCallback::DisablePcieMasterGeneric,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_config_collision_dist()
pub fn igc_config_collision_dist<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(
        b,
        h.mac_ops.config_collision_dist,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_rar_set()
pub fn igc_rar_set<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    address: [u8; 6],
    index: u32,
) -> DevResult {
    unit(invoke_optional(
        b,
        h.mac_ops.rar_set,
        IgcApiRequest::RarSet { address, index },
    )?)
}
// upstream: igc_api.c igc_validate_mdi_setting()
pub fn igc_validate_mdi_setting<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(
        b,
        h.mac_ops.validate_mdi_setting,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_hash_mc_addr()
pub fn igc_hash_mc_addr<B: IgcApiBackend>(b: &mut B, address: [u8; 6]) -> DevResult<u32> {
    match b.invoke(
        IgcApiCallback::HashMcAddrGeneric,
        IgcApiRequest::HashAddress { address },
    )? {
        IgcApiValue::U32(v) => Ok(v),
        _ => Err(DevError::Io),
    }
}
// upstream: igc_api.c igc_check_reset_block()
pub fn igc_check_reset_block<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(
        b,
        h.phy_ops.check_reset_block,
        IgcApiRequest::None,
    )?)
}
// upstream: igc_api.c igc_read_phy_reg()
pub fn igc_read_phy_reg<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    offset: u32,
) -> DevResult<u16> {
    match invoke_optional(b, h.phy_ops.read, IgcApiRequest::PhyRead { offset })? {
        IgcApiValue::U16(v) => Ok(v),
        IgcApiValue::Unit => Ok(0),
        _ => Err(DevError::Io),
    }
}
// upstream: igc_api.c igc_write_phy_reg()
pub fn igc_write_phy_reg<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    offset: u32,
    value: u16,
) -> DevResult {
    unit(invoke_optional(
        b,
        h.phy_ops.write,
        IgcApiRequest::PhyWrite { offset, value },
    )?)
}
// upstream: igc_api.c igc_release_phy()
pub fn igc_release_phy<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(b, h.phy_ops.release, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_acquire_phy()
pub fn igc_acquire_phy<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(b, h.phy_ops.acquire, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_get_phy_info()
pub fn igc_get_phy_info<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(b, h.phy_ops.get_info, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_phy_hw_reset()
pub fn igc_phy_hw_reset<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(b, h.phy_ops.reset, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_set_d0_lplu_state()
pub fn igc_set_d0_lplu_state<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    active: bool,
) -> DevResult {
    unit(invoke_optional(
        b,
        h.phy_ops.set_d0_lplu_state,
        IgcApiRequest::Lplu { active },
    )?)
}
// upstream: igc_api.c igc_set_d3_lplu_state()
pub fn igc_set_d3_lplu_state<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    active: bool,
) -> DevResult {
    unit(invoke_optional(
        b,
        h.phy_ops.set_d3_lplu_state,
        IgcApiRequest::Lplu { active },
    )?)
}
// upstream: igc_api.c igc_read_mac_addr()
pub fn igc_read_mac_addr<B: IgcApiBackend>(b: &mut B) -> DevResult<[u8; 6]> {
    match b.invoke(IgcApiCallback::ReadMacAddrGeneric, IgcApiRequest::None)? {
        IgcApiValue::Bytes(v) if v.len() >= 6 => Ok(v[..6].try_into().unwrap()),
        _ => Err(DevError::Io),
    }
}
// upstream: igc_api.c igc_read_pba_string()
pub fn igc_read_pba_string<B: IgcApiBackend>(b: &mut B, capacity: usize) -> DevResult<Vec<u8>> {
    match b.invoke(
        IgcApiCallback::ReadPbaGeneric,
        IgcApiRequest::PbaString { capacity },
    )? {
        IgcApiValue::Bytes(v) if v.len() <= capacity => Ok(v),
        _ => Err(DevError::Io),
    }
}
// upstream: igc_api.c igc_validate_nvm_checksum()
pub fn igc_validate_nvm_checksum<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_required(b, h.nvm_ops.validate, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_update_nvm_checksum()
pub fn igc_update_nvm_checksum<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_required(b, h.nvm_ops.update, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_reload_nvm()
pub fn igc_reload_nvm<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(b, h.nvm_ops.reload, IgcApiRequest::None)?)
}
// upstream: igc_api.c igc_read_nvm()
pub fn igc_read_nvm<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    offset: u16,
    words: u16,
) -> DevResult<Vec<u16>> {
    match invoke_required(b, h.nvm_ops.read, IgcApiRequest::NvmRead { offset, words })? {
        IgcApiValue::Words(v) if v.len() == words as usize => Ok(v),
        _ => Err(DevError::Io),
    }
}
// upstream: igc_api.c igc_write_nvm()
pub fn igc_write_nvm<B: IgcApiBackend>(
    h: &IgcHardware,
    b: &mut B,
    offset: u16,
    words: Vec<u16>,
) -> DevResult {
    unit(invoke_optional(
        b,
        h.nvm_ops.write,
        IgcApiRequest::NvmWrite { offset, words },
    )?)
}
// upstream: igc_api.c igc_power_up_phy()
pub fn igc_power_up_phy<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(b, h.phy_ops.power_up, IgcApiRequest::None)?)?;
    igc_setup_link(h, b)
}
// upstream: igc_api.c igc_power_down_phy()
pub fn igc_power_down_phy<B: IgcApiBackend>(h: &IgcHardware, b: &mut B) -> DevResult {
    unit(invoke_optional(
        b,
        h.phy_ops.power_down,
        IgcApiRequest::None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Backend {
        calls: Vec<IgcApiCallback>,
        fail_at: Option<IgcApiCallback>,
    }
    impl IgcApiBackend for Backend {
        fn invoke(
            &mut self,
            callback: IgcApiCallback,
            _request: IgcApiRequest,
        ) -> DevResult<IgcApiValue> {
            self.calls.push(callback);
            if self.fail_at == Some(callback) {
                Err(DevError::Io)
            } else {
                Ok(IgcApiValue::Unit)
            }
        }
    }

    #[test]
    fn mac_type_and_setup_match_all_bound_i225_ids_and_initialize_in_order() {
        let mut backend = Backend::default();
        for device in ids::DEVICES {
            let mut hw = IgcHardware::new(device.id, true);
            igc_setup_init_funcs(&mut hw, &mut backend, false).unwrap();
            assert_eq!(hw.mac_type, Some(IgcMacType::I225));
            assert_eq!(
                hw.mac_ops.init_params,
                Some(IgcApiCallback::MacInitParamsI225)
            );
            assert_eq!(
                hw.nvm_ops.init_params,
                Some(IgcApiCallback::NvmInitParamsI225)
            );
            assert_eq!(
                hw.phy_ops.init_params,
                Some(IgcApiCallback::PhyInitParamsI225)
            );
        }
        assert!(backend.calls.is_empty());

        let mut hw = IgcHardware::new(ids::DEVICES[0].id, true);
        igc_setup_init_funcs(&mut hw, &mut backend, true).unwrap();
        assert_eq!(
            backend.calls,
            [
                IgcApiCallback::MacInitParamsI225,
                IgcApiCallback::NvmInitParamsI225,
                IgcApiCallback::PhyInitParamsI225,
            ]
        );
    }

    #[test]
    fn setup_fails_before_initializers_for_unknown_or_unmapped_hardware() {
        let mut backend = Backend::default();
        let mut unknown = IgcHardware::new(0xffff, true);
        assert!(igc_setup_init_funcs(&mut unknown, &mut backend, true).is_err());
        let mut unmapped = IgcHardware::new(ids::DEVICES[0].id, false);
        assert!(matches!(
            igc_setup_init_funcs(&mut unmapped, &mut backend, true),
            Err(DevError::BadState)
        ));
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn init_parameter_failure_stops_following_operation_tables() {
        let mut backend = Backend {
            fail_at: Some(IgcApiCallback::NvmInitParamsI225),
            ..Backend::default()
        };
        let mut hw = IgcHardware::new(ids::DEVICES[0].id, true);
        assert!(matches!(
            igc_setup_init_funcs(&mut hw, &mut backend, true),
            Err(DevError::Io)
        ));
        assert_eq!(
            backend.calls,
            [
                IgcApiCallback::MacInitParamsI225,
                IgcApiCallback::NvmInitParamsI225
            ]
        );
    }

    #[test]
    fn api_dispatch_preserves_optional_success_and_required_config_error() {
        let mut hw = IgcHardware::new(ids::DEVICES[0].id, true);
        let mut backend = Backend::default();
        assert!(igc_get_bus_info(&hw, &mut backend).is_ok());
        assert!(backend.calls.is_empty());
        assert!(matches!(
            igc_check_for_link(&hw, &mut backend),
            Err(DevError::Unsupported)
        ));
        assert!(backend.calls.is_empty());

        init_generic_ops(&mut hw);
        assert!(igc_get_bus_info(&hw, &mut backend).is_ok());
        assert_eq!(backend.calls, [IgcApiCallback::BusInfoGeneric]);
    }
}
