//! Intel VT-d fault log and interrupt/task handling translated from FreeBSD
//! sys/x86/iommu/intel_fault.c (snapshot c2b7fe4, BSD-2-Clause).
//! Copyright (c) 2013 The FreeBSD Foundation; Konstantin Belousov under
//! Foundation sponsorship. Interrupt/taskqueue, context lookup and spin-lock
//! services are mapped through FaultIo. Full grant in LICENSES/.
use alloc::vec::Vec;

use crate::{Error, reg::*, utils::RegisterIo};

const FSTS: u64 = DMAR_FSTS_REG;
const FECTL: u64 = DMAR_FECTL_REG;
const DEFAULT_FAULT_LOG_WORDS: usize = 256;
const MAX_FAULT_LOG_WORDS: usize = 1 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultStatus {
    InvalidationTimeout,
    InvalidationCompletion,
    InvalidationQueue,
    AdvancedPending,
    AdvancedOverflow,
    PrimaryOverflow,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultRecord {
    pub address: u64,
    pub info: u64,
}

/// Circular log indices are word offsets, reserving one two-word record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultLog {
    words: Vec<u64>,
    pub head: usize,
    pub tail: usize,
}
impl FaultLog {
    pub fn new(size_words: usize) -> Result<Self, Error> {
        if size_words < 4 || size_words % 2 != 0 || size_words > MAX_FAULT_LOG_WORDS {
            return Err(Error::InvalidRange);
        }
        let mut words = Vec::new();
        words
            .try_reserve_exact(size_words)
            .map_err(|_| Error::OutOfMemory)?;
        words.resize(size_words, 0);
        Ok(Self {
            words,
            head: 0,
            tail: 0,
        })
    }
    pub const fn size_words(&self) -> usize {
        self.words.len()
    }
    /// upstream: intel_fault.c dmar_fault_next()
    pub fn next_index(&self, index: usize) -> usize {
        let next = index + 2;
        if next == self.size_words() { 0 } else { next }
    }
    fn push(&mut self, record: FaultRecord) -> bool {
        let next = self.next_index(self.head);
        if next == self.tail {
            return false;
        }
        self.words[self.head] = record.address;
        self.words[self.head + 1] = record.info;
        self.head = next;
        true
    }
    fn pop(&mut self) -> Option<FaultRecord> {
        if self.tail == self.head {
            return None;
        }
        let record = FaultRecord {
            address: self.words[self.tail],
            info: self.words[self.tail + 1],
        };
        self.tail = self.next_index(self.tail);
        Some(record)
    }
}

pub trait FaultIo: RegisterIo {
    fn unit_index(&self) -> u32;
    fn report_status(&mut self, status: FaultStatus);
    fn schedule_fault_task(&mut self);
    fn drain_fault_task(&mut self);
    fn lookup_context(&mut self, source_id: u16) -> bool;
    fn remember_context_fault(&mut self, source_id: u16, record: FaultRecord);
    fn report_fault(&mut self, source_id: u16, record: FaultRecord);
}

/// upstream: intel_fault.c dmar_fault_intr_clear()
pub fn dmar_fault_intr_clear<I: FaultIo>(io: &mut I, status: u32) -> u32 {
    let mut clear = 0;
    for (bit, notice) in [
        (DMAR_FSTS_ITE as u32, FaultStatus::InvalidationTimeout),
        (DMAR_FSTS_ICE as u32, FaultStatus::InvalidationCompletion),
        (DMAR_FSTS_IQE as u32, FaultStatus::InvalidationQueue),
        (DMAR_FSTS_APF as u32, FaultStatus::AdvancedPending),
        (DMAR_FSTS_AFO as u32, FaultStatus::AdvancedOverflow),
    ] {
        if status & bit != 0 {
            io.report_status(notice);
            clear |= bit;
        }
    }
    if clear != 0 {
        io.write32(FSTS, clear);
    }
    clear
}

