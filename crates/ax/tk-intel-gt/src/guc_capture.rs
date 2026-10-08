// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_capture.c and
// guc_capture_fwif.h. Copyright © 2021-2022 Intel Corporation.

use alloc::{string::String, vec::Vec};
use core::fmt::Write as FmtWrite;

pub const CAPTURE_TYPE_GLOBAL: u8 = 0;
pub const CAPTURE_TYPE_ENGINE_CLASS: u8 = 1;
pub const CAPTURE_TYPE_ENGINE_INSTANCE: u8 = 2;
pub const CAPTURE_TYPE_MAX: u8 = 3;
pub const CAPTURE_GROUP_FULL: u8 = 0;
pub const CAPTURE_GROUP_PARTIAL: u8 = 1;
pub const CAPTURE_GROUP_TYPE_MAX: u8 = 2;
pub const CAPTURE_LIST_HEADER_DWORDS: usize = 1;
pub const CAPTURE_LIST_ENTRY_DWORDS: usize = 4;
pub const CAPTURE_REGISTER_VALUE_PLACEHOLDER: u32 = 0xdead_f00d;
pub const PAGE_SIZE: usize = 4096;
pub const CAPTURE_OUTPUT_HEADER_BYTES: usize = 5 * 4;
pub const CAPTURE_GROUP_HEADER_BYTES: usize = 2 * 4;
pub const CAPTURE_OVERBUFFER_MULTIPLIER: usize = 3;
pub const CAPTURE_PREALLOC_NODE_COUNT: usize = 3 * 16 * 32;
pub const CAPTURE_PREALLOC_DEFAULT_REGISTERS: usize = 64;
pub const GUC_ENGINE_CLASS_MASK: u32 = 0x7;
pub const GUC_ENGINE_INSTANCE_SHIFT: u32 = 3;
pub const GUC_ENGINE_INSTANCE_MASK: u32 = 0xf << GUC_ENGINE_INSTANCE_SHIFT;
pub const CTX_GTT_ADDRESS_MASK: u32 = 0xffff_f000;
pub const CAPTURE_STEERING_GROUP_SHIFT: u32 = 12;
pub const CAPTURE_STEERING_INSTANCE_SHIFT: u32 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureError {
    InvalidBuffer,
    Empty,
    Misaligned,
    Truncated,
    InvalidType,
}

/// Byte-stream reader for the GuC log's error-capture circular region.
pub struct CaptureBuffer<'a> {
    data: &'a [u8],
    rd: usize,
    wr: usize,
}

impl<'a> CaptureBuffer<'a> {
    pub fn new(data: &'a [u8], rd: usize, wr: usize) -> Result<Self, CaptureError> {
        if data.is_empty() || rd > data.len() || wr > data.len() {
            return Err(CaptureError::InvalidBuffer);
        }
        Ok(Self { data, rd, wr })
    }

    /// upstream: intel_guc_capture.c guc_capture_buf_cnt().
    pub fn count(&self) -> usize {
        if self.wr >= self.rd {
            self.wr - self.rd
        } else {
            self.data.len() - self.rd + self.wr
        }
    }

    /// upstream: intel_guc_capture.c guc_capture_buf_cnt_to_end().
    pub fn count_to_end(&self) -> usize {
        if self.rd > self.wr {
            self.data.len() - self.rd
        } else {
            self.wr - self.rd
        }
    }

    fn read_dword(&mut self) -> Result<u32, CaptureError> {
        if self.count() < 4 {
            return Err(CaptureError::Truncated);
        }
        // upstream: intel_guc_capture.c guc_capture_log_remove_dw().
        for _ in 0..2 {
            let available = self.count_to_end();
            if available >= 4 {
                let value = u32::from_le_bytes(
                    self.data[self.rd..self.rd + 4]
                        .try_into()
                        .map_err(|_| CaptureError::Truncated)?,
                );
                self.rd += 4;
                return Ok(value);
            }
            // GuC promises dword aligned records; match upstream's recovery
            // behavior by skipping a non-zero tail before retrying at offset 0.
            self.rd = 0;
        }
        Err(CaptureError::Truncated)
    }

