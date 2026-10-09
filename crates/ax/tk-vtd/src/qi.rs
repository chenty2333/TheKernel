//! Intel VT-d queued invalidation translation from FreeBSD
//! sys/x86/iommu/intel_qi.c (snapshot c2b7fe4, BSD-2-Clause).
//! Copyright (c) 2013 The FreeBSD Foundation; Konstantin Belousov under
//! Foundation sponsorship. Taskqueue, coherent DMA queue allocation, and
//! sequence wakeups are mapped by QiIo; grant in LICENSES/BSD-2-Clause.txt.
use crate::{Error, reg::*, utils::RegisterIo};

const PAGE_SIZE: u32 = 4096;
const IQH: u64 = DMAR_IQH_REG;
const IQT: u64 = DMAR_IQT_REG;
const IQA: u64 = DMAR_IQA_REG;
const ICS: u64 = DMAR_ICS_REG;
const IECTL: u64 = DMAR_IECTL_REG;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GenerationSequence {
    pub generation: u64,
    pub sequence: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QiQueue {
    pub size_bytes: u32,
    pub tail_bytes: u32,
    pub available_bytes: u32,
    pub queue_full: u64,
    pub sequence_waiters: u32,
    pub wait_sequence: u32,
    pub wait_generation: u64,
    pub enabled: bool,
}
impl QiQueue {
    pub fn new(size_bytes: u32) -> Result<Self, Error> {
        if size_bytes < 32 || !size_bytes.is_power_of_two() {
            return Err(Error::InvalidRange);
        }
        Ok(Self {
            size_bytes,
            tail_bytes: 0,
            available_bytes: size_bytes - DMAR_IQ_DESCR_SZ as u32,
            queue_full: 0,
            sequence_waiters: 0,
            wait_sequence: 1,
            wait_generation: 0,
            enabled: false,
        })
    }
}

/// QI-specific operations provided by the kernel; queue pages must be coherent.
pub trait QiIo: RegisterIo {
    fn qi_queue_physical(&self) -> u64;
    fn qi_wait_sequence_physical(&self) -> u64;
    fn qi_interrupt_entry_count(&self) -> u32;
    /// Whether a wait-descriptor may request a completion interrupt.
    /// Implementations without an installed interrupt route still use the
    /// memory-write completion word and poll it synchronously.
    fn qi_interrupt_enabled(&self) -> bool {
        true
    }
    /// Stores the two adjacent volatile 64-bit descriptor words at byte offset.
    fn qi_store_descriptor(&mut self, byte_offset: u32, low: u64, high: u64);
    fn qi_hardware_sequence(&mut self) -> u64;
    fn qi_advance_waiter_count(&mut self, delta: i32);
    fn qi_is_cold(&self) -> bool;
    fn qi_wait_for_progress(&mut self, nowait: bool);
    fn qi_drain_tlb_flushes(&mut self);
    fn qi_enqueue_completion_task(&mut self);
    fn qi_wake_sequence_waiters(&mut self);
    fn qi_common_init(&mut self, queue_bytes: u32, descriptor_bytes: u32) -> Result<(), Error>;
    fn qi_common_fini(&mut self);
    fn qi_enable_interrupt(&mut self);
    fn qi_disable_interrupt(&mut self);
    fn qi_queue_supported_by_tunable(&self) -> bool;
    fn qi_set_enabled(&mut self, enabled: bool);
    fn qi_set_queue_bytes(&mut self, bytes: u32);
    fn qi_release_queue(&mut self);
    fn qi_referenced_irte_count(&self) -> u32;
    fn qi_clear_wait_completion(&mut self);
}

fn wait_register<I: QiIo>(io: &mut I, register: u64, mask: u64, set: bool) -> Result<(), Error> {
    let timeout = crate::utils::dmar_get_timeout();
    let started = io.now_ns();
    loop {
        if (io.read32(register) as u64 & mask != 0) == set {
            return Ok(());
        }
        if timeout != 0 && io.now_ns().wrapping_sub(started) >= timeout {
            return Err(Error::Timeout);
        }
        io.spin_wait();
    }
}

/// upstream: intel_qi.c dmar_enable_qi()
pub fn dmar_enable_qi<I: QiIo>(io: &mut I) -> Result<(), Error> {
    let command = io.hw_gcmd() | DMAR_GCMD_QIE as u32;
    io.set_hw_gcmd(command);
    io.write32(DMAR_GCMD_REG, command);
    wait_register(io, DMAR_GSTS_REG, DMAR_GSTS_QIES, true)
}

/// upstream: intel_qi.c dmar_disable_qi()
pub fn dmar_disable_qi<I: QiIo>(io: &mut I) -> Result<(), Error> {
    let command = io.hw_gcmd() & !(DMAR_GCMD_QIE as u32);
    io.set_hw_gcmd(command);
    io.write32(DMAR_GCMD_REG, command);
    wait_register(io, DMAR_GSTS_REG, DMAR_GSTS_QIES, false)
}

/// upstream: intel_qi.c dmar_qi_advance_tail()
pub fn dmar_qi_advance_tail<I: QiIo>(io: &mut I, queue: &QiQueue) {
    io.write32(IQT, queue.tail_bytes);
}

/// upstream: intel_qi.c dmar_qi_ensure()
pub fn dmar_qi_ensure<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    descriptor_count: u32,
) -> Result<(), Error> {
    let bytes = descriptor_count
        .checked_shl(DMAR_IQ_DESCR_SZ_SHIFT as u32)
        .ok_or(Error::InvalidRange)?;
    if bytes > queue.size_bytes - DMAR_IQ_DESCR_SZ as u32 {
        return Err(Error::InvalidRange);
    }
    loop {
        if bytes <= queue.available_bytes {
            break;
        }
        let head = io.read32(IQH) & DMAR_IQH_MASK as u32;
        queue.available_bytes = head
            .wrapping_sub(queue.tail_bytes)
            .wrapping_sub(DMAR_IQ_DESCR_SZ as u32);
        if head <= queue.tail_bytes {
            queue.available_bytes = queue.available_bytes.wrapping_add(queue.size_bytes);
        }
        if bytes <= queue.available_bytes {
            break;
        }
        dmar_qi_advance_tail(io, queue);
        queue.queue_full = queue.queue_full.saturating_add(1);
        io.spin_wait();
    }
    queue.available_bytes -= bytes;
    Ok(())
}