/// Fast interrupt: copy hardware fault registers into a bounded ring, clear
/// each hardware record, clear PFO, and queue one deferred report task.
/// upstream: intel_fault.c dmar_fault_intr()
pub fn dmar_fault_intr<I: FaultIo>(io: &mut I, log: &mut FaultLog) -> Result<bool, Error> {
    let fsts = io.read32(FSTS);
    dmar_fault_intr_clear(io, fsts);
    let mut queued = false;
    if fsts & DMAR_FSTS_PPF as u32 != 0 {
        let nfr = DMAR_CAP_NFR(io.hw_cap()) as usize;
        if nfr == 0 {
            return Err(Error::InvalidStructure);
        }
        let mut index = DMAR_FSTS_FRI(fsts as u64) as usize;
        if index >= nfr {
            return Err(Error::InvalidStructure);
        }
        loop {
            let offset = (DMAR_CAP_FRO(io.hw_cap()) + index as u64) * 16;
            let high = io.read64(offset + 8);
            if high & DMAR_FRCD2_F == 0 {
                break;
            }
            let low = io.read64(offset);
            io.write32(offset + 12, DMAR_FRCD2_F32 as u32);
            queued |= log.push(FaultRecord {
                address: low,
                info: high,
            });
            index += 1;
            if index >= nfr {
                index = 0;
            }
        }
    }
    // Sandy/Ivy/Haswell errata require clearing PFO even with no PPF record.
    if fsts & DMAR_FSTS_PFO as u32 != 0 {
        io.report_status(FaultStatus::PrimaryOverflow);
        io.write32(FSTS, DMAR_FSTS_PFO as u32);
    }
    if queued {
        io.schedule_fault_task();
    }
    Ok(queued)
}

/// Deferred report path resolves the requester, stores its last fault, and
/// prints/renders PCI address, access, address type and reason.
/// upstream: intel_fault.c dmar_fault_task()
pub fn dmar_fault_task<I: FaultIo>(io: &mut I, log: &mut FaultLog) {
    while let Some(record) = log.pop() {
        let sid = DMAR_FRCD2_SID(record.info) as u16;
        let context_found = io.lookup_context(sid);
        if context_found {
            io.remember_context_fault(sid, record);
        }
        io.report_fault(sid, record);
    }
}

/// Clear every outstanding fault-record F bit and write back FSTS.
/// upstream: intel_fault.c dmar_clear_faults()
pub fn dmar_clear_faults<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    let count = DMAR_CAP_NFR(io.hw_cap()) as usize;
    let first = DMAR_CAP_FRO(io.hw_cap()) * 16;
    for index in 0..count {
        let offset = first + index as u64 * 16 + 12;
        if io.read32(offset) & DMAR_FRCD2_F32 as u32 != 0 {
            io.write32(offset, DMAR_FRCD2_F32 as u32);
        }
    }
    let status = io.read32(FSTS);
    io.write32(FSTS, status);
    Ok(())
}

/// Initialize a zeroed 128-record software ring and the masked fault interrupt.
/// Allocation/taskqueue creation and lock initialization map to the kernel seam.
/// upstream: intel_fault.c dmar_init_fault_log()
pub fn dmar_init_fault_log<I: FaultIo>(
    io: &mut I,
    size_words: Option<usize>,
    mut setup_task: impl FnMut() -> Result<(), Error>,
) -> Result<FaultLog, Error> {
    let size = size_words.unwrap_or(DEFAULT_FAULT_LOG_WORDS);
    let mut log = FaultLog::new(size)?;
    setup_task()?;
    dmar_disable_fault_intr(io)?;
    dmar_clear_faults(io)?;
    dmar_enable_fault_intr(io)?;
    log.head = 0;
    log.tail = 0;
    Ok(log)
}

/// Mask interrupts, drain deferred logging, and release fault storage.
/// upstream: intel_fault.c dmar_fini_fault_log()
pub fn dmar_fini_fault_log<I: FaultIo>(
    io: &mut I,
    log: Option<FaultLog>,
    taskqueue_present: bool,
) -> Result<Option<FaultLog>, Error> {
    if !taskqueue_present {
        return Ok(log);
    }
    dmar_disable_fault_intr(io)?;
    io.drain_fault_task();
    Ok(None)
}

