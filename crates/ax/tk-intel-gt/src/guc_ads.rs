// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_ads.c,
// intel_guc_fwif.h: Gen12 ADS fixed ABI, section layout, policy, and GT info.
// Copyright © 2014-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

use crate::{Error, GtIo};

pub const PAGE_SIZE: usize = 4096;
pub const GUC_MAX_ENGINE_CLASSES: usize = 16;
pub const GUC_MAX_INSTANCES_PER_CLASS: usize = 32;
pub const GUC_CAPTURE_CLASS_COUNT: usize = 5;
pub const GUC_GENERIC_GT_SYSINFO_MAX: usize = 16;
pub const GUC_INVALID_ENGINE_INSTANCE: u8 = GUC_MAX_INSTANCES_PER_CLASS as u8;
pub const GLOBAL_POLICY_DISABLE_ENGINE_RESET: u32 = 1;
pub const GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US: u32 = 500_000;
pub const GLOBAL_POLICY_MAX_NUM_WI: u32 = 15;
pub const ACTION_GLOBAL_SCHED_POLICY_CHANGE: u32 = 0x506;
const GUC_WORKAROUND_KLV_SERIALIZED_RA_MODE: u16 = 0x9001;
const GUC_WORKAROUND_KLV_BLOCK_INTERRUPTS_WHEN_MGSR_BLOCKED: u16 = 0x9002;
const GUC_WORKAROUND_KLV_AVOID_GFX_CLEAR_WHILE_ACTIVE: u16 = 0x9006;

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

/// Data collected from one engine by the caller's existing GT/WA layer.
#[derive(Clone)]
pub struct EngineRegset {
    pub engine: EngineMapEntry,
    pub registers: Vec<MmioReg>,
}

/// Saved default LRC image used by GuC watchdog recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoldenContext {
    pub guc_class: u8,
    pub image: Vec<u8>,
    /// The engine-state subrange starts after the common HWSP and LRC header.
    pub engine_state_offset: usize,
}

/// One GuC capture-list image. `capture_class` is the GuC capture bucket,
/// not an engine class; instance lists use the engine's instance as `slot`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureList {
    pub index: u8,
    pub capture_class: u8,
    pub instance_list: bool,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdsRuntimeInfo {
    pub generic_gt_sysinfo: [u32; GUC_GENERIC_GT_SYSINFO_MAX],
    pub graphics_ip_major: u8,
    pub graphics_ip_minor: u8,
    pub firmware_version: (u8, u8, u8),
    pub dgfx: bool,
}