/// upstream: intel_qi.c dmar_qi_emit()
pub fn dmar_qi_emit<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    low: u64,
    high: u64,
) -> Result<(), Error> {
    if queue.tail_bytes >= queue.size_bytes || queue.tail_bytes % DMAR_IQ_DESCR_SZ as u32 != 0 {
        return Err(Error::InvalidRange);
    }
    io.qi_store_descriptor(queue.tail_bytes, low, high);
    queue.tail_bytes = (queue.tail_bytes + DMAR_IQ_DESCR_SZ as u32) & (queue.size_bytes - 1);
    Ok(())
}

/// upstream: intel_qi.c dmar_qi_emit_wait_descr()
pub fn dmar_qi_emit_wait_descr<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    sequence: u32,
    interrupt: bool,
    mem_write: bool,
    fence: bool,
) -> Result<(), Error> {
    let low = DMAR_IQ_DESCR_WAIT_ID
        | if interrupt { DMAR_IQ_DESCR_WAIT_IF } else { 0 }
        | if mem_write { DMAR_IQ_DESCR_WAIT_SW } else { 0 }
        | if fence { DMAR_IQ_DESCR_WAIT_FN } else { 0 }
        | if mem_write {
            DMAR_IQ_DESCR_WAIT_SD(sequence as u64)
        } else {
            0
        };
    let high = if mem_write {
        io.qi_wait_sequence_physical()
    } else {
        0
    };
    dmar_qi_emit(io, queue, low, high)
}

// upstream: iommu_utils.c iommu_qi_seq_processed()
fn sequence_processed<I: QiIo>(io: &mut I, queue: &QiQueue, sequence: GenerationSequence) -> bool {
    let hardware = io.qi_hardware_sequence();
    sequence.generation < queue.wait_generation
        || (sequence.generation == queue.wait_generation
            && u64::from(sequence.sequence) <= hardware)
}