/// upstream: intel_fault.c dmar_enable_fault_intr()
pub fn dmar_enable_fault_intr<I: FaultIo>(io: &mut I) -> Result<(), Error> {
    let control = io.read32(FECTL) & !(DMAR_FECTL_IM as u32);
    io.write32(FECTL, control);
    Ok(())
}
/// upstream: intel_fault.c dmar_disable_fault_intr()
pub fn dmar_disable_fault_intr<I: FaultIo>(io: &mut I) -> Result<(), Error> {
    let control = io.read32(FECTL) | DMAR_FECTL_IM as u32;
    io.write32(FECTL, control);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    struct FakeIo {
        regs: [u32; 256],
        cap: u64,
        ecap: u64,
        now: u64,
        status: Vec<FaultStatus>,
        scheduled: u32,
        drains: u32,
        remembered: Vec<(u16, FaultRecord)>,
        reported: Vec<(u16, FaultRecord)>,
    }
    impl Default for FakeIo {
        fn default() -> Self {
            Self {
                regs: [0; 256],
                cap: 0,
                ecap: 0,
                now: 0,
                status: Vec::new(),
                scheduled: 0,
                drains: 0,
                remembered: Vec::new(),
                reported: Vec::new(),
            }
        }
    }
    impl RegisterIo for FakeIo {
        fn read32(&mut self, o: u64) -> u32 {
            self.regs[o as usize / 4]
        }
        fn write32(&mut self, o: u64, v: u32) {
            self.regs[o as usize / 4] = v;
        }
        fn read64(&mut self, o: u64) -> u64 {
            u64::from(self.regs[o as usize / 4]) | u64::from(self.regs[o as usize / 4 + 1]) << 32
        }
        fn write64(&mut self, o: u64, v: u64) {
            self.regs[o as usize / 4] = v as u32;
            self.regs[o as usize / 4 + 1] = (v >> 32) as u32;
        }
        fn hw_cap(&self) -> u64 {
            self.cap
        }
        fn hw_ecap(&self) -> u64 {
            self.ecap
        }
        fn hw_gcmd(&self) -> u32 {
            0
        }
        fn set_hw_gcmd(&mut self, _: u32) {}
        fn qi_enabled(&self) -> bool {
            false
        }
        fn root_table_physical(&self) -> u64 {
            0
        }
        fn interrupt_table_physical(&self) -> u64 {
            0
        }
        fn interrupt_entry_count(&self) -> u32 {
            0
        }
        fn x2apic_mode(&self) -> bool {
            false
        }
        fn now_ns(&mut self) -> u64 {
            self.now += 1;
            self.now
        }
    }
    impl FaultIo for FakeIo {
        fn unit_index(&self) -> u32 {
            0
        }
        fn report_status(&mut self, status: FaultStatus) {
            self.status.push(status);
        }
        fn schedule_fault_task(&mut self) {
            self.scheduled += 1;
        }
        fn drain_fault_task(&mut self) {
            self.drains += 1;
        }
        fn lookup_context(&mut self, sid: u16) -> bool {
            sid == 0x1234
        }
        fn remember_context_fault(&mut self, sid: u16, record: FaultRecord) {
            self.remembered.push((sid, record));
        }
        fn report_fault(&mut self, sid: u16, record: FaultRecord) {
            self.reported.push((sid, record));
        }
    }

    #[test]
    fn fault_ring_uses_two_word_offsets_and_reserves_one_record() {
        let mut log = FaultLog::new(6).unwrap();
        assert!(log.push(FaultRecord {
            address: 1,
            info: 2
        }));
        assert!(log.push(FaultRecord {
            address: 3,
            info: 4
        }));
        assert!(!log.push(FaultRecord {
            address: 5,
            info: 6
        }));
        assert_eq!(
            log.pop(),
            Some(FaultRecord {
                address: 1,
                info: 2
            })
        );
        assert!(log.push(FaultRecord {
            address: 5,
            info: 6
        }));
    }

    #[test]
    fn fault_interrupt_clears_records_and_deferred_task_saves_context() {
        let mut io = FakeIo::default();
        io.cap = (1 << 40) | (0x20 << 24); // 2 records, FRO = 0x20
        let record_offset = 0x20 * 16;
        io.regs[DMAR_FSTS_REG as usize / 4] = 1 << 1;
        io.regs[(record_offset as usize) / 4] = 0x5678;
        io.regs[(record_offset as usize) / 4 + 2] = 0x1234;
        io.regs[(record_offset as usize) / 4 + 3] = (DMAR_FRCD2_F >> 32) as u32;
        let mut log = FaultLog::new(8).unwrap();
        assert!(dmar_fault_intr(&mut io, &mut log).unwrap());
        assert_eq!(io.scheduled, 1);
        dmar_fault_task(&mut io, &mut log);
        assert_eq!(io.remembered.len(), 1);
        assert_eq!(io.remembered[0].0, 0x1234);
        assert_eq!(io.reported.len(), 1);
    }

    #[test]
    fn interrupt_status_masks_are_toggled_without_touching_other_bits() {
        let mut io = FakeIo::default();
        io.regs[FECTL as usize / 4] = 0x55;
        dmar_disable_fault_intr(&mut io).unwrap();
        assert_eq!(io.regs[FECTL as usize / 4], 0x55 | DMAR_FECTL_IM as u32);
        dmar_enable_fault_intr(&mut io).unwrap();
        assert_eq!(io.regs[FECTL as usize / 4], 0x55);
    }
}
