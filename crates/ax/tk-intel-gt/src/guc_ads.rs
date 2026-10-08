// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_ads.c,
// intel_guc_fwif.h: Gen12 ADS fixed ABI, section layout, policy, and GT info.
// Copyright © 2014-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

use crate::Error;

pub const PAGE_SIZE: usize = 4096;
pub const GUC_MAX_ENGINE_CLASSES: usize = 16;
pub const GUC_MAX_INSTANCES_PER_CLASS: usize = 32;
pub const GUC_GENERIC_GT_SYSINFO_MAX: usize = 16;
pub const GUC_INVALID_ENGINE_INSTANCE: u8 = GUC_MAX_INSTANCES_PER_CLASS as u8;
pub const GLOBAL_POLICY_DISABLE_ENGINE_RESET: u32 = 1;
pub const GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US: u32 = 500_000;
pub const GLOBAL_POLICY_MAX_NUM_WI: u32 = 15;

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct MmioRegSet {
    pub address: u32,
    pub count: u16,
    pub reserved: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct MmioReg {
    pub offset: u32,
    pub value: u32,
    pub flags: u32,
    pub mask: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Policies {
    pub submission_queue_depth: [u32; GUC_MAX_ENGINE_CLASSES],
    pub dpc_promote_time: u32,
    pub is_valid: u32,
    pub max_num_work_items: u32,
    pub global_flags: u32,
    pub reserved: [u32; 4],
}

impl Default for Policies {
    fn default() -> Self {
        Self {
            submission_queue_depth: [0; GUC_MAX_ENGINE_CLASSES],
            dpc_promote_time: GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US,
            is_valid: 1,
            max_num_work_items: GLOBAL_POLICY_MAX_NUM_WI,
            global_flags: 0,
            reserved: [0; 4],
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct GtSystemInfo {
    pub mapping_table: [[u8; GUC_MAX_INSTANCES_PER_CLASS]; GUC_MAX_ENGINE_CLASSES],
    pub engine_enabled_masks: [u32; GUC_MAX_ENGINE_CLASSES],
    pub generic_gt_sysinfo: [u32; GUC_GENERIC_GT_SYSINFO_MAX],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct EngineUsageRecord {
    pub current_context_index: u32,
    pub last_switch_in_stamp: u32,
    pub reserved0: u32,
    pub total_runtime: u32,
    pub reserved1: [u32; 4],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct EngineUsage {
    pub engines: [[EngineUsageRecord; GUC_MAX_INSTANCES_PER_CLASS]; GUC_MAX_ENGINE_CLASSES],
}

impl Default for EngineUsage {
    fn default() -> Self {
        Self {
            engines: [[EngineUsageRecord::default(); GUC_MAX_INSTANCES_PER_CLASS];
                GUC_MAX_ENGINE_CLASSES],
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Ads {
    pub reg_state_list: [[MmioRegSet; GUC_MAX_INSTANCES_PER_CLASS]; GUC_MAX_ENGINE_CLASSES],
    pub reserved0: u32,
    pub scheduler_policies: u32,
    pub gt_system_info: u32,
    pub reserved1: u32,
    pub control_data: u32,
    pub golden_context_lrca: [u32; GUC_MAX_ENGINE_CLASSES],
    pub engine_state_size: [u32; GUC_MAX_ENGINE_CLASSES],
    pub private_data: u32,
    pub reserved2: u32,
    pub capture_instance: [[u32; GUC_MAX_ENGINE_CLASSES]; 2],
    pub capture_class: [[u32; GUC_MAX_ENGINE_CLASSES]; 2],
    pub capture_global: [u32; 2],
    pub workaround_klv_addr_low: u32,
    pub workaround_klv_addr_high: u32,
    pub workaround_klv_size: u32,
    pub reserved: [u32; 11],
}

impl Default for Ads {
    fn default() -> Self {
        Self {
            reg_state_list: [[MmioRegSet::default(); GUC_MAX_INSTANCES_PER_CLASS];
                GUC_MAX_ENGINE_CLASSES],
            reserved0: 0,
            scheduler_policies: 0,
            gt_system_info: 0,
            reserved1: 0,
            control_data: 0,
            golden_context_lrca: [0; GUC_MAX_ENGINE_CLASSES],
            engine_state_size: [0; GUC_MAX_ENGINE_CLASSES],
            private_data: 0,
            reserved2: 0,
            capture_instance: [[0; GUC_MAX_ENGINE_CLASSES]; 2],
            capture_class: [[0; GUC_MAX_ENGINE_CLASSES]; 2],
            capture_global: [0; 2],
            workaround_klv_addr_low: 0,
            workaround_klv_addr_high: 0,
            workaround_klv_size: 0,
            reserved: [0; 11],
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct AdsFixed {
    ads: Ads,
    policies: Policies,
    system_info: GtSystemInfo,
    engine_usage: EngineUsage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdsSizes {
    pub regset: usize,
    pub golden_context: usize,
    pub workaround_klv: usize,
    pub capture: usize,
    pub private_data: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdsLayout {
    pub regset_offset: usize,
    pub golden_context_offset: usize,
    pub workaround_klv_offset: usize,
    pub capture_offset: usize,
    pub private_data_offset: usize,
    pub total_size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineMapEntry {
    pub guc_class: u8,
    pub instance: u8,
    /// `ilog2(engine.logical_mask)` used as the GuC mapping-table column.
    pub logical_index: u8,
}

/// upstream: intel_guc_ads.c guc_ads_regset_size().
pub const fn guc_ads_regset_size(bytes: usize) -> usize {
    bytes
}

/// upstream: intel_guc_ads.c guc_ads_golden_ctxt_size().
pub fn guc_ads_golden_context_size(bytes: usize) -> Result<usize, Error> {
    page_align(bytes)
}

/// upstream: intel_guc_ads.c guc_ads_waklv_size().
pub fn guc_ads_workaround_klv_size(bytes: usize) -> Result<usize, Error> {
    page_align(bytes)
}

/// upstream: intel_guc_ads.c guc_ads_capture_size().
pub fn guc_ads_capture_size(bytes: usize) -> Result<usize, Error> {
    page_align(bytes)
}

/// upstream: intel_guc_ads.c guc_ads_private_data_size().
pub fn guc_ads_private_data_size(bytes: usize) -> Result<usize, Error> {
    page_align(bytes)
}

fn page_align(value: usize) -> Result<usize, Error> {
    value
        .checked_add(PAGE_SIZE - 1)
        .map(|aligned| aligned & !(PAGE_SIZE - 1))
        .ok_or(Error::Refused)
}

/// upstream: intel_guc_ads.c guc_ads_*_offset()/guc_ads_blob_size().
pub fn layout(sizes: AdsSizes) -> Result<AdsLayout, Error> {
    let regset_offset = size_of::<AdsFixed>();
    let golden_context_offset = page_align(
        regset_offset
            .checked_add(guc_ads_regset_size(sizes.regset))
            .ok_or(Error::Refused)?,
    )?;
    let workaround_klv_offset = page_align(
        golden_context_offset
            .checked_add(guc_ads_golden_context_size(sizes.golden_context)?)
            .ok_or(Error::Refused)?,
    )?;
    let capture_offset = page_align(
        workaround_klv_offset
            .checked_add(guc_ads_workaround_klv_size(sizes.workaround_klv)?)
            .ok_or(Error::Refused)?,
    )?;
    let private_data_offset = page_align(
        capture_offset
            .checked_add(guc_ads_capture_size(sizes.capture)?)
            .ok_or(Error::Refused)?,
    )?;
    let total_size = private_data_offset
        .checked_add(guc_ads_private_data_size(sizes.private_data)?)
        .ok_or(Error::Refused)?;
    Ok(AdsLayout {
        regset_offset,
        golden_context_offset,
        workaround_klv_offset,
        capture_offset,
        private_data_offset,
        total_size,
    })
}

/// upstream: intel_guc_ads.c guc_policies_init().
pub fn guc_policies_init(reset_parameter: u8) -> Policies {
    let mut policies = Policies::default();
    if reset_parameter < 2 {
        policies.global_flags |= GLOBAL_POLICY_DISABLE_ENGINE_RESET;
    }
    policies
}

/// upstream: intel_guc_ads.c fill_engine_enable_masks().
pub fn fill_engine_enable_masks(entries: &[EngineMapEntry]) -> Result<[u32; 16], Error> {
    let mut masks = [0; GUC_MAX_ENGINE_CLASSES];
    for entry in entries {
        if usize::from(entry.guc_class) >= GUC_MAX_ENGINE_CLASSES
            || usize::from(entry.instance) >= GUC_MAX_INSTANCES_PER_CLASS
            || usize::from(entry.logical_index) >= GUC_MAX_INSTANCES_PER_CLASS
        {
            return Err(Error::Refused);
        }
        masks[usize::from(entry.guc_class)] |= 1 << entry.instance;
    }
    Ok(masks)
}

/// upstream: intel_guc_ads.c guc_mapping_table_init().
pub fn guc_mapping_table_init(entries: &[EngineMapEntry]) -> Result<[[u8; 32]; 16], Error> {
    let mut mapping =
        [[GUC_INVALID_ENGINE_INSTANCE; GUC_MAX_INSTANCES_PER_CLASS]; GUC_MAX_ENGINE_CLASSES];
    for entry in entries {
        let class = usize::from(entry.guc_class);
        let logical = usize::from(entry.logical_index);
        if class >= GUC_MAX_ENGINE_CLASSES || logical >= GUC_MAX_INSTANCES_PER_CLASS {
            return Err(Error::Refused);
        }
        mapping[class][logical] = entry.instance;
    }
    Ok(mapping)
}

/// upstream: intel_guc_ads.c __guc_ads_init() static fields and system info.
pub fn build_static_ads(
    base_ggtt: u32,
    layout: AdsLayout,
    reset_parameter: u8,
    entries: &[EngineMapEntry],
) -> Result<Vec<u8>, Error> {
    if base_ggtt == 0 || base_ggtt & (PAGE_SIZE as u32 - 1) != 0 {
        return Err(Error::Refused);
    }
    let policies_address = base_ggtt
        .checked_add(u32::try_from(offset_of!(AdsFixed, policies)).map_err(|_| Error::Refused)?)
        .ok_or(Error::Refused)?;
    let info_address = base_ggtt
        .checked_add(u32::try_from(offset_of!(AdsFixed, system_info)).map_err(|_| Error::Refused)?)
        .ok_or(Error::Refused)?;
    let private_address = base_ggtt
        .checked_add(u32::try_from(layout.private_data_offset).map_err(|_| Error::Refused)?)
        .ok_or(Error::Refused)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(layout.total_size)
        .map_err(|_| Error::Refused)?;
    bytes.resize(layout.total_size, 0);
    write_u32(
        &mut bytes,
        offset_of!(AdsFixed, ads.scheduler_policies),
        policies_address,
    )?;
    write_u32(
        &mut bytes,
        offset_of!(AdsFixed, ads.gt_system_info),
        info_address,
    )?;
    write_u32(
        &mut bytes,
        offset_of!(AdsFixed, ads.private_data),
        private_address,
    )?;

    let policies = guc_policies_init(reset_parameter);
    write_u32(
        &mut bytes,
        offset_of!(AdsFixed, policies.dpc_promote_time),
        policies.dpc_promote_time,
    )?;
    write_u32(
        &mut bytes,
        offset_of!(AdsFixed, policies.is_valid),
        policies.is_valid,
    )?;
    write_u32(
        &mut bytes,
        offset_of!(AdsFixed, policies.max_num_work_items),
        policies.max_num_work_items,
    )?;
    write_u32(
        &mut bytes,
        offset_of!(AdsFixed, policies.global_flags),
        policies.global_flags,
    )?;

    let mapping = guc_mapping_table_init(entries)?;
    let masks = fill_engine_enable_masks(entries)?;
    let map_offset = offset_of!(AdsFixed, system_info.mapping_table);
    for (class, row) in mapping.iter().enumerate() {
        for (logical, instance) in row.iter().copied().enumerate() {
            let byte_offset = map_offset + class * GUC_MAX_INSTANCES_PER_CLASS + logical;
            *bytes.get_mut(byte_offset).ok_or(Error::Refused)? = instance;
        }
    }
    let mask_offset = offset_of!(AdsFixed, system_info.engine_enabled_masks);
    for (class, mask) in masks.iter().copied().enumerate() {
        write_u32(&mut bytes, mask_offset + class * 4, mask)?;
    }
    Ok(bytes)
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), Error> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(4).ok_or(Error::Refused)?)
        .ok_or(Error::Refused)?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> [EngineMapEntry; 2] {
        [
            EngineMapEntry {
                guc_class: 0,
                instance: 0,
                logical_index: 0,
            },
            EngineMapEntry {
                guc_class: 3,
                instance: 0,
                logical_index: 0,
            },
        ]
    }

    fn dword(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn ads_fixed_abi_and_dynamic_offsets_match_source_layout() {
        assert_eq!(size_of::<MmioRegSet>(), 8);
        assert_eq!(size_of::<MmioReg>(), 16);
        assert_eq!(size_of::<Policies>(), 96);
        assert_eq!(size_of::<GtSystemInfo>(), 640);
        assert_eq!(size_of::<EngineUsageRecord>(), 32);
        assert_eq!(size_of::<EngineUsage>(), 16 * 32 * 32);
        assert_eq!(size_of::<Ads>(), 4572);
        let layout = layout(AdsSizes {
            regset: 16 * size_of::<MmioReg>(),
            golden_context: 64,
            workaround_klv: 1,
            capture: 1,
            private_data: 5000,
        })
        .unwrap();
        assert_eq!(layout.regset_offset, size_of::<AdsFixed>());
        assert_eq!(layout.golden_context_offset % PAGE_SIZE, 0);
        assert_eq!(layout.workaround_klv_offset % PAGE_SIZE, 0);
        assert_eq!(layout.capture_offset % PAGE_SIZE, 0);
        assert_eq!(layout.private_data_offset % PAGE_SIZE, 0);
        assert_eq!(layout.total_size % PAGE_SIZE, 0);
    }

    #[test]
    fn ads_policies_and_engine_maps_match_guc_abi_fields() {
        let policy_disabled = guc_policies_init(1);
        let policy_enabled = guc_policies_init(2);
        let disabled_flags = policy_disabled.global_flags;
        let enabled_flags = policy_enabled.global_flags;
        assert_eq!(disabled_flags, GLOBAL_POLICY_DISABLE_ENGINE_RESET);
        assert_eq!(enabled_flags, 0);
        let mapping = guc_mapping_table_init(&entries()).unwrap();
        assert_eq!(mapping[0][0], 0);
        assert_eq!(mapping[3][0], 0);
        assert_eq!(mapping[2][0], GUC_INVALID_ENGINE_INSTANCE);
        let masks = fill_engine_enable_masks(&entries()).unwrap();
        assert_eq!(masks[0], 1);
        assert_eq!(masks[3], 1);
    }

    #[test]
    fn static_ads_blob_publishes_policy_and_system_info_ggtt_offsets() {
        let layout = layout(AdsSizes {
            regset: 0,
            golden_context: 0,
            workaround_klv: 0,
            capture: PAGE_SIZE,
            private_data: 0,
        })
        .unwrap();
        let bytes = build_static_ads(0x20_0000, layout, 1, &entries()).unwrap();
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.scheduler_policies)),
            0x20_0000 + offset_of!(AdsFixed, policies) as u32
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.gt_system_info)),
            0x20_0000 + offset_of!(AdsFixed, system_info) as u32
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.private_data)),
            0x20_0000 + layout.private_data_offset as u32
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, policies.dpc_promote_time)),
            GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US
        );
        assert_eq!(bytes[offset_of!(AdsFixed, system_info.mapping_table)], 0);
        assert_eq!(
            dword(
                &bytes,
                offset_of!(AdsFixed, system_info.engine_enabled_masks)
            ),
            1
        );
    }
}