// upstream: iommu_utils.c iommu_qi_emit_wait_seq()
fn emit_wait_sequence<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    out: &mut GenerationSequence,
    emit_wait: bool,
) -> Result<(), Error> {
    if queue.wait_sequence == u32::MAX {
        let previous = GenerationSequence {
            generation: queue.wait_generation,
            sequence: queue.wait_sequence,
        };
        dmar_qi_ensure(io, queue, 1)?;
        dmar_qi_emit_wait_descr(io, queue, previous.sequence, false, true, false)?;
        dmar_qi_advance_tail(io, queue);
        while !sequence_processed(io, queue, previous) {
            io.spin_wait();
        }
        queue.wait_generation = queue
            .wait_generation
            .checked_add(1)
            .ok_or(Error::InvalidRange)?;
        queue.wait_sequence = 1;
    }
    let sequence = queue.wait_sequence;
    queue.wait_sequence += 1;
    *out = GenerationSequence {
        generation: queue.wait_generation,
        sequence,
    };
    if emit_wait {
        dmar_qi_ensure(io, queue, 1)?;
        let interrupt = io.qi_interrupt_enabled();
        dmar_qi_emit_wait_descr(io, queue, sequence, interrupt, true, false)?;
    }
    Ok(())
}

/// upstream: intel_qi.c dmar_qi_invalidate_emit()
pub fn dmar_qi_invalidate_emit<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    domain_id: u16,
    mut base: u64,
    mut size: u64,
    sequence: &mut GenerationSequence,
    emit_wait: bool,
) -> Result<(), Error> {
    if size == 0 || base.checked_add(size).is_none() {
        return Err(Error::InvalidRange);
    }
    while size > 0 {
        let (am, isize) = crate::utils::calc_am(io.hw_cap(), base, size);
        if isize == 0 {
            return Err(Error::InvalidRange);
        }
        dmar_qi_ensure(io, queue, 1)?;
        dmar_qi_emit(
            io,
            queue,
            DMAR_IQ_DESCR_IOTLB_INV
                | DMAR_IQ_DESCR_IOTLB_PAGE
                | DMAR_IQ_DESCR_IOTLB_DW
                | DMAR_IQ_DESCR_IOTLB_DR
                | DMAR_IQ_DESCR_IOTLB_DID(domain_id as u64),
            base | u64::from(am),
        )?;
        base = base.checked_add(isize).ok_or(Error::InvalidRange)?;
        size -= isize;
    }
    emit_wait_sequence(io, queue, sequence, emit_wait)
}

// upstream: iommu_utils.c iommu_qi_wait_for_seq()
fn wait_for_sequence<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    sequence: GenerationSequence,
    nowait: bool,
) -> Result<(), Error> {
    if queue.sequence_waiters == 0 {
        return Err(Error::InvalidRange);
    }
    while !sequence_processed(io, queue, sequence) {
        io.qi_wait_for_progress(io.qi_is_cold() || nowait);
    }
    queue.sequence_waiters -= 1;
    io.qi_advance_waiter_count(-1);
    Ok(())
}

// upstream: intel_qi.c dmar_qi_invalidate_glob_impl()
fn invalidate_global<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    descriptor: u64,
) -> Result<(), Error> {
    let mut sequence = GenerationSequence::default();
    dmar_qi_ensure(io, queue, 2)?;
    dmar_qi_emit(io, queue, descriptor, 0)?;
    emit_wait_sequence(io, queue, &mut sequence, true)?;
    queue.sequence_waiters = queue.sequence_waiters.saturating_add(1);
    io.qi_advance_waiter_count(1);
    dmar_qi_advance_tail(io, queue);
    wait_for_sequence(io, queue, sequence, false)
}

/// upstream: intel_qi.c dmar_qi_invalidate_ctx_glob_locked()
pub fn dmar_qi_invalidate_ctx_glob_locked<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
) -> Result<(), Error> {
    invalidate_global(io, queue, DMAR_IQ_DESCR_CTX_INV | DMAR_IQ_DESCR_CTX_GLOB)
}
/// upstream: intel_qi.c dmar_qi_invalidate_iotlb_glob_locked()
pub fn dmar_qi_invalidate_iotlb_glob_locked<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
) -> Result<(), Error> {
    invalidate_global(
        io,
        queue,
        DMAR_IQ_DESCR_IOTLB_INV
            | DMAR_IQ_DESCR_IOTLB_GLOB
            | DMAR_IQ_DESCR_IOTLB_DW
            | DMAR_IQ_DESCR_IOTLB_DR,
    )
}
/// upstream: intel_qi.c dmar_qi_invalidate_iec_glob()
pub fn dmar_qi_invalidate_iec_glob<I: QiIo>(io: &mut I, queue: &mut QiQueue) -> Result<(), Error> {
    invalidate_global(io, queue, DMAR_IQ_DESCR_IEC_INV)
}

