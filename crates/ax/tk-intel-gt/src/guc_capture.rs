// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_capture.c and
// guc_capture_fwif.h. Copyright © 2021-2022 Intel Corporation.

use alloc::vec::Vec;

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
        if data.is_empty() || rd >= data.len() || wr >= data.len() {
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

#[derive(Clone, Copy)]
pub struct CaptureRegisterList<'a> {
    pub owner: u32,
    pub list_type: u32,
    pub engine: u32,
    pub registers: &'a [CaptureRegister],
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