    fn read_words<const N: usize>(&mut self) -> Result<[u32; N], CaptureError> {
        if self.count() < N * 4 {
            return Err(CaptureError::Truncated);
        }
        if self.count_to_end() >= N * 4 {
            let mut words = [0; N];
            for (index, word) in words.iter_mut().enumerate() {
                let offset = self.rd + index * 4;
                *word = u32::from_le_bytes(
                    self.data[offset..offset + 4]
                        .try_into()
                        .map_err(|_| CaptureError::Truncated)?,
                );
            }
            self.rd += N * 4;
            return Ok(words);
        }
        let mut words = [0; N];
        for word in &mut words {
            *word = self.read_dword()?;
        }
        Ok(words)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureRegister {
    pub offset: u32,
    pub value: u32,
    pub flags: u32,
    pub mask: u32,
}

const GEN12_GLOBAL_REGS: [u32; 9] = [
    0xa188, // FORCEWAKE_MT
    0x40a0, // ERROR_GEN6
    0x40b0, // DONE_REG
    0x4024, // HSW_GTT_CACHE_EN
    0xceb8, // GEN12_FAULT_TLB_DATA0
    0xcebc, // GEN12_FAULT_TLB_DATA1
    0x43f4, // GEN12_AUX_ERR_DBG
    0xcf68, // GEN12_GAM_DONE
    0xcec4, // GEN12_RING_FAULT_REG
];
const GEN12_ENGINE_INSTANCE_REGS: [u32; 33] = [
    0x50, 0xb8, 0x78, 0x60, 0xb0, 0x64, 0x68, 0x70, 0x140, 0x168, 0x110, 0x180, 0x74, 0x5c, 0xc0,
    0x6c, 0x94, 0x38, 0x34, 0x30, 0x3c, 0x9c, 0x244, 0x80, 0x29c, 0x270, 0x274, 0x278, 0x27c,
    0x280, 0x284, 0x288, 0x28c,
];
const GEN12_RENDER_CLASS_REGS: [u32; 3] = [0x7100, 0x7104, 0x7108];
const GEN12_VIDEO_ENHANCE_CLASS_REGS: [u32; 4] = [0x1cc000, 0x1cd000, 0x1ce000, 0x1cf000];
const GEN12_GLOBAL_REG_NAMES: [&str; 9] = [
    "FORCEWAKE",
    "ERROR_GEN6",
    "DONE_REG",
    "HSW_GTT_CACHE_EN",
    "GEN12_FAULT_TLB_DATA0",
    "GEN12_FAULT_TLB_DATA1",
    "AUX_ERR_DBG",
    "GAM_DONE",
    "FAULT_REG",
];
const GEN12_ENGINE_INSTANCE_REG_NAMES: [&str; 33] = [
    "RC PSMI",
    "ESR",
    "RING_DMA_FADD_LDW",
    "RING_DMA_FADD_UDW",
    "EIR",
    "IPEIR",
    "IPEHR",
    "INSTPS",
    "RING_BBADDR_LOW32",
    "RING_BBADDR_UP32",
    "BB_STATE",
    "CCID",
    "ACTHD_LDW",
    "ACTHD_UDW",
    "INSTPM",
    "INSTDONE",
    "RING_NOPID",
    "START",
    "HEAD",
    "TAIL",
    "CTL",
    "MODE",
    "RING_CONTEXT_CONTROL",
    "HWS",
    "GFX_MODE",
    "PDP0_LDW",
    "PDP0_UDW",
    "PDP1_LDW",
    "PDP1_UDW",
    "PDP2_LDW",
    "PDP2_UDW",
    "PDP3_LDW",
    "PDP3_UDW",
];
const GEN12_RENDER_CLASS_REG_NAMES: [&str; 3] = [
    "GEN7_SC_INSTDONE",
    "GEN12_SC_INSTDONE_EXTRA",
    "GEN12_SC_INSTDONE_EXTRA2",
];
const GEN12_VIDEO_ENHANCE_CLASS_REG_NAMES: [&str; 4] =
    ["SFC_DONE[0]", "SFC_DONE[1]", "SFC_DONE[2]", "SFC_DONE[3]"];

/// Return the static Xe_LP/Gen12 register list selected by the source table.
/// Engine-instance entries are relative to the engine MMIO base.
/// upstream: intel_guc_capture.c xe_lp_lists and xe_lp_*_regs.
pub fn gen12_static_registers(
    owner: u32,
    list_type: u32,
    guc_class: u32,
    engine_base: u32,
) -> Option<Vec<CaptureRegister>> {
    if owner != 0 {
        return None;
    }
    let offsets: &[u32] = match list_type {
        0 => &GEN12_GLOBAL_REGS,
        1 if guc_class == 0 => &GEN12_RENDER_CLASS_REGS,
        1 if guc_class == 2 => &GEN12_VIDEO_ENHANCE_CLASS_REGS,
        1 if matches!(guc_class, 1 | 3 | 4) => &[],
        2 if guc_class <= 4 => &GEN12_ENGINE_INSTANCE_REGS,
        2 => return None,
        _ => return None,
    };
    let mut registers = Vec::new();
    registers.try_reserve_exact(offsets.len()).ok()?;
    for offset in offsets {
        let offset = if list_type == 2 {
            engine_base.checked_add(*offset)?
        } else {
            *offset
        };
        registers.push(CaptureRegister {
            offset,
            value: 0,
            flags: 0,
            mask: 0,
        });
    }
    Some(registers)
}

/// Resolve a register's source name for GuC error-state output.
/// upstream: intel_guc_capture.c guc_capture_reg_to_str().
pub fn gen12_register_name(
    list_type: u32,
    guc_class: u32,
    engine_base: u32,
    offset: u32,
) -> Option<&'static str> {
    let (offsets, names): (&[u32], &[&str]) = match list_type {
        0 => (&GEN12_GLOBAL_REGS, &GEN12_GLOBAL_REG_NAMES),
        1 if guc_class == 0 => (&GEN12_RENDER_CLASS_REGS, &GEN12_RENDER_CLASS_REG_NAMES),
        1 if guc_class == 2 => (
            &GEN12_VIDEO_ENHANCE_CLASS_REGS,
            &GEN12_VIDEO_ENHANCE_CLASS_REG_NAMES,
        ),
        2 if guc_class <= 4 => (
            &GEN12_ENGINE_INSTANCE_REGS,
            &GEN12_ENGINE_INSTANCE_REG_NAMES,
        ),
        _ => return None,
    };
    let relative = if list_type == 2 {
        offset.checked_sub(engine_base)?
    } else {
        offset
    };
    offsets
        .iter()
        .position(|candidate| *candidate == relative)
        .and_then(|index| names.get(index).copied())
}

#[derive(Clone, Copy)]
pub struct CaptureRegisterList<'a> {
    pub owner: u32,
    pub list_type: u32,
    pub engine: u32,
    pub registers: &'a [CaptureRegister],
}

/// Expand render-class MCR capture registers for each discovered slice /
/// subslice pair. The addresses are supplied from the platform register table.
/// upstream: intel_guc_capture.c guc_capture_alloc_steered_lists().
pub fn expand_steered_registers(
    steering: &[(u8, u8)],
    gen8_register_offsets: &[u32; 2],
    xehpg_register_offset: Option<u32>,
) -> Result<Vec<CaptureRegister>, CaptureError> {
    let per_steering = 2usize + usize::from(xehpg_register_offset.is_some());
    let capacity = steering
        .len()
        .checked_mul(per_steering)
        .ok_or(CaptureError::InvalidBuffer)?;
    let mut registers = Vec::new();
    registers
        .try_reserve_exact(capacity)
        .map_err(|_| CaptureError::InvalidBuffer)?;
    for (slice, subslice) in steering {
        let flags = (u32::from(*slice) << CAPTURE_STEERING_GROUP_SHIFT)
            | (u32::from(*subslice) << CAPTURE_STEERING_INSTANCE_SHIFT);
        for offset in gen8_register_offsets {
            registers.push(CaptureRegister {
                offset: *offset,
                value: 0,
                flags,
                mask: 0,
            });
        }
        if let Some(offset) = xehpg_register_offset {
            registers.push(CaptureRegister {
                offset,
                value: 0,
                flags,
                mask: 0,
            });
        }
    }
    Ok(registers)
}

/// Find a static list; global lists match regardless of the requested engine
/// class, mirroring the source's `type == GLOBAL` special case.
/// upstream: intel_guc_capture.c guc_capture_get_one_list().
pub fn get_one_list<'a>(
    lists: &'a [CaptureRegisterList<'a>],
    owner: u32,
    list_type: u32,
    engine: u32,
) -> Option<&'a CaptureRegisterList<'a>> {
    lists.iter().find(|entry| {
        entry.owner == owner
            && entry.list_type == list_type
            && (entry.engine == engine || entry.list_type == u32::from(CAPTURE_TYPE_GLOBAL))
    })
}

/// Count the base plus topology-expanded (steered) register lists.
/// upstream: intel_guc_capture.c guc_cap_list_num_regs().
pub fn capture_list_register_count(
    base: Option<&CaptureRegisterList<'_>>,
    extended: Option<&CaptureRegisterList<'_>>,
) -> Result<usize, CaptureError> {
    base.map_or(Ok(0), |entry| Ok(entry.registers.len()))
        .and_then(|count| {
            count
                .checked_add(extended.map_or(0, |entry| entry.registers.len()))
                .ok_or(CaptureError::InvalidBuffer)
        })
}

/// Compute the page-aligned size returned by `intel_guc_capture_getlistsize`.
/// upstream: intel_guc_capture.c guc_capture_getlistsize().
pub fn capture_list_size(register_count: usize) -> Result<usize, CaptureError> {
    if register_count == 0 || register_count > u16::MAX as usize {
        return Err(CaptureError::InvalidBuffer);
    }
    let dwords = 1usize
        .checked_add(
            register_count
                .checked_mul(CAPTURE_LIST_ENTRY_DWORDS)
                .ok_or(CaptureError::InvalidBuffer)?,
        )
        .ok_or(CaptureError::InvalidBuffer)?;
    let data_size = dwords.checked_mul(4).ok_or(CaptureError::InvalidBuffer)?;
    data_size
        .checked_add(PAGE_SIZE - 1)
        .map(|size| size & !(PAGE_SIZE - 1))
        .ok_or(CaptureError::InvalidBuffer)
}