/// upstream: intel_qi.c dmar_qi_invalidate_iec()
pub fn dmar_qi_invalidate_iec<I: QiIo>(
    io: &mut I,
    queue: &mut QiQueue,
    mut start: u32,
    mut count: u32,
) -> Result<(), Error> {
    let end = start.checked_add(count).ok_or(Error::InvalidRange)?;
    if start >= io.qi_referenced_irte_count() || count == 0 || end > io.qi_referenced_irte_count() {
        return Err(Error::InvalidRange);
    }
    while count > 0 {
        let mask = (start | count).trailing_zeros();
        let chunk = 1u32.checked_shl(mask).ok_or(Error::InvalidRange)?;
        dmar_qi_ensure(io, queue, 1)?;
        dmar_qi_emit(
            io,
            queue,
            DMAR_IQ_DESCR_IEC_INV
                | DMAR_IQ_DESCR_IEC_IDX
                | DMAR_IQ_DESCR_IEC_IIDX(start as u64)
                | DMAR_IQ_DESCR_IEC_IM(mask as u64),
            0,
        )?;
        count -= chunk;
        start += chunk;
    }
    dmar_qi_ensure(io, queue, 1)?;
    let mut sequence = GenerationSequence::default();
    emit_wait_sequence(io, queue, &mut sequence, true)?;
    queue.sequence_waiters = queue.sequence_waiters.saturating_add(1);
    io.qi_advance_waiter_count(1);
    dmar_qi_advance_tail(io, queue);
    wait_for_sequence(io, queue, sequence, true)
}

/// upstream: intel_qi.c dmar_qi_intr()
pub fn dmar_qi_intr<I: QiIo>(io: &mut I, queue: &QiQueue) -> Result<(), Error> {
    if !queue.enabled {
        return Err(Error::InvalidStructure);
    }
    io.qi_enqueue_completion_task();
    Ok(())
}

/// upstream: intel_qi.c dmar_qi_task()
pub fn dmar_qi_task<I: QiIo>(io: &mut I, queue: &QiQueue) {
    io.qi_drain_tlb_flushes();
    if io.read32(ICS) & DMAR_ICS_IWC as u32 != 0 {
        io.write32(ICS, DMAR_ICS_IWC as u32);
        io.qi_drain_tlb_flushes();
    }
    if queue.sequence_waiters > 0 {
        io.qi_wake_sequence_waiters();
    }
}

/// upstream: intel_qi.c dmar_init_qi()
pub fn dmar_init_qi<I: QiIo>(
    io: &mut I,
    queue_max_order: u32,
    requested_order: u32,
) -> Result<Option<QiQueue>, Error> {
    if io.hw_ecap() & DMAR_ECAP_QI == 0 || io.hw_cap() & DMAR_CAP_CM != 0 {
        return Ok(None);
    }
    if !io.qi_queue_supported_by_tunable() {
        io.qi_set_enabled(false);
        return Ok(None);
    }
    io.qi_set_enabled(true);
    let order = requested_order
        .min(queue_max_order)
        .min(DMAR_IQA_QS_MAX as u32);
    let bytes = (1u32.checked_shl(order).ok_or(Error::InvalidRange)?)
        .checked_mul(PAGE_SIZE)
        .ok_or(Error::InvalidRange)?;
    io.qi_common_init(bytes, DMAR_IQ_DESCR_SZ as u32)?;
    io.qi_set_queue_bytes(bytes);
    let mut queue = QiQueue::new(bytes)?;
    io.write64(IQT, 0);
    io.write64(IQA, io.qi_queue_physical() | u64::from(order));
    dmar_enable_qi(io)?;
    io.qi_clear_wait_completion();
    io.qi_enable_interrupt();
    queue.enabled = true;
    Ok(Some(queue))
}

/// upstream: intel_qi.c dmar_fini_qi_helper()
pub fn dmar_fini_qi_helper<I: QiIo>(io: &mut I) -> Result<(), Error> {
    io.qi_disable_interrupt();
    dmar_disable_qi(io)
}