#[derive(Clone)]
pub struct AdsBuildInput {
    pub base_ggtt: u32,
    pub reset_parameter: u8,
    pub runtime: AdsRuntimeInfo,
    pub engines: Vec<EngineMapEntry>,
    pub regsets: Vec<EngineRegset>,
    pub golden_contexts: Vec<GoldenContext>,
    pub capture_lists: Vec<CaptureList>,
    pub private_data_size: usize,
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

/// upstream: intel_guc_ads.c intel_guc_ads_print_policy_info().
pub fn policy_info(policies: Policies) -> (u32, u32, u32) {
    (
        policies.dpc_promote_time,
        policies.max_num_work_items,
        policies.global_flags,
    )
}

/// upstream: intel_guc_ads.c guc_action_policies_update().
pub fn action_policies_update(io: &impl GtIo, policy_ggtt: u32) -> Result<(), Error> {
    let response =
        crate::guc_fw::send_mmio(io, &[ACTION_GLOBAL_SCHED_POLICY_CHANGE, policy_ggtt], None)?;
    if response == 0 {
        Ok(())
    } else {
        Err(Error::Unavailable(ACTION_GLOBAL_SCHED_POLICY_CHANGE))
    }
}

/// upstream: intel_guc_ads.c intel_guc_global_policies_update().
pub fn global_policies_update(
    io: &impl GtIo,
    bytes: &mut [u8],
    policy_ggtt: u32,
    reset_parameter: u8,
    guc_ready: bool,
) -> Result<(), Error> {
    if policy_ggtt == 0 || policy_ggtt & 3 != 0 || bytes.len() < size_of::<AdsFixed>() {
        return Err(Error::Refused);
    }
    let policies = guc_policies_init(reset_parameter);
    let offset = offset_of!(AdsFixed, policies);
    write_u32(
        bytes,
        offset + offset_of!(Policies, dpc_promote_time),
        policies.dpc_promote_time,
    )?;
    write_u32(
        bytes,
        offset + offset_of!(Policies, max_num_work_items),
        policies.max_num_work_items,
    )?;
    write_u32(
        bytes,
        offset + offset_of!(Policies, global_flags),
        policies.global_flags,
    )?;
    write_u32(
        bytes,
        offset + offset_of!(Policies, is_valid),
        policies.is_valid,
    )?;
    if guc_ready {
        action_policies_update(io, policy_ggtt)?;
    }
    Ok(())
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
        if class >= GUC_MAX_ENGINE_CLASSES
            || logical >= GUC_MAX_INSTANCES_PER_CLASS
            || usize::from(entry.instance) >= GUC_MAX_INSTANCES_PER_CLASS
        {
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
    if layout.regset_offset != size_of::<AdsFixed>()
        || layout.golden_context_offset % PAGE_SIZE != 0
        || layout.workaround_klv_offset % PAGE_SIZE != 0
        || layout.capture_offset % PAGE_SIZE != 0
        || layout.private_data_offset % PAGE_SIZE != 0
        || layout.total_size % PAGE_SIZE != 0
        || layout.golden_context_offset < layout.regset_offset
        || layout.workaround_klv_offset < layout.golden_context_offset
        || layout.capture_offset < layout.workaround_klv_offset
        || layout.private_data_offset < layout.capture_offset
        || layout.total_size < layout.private_data_offset
    {
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

/// upstream: intel_guc_ads.c guc_mmio_reg_add() duplicate check and ordered
/// insertion (the caller has already calculated MMIO offsets/steering flags).
pub fn sorted_unique_regset(registers: &[MmioReg]) -> Result<Vec<MmioReg>, Error> {
    let mut sorted = Vec::new();
    sorted
        .try_reserve_exact(registers.len())
        .map_err(|_| Error::Refused)?;
    for register in registers {
        // The upstream bsearch happens before insertion, so a duplicate
        // offset keeps the first value/flags/mask supplied by the engine.
        if !sorted
            .iter()
            .any(|previous: &MmioReg| previous.offset == register.offset)
        {
            sorted.push(*register);
        }
    }
    sorted.sort_unstable_by_key(|reg| reg.offset);
    Ok(sorted)
}

/// upstream: intel_guc_ads.c guc_get_capture_engine_mask().
pub fn capture_class_mask(capture_class: u8, enabled_masks: &[u32; GUC_MAX_ENGINE_CLASSES]) -> u32 {
    match capture_class {
        0 => enabled_masks[0] | enabled_masks[4], // render + compute
        1 => enabled_masks[2],                    // video
        2 => enabled_masks[3],                    // video-enhance
        3 => enabled_masks[4],                    // blitter
        4 => enabled_masks[5],                    // GSC/other
        _ => 0,
    }
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), Error> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(2).ok_or(Error::Refused)?)
        .ok_or(Error::Refused)?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32_array(bytes: &mut [u8], offset: usize, values: &[u32]) -> Result<(), Error> {
    for (index, value) in values.iter().copied().enumerate() {
        write_u32(bytes, offset + index * 4, value)?;
    }
    Ok(())
}

/// upstream: intel_guc_ads.c guc_mmio_reg_state_create() and
/// guc_mmio_reg_state_init(). Returns the packed register section and per-engine
/// (offset,count) metadata, with each engine's list sorted and deduplicated.
fn prepare_regsets(
    regsets: &[EngineRegset],
) -> Result<(Vec<u8>, Vec<(EngineMapEntry, usize, usize)>), Error> {
    let mut storage = Vec::new();
    let mut metadata = Vec::new();
    metadata
        .try_reserve_exact(regsets.len())
        .map_err(|_| Error::Refused)?;
    for regset in regsets {
        let registers = sorted_unique_regset(&regset.registers)?;
        let start = storage.len();
        let byte_count = registers
            .len()
            .checked_mul(size_of::<MmioReg>())
            .ok_or(Error::Refused)?;
        storage
            .try_reserve_exact(byte_count)
            .map_err(|_| Error::Refused)?;
        for reg in &registers {
            storage.extend_from_slice(&reg.offset.to_le_bytes());
            storage.extend_from_slice(&reg.value.to_le_bytes());
            storage.extend_from_slice(&reg.flags.to_le_bytes());
            storage.extend_from_slice(&reg.mask.to_le_bytes());
        }
        metadata.push((regset.engine, start, registers.len()));
    }
    Ok((storage, metadata))
}

/// upstream: intel_guc_ads.c guc_prep_golden_context().
fn prepare_golden_contexts(
    bytes: &mut [u8],
    base: u32,
    layout: AdsLayout,
    contexts: &[GoldenContext],
) -> Result<(), Error> {
    let mut cursor = layout.golden_context_offset;
    for context in contexts {
        let class = usize::from(context.guc_class);
        if class >= GUC_MAX_ENGINE_CLASSES
            || context.engine_state_offset > context.image.len()
            || context.image.is_empty()
        {
            return Err(Error::Refused);
        }
        let allocation = page_align(context.image.len())?;
        let end = cursor.checked_add(allocation).ok_or(Error::Refused)?;
        if end > layout.workaround_klv_offset {
            return Err(Error::Refused);
        }
        let image_end = cursor
            .checked_add(context.image.len())
            .ok_or(Error::Refused)?;
        bytes
            .get_mut(cursor..image_end)
            .ok_or(Error::Refused)?
            .copy_from_slice(&context.image);
        write_u32(
            bytes,
            offset_of!(AdsFixed, ads.golden_context_lrca) + class * 4,
            base.checked_add(u32::try_from(cursor).map_err(|_| Error::Refused)?)
                .ok_or(Error::Refused)?,
        )?;
        write_u32(
            bytes,
            offset_of!(AdsFixed, ads.engine_state_size) + class * 4,
            u32::try_from(context.image.len() - context.engine_state_offset)
                .map_err(|_| Error::Refused)?,
        )?;
        cursor = end;
    }
    Ok(())
}

/// upstream: intel_guc_ads.c guc_capture_prep_lists().
fn prepare_capture_lists(
    bytes: &mut [u8],
    base: u32,
    layout: AdsLayout,
    lists: &[CaptureList],
) -> Result<(), Error> {
    if layout.capture_offset == layout.private_data_offset {
        return if lists.is_empty() {
            Ok(())
        } else {
            Err(Error::Refused)
        };
    }
    let null_list = base
        .checked_add(u32::try_from(layout.capture_offset).map_err(|_| Error::Refused)?)
        .ok_or(Error::Refused)?;
    let mut cursor = layout
        .capture_offset
        .checked_add(PAGE_SIZE)
        .ok_or(Error::Refused)?;
    if cursor > layout.private_data_offset {
        return Err(Error::Refused);
    }
    for index in 0usize..2 {
        for class in 0..GUC_MAX_ENGINE_CLASSES {
            let matching_class = lists.iter().find(|list| {
                usize::from(list.index) == index
                    && usize::from(list.capture_class) == class
                    && !list.instance_list
            });
            let matching_instance = lists.iter().find(|list| {
                usize::from(list.index) == index
                    && usize::from(list.capture_class) == class
                    && list.instance_list
            });
            let class_mask = offset_of!(AdsFixed, ads.capture_class)
                + index * 4 * GUC_MAX_ENGINE_CLASSES
                + class * 4;
            let instance_mask = offset_of!(AdsFixed, ads.capture_instance)
                + index * 4 * GUC_MAX_ENGINE_CLASSES
                + class * 4;
            for (list, destination) in [
                (matching_class, class_mask),
                (matching_instance, instance_mask),
            ] {
                let Some(list) = list else {
                    write_u32(bytes, destination, null_list)?;
                    continue;
                };
                if list.bytes.is_empty() || list.index > 1 {
                    return Err(Error::Refused);
                }
                let start = cursor;
                let end = start.checked_add(list.bytes.len()).ok_or(Error::Refused)?;
                if end > layout.private_data_offset {
                    return Err(Error::Refused);
                }
                bytes
                    .get_mut(start..end)
                    .ok_or(Error::Refused)?
                    .copy_from_slice(&list.bytes);
                let address = base
                    .checked_add(u32::try_from(start).map_err(|_| Error::Refused)?)
                    .ok_or(Error::Refused)?;
                write_u32(bytes, destination, address)?;
                cursor = end;
            }
        }
        let global_offset = offset_of!(AdsFixed, ads.capture_global) + index * 4;
        let global = lists.iter().find(|list| {
            usize::from(list.index) == index && list.capture_class == u8::MAX && !list.instance_list
        });
        if let Some(list) = global {
            let end = cursor.checked_add(list.bytes.len()).ok_or(Error::Refused)?;
            if list.bytes.is_empty() || end > layout.private_data_offset {
                return Err(Error::Refused);
            }
            bytes
                .get_mut(cursor..end)
                .ok_or(Error::Refused)?
                .copy_from_slice(&list.bytes);
            write_u32(
                bytes,
                global_offset,
                base.checked_add(u32::try_from(cursor).map_err(|_| Error::Refused)?)
                    .ok_or(Error::Refused)?,
            )?;
            cursor = end;
        } else {
            write_u32(bytes, global_offset, null_list)?;
        }
    }
    Ok(())
}

/// upstream: intel_guc_ads.c guc_waklv_enable_simple()/guc_waklv_init().
fn prepare_workaround_klvs(
    bytes: &mut [u8],
    base: u32,
    layout: AdsLayout,
    ids: &[u16],
) -> Result<(), Error> {
    let mut cursor = layout.workaround_klv_offset;
    for id in ids {
        let value = u32::from(*id) << 16; // key, zero-length value
        let end = cursor.checked_add(4).ok_or(Error::Refused)?;
        if end > layout.capture_offset {
            return Err(Error::Refused);
        }
        write_u32(bytes, cursor, value)?;
        cursor = end;
    }
    if cursor != layout.workaround_klv_offset {
        write_u32(
            bytes,
            offset_of!(AdsFixed, ads.workaround_klv_addr_low),
            base.checked_add(
                u32::try_from(layout.workaround_klv_offset).map_err(|_| Error::Refused)?,
            )
            .ok_or(Error::Refused)?,
        )?;
        write_u32(bytes, offset_of!(AdsFixed, ads.workaround_klv_addr_high), 0)?;
        write_u32(
            bytes,
            offset_of!(AdsFixed, ads.workaround_klv_size),
            u32::try_from(cursor - layout.workaround_klv_offset).map_err(|_| Error::Refused)?,
        )?;
    }
    Ok(())
}

/// upstream: intel_guc_ads.c guc_waklv_init() platform/firmware gates.
pub fn workaround_klv_ids(runtime: AdsRuntimeInfo) -> Vec<u16> {
    let graphics_ip = (runtime.graphics_ip_major, runtime.graphics_ip_minor);
    let mut ids = Vec::new();
    if (12, 70) <= graphics_ip && graphics_ip < (12, 75) {
        ids.push(GUC_WORKAROUND_KLV_SERIALIZED_RA_MODE);
        ids.push(GUC_WORKAROUND_KLV_AVOID_GFX_CLEAR_WHILE_ACTIVE);
    }
    if runtime.firmware_version >= (70, 21, 1)
        && (((12, 70) <= graphics_ip && graphics_ip < (12, 75))
            || graphics_ip == (13, 0)
            || runtime.dgfx)
    {
        ids.push(GUC_WORKAROUND_KLV_BLOCK_INTERRUPTS_WHEN_MGSR_BLOCKED);
    }
    ids
}

/// upstream: intel_guc_ads.c __guc_ads_init(), guc_init_golden_context(),
/// guc_ads_private_data_reset(), and intel_guc_ads_reset().
pub fn build_ads(input: &AdsBuildInput) -> Result<(AdsLayout, Vec<u8>), Error> {
    if input.base_ggtt == 0 || input.base_ggtt & (PAGE_SIZE as u32 - 1) != 0 {
        return Err(Error::Refused);
    }
    let (regset, regset_meta) = prepare_regsets(&input.regsets)?;
    for (index, list) in input.capture_lists.iter().enumerate() {
        if list.index >= 2
            || (usize::from(list.capture_class) >= GUC_CAPTURE_CLASS_COUNT
                && list.capture_class != u8::MAX)
            || (list.capture_class == u8::MAX && list.instance_list)
            || input.capture_lists[..index].iter().any(|previous| {
                previous.index == list.index
                    && previous.capture_class == list.capture_class
                    && previous.instance_list == list.instance_list
            })
        {
            return Err(Error::Refused);
        }
    }
    for (index, regset) in input.regsets.iter().enumerate() {
        if !input.engines.contains(&regset.engine)
            || input.regsets[..index]
                .iter()
                .any(|previous| previous.engine == regset.engine)
        {
            return Err(Error::Refused);
        }
    }
    let golden_size = input
        .golden_contexts
        .iter()
        .try_fold(0usize, |sum, context| {
            sum.checked_add(page_align(context.image.len())?)
                .ok_or(Error::Refused)
        })?;
    let capture_size = input
        .capture_lists
        .iter()
        .try_fold(PAGE_SIZE, |sum, list| {
            sum.checked_add(list.bytes.len()).ok_or(Error::Refused)
        })?;
    let klv_ids = workaround_klv_ids(input.runtime);
    let klv_size = klv_ids.len().checked_mul(4).ok_or(Error::Refused)?;
    let layout = layout(AdsSizes {
        regset: regset.len(),
        golden_context: golden_size,
        workaround_klv: klv_size,
        capture: capture_size,
        private_data: input.private_data_size,
    })?;
    let mut bytes = build_static_ads(
        input.base_ggtt,
        layout,
        input.reset_parameter,
        &input.engines,
    )?;

    // Upstream guc_mmio_reg_state_init() writes one table entry per engine.
    let reg_cursor = layout.regset_offset;
    for (engine, relative, count) in regset_meta {
        let class = usize::from(engine.guc_class);
        let instance = usize::from(engine.instance);
        if class >= GUC_MAX_ENGINE_CLASSES || instance >= GUC_MAX_INSTANCES_PER_CLASS {
            return Err(Error::Refused);
        }
        let entry = offset_of!(AdsFixed, ads.reg_state_list)
            + (class * GUC_MAX_INSTANCES_PER_CLASS + instance) * size_of::<MmioRegSet>();
        let addr = if count == 0 {
            0
        } else {
            input
                .base_ggtt
                .checked_add(u32::try_from(reg_cursor + relative).map_err(|_| Error::Refused)?)
                .ok_or(Error::Refused)?
        };
        write_u32(&mut bytes, entry + offset_of!(MmioRegSet, address), addr)?;
        write_u16(
            &mut bytes,
            entry + offset_of!(MmioRegSet, count),
            u16::try_from(count).map_err(|_| Error::Refused)?,
        )?;
    }
    let reg_end = reg_cursor.checked_add(regset.len()).ok_or(Error::Refused)?;
    bytes
        .get_mut(reg_cursor..reg_end)
        .ok_or(Error::Refused)?
        .copy_from_slice(&regset);

    // Upstream reserves rounded context slots; callers provide saved LRCs.
    prepare_golden_contexts(&mut bytes, input.base_ggtt, layout, &input.golden_contexts)?;
    prepare_capture_lists(&mut bytes, input.base_ggtt, layout, &input.capture_lists)?;
    prepare_workaround_klvs(&mut bytes, input.base_ggtt, layout, &klv_ids)?;
    let info_offset = offset_of!(AdsFixed, system_info.generic_gt_sysinfo);
    write_u32_array(&mut bytes, info_offset, &input.runtime.generic_gt_sysinfo)?;
    Ok((layout, bytes))
}

/// upstream: intel_guc_ads.c intel_guc_engine_usage_offset().
pub fn engine_usage_offset(base_ggtt: u32) -> Result<u32, Error> {
    if base_ggtt == 0 || base_ggtt & (PAGE_SIZE as u32 - 1) != 0 {
        return Err(Error::Refused);
    }
    base_ggtt
        .checked_add(u32::try_from(offset_of!(AdsFixed, engine_usage)).map_err(|_| Error::Refused)?)
        .ok_or(Error::Refused)
}

/// upstream: intel_guc_ads.c intel_guc_engine_usage_record_map().
pub fn engine_usage_record_offset(guc_class: u8, logical_index: u8) -> Result<usize, Error> {
    if usize::from(guc_class) >= GUC_MAX_ENGINE_CLASSES
        || usize::from(logical_index) >= GUC_MAX_INSTANCES_PER_CLASS
    {
        return Err(Error::Refused);
    }
    Ok(offset_of!(AdsFixed, engine_usage.engines)
        + (usize::from(guc_class) * GUC_MAX_INSTANCES_PER_CLASS + usize::from(logical_index))
            * size_of::<EngineUsageRecord>())
}

/// upstream: intel_guc_ads.c guc_ads_private_data_reset().
pub fn reset_private_data(bytes: &mut [u8], layout: AdsLayout) -> Result<(), Error> {
    let end = layout
        .private_data_offset
        .checked_add(page_align(
            layout.total_size.saturating_sub(layout.private_data_offset),
        )?)
        .ok_or(Error::Refused)?;
    bytes
        .get_mut(layout.private_data_offset..end)
        .ok_or(Error::Refused)?
        .fill(0);
    Ok(())
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
    use alloc::vec;

    use super::*;

    struct TestIo;
    impl GtIo for TestIo {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            Err(Error::Unavailable(offset))
        }
        fn write(&self, offset: u32, _value: u32) -> Result<(), Error> {
            Err(Error::Unavailable(offset))
        }
        fn now_us(&self) -> u64 {
            0
        }
        fn delay_us(&self, _micros: u32) {}
    }

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

    fn word(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
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
        assert_eq!(
            policy_info(policy_disabled),
            (
                GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US,
                GLOBAL_POLICY_MAX_NUM_WI,
                GLOBAL_POLICY_DISABLE_ENGINE_RESET
            )
        );
        let mut masks = [0; GUC_MAX_ENGINE_CLASSES];
        masks[0] = 1;
        masks[4] = 2;
        assert_eq!(capture_class_mask(0, &masks), 3);
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

    #[test]
    fn ads_dynamic_sections_publish_regsets_golden_contexts_waklv_and_usage() {
        let engine = EngineMapEntry {
            guc_class: 0,
            instance: 0,
            logical_index: 0,
        };
        let input = AdsBuildInput {
            base_ggtt: 0x40_0000,
            reset_parameter: 2,
            runtime: AdsRuntimeInfo {
                generic_gt_sysinfo: [0x55; GUC_GENERIC_GT_SYSINFO_MAX],
                graphics_ip_major: 12,
                graphics_ip_minor: 0,
                firmware_version: (70, 10, 0),
                dgfx: false,
            },
            engines: vec![engine],
            regsets: vec![EngineRegset {
                engine,
                registers: vec![
                    MmioReg {
                        offset: 0x20,
                        value: 3,
                        flags: 1,
                        mask: 0xff,
                    },
                    MmioReg {
                        offset: 0x10,
                        value: 2,
                        flags: 0,
                        mask: 0,
                    },
                    MmioReg {
                        offset: 0x20,
                        value: 9,
                        flags: 0,
                        mask: 0,
                    },
                ],
            }],
            golden_contexts: vec![GoldenContext {
                guc_class: 0,
                image: vec![0xaa; 32],
                engine_state_offset: 8,
            }],
            capture_lists: vec![],
            private_data_size: 32,
        };
        let (layout, mut bytes) = build_ads(&input).unwrap();
        let regset_entry = offset_of!(AdsFixed, ads.reg_state_list);
        assert_eq!(
            dword(&bytes, regset_entry),
            input.base_ggtt + layout.regset_offset as u32
        );
        assert_eq!(
            word(&bytes, regset_entry + offset_of!(MmioRegSet, count)),
            2
        );
        assert_eq!(dword(&bytes, layout.regset_offset), 0x10);
        assert_eq!(
            dword(&bytes, layout.regset_offset + size_of::<MmioReg>()),
            0x20
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.golden_context_lrca)),
            input.base_ggtt + layout.golden_context_offset as u32
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.engine_state_size)),
            24
        );
        assert_eq!(dword(&bytes, layout.workaround_klv_offset), 0);
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, system_info.generic_gt_sysinfo)),
            0x55
        );
        assert_eq!(
            engine_usage_offset(input.base_ggtt).unwrap(),
            input.base_ggtt + offset_of!(AdsFixed, engine_usage) as u32
        );
        assert_eq!(
            engine_usage_record_offset(0, 0).unwrap(),
            offset_of!(AdsFixed, engine_usage.engines)
        );
        bytes[layout.private_data_offset] = 0xff;
        reset_private_data(&mut bytes, layout).unwrap();
        assert_eq!(bytes[layout.private_data_offset], 0);
    }

    #[test]
    fn ads_capture_lists_use_reserved_null_page_and_separate_class_instance_global_slots() {
        let page = vec![0x11, 0x22];
        let input = AdsBuildInput {
            base_ggtt: 0x60_0000,
            reset_parameter: 2,
            runtime: AdsRuntimeInfo {
                generic_gt_sysinfo: [0; GUC_GENERIC_GT_SYSINFO_MAX],
                graphics_ip_major: 12,
                graphics_ip_minor: 0,
                firmware_version: (70, 10, 0),
                dgfx: false,
            },
            engines: vec![],
            regsets: vec![],
            golden_contexts: vec![],
            capture_lists: vec![
                CaptureList {
                    index: 0,
                    capture_class: 0,
                    instance_list: false,
                    bytes: page.clone(),
                },
                CaptureList {
                    index: 0,
                    capture_class: 0,
                    instance_list: true,
                    bytes: page.clone(),
                },
                CaptureList {
                    index: 0,
                    capture_class: u8::MAX,
                    instance_list: false,
                    bytes: page,
                },
            ],
            private_data_size: 0,
        };
        let (layout, bytes) = build_ads(&input).unwrap();
        let null_page = input.base_ggtt + layout.capture_offset as u32;
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.capture_class)),
            input.base_ggtt + layout.capture_offset as u32 + PAGE_SIZE as u32
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.capture_instance)),
            input.base_ggtt + layout.capture_offset as u32 + PAGE_SIZE as u32 + 2
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.capture_class) + 4),
            null_page
        );
        assert_eq!(
            dword(&bytes, offset_of!(AdsFixed, ads.capture_global)),
            input.base_ggtt + layout.capture_offset as u32 + PAGE_SIZE as u32 + 4
        );
        assert!(
            bytes[layout.capture_offset..layout.capture_offset + PAGE_SIZE]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn waklv_platform_and_firmware_gates_match_gen12_source() {
        let runtime = AdsRuntimeInfo {
            generic_gt_sysinfo: [0; GUC_GENERIC_GT_SYSINFO_MAX],
            graphics_ip_major: 12,
            graphics_ip_minor: 70,
            firmware_version: (70, 21, 1),
            dgfx: false,
        };
        assert_eq!(workaround_klv_ids(runtime), [0x9001, 0x9006, 0x9002]);
        assert!(
            workaround_klv_ids(AdsRuntimeInfo {
                graphics_ip_minor: 0,
                ..runtime
            })
            .is_empty()
        );
    }

    #[test]
    fn policy_update_rewrites_fields_and_skips_action_before_guc_ready() {
        let layout = layout(AdsSizes {
            regset: 0,
            golden_context: 0,
            workaround_klv: 0,
            capture: 0,
            private_data: 0,
        })
        .unwrap();
        let mut bytes = build_static_ads(0x80_0000, layout, 2, &[]).unwrap();
        global_policies_update(&TestIo, &mut bytes, 0x80_1000, 1, false).unwrap();
        let policy = offset_of!(AdsFixed, policies);
        assert_eq!(
            dword(&bytes, policy + offset_of!(Policies, dpc_promote_time)),
            GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US
        );
        assert_eq!(
            dword(&bytes, policy + offset_of!(Policies, max_num_work_items)),
            GLOBAL_POLICY_MAX_NUM_WI
        );
        assert_eq!(
            dword(&bytes, policy + offset_of!(Policies, global_flags)),
            GLOBAL_POLICY_DISABLE_ENGINE_RESET
        );
        assert_eq!(dword(&bytes, policy + offset_of!(Policies, is_valid)), 1);
    }
}