/// Initialize a GuC ADS list from its base and steered-register extensions.
/// upstream: intel_guc_capture.c guc_capture_list_init().
pub fn build_ads_capture_list_from_groups(
    base: &CaptureRegisterList<'_>,
    extended: Option<&CaptureRegisterList<'_>>,
) -> Result<Vec<u8>, CaptureError> {
    let count = capture_list_register_count(Some(base), extended)?;
    if count == 0 || count > u16::MAX as usize {
        return Err(CaptureError::InvalidBuffer);
    }
    let mut registers = Vec::new();
    registers
        .try_reserve_exact(count)
        .map_err(|_| CaptureError::InvalidBuffer)?;
    registers.extend_from_slice(base.registers);
    if let Some(extended) = extended {
        registers.extend_from_slice(extended.registers);
    }
    build_ads_capture_list(&registers)
}

/// Build the page-sized `guc_debug_capture_list` image consumed by ADS.
/// upstream: intel_guc_capture.c guc_capture_getlistsize()/guc_capture_list_init().
pub fn build_ads_capture_list(registers: &[CaptureRegister]) -> Result<Vec<u8>, CaptureError> {
    if registers.is_empty() || registers.len() > u16::MAX as usize {
        return Err(CaptureError::InvalidBuffer);
    }
    let data_size = CAPTURE_LIST_HEADER_DWORDS
        .checked_add(
            registers
                .len()
                .checked_mul(CAPTURE_LIST_ENTRY_DWORDS)
                .ok_or(CaptureError::InvalidBuffer)?,
        )
        .and_then(|dwords| dwords.checked_mul(4))
        .ok_or(CaptureError::InvalidBuffer)?;
    let size = data_size
        .checked_add(PAGE_SIZE - 1)
        .ok_or(CaptureError::InvalidBuffer)?
        & !(PAGE_SIZE - 1);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| CaptureError::InvalidBuffer)?;
    bytes.resize(size, 0);
    bytes[..4].copy_from_slice(&(registers.len() as u32).to_le_bytes());
    let mut offset = 4;
    for register in registers {
        for value in [
            register.offset,
            CAPTURE_REGISTER_VALUE_PLACEHOLDER,
            register.flags,
            register.mask,
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            offset += 4;
        }
    }
    Ok(bytes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureList {
    pub vfid: u8,
    pub capture_type: u8,
    pub engine_class: u8,
    pub engine_instance: u8,
    pub lrca: u32,
    pub guc_id: u32,
    pub registers: Vec<CaptureRegister>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureGroup {
    pub vfid: u8,
    pub group_type: u8,
    pub lists: Vec<CaptureList>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureLogState {
    pub read_ptr: usize,
    pub sampled_write_ptr: usize,
    pub buffer_full_count: u32,
    pub flush_to_file: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureLogStats {
    pub flush: u64,
    pub overflow: u32,
    pub sampled_overflow: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CaptureLogResult {
    pub groups: Vec<CaptureGroup>,
    pub parse_error: Option<CaptureError>,
    pub overflow: bool,
    pub flush_count: u32,
}

/// Snapshot and drain a GuC capture log region. Overflow or invalid offsets
/// force a full-ring pass; the error-capture read pointer and flush flag are
/// advanced/cleared even when parsing stopped, as in the firmware flush path.
/// upstream: intel_guc_capture.c __guc_capture_process_output().
pub fn process_capture_log(
    data: &[u8],
    state: &mut CaptureLogState,
    stats: &mut CaptureLogStats,
    reset_in_progress: bool,
) -> CaptureLogResult {
    let flush_count = state.flush_to_file;
    stats.flush = stats.flush.saturating_add(u64::from(flush_count));
    let full_count = state.buffer_full_count;
    let previous = stats.sampled_overflow;
    let overflow = full_count != previous;
    if overflow {
        stats.overflow = full_count;
        stats.sampled_overflow = stats
            .sampled_overflow
            .wrapping_add(full_count.wrapping_sub(previous));
        if full_count < previous {
            // Firmware exposes buffer_full_cnt as a 4-bit counter.
            stats.sampled_overflow = stats.sampled_overflow.wrapping_add(16);
        }
    }

    let size = data.len();
    let invalid = state.read_ptr > size || state.sampled_write_ptr > size;
    let (read_ptr, write_ptr) = if overflow || invalid {
        (0, size)
    } else {
        (state.read_ptr, state.sampled_write_ptr)
    };
    let mut groups = Vec::new();
    let mut parse_error = None;
    if !reset_in_progress {
        match CaptureBuffer::new(data, read_ptr, write_ptr) {
            Err(error) => parse_error = Some(error),
            Ok(mut buffer) => {
                while buffer.count() != 0 {
                    match extract_group(&mut buffer) {
                        Ok(group) => groups.push(group),
                        Err(error) => {
                            parse_error = Some(error);
                            break;
                        }
                    }
                }
            }
        }
    }
    state.read_ptr = write_ptr;
    state.flush_to_file = 0;
    CaptureLogResult {
        groups,
        parse_error,
        overflow,
        flush_count,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureOutputNode {
    pub is_partial: bool,
    pub engine_class: u8,
    pub engine_instance: u8,
    pub guc_id: u32,
    pub lrca: u32,
    pub lists: [Option<CaptureList>; 3],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapturedEngineState {
    pub ipehr: u32,
    pub instdone: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureEngineInfo<'a> {
    pub name: &'a str,
    pub class: u8,
    pub instance: u8,
    pub logical_mask: u32,
}

/// Format a parsed node in the same layered order used by i915 coredumps.
/// Unknown register names fall back to the MMIO offset, and steered entries
/// retain the slice/subslice selectors encoded in their flags.
/// upstream: intel_guc_capture.c intel_guc_capture_print_engine_node().
pub fn format_capture_node(
    engine_name: &str,
    node: Option<&CaptureOutputNode>,
    engine: Option<CaptureEngineInfo<'_>>,
    engine_base: u32,
    steered_offsets: &[u32],
) -> Result<String, CaptureError> {
    let register_count = node
        .map(|node| {
            node.lists
                .iter()
                .filter_map(Option::as_ref)
                .map(|list| list.registers.len())
                .sum::<usize>()
        })
        .unwrap_or(0);
    let mut output = String::new();
    output
        .try_reserve(register_count.saturating_mul(80).saturating_add(512))
        .map_err(|_| CaptureError::InvalidBuffer)?;
    writeln!(
        output,
        "global --- GuC Error Capture on {engine_name} command stream:"
    )
    .map_err(|_| CaptureError::InvalidBuffer)?;
    let Some(node) = node else {
        output.push_str("  No matching ee-node\n");
        return Ok(output);
    };
    writeln!(
        output,
        "Coverage:  {}",
        if node.is_partial {
            "partial-capture"
        } else {
            "full-capture"
        }
    )
    .map_err(|_| CaptureError::InvalidBuffer)?;
    for list_type in 0..usize::from(CAPTURE_TYPE_MAX) {
        let list = node.lists[list_type].as_ref();
        let type_name = match list_type as u8 {
            CAPTURE_TYPE_GLOBAL => "Global",
            CAPTURE_TYPE_ENGINE_CLASS => "Engine-Class",
            _ => "Engine-Instance",
        };
        writeln!(output, "  RegListType: {type_name}").map_err(|_| CaptureError::InvalidBuffer)?;
        writeln!(output, "    Owner-Id: {}", list.map_or(0, |list| list.vfid))
            .map_err(|_| CaptureError::InvalidBuffer)?;
        match list_type as u8 {
            CAPTURE_TYPE_ENGINE_CLASS => {
                writeln!(output, "    GuC-Eng-Class: {}", node.engine_class)
                    .map_err(|_| CaptureError::InvalidBuffer)?;
                writeln!(
                    output,
                    "    i915-Eng-Class: {}",
                    guc_class_to_engine_class(node.engine_class)
                )
                .map_err(|_| CaptureError::InvalidBuffer)?;
            }
            CAPTURE_TYPE_ENGINE_INSTANCE => {
                if let Some(engine) = engine {
                    writeln!(output, "    i915-Eng-Name: {} command stream", engine.name)
                        .map_err(|_| CaptureError::InvalidBuffer)?;
                    writeln!(output, "    i915-Eng-Inst-Class: 0x{:02x}", engine.class)
                        .map_err(|_| CaptureError::InvalidBuffer)?;
                    writeln!(output, "    i915-Eng-Inst-Id: 0x{:02x}", engine.instance)
                        .map_err(|_| CaptureError::InvalidBuffer)?;
                    writeln!(
                        output,
                        "    i915-Eng-LogicalMask: 0x{:08x}",
                        engine.logical_mask
                    )
                    .map_err(|_| CaptureError::InvalidBuffer)?;
                } else {
                    output.push_str("    i915-Eng-Lookup Fail!\n");
                }
                writeln!(
                    output,
                    "    GuC-Engine-Inst-Id: 0x{:08x}",
                    node.engine_instance
                )
                .map_err(|_| CaptureError::InvalidBuffer)?;
                writeln!(output, "    GuC-Context-Id: 0x{:08x}", node.guc_id)
                    .map_err(|_| CaptureError::InvalidBuffer)?;
                writeln!(output, "    LRCA: 0x{:08x}", node.lrca)
                    .map_err(|_| CaptureError::InvalidBuffer)?;
            }
            _ => {}
        }
        let registers = list.map_or(&[][..], |list| list.registers.as_slice());
        writeln!(output, "    NumRegs: {}", registers.len())
            .map_err(|_| CaptureError::InvalidBuffer)?;
        for register in registers {
            if let Some(name) = gen12_register_name(
                list_type as u32,
                u32::from(node.engine_class),
                engine_base,
                register.offset,
            ) {
                write!(output, "      {name}").map_err(|_| CaptureError::InvalidBuffer)?;
            } else {
                write!(output, "      REG-0x{:08x}", register.offset)
                    .map_err(|_| CaptureError::InvalidBuffer)?;
            }
            let capture_steering_mask =
                (0xf << CAPTURE_STEERING_GROUP_SHIFT) | (0xf << CAPTURE_STEERING_INSTANCE_SHIFT);
            if register.flags & capture_steering_mask != 0
                || steered_offsets.contains(&register.offset)
            {
                let group = (register.flags >> CAPTURE_STEERING_GROUP_SHIFT) & 0xf;
                let instance = (register.flags >> CAPTURE_STEERING_INSTANCE_SHIFT) & 0xf;
                write!(output, "[{group}][{instance}]").map_err(|_| CaptureError::InvalidBuffer)?;
            }
            writeln!(output, ":  0x{:08x}", register.value)
                .map_err(|_| CaptureError::InvalidBuffer)?;
        }
    }
    Ok(output)
}

/// upstream: intel_guc_fwif.h guc_class_to_engine_class().
pub const fn guc_class_to_engine_class(guc_class: u8) -> u8 {
    match guc_class {
        0 => 0, // render
        1 => 1, // video decode
        2 => 2, // video enhancement
        3 => 3, // blitter
        4 => 5, // compute
        5 => 4, // GSC/other
        _ => u8::MAX,
    }
}

/// Match a parsed node to a context and engine using the GuC encoded class /
/// instance, context id, and page-aligned LRCA comparison.
/// upstream: intel_guc_capture.c intel_guc_capture_is_matching_engine().
pub fn is_matching_engine(
    node: &CaptureOutputNode,
    engine_guc_id: u32,
    context_guc_id: u32,
    context_lrca: u32,
) -> bool {
    let engine_class = (engine_guc_id & GUC_ENGINE_CLASS_MASK) as u8;
    let engine_instance =
        ((engine_guc_id & GUC_ENGINE_INSTANCE_MASK) >> GUC_ENGINE_INSTANCE_SHIFT) as u8;
    node.engine_class == engine_class
        && node.engine_instance == engine_instance
        && node.guc_id == context_guc_id
        && (node.lrca & CTX_GTT_ADDRESS_MASK) == (context_lrca & CTX_GTT_ADDRESS_MASK)
}

/// Remove and return the first matching output node, corresponding to the
/// coredump attach operation. Nonmatching nodes preserve their order.
/// upstream: intel_guc_capture.c intel_guc_capture_get_matching_node().
pub fn take_matching_node(
    nodes: &mut Vec<CaptureOutputNode>,
    engine_guc_id: u32,
    context_guc_id: u32,
    context_lrca: u32,
) -> Option<CaptureOutputNode> {
    let index = nodes
        .iter()
        .position(|node| is_matching_engine(node, engine_guc_id, context_guc_id, context_lrca))?;
    Some(nodes.remove(index))
}

/// Extract the two engine error-code registers from a matched node.
/// upstream: intel_guc_capture.c guc_capture_find_ecode().
pub fn find_engine_error_state(
    node: &CaptureOutputNode,
    ipehr_offset: u32,
    instdone_offset: u32,
) -> CapturedEngineState {
    let mut state = CapturedEngineState::default();
    if let Some(instance) = node.lists[usize::from(CAPTURE_TYPE_ENGINE_INSTANCE)].as_ref() {
        for register in &instance.registers {
            if register.offset == ipehr_offset {
                state.ipehr = register.value;
            } else if register.offset == instdone_offset {
                state.instdone = register.value;
            }
        }
    }
    state
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureBufferAssessment {
    Adequate,
    LowSpareCapacity { minimum: usize, preferred: usize },
    TooSmall { minimum: usize, available: usize },
}

/// Return the cached 16-byte null ADS-list header used for absent lists.
/// upstream: intel_guc_capture.c intel_guc_capture_getnullheader().
pub const fn null_capture_header() -> [u8; 4 * 4] {
    [0; 4 * 4]
}

/// Compute the worst-case minimum output size, counting the global list for
/// every engine instance just as GuC emits independent capture groups.
/// upstream: intel_guc_capture.c guc_capture_output_min_size_est().
pub fn capture_output_min_size(
    engines: &[u8],
    mut list_size: impl FnMut(u8, u8) -> Option<usize>,
) -> Result<usize, CaptureError> {
    let mut minimum = 0usize;
    for engine_class in engines {
        minimum = minimum
            .checked_add(CAPTURE_GROUP_HEADER_BYTES)
            .and_then(|size| size.checked_add(3usize.checked_mul(CAPTURE_OUTPUT_HEADER_BYTES)?))
            .ok_or(CaptureError::InvalidBuffer)?;
        for list_type in [
            CAPTURE_TYPE_GLOBAL,
            CAPTURE_TYPE_ENGINE_CLASS,
            CAPTURE_TYPE_ENGINE_INSTANCE,
        ] {
            if let Some(size) = list_size(list_type, *engine_class) {
                minimum = minimum
                    .checked_add(size)
                    .ok_or(CaptureError::InvalidBuffer)?;
            }
        }
    }
    Ok(minimum)
}

/// Compare the configured log capture region with the minimum and 3x spare
/// capacity thresholds used by the upstream diagnostic.
/// upstream: intel_guc_capture.c check_guc_capture_size().
pub fn assess_capture_buffer(minimum: usize, available: usize) -> CaptureBufferAssessment {
    if minimum > available {
        CaptureBufferAssessment::TooSmall { minimum, available }
    } else {
        let preferred = minimum.saturating_mul(CAPTURE_OVERBUFFER_MULTIPLIER);
        if preferred > available {
            CaptureBufferAssessment::LowSpareCapacity { minimum, preferred }
        } else {
            CaptureBufferAssessment::Adequate
        }
    }
}

impl CaptureOutputNode {
    fn empty(is_partial: bool) -> Self {
        Self {
            is_partial,
            engine_class: 0,
            engine_instance: 0,
            guc_id: 0,
            lrca: 0,
            lists: [None, None, None],
        }
    }

    fn clone_lists(&self, keep_global: bool, keep_class: bool) -> Self {
        let mut node = Self::empty(self.is_partial);
        if keep_global {
            node.lists[usize::from(CAPTURE_TYPE_GLOBAL)] =
                self.lists[usize::from(CAPTURE_TYPE_GLOBAL)].clone();
        }
        if keep_class {
            node.lists[usize::from(CAPTURE_TYPE_ENGINE_CLASS)] =
                self.lists[usize::from(CAPTURE_TYPE_ENGINE_CLASS)].clone();
            node.engine_class = self.engine_class;
        }
        node
    }
}

/// Split dependent-engine capture lists into engine nodes, cloning common
/// global/class state in the same cases as the upstream parsed-output list.
/// `max_mmio_per_node` mirrors the preallocated node capacity and clips larger
/// lists rather than allocating unbounded output from firmware data.
/// upstream: intel_guc_capture.c guc_capture_extract_reglists().
pub fn build_capture_output_nodes(
    group: &CaptureGroup,
    max_mmio_per_node: usize,
) -> Result<Vec<CaptureOutputNode>, CaptureError> {
    let mut output = Vec::new();
    let mut current: Option<CaptureOutputNode> = None;
    for input in &group.lists {
        let list_type = input.capture_type;
        if list_type >= CAPTURE_TYPE_MAX {
            continue;
        }
        let next_node = current.as_ref().and_then(|node| {
            let boundary = match list_type {
                CAPTURE_TYPE_GLOBAL => Some((false, false)),
                CAPTURE_TYPE_ENGINE_CLASS
                    if node.lists[usize::from(CAPTURE_TYPE_ENGINE_CLASS)].is_some() =>
                {
                    Some((true, false))
                }
                CAPTURE_TYPE_ENGINE_INSTANCE
                    if node.lists[usize::from(CAPTURE_TYPE_ENGINE_INSTANCE)].is_some() =>
                {
                    Some((true, true))
                }
                _ => None,
            };
            boundary.map(|(keep_global, keep_class)| node.clone_lists(keep_global, keep_class))
        });
        if let Some(next_node) = next_node {
            output
                .try_reserve(1)
                .map_err(|_| CaptureError::InvalidBuffer)?;
            output.push(current.take().ok_or(CaptureError::InvalidBuffer)?);
            current = Some(next_node);
        }
        if current.is_none() {
            current = Some(CaptureOutputNode::empty(
                group.group_type == CAPTURE_GROUP_PARTIAL,
            ));
        }
        let node = current.as_mut().ok_or(CaptureError::InvalidBuffer)?;
        let mut list = input.clone();
        list.registers.truncate(max_mmio_per_node);
        match list_type {
            CAPTURE_TYPE_ENGINE_CLASS => node.engine_class = input.engine_class,
            CAPTURE_TYPE_ENGINE_INSTANCE => {
                node.engine_class = input.engine_class;
                node.engine_instance = input.engine_instance;
                node.lrca = input.lrca;
                node.guc_id = input.guc_id;
            }
            _ => {}
        }
        node.lists[usize::from(list_type)] = Some(list);
    }
    if let Some(node) = current {
        output
            .try_reserve(1)
            .map_err(|_| CaptureError::InvalidBuffer)?;
        output.push(node);
    }
    Ok(output)
}

struct PooledCaptureNode {
    output: CaptureOutputNode,
    register_buffers: [Vec<CaptureRegister>; 3],
}

impl PooledCaptureNode {
    fn new(max_registers: usize) -> Result<Self, CaptureError> {
        let mut register_buffers: [Vec<CaptureRegister>; 3] = core::array::from_fn(|_| Vec::new());
        for registers in &mut register_buffers {
            registers
                .try_reserve_exact(max_registers)
                .map_err(|_| CaptureError::InvalidBuffer)?;
        }
        Ok(Self {
            output: CaptureOutputNode::empty(false),
            register_buffers,
        })
    }

    fn reset(&mut self) {
        for index in 0..3 {
            if let Some(mut list) = self.output.lists[index].take() {
                list.registers.clear();
                self.register_buffers[index] = list.registers;
            }
            self.register_buffers[index].clear();
        }
        self.output.is_partial = false;
        self.output.engine_class = 0;
        self.output.engine_instance = 0;
        self.output.guc_id = 0;
        self.output.lrca = 0;
    }

    fn set_list(&mut self, input: &CaptureList, max_registers: usize) -> Result<(), CaptureError> {
        let index = usize::from(input.capture_type);
        if index >= 3 {
            return Err(CaptureError::InvalidType);
        }
        let count = input.registers.len().min(max_registers);
        let registers = &mut self.register_buffers[index];
        registers.clear();
        if registers.capacity() < count {
            registers
                .try_reserve_exact(count - registers.len())
                .map_err(|_| CaptureError::InvalidBuffer)?;
        }
        registers.extend_from_slice(&input.registers[..count]);
        let registers = core::mem::take(registers);
        self.output.lists[index] = Some(CaptureList {
            vfid: input.vfid,
            capture_type: input.capture_type,
            engine_class: input.engine_class,
            engine_instance: input.engine_instance,
            lrca: input.lrca,
            guc_id: input.guc_id,
            registers,
        });
        Ok(())
    }

    fn copy_lists(&mut self, source: &CaptureOutputNode, keep_global: bool, keep_class: bool) {
        self.output.is_partial = source.is_partial;
        let keep = [keep_global, keep_class, false];
        for index in 0..3 {
            if !keep[index] {
                continue;
            }
            let Some(list) = source.lists[index].as_ref() else {
                continue;
            };
            let registers = &mut self.register_buffers[index];
            registers.clear();
            // Each source list was clipped to the same preallocated maximum.
            registers.extend_from_slice(&list.registers);
            let registers = core::mem::take(registers);
            self.output.lists[index] = Some(CaptureList {
                vfid: list.vfid,
                capture_type: list.capture_type,
                engine_class: list.engine_class,
                engine_instance: list.engine_instance,
                lrca: list.lrca,
                guc_id: list.guc_id,
                registers,
            });
        }
        if keep_class {
            self.output.engine_class = source.engine_class;
        }
    }
}

/// Bounded preallocated capture-node cache matching the upstream cachelist /
/// outlist policy. When the cache is empty, the newest outlist node is reused.
pub struct CaptureNodeCache {
    free: Vec<PooledCaptureNode>,
    output: Vec<PooledCaptureNode>,
    max_registers: usize,
}

impl CaptureNodeCache {
    /// upstream: guc_capture_create_prealloc_nodes()/guc_capture_alloc_one_node().
    pub fn new(node_count: usize, max_registers: usize) -> Result<Self, CaptureError> {
        if node_count == 0 || max_registers == 0 {
            return Err(CaptureError::InvalidBuffer);
        }
        let mut free = Vec::new();
        let mut output = Vec::new();
        free.try_reserve_exact(node_count)
            .map_err(|_| CaptureError::InvalidBuffer)?;
        output
            .try_reserve_exact(node_count)
            .map_err(|_| CaptureError::InvalidBuffer)?;
        for _ in 0..node_count {
            free.push(PooledCaptureNode::new(max_registers)?);
        }
        Ok(Self {
            free,
            output,
            max_registers,
        })
    }

    /// Use the upstream cache depth and default per-node register capacity.
    pub fn new_upstream() -> Result<Self, CaptureError> {
        Self::new(
            CAPTURE_PREALLOC_NODE_COUNT,
            CAPTURE_PREALLOC_DEFAULT_REGISTERS,
        )
    }

    /// upstream: guc_capture_get_prealloc_node(); steal newest outlist node on pressure.
    fn take_node(&mut self) -> Option<PooledCaptureNode> {
        let mut node = self.free.pop().or_else(|| self.output.pop())?;
        node.reset();
        Some(node)
    }

    /// Split and append all nodes from one GuC group without allocating node
    /// or register arrays in the capture processing path.
    /// upstream: intel_guc_capture.c guc_capture_extract_reglists().
    pub fn process_group(&mut self, group: &CaptureGroup) -> Result<usize, CaptureError> {
        let mut generated = 0usize;
        let mut current: Option<PooledCaptureNode> = None;
        for input in &group.lists {
            let list_type = input.capture_type;
            if list_type >= CAPTURE_TYPE_MAX {
                continue;
            }
            let boundary = current.as_ref().and_then(|node| match list_type {
                CAPTURE_TYPE_GLOBAL => Some((false, false)),
                CAPTURE_TYPE_ENGINE_CLASS
                    if node.output.lists[usize::from(CAPTURE_TYPE_ENGINE_CLASS)].is_some() =>
                {
                    Some((true, false))
                }
                CAPTURE_TYPE_ENGINE_INSTANCE
                    if node.output.lists[usize::from(CAPTURE_TYPE_ENGINE_INSTANCE)].is_some() =>
                {
                    Some((true, true))
                }
                _ => None,
            });
            if let Some((keep_global, keep_class)) = boundary {
                let old = current.take().ok_or(CaptureError::InvalidBuffer)?;
                let Some(mut next) = self.take_node() else {
                    self.output.push(old);
                    return Err(CaptureError::InvalidBuffer);
                };
                next.output.is_partial = old.output.is_partial;
                next.copy_lists(&old.output, keep_global, keep_class);
                self.output.push(old);
                generated += 1;
                current = Some(next);
            }
            if current.is_none() {
                current = Some(self.take_node().ok_or(CaptureError::InvalidBuffer)?);
                if let Some(node) = current.as_mut() {
                    node.output.is_partial = group.group_type == CAPTURE_GROUP_PARTIAL;
                }
            }
            let node = current.as_mut().ok_or(CaptureError::InvalidBuffer)?;
            node.set_list(input, self.max_registers)?;
            match list_type {
                CAPTURE_TYPE_ENGINE_CLASS => node.output.engine_class = input.engine_class,
                CAPTURE_TYPE_ENGINE_INSTANCE => {
                    node.output.engine_class = input.engine_class;
                    node.output.engine_instance = input.engine_instance;
                    node.output.lrca = input.lrca;
                    node.output.guc_id = input.guc_id;
                }
                _ => {}
            }
        }
        if let Some(node) = current {
            self.output.push(node);
            generated += 1;
        }
        Ok(generated)
    }

    /// upstream: intel_guc_capture_get_matching_node().
    pub fn take_matching_node(
        &mut self,
        engine_guc_id: u32,
        context_guc_id: u32,
        context_lrca: u32,
    ) -> Option<CaptureOutputNode> {
        let index = self.output.iter().position(|node| {
            is_matching_engine(&node.output, engine_guc_id, context_guc_id, context_lrca)
        })?;
        let mut pooled = self.output.remove(index);
        let output = pooled.output.clone();
        pooled.reset();
        self.free.push(pooled);
        Some(output)
    }

    pub fn free_len(&self) -> usize {
        self.free.len()
    }

    pub fn output_len(&self) -> usize {
        self.output.len()
    }
}

/// Decode a GuC capture group. Captures remain separate, preserving their
/// original order and metadata for the engine-reset/core-dump consumer.
/// upstream: intel_guc_capture.c guc_capture_extract_reglists().
pub fn extract_group(buffer: &mut CaptureBuffer<'_>) -> Result<CaptureGroup, CaptureError> {
    let available = buffer.count();
    if available == 0 {
        return Err(CaptureError::Empty);
    }
    if available % 4 != 0 {
        return Err(CaptureError::Misaligned);
    }
    // upstream: guc_capture_log_get_group_hdr().
    let [group_owner, group_info] = buffer.read_words::<2>()?;
    let group_type = ((group_info >> 8) & 0xff) as u8;
    if group_type >= CAPTURE_GROUP_TYPE_MAX {
        return Err(CaptureError::InvalidType);
    }
    let num_lists = (group_info & 0xff) as usize;
    let mut lists = Vec::new();
    lists
        .try_reserve_exact(num_lists)
        .map_err(|_| CaptureError::InvalidBuffer)?;

    for _ in 0..num_lists {
        // upstream: guc_capture_log_get_data_hdr().
        let [owner, info, lrca, guc_id, count_word] = buffer.read_words::<5>()?;
        let capture_type = (info & 0xf) as u8;
        let num_registers = (count_word & 0x3ff) as usize;
        let mut registers = Vec::new();
        if capture_type < CAPTURE_TYPE_MAX {
            registers
                .try_reserve_exact(num_registers)
                .map_err(|_| CaptureError::InvalidBuffer)?;
        }
        for _ in 0..num_registers {
            // upstream: guc_capture_log_get_register().
            let [offset, value, flags, mask] = buffer.read_words::<4>()?;
            if capture_type < CAPTURE_TYPE_MAX {
                registers.push(CaptureRegister {
                    offset,
                    value,
                    flags,
                    mask,
                });
            }
        }
        if capture_type < CAPTURE_TYPE_MAX {
            lists.push(CaptureList {
                vfid: (owner & 0xff) as u8,
                capture_type,
                engine_class: ((info >> 4) & 0xf) as u8,
                engine_instance: ((info >> 8) & 0xf) as u8,
                lrca,
                guc_id,
                registers,
            });
        }
    }
    Ok(CaptureGroup {
        vfid: (group_owner & 0xff) as u8,
        group_type,
        lists,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn words_to_bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    #[test]
    fn capture_group_extracts_register_metadata() {
        let mut bytes = words_to_bytes(&[
            3,
            1 << 8 | 1, // partial group, one capture
            7,
            2 << 4 | 1 << 8 | 2, // vfid, instance, class 2
            0x1000,
            9,
            1, // LRCA, GuC id, one MMIO
            0x1234,
            0x5678,
            3,
            0xffff,
        ]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut ring = CaptureBuffer::new(&bytes, 0, bytes.len() - 4).unwrap();
        let group = extract_group(&mut ring).unwrap();
        assert_eq!(group.vfid, 3);
        assert_eq!(group.group_type, CAPTURE_GROUP_PARTIAL);
        assert_eq!(group.lists.len(), 1);
        assert_eq!(group.lists[0].vfid, 7);
        assert_eq!(group.lists[0].engine_class, 2);
        assert_eq!(group.lists[0].engine_instance, 1);
        assert_eq!(group.lists[0].registers[0].offset, 0x1234);
        assert_eq!(group.lists[0].registers[0].value, 0x5678);
        assert_eq!(ring.count(), 0);
    }

    #[test]
    fn capture_buffer_reads_dwords_across_ring_wrap() {
        let bytes = words_to_bytes(&[10, 11, 12, 13]);
        let mut ring = CaptureBuffer::new(&bytes, 12, 8).unwrap();
        assert_eq!(ring.count(), 12);
        assert_eq!(ring.read_dword(), Ok(13));
        assert_eq!(ring.read_dword(), Ok(10));
        assert_eq!(ring.read_dword(), Ok(11));
        assert_eq!(ring.count(), 0);
    }

    #[test]
    fn capture_buffer_allows_end_offsets_and_processes_log_state() {
        let mut data = words_to_bytes(&[0, 0]); // empty group header at end offset
        let ring = CaptureBuffer::new(&data, data.len(), data.len()).unwrap();
        assert_eq!(ring.count(), 0);

        let mut state = CaptureLogState {
            read_ptr: data.len(),
            sampled_write_ptr: data.len(),
            buffer_full_count: 0,
            flush_to_file: 2,
        };
        let mut stats = CaptureLogStats::default();
        let result = process_capture_log(&data, &mut state, &mut stats, false);
        assert!(result.groups.is_empty());
        assert_eq!(result.parse_error, None);
        assert_eq!(result.flush_count, 2);
        assert_eq!(stats.flush, 2);
        assert_eq!(state.read_ptr, data.len());
        assert_eq!(state.flush_to_file, 0);

        // A fresh firmware overflow forces a full buffer parse and updates the
        // sampled counter, including the 4-bit rollover rule.
        state.read_ptr = 1;
        state.sampled_write_ptr = 2;
        state.buffer_full_count = 0;
        stats.sampled_overflow = 15;
        data.resize(16, 0);
        let result = process_capture_log(&data, &mut state, &mut stats, true);
        assert!(result.overflow);
        assert_eq!(stats.sampled_overflow, 16);
        assert_eq!(state.read_ptr, data.len());
    }

    #[test]
    fn ads_capture_list_is_packed_and_page_aligned() {
        let registers = [CaptureRegister {
            offset: 0x1234,
            value: 0,
            flags: 3,
            mask: 0xff,
        }];
        let bytes = build_ads_capture_list(&registers).unwrap();
        assert_eq!(bytes.len(), PAGE_SIZE);
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 0x1234);
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            CAPTURE_REGISTER_VALUE_PLACEHOLDER
        );
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 0xff);
        assert!(bytes[20..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn capture_lists_select_global_and_append_topology_extensions() {
        let global_regs = [CaptureRegister {
            offset: 0x10,
            value: 0,
            flags: 0,
            mask: 0,
        }];
        let class_regs = [CaptureRegister {
            offset: 0x20,
            value: 0,
            flags: 0,
            mask: 0,
        }];
        let ext_regs = [CaptureRegister {
            offset: 0x30,
            value: 0,
            flags: 2,
            mask: 0,
        }];
        let lists = [
            CaptureRegisterList {
                owner: 0,
                list_type: u32::from(CAPTURE_TYPE_GLOBAL),
                engine: 0,
                registers: &global_regs,
            },
            CaptureRegisterList {
                owner: 0,
                list_type: u32::from(CAPTURE_TYPE_ENGINE_CLASS),
                engine: 2,
                registers: &class_regs,
            },
        ];
        let extensions = [CaptureRegisterList {
            owner: 0,
            list_type: u32::from(CAPTURE_TYPE_ENGINE_CLASS),
            engine: 2,
            registers: &ext_regs,
        }];
        let global = get_one_list(&lists, 0, 0, 4).unwrap();
        assert_eq!(global.registers.len(), 1);
        let class = get_one_list(&lists, 0, 1, 2).unwrap();
        let extension = get_one_list(&extensions, 0, 1, 2);
        assert_eq!(capture_list_register_count(Some(class), extension), Ok(2));
        assert_eq!(capture_list_size(2), Ok(PAGE_SIZE));
        let bytes = build_ads_capture_list_from_groups(class, extension).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 0x20);
        assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 0x30);
    }

    #[test]
    fn capture_steered_registers_expand_per_slice_and_subslice() {
        let regs =
            expand_steered_registers(&[(1, 2), (3, 4)], &[0x100, 0x104], Some(0x108)).unwrap();
        assert_eq!(regs.len(), 6);
        assert_eq!(regs[0].offset, 0x100);
        assert_eq!(regs[2].offset, 0x108);
        assert_eq!(
            regs[0].flags,
            (1 << CAPTURE_STEERING_GROUP_SHIFT) | (2 << CAPTURE_STEERING_INSTANCE_SHIFT)
        );
        assert_eq!(
            regs[3].flags,
            (3 << CAPTURE_STEERING_GROUP_SHIFT) | (4 << CAPTURE_STEERING_INSTANCE_SHIFT)
        );
    }

    #[test]
    fn gen12_static_capture_lists_match_xe_lp_class_tables() {
        let global = gen12_static_registers(0, 0, 4, 0).unwrap();
        assert_eq!(global.len(), 9);
        assert_eq!(global[0].offset, 0xa188);
        assert_eq!(global[4].offset, 0xceb8);
        assert_eq!(gen12_static_registers(0, 1, 0, 0).unwrap().len(), 3);
        assert_eq!(gen12_static_registers(0, 1, 2, 0).unwrap().len(), 4);
        assert!(gen12_static_registers(0, 1, 1, 0).unwrap().is_empty());
        let instance = gen12_static_registers(0, 2, 3, 0x2000).unwrap();
        assert_eq!(instance.len(), 33);
        assert_eq!(instance[0].offset, 0x2050);
        assert_eq!(instance[32].offset, 0x228c);
        assert!(gen12_static_registers(1, 0, 0, 0).is_none());
        assert_eq!(
            gen12_register_name(0, 0, 0, 0xceb8),
            Some("GEN12_FAULT_TLB_DATA0")
        );
        assert_eq!(gen12_register_name(1, 2, 0, 0x1ce000), Some("SFC_DONE[2]"));
        assert_eq!(gen12_register_name(2, 3, 0x2000, 0x2068), Some("IPEHR"));
        assert_eq!(gen12_register_name(2, 3, 0x2000, 0x2064), Some("IPEIR"));
        assert_eq!(gen12_register_name(2, 3, 0x2000, 0x9999), None);
    }

    #[test]
    fn dependent_capture_nodes_clone_global_and_class_registers() {
        let regs = vec![CaptureRegister {
            offset: 1,
            value: 0,
            flags: 0,
            mask: 0,
        }];
        let make = |capture_type, engine_instance| CaptureList {
            vfid: 0,
            capture_type,
            engine_class: 0,
            engine_instance,
            lrca: u32::from(engine_instance),
            guc_id: u32::from(engine_instance),
            registers: regs.clone(),
        };
        let group = CaptureGroup {
            vfid: 0,
            group_type: CAPTURE_GROUP_FULL,
            lists: vec![
                make(CAPTURE_TYPE_GLOBAL, 0),
                make(CAPTURE_TYPE_ENGINE_CLASS, 0),
                make(CAPTURE_TYPE_ENGINE_INSTANCE, 0),
                make(CAPTURE_TYPE_ENGINE_INSTANCE, 1),
            ],
        };
        let nodes = build_capture_output_nodes(&group, 1).unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0].lists[usize::from(CAPTURE_TYPE_GLOBAL)].is_some());
        assert!(nodes[0].lists[usize::from(CAPTURE_TYPE_ENGINE_CLASS)].is_some());
        assert_eq!(nodes[0].guc_id, 0);
        assert!(nodes[1].lists[usize::from(CAPTURE_TYPE_GLOBAL)].is_some());
        assert!(nodes[1].lists[usize::from(CAPTURE_TYPE_ENGINE_CLASS)].is_some());
        assert_eq!(nodes[1].guc_id, 1);
    }

    #[test]
    fn preallocated_capture_cache_reuses_nodes_and_clips_register_lists() {
        let registers = vec![
            CaptureRegister {
                offset: 1,
                value: 1,
                flags: 0,
                mask: 0,
            },
            CaptureRegister {
                offset: 2,
                value: 2,
                flags: 0,
                mask: 0,
            },
            CaptureRegister {
                offset: 3,
                value: 3,
                flags: 0,
                mask: 0,
            },
        ];
        let make = |capture_type, instance| CaptureList {
            vfid: 0,
            capture_type,
            engine_class: 0,
            engine_instance: instance,
            lrca: 0x1000 + u32::from(instance) * 0x1000,
            guc_id: u32::from(instance),
            registers: registers.clone(),
        };
        let group = CaptureGroup {
            vfid: 0,
            group_type: CAPTURE_GROUP_FULL,
            lists: vec![
                make(CAPTURE_TYPE_GLOBAL, 0),
                make(CAPTURE_TYPE_ENGINE_CLASS, 0),
                make(CAPTURE_TYPE_ENGINE_INSTANCE, 0),
                make(CAPTURE_TYPE_ENGINE_INSTANCE, 1),
            ],
        };
        let mut cache = CaptureNodeCache::new(3, 2).unwrap();
        assert_eq!(cache.free_len(), 3);
        assert_eq!(cache.process_group(&group), Ok(2));
        assert_eq!(cache.output_len(), 2);
        assert_eq!(cache.free_len(), 1);
        let matched = cache
            .take_matching_node(0 | (1 << GUC_ENGINE_INSTANCE_SHIFT), 1, 0x2001)
            .unwrap();
        assert_eq!(matched.guc_id, 1);
        assert_eq!(
            matched.lists[usize::from(CAPTURE_TYPE_ENGINE_INSTANCE)]
                .as_ref()
                .unwrap()
                .registers
                .len(),
            2
        );
        assert_eq!(cache.free_len(), 2);
        assert_eq!(cache.output_len(), 1);
    }

    #[test]
    fn capture_buffer_minimum_and_spare_size_follow_upstream_estimate() {
        assert_eq!(null_capture_header(), [0; 16]);
        let minimum = capture_output_min_size(&[0, 1], |list_type, _class| {
            Some(match list_type {
                CAPTURE_TYPE_GLOBAL => 4,
                CAPTURE_TYPE_ENGINE_CLASS => 8,
                _ => 12,
            })
        })
        .unwrap();
        assert_eq!(
            minimum,
            2 * (CAPTURE_GROUP_HEADER_BYTES + 3 * CAPTURE_OUTPUT_HEADER_BYTES + 24)
        );
        assert_eq!(
            assess_capture_buffer(minimum, minimum * CAPTURE_OVERBUFFER_MULTIPLIER),
            CaptureBufferAssessment::Adequate
        );
        assert_eq!(
            assess_capture_buffer(minimum, minimum * 3 - 1),
            CaptureBufferAssessment::LowSpareCapacity {
                minimum,
                preferred: minimum * 3
            }
        );
        assert_eq!(
            assess_capture_buffer(minimum, minimum - 1),
            CaptureBufferAssessment::TooSmall {
                minimum,
                available: minimum - 1
            }
        );
    }

    #[test]
    fn capture_node_matches_guc_id_and_page_aligned_lrca_then_extracts_error_regs() {
        let mut node = CaptureOutputNode::empty(false);
        node.engine_class = 2;
        node.engine_instance = 3;
        node.guc_id = 0x1234;
        node.lrca = 0x4567_8000;
        node.lists[usize::from(CAPTURE_TYPE_ENGINE_INSTANCE)] = Some(CaptureList {
            vfid: 0,
            capture_type: CAPTURE_TYPE_ENGINE_INSTANCE,
            engine_class: 2,
            engine_instance: 3,
            lrca: node.lrca,
            guc_id: node.guc_id,
            registers: vec![
                CaptureRegister {
                    offset: 0x2068,
                    value: 0xaa,
                    flags: 0,
                    mask: 0,
                },
                CaptureRegister {
                    offset: 0x206c,
                    value: 0xbb,
                    flags: 0,
                    mask: 0,
                },
            ],
        });
        let engine_id = 2 | (3 << GUC_ENGINE_INSTANCE_SHIFT);
        assert!(is_matching_engine(&node, engine_id, 0x1234, 0x4567_8fff));
        assert!(!is_matching_engine(&node, engine_id, 0x1235, node.lrca));
        assert_eq!(
            find_engine_error_state(&node, 0x2068, 0x206c),
            CapturedEngineState {
                ipehr: 0xaa,
                instdone: 0xbb
            }
        );
        let mut nodes = vec![node];
        assert!(take_matching_node(&mut nodes, engine_id, 0x1234, 0x4567_8001).is_some());
        assert!(nodes.is_empty());
    }

    #[test]
    fn capture_node_formatter_emits_source_headers_names_and_values() {
        let mut node = CaptureOutputNode::empty(true);
        node.engine_class = 0;
        node.engine_instance = 1;
        node.guc_id = 0x22;
        node.lrca = 0x1234_5000;
        node.lists[usize::from(CAPTURE_TYPE_GLOBAL)] = Some(CaptureList {
            vfid: 0,
            capture_type: CAPTURE_TYPE_GLOBAL,
            engine_class: 0,
            engine_instance: 0,
            lrca: u32::MAX,
            guc_id: u32::MAX,
            registers: vec![CaptureRegister {
                offset: 0xceb8,
                value: 0xfeed,
                flags: 0,
                mask: 0,
            }],
        });
        let formatted = format_capture_node("rcs0", Some(&node), None, 0x2000, &[]).unwrap();
        assert!(formatted.contains("Coverage:  partial-capture"));
        assert!(formatted.contains("GEN12_FAULT_TLB_DATA0:  0x0000feed"));
        assert!(formatted.contains("i915-Eng-Lookup Fail!"));
        assert!(formatted.contains("GuC-Context-Id: 0x00000022"));
        let extended = format_capture_node("rcs0", Some(&node), None, 0x2000, &[0xceb8]).unwrap();
        assert!(extended.contains("GEN12_FAULT_TLB_DATA0[0][0]"));
    }

    #[test]
    fn unknown_capture_types_are_skipped_as_upstream_does() {
        let mut bytes = words_to_bytes(&[
            0, 1, // full group, one capture
            0, 9, // unknown capture type
            0, 0, 1, // one MMIO to skip
            4, 5, 6, 7,
        ]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut ring = CaptureBuffer::new(&bytes, 0, bytes.len() - 4).unwrap();
        let group = extract_group(&mut ring).unwrap();
        assert!(group.lists.is_empty());
    }
}