/// upstream: intel_qi.c dmar_fini_qi()
pub fn dmar_fini_qi<I: QiIo>(io: &mut I, queue: &mut QiQueue) -> Result<(), Error> {
    if !queue.enabled {
        return Ok(());
    }
    io.qi_common_fini();
    dmar_qi_ensure(io, queue, 1)?;
    let mut sequence = GenerationSequence::default();
    emit_wait_sequence(io, queue, &mut sequence, true)?;
    queue.sequence_waiters = queue.sequence_waiters.saturating_add(1);
    io.qi_advance_waiter_count(1);
    dmar_qi_advance_tail(io, queue);
    wait_for_sequence(io, queue, sequence, false)?;
    dmar_fini_qi_helper(io)?;
    if queue.sequence_waiters != 0 {
        return Err(Error::InvalidStructure);
    }
    queue.enabled = false;
    io.qi_set_enabled(false);
    io.qi_release_queue();
    Ok(())
}

/// upstream: intel_qi.c dmar_enable_qi_intr()
pub fn dmar_enable_qi_intr<I: QiIo>(io: &mut I) -> Result<(), Error> {
    if io.hw_ecap() & DMAR_ECAP_QI == 0 {
        return Err(Error::Unsupported);
    }
    let value = io.read32(IECTL) & !(DMAR_IECTL_IM as u32);
    io.write32(IECTL, value);
    Ok(())
}

/// upstream: intel_qi.c dmar_disable_qi_intr()
pub fn dmar_disable_qi_intr<I: QiIo>(io: &mut I) -> Result<(), Error> {
    if io.hw_ecap() & DMAR_ECAP_QI == 0 {
        return Err(Error::Unsupported);
    }
    let value = io.read32(IECTL) | DMAR_IECTL_IM as u32;
    io.write32(IECTL, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    struct FakeQi {
        regs: [u32; 128],
        descriptors: Vec<(u32, u64, u64)>,
        cap: u64,
        ecap: u64,
        gcmd: u32,
        now: u64,
        qphys: u64,
        wphys: u64,
        irtes: u32,
        enabled: bool,
        waiter_count: i32,
        common: bool,
        irq_enabled: bool,
        cleared: bool,
        release: bool,
        enqueued: bool,
        drains: u8,
        wakes: u8,
    }
    impl Default for FakeQi {
        fn default() -> Self {
            Self {
                regs: [0; 128],
                descriptors: Vec::new(),
                cap: 0,
                ecap: 0,
                gcmd: 0,
                now: 0,
                qphys: 0x8000,
                wphys: 0x9000,
                irtes: 256,
                enabled: false,
                waiter_count: 0,
                common: false,
                irq_enabled: false,
                cleared: false,
                release: false,
                enqueued: false,
                drains: 0,
                wakes: 0,
            }
        }
    }
    impl RegisterIo for FakeQi {
        fn read32(&mut self, o: u64) -> u32 {
            self.regs[o as usize / 4]
        }
        fn write32(&mut self, o: u64, v: u32) {
            self.regs[o as usize / 4] = v;
            if o == DMAR_GCMD_REG {
                self.regs[DMAR_GSTS_REG as usize / 4] = if v & DMAR_GCMD_QIE as u32 != 0 {
                    DMAR_GSTS_QIES as u32
                } else {
                    0
                };
            }
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
            self.gcmd
        }
        fn set_hw_gcmd(&mut self, v: u32) {
            self.gcmd = v;
        }
        fn qi_enabled(&self) -> bool {
            self.enabled
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
        fn spin_wait(&mut self) {
            self.now += 1;
        }
    }
    impl QiIo for FakeQi {
        fn qi_queue_physical(&self) -> u64 {
            self.qphys
        }
        fn qi_wait_sequence_physical(&self) -> u64 {
            self.wphys
        }
        fn qi_interrupt_entry_count(&self) -> u32 {
            self.irtes
        }
        fn qi_store_descriptor(&mut self, o: u32, lo: u64, hi: u64) {
            self.descriptors.push((o, lo, hi));
        }
        fn qi_hardware_sequence(&mut self) -> u64 {
            u64::MAX
        }
        fn qi_advance_waiter_count(&mut self, d: i32) {
            self.waiter_count += d;
        }
        fn qi_is_cold(&self) -> bool {
            true
        }
        fn qi_wait_for_progress(&mut self, _: bool) {
            self.now += 1;
        }
        fn qi_drain_tlb_flushes(&mut self) {
            self.drains += 1;
        }
        fn qi_enqueue_completion_task(&mut self) {
            self.enqueued = true;
        }
        fn qi_wake_sequence_waiters(&mut self) {
            self.wakes += 1;
        }
        fn qi_common_init(&mut self, _: u32, _: u32) -> Result<(), Error> {
            self.common = true;
            Ok(())
        }
        fn qi_common_fini(&mut self) {
            self.common = false;
        }
        fn qi_enable_interrupt(&mut self) {
            self.irq_enabled = true;
        }
        fn qi_disable_interrupt(&mut self) {
            self.irq_enabled = false;
        }
        fn qi_queue_supported_by_tunable(&self) -> bool {
            true
        }
        fn qi_set_enabled(&mut self, v: bool) {
            self.enabled = v;
        }
        fn qi_set_queue_bytes(&mut self, _: u32) {}
        fn qi_release_queue(&mut self) {
            self.release = true;
        }
        fn qi_referenced_irte_count(&self) -> u32 {
            self.irtes
        }
        fn qi_clear_wait_completion(&mut self) {
            self.cleared = true;
        }
    }

    #[test]
    fn queue_reserves_one_descriptor_and_wraps_descriptor_pairs() {
        let mut io = FakeQi::default();
        let mut q = QiQueue::new(4096).unwrap();
        dmar_qi_ensure(&mut io, &mut q, 1).unwrap();
        dmar_qi_emit(&mut io, &mut q, 0x11, 0x22).unwrap();
        assert_eq!(q.tail_bytes, 16);
        assert_eq!(io.descriptors, [(0, 0x11, 0x22)]);
        assert_eq!(q.available_bytes, 4096 - 32);
    }

    #[test]
    fn iotlb_invalidation_emits_domain_address_mask_and_wait_descriptor() {
        let mut io = FakeQi::default();
        let mut q = QiQueue::new(4096).unwrap();
        let mut sequence = GenerationSequence::default();
        dmar_qi_invalidate_emit(&mut io, &mut q, 7, 0x2000, 0x1000, &mut sequence, true).unwrap();
        assert_eq!(
            io.descriptors[0].1 & DMAR_IQ_DESCR_IOTLB_DID(0xffff),
            DMAR_IQ_DESCR_IOTLB_DID(7)
        );
        assert_eq!(io.descriptors[0].2, 0x2000);
        assert_eq!(sequence.sequence, 1);
        assert_ne!(io.descriptors[1].1 & DMAR_IQ_DESCR_WAIT_IF, 0);
    }

    #[test]
    fn global_invalidation_accounts_waiter_before_tail_and_completion() {
        let mut io = FakeQi::default();
        let mut q = QiQueue::new(4096).unwrap();
        invalidate_global(&mut io, &mut q, DMAR_IQ_DESCR_IOTLB_INV).unwrap();
        assert_eq!(q.sequence_waiters, 0);
        assert_eq!(io.waiter_count, 0);
        assert_eq!(q.tail_bytes, 32);
    }

    #[test]
    fn iec_chunking_task_interrupt_and_lifecycle_follow_upstream_order() {
        let mut io = FakeQi::default();
        let mut q = QiQueue::new(4096).unwrap();
        dmar_qi_invalidate_iec(&mut io, &mut q, 0, 8).unwrap();
        assert_eq!(
            io.descriptors[0].1 & DMAR_IQ_DESCR_IEC_IM(31),
            DMAR_IQ_DESCR_IEC_IM(3)
        );
        io.ecap = DMAR_ECAP_QI;
        let mut initialized = dmar_init_qi(&mut io, 7, 3).unwrap().unwrap();
        assert_eq!(io.read64(IQA), 0x8003);
        assert!(io.cleared && io.irq_enabled && initialized.enabled);
        dmar_qi_task(&mut io, &initialized);
        assert_eq!(io.drains, 1);
        assert!(dmar_qi_intr(&mut io, &initialized).is_ok());
        assert!(io.enqueued);
        dmar_fini_qi(&mut io, &mut initialized).unwrap();
        assert!(io.release);
        assert!(!io.irq_enabled && !io.enabled);
    }
}
