// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
// Native sequence adapts Linux7.2.3 gt/intel_execlists_submission.c
// enable_execlists/reset_csb_pointers (Copyright © 2014 Intel Corporation).
// Full MIT grant and inventory: crates/ax/tk-intel-gt/LICENSE-MIT and NOTICE.
//! One kernel-owned BCS copy, no arbitrary batch or Mesa claim. Software
//! preparation uses existing SharedPages/GGTT; DMA ownership precedes ELSQ load.
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{Ordering, fence};

use intel_gt::{Error, GtIo, bcs, lrc, ppgtt};

use super::super::{
    gtt::{Binding, Gtt},
    pci,
};
use crate::mm::{SharedFixedView, SharedPages};
const PAGE: usize = 4096;
const PAYLOAD: usize = 64 * 64 * 4;
struct Ram {
    pages: Arc<SharedPages>,
    _pin: SharedFixedView,
    physical: Vec<u64>,
}
impl Ram {
    fn allocate(count: usize) -> Result<Self, Error> {
        let bytes = count.checked_mul(PAGE).ok_or(Error::Refused)?;
        let pages = Arc::try_new(
            SharedPages::new_fixed(bytes, axhal::paging::PageSize::Size4K)
                .map_err(|_| Error::Refused)?,
        )
        .map_err(|_| Error::Refused)?;
        let pin = pages.fixed_view().map_err(|_| Error::Refused)?;
        let mut physical = Vec::new();
        physical
            .try_reserve_exact(count)
            .map_err(|_| Error::Refused)?;
        for i in 0..count {
            let p = pages.paddr_at(i).map_err(|_| Error::Refused)?.as_usize() as u64;
            ppgtt::physical(p)?;
            physical.push(p);
        }
        Ok(Self {
            pages,
            _pin: pin,
            physical,
        })
    }
    fn from_pages(pages: Arc<SharedPages>) -> Result<Self, Error> {
        if pages.is_external() || pages.page_size() != axhal::paging::PageSize::Size4K {
            return Err(Error::Refused);
        }
        let pin = pages.fixed_view().map_err(|_| Error::Refused)?;
        let count = pin.len() / PAGE;
        if count == 0 || count > 16 {
            return Err(Error::Refused);
        }
        let mut physical = Vec::new();
        physical
            .try_reserve_exact(count)
            .map_err(|_| Error::Refused)?;
        for i in 0..count {
            let p = pages.paddr_at(i).map_err(|_| Error::Refused)?.as_usize() as u64;
            ppgtt::physical(p)?;
            physical.push(p);
        }
        Ok(Self {
            pages,
            _pin: pin,
            physical,
        })
    }
    fn write(&self, offset: usize, data: &[u8]) -> Result<(), Error> {
        self.pages
            .write_bytes(offset, data)
            .map_err(|_| Error::Refused)
    }
    fn read(&self, offset: usize, data: &mut [u8]) -> Result<(), Error> {
        self.pages
            .read_bytes(offset, data)
            .map_err(|_| Error::Refused)
    }
    fn dwords(&self, page: usize, words: &[u32]) -> Result<(), Error> {
        let mut data = Vec::new();
        data.try_reserve_exact(words.len() * 4)
            .map_err(|_| Error::Refused)?;
        for word in words {
            data.extend_from_slice(&word.to_le_bytes());
        }
        self.write(page * PAGE, &data)
    }
    fn table(&self, page: usize, words: &[u64; 512]) -> Result<(), Error> {
        let mut data = Vec::new();
        data.try_reserve_exact(PAGE).map_err(|_| Error::Refused)?;
        for word in words {
            data.extend_from_slice(&word.to_le_bytes());
        }
        self.write(page * PAGE, &data)
    }
    fn flush(&self) {
        // x86_64 only. Flush both before GPU reads and before CPU reads of GPU
        // output. Fixed-view pins exclude physical folio replacement/reclaim.
        for &physical in &self.physical {
            let base =
                axhal::mem::phys_to_virt(axhal::mem::PhysAddr::from_usize(physical as usize))
                    .as_usize();
            for offset in (0..PAGE).step_by(64) {
                // SAFETY: retained ordinary system-RAM page, each address is a
                // cache line inside it. CLFLUSH neither frees nor changes PTEs.
                unsafe { core::arch::x86_64::_mm_clflush((base + offset) as *const u8) };
            }
        }
        fence(Ordering::SeqCst);
    }
}
pub(super) struct Memory {
    gtt: Arc<Gtt>,
    tables: Ram,
    source: Ram,
    destination: Ram,
    context: Ram,
    ring: Ram,
    batch: Ram,
    status: Ram,
    bindings: Vec<Binding>,
    descriptor: u64,
    operation: bcs::Copy,
    selftest: bool,
}
impl Memory {
    fn allocate(gtt: Arc<Gtt>) -> Result<Self, Error> {
        let mut bindings = Vec::new();
        bindings.try_reserve_exact(3).map_err(|_| Error::Refused)?;
        Ok(Self {
            gtt,
            tables: Ram::allocate(8)?,
            source: Ram::allocate(6)?,
            destination: Ram::allocate(6)?,
            context: Ram::allocate(4)?,
            ring: Ram::allocate(1)?,
            batch: Ram::allocate(1)?,
            status: Ram::allocate(1)?,
            bindings,
            descriptor: 0,
            operation: bcs::Copy {
                source: 0x11000,
                destination: 0x21000,
                source_bytes: PAYLOAD as u64,
                destination_bytes: PAYLOAD as u64,
                width: 64,
                height: 64,
                pitch: 256,
            },
            selftest: true,
        })
    }
    fn from_objects(
        gtt: Arc<Gtt>,
        source: Arc<SharedPages>,
        destination: Arc<SharedPages>,
        operation: bcs::Copy,
    ) -> Result<Self, Error> {
        if Arc::ptr_eq(&source, &destination) {
            return Err(Error::Refused);
        }
        let source = Ram::from_pages(source)?;
        let destination = Ram::from_pages(destination)?;
        let batch = bcs::batch(operation)?;
        bcs::decode_copy(
            batch[3..].try_into().unwrap(),
            (source.physical.len() * PAGE) as u64,
            (destination.physical.len() * PAGE) as u64,
        )?;
        let mut memory = Self::allocate(gtt)?;
        memory.source = source;
        memory.destination = destination;
        memory.operation = operation;
        memory.selftest = false;
        Ok(memory)
    }
    fn bind_and_build(&mut self) -> Result<(), Error> {
        for r in [&self.context, &self.ring, &self.status] {
            self.bindings.push(
                self.gtt
                    .bind_pages(&r.physical)
                    .map_err(|_| Error::Quarantined)?,
            );
        }
        let ctx = self.bindings[0].address as u32;
        let ring = self.bindings[1].address as u32;
        let p = &self.tables.physical;
        let mut table_data = zero_words::<u64>(512)?;
        let table: &mut [u64; 512] = table_data.as_mut_slice().try_into().unwrap();
        // Main root/PDPT/PD/PT and scratch PDPT/PD/PT/data. All unused VA
        // branches reach owned read-only scratch, not arbitrary RAM or zero.
        ppgtt::directory(table, p[4], p[1])?;
        self.tables.table(0, table)?;
        ppgtt::directory(table, p[5], p[2])?;
        self.tables.table(1, table)?;
        ppgtt::directory(table, p[6], p[3])?;
        self.tables.table(2, table)?;
        ppgtt::leaf(table, p[7], 3)?;
        ppgtt::map(table, 0x10000, &self.source.physical, 3, false)?;
        ppgtt::map(table, 0x20000, &self.destination.physical, 3, true)?;
        ppgtt::map(table, 0x30000, &self.batch.physical, 3, false)?;
        self.tables.table(3, table)?;
        table.fill(ppgtt::pde(p[5])?);
        self.tables.table(4, table)?;
        table.fill(ppgtt::pde(p[6])?);
        self.tables.table(5, table)?;
        ppgtt::leaf(table, p[7], 3)?;
        self.tables.table(6, table)?;
        let mut regs_data = zero_words::<u32>(1024)?;
        let mut indirect_data = zero_words::<u32>(1024)?;
        let mut per_data = zero_words::<u32>(1024)?;
        let regs: &mut [u32; 1024] = regs_data.as_mut_slice().try_into().unwrap();
        let indirect: &mut [u32; 1024] = indirect_data.as_mut_slice().try_into().unwrap();
        let per_ctx: &mut [u32; 1024] = per_data.as_mut_slice().try_into().unwrap();
        self.descriptor = lrc::build(regs, indirect, per_ctx, ctx, ring, 120, p[0])?;
        self.context.dwords(1, regs)?;
        self.context.dwords(2, indirect)?;
        self.context.dwords(3, per_ctx)?;
        let batch = bcs::batch(self.operation)?;
        self.batch.dwords(0, &batch)?;
        let count = bcs::ring(regs, 0x30000, ctx, 1)?;
        self.ring.dwords(0, &regs[..count])?;
        if self.selftest {
            let mut data = Vec::new();
            data.try_reserve_exact(6 * PAGE)
                .map_err(|_| Error::Refused)?;
            data.resize(6 * PAGE, 0xa5);
            for (i, b) in data[PAGE..PAGE + PAYLOAD].iter_mut().enumerate() {
                *b = pattern(i);
            }
            self.source.write(0, &data)?;
            data.fill(0x5a);
            data[PAGE..PAGE + PAYLOAD].fill(0);
            self.destination.write(0, &data)?;
        }
        self.status.write(0x10 * 4, &[0xff; 12 * 8])?;
        self.status.write(0x2f * 4, &11u32.to_le_bytes())?;
        for r in [
            &self.tables,
            &self.source,
            &self.destination,
            &self.context,
            &self.ring,
            &self.batch,
            &self.status,
        ] {
            r.flush();
        }
        Ok(())
    }
    fn verify(&self) -> Result<(), Error> {
        self.source.flush();
        self.destination.flush();
        let mut data = Vec::new();
        data.try_reserve_exact(6 * PAGE)
            .map_err(|_| Error::Refused)?;
        data.resize(6 * PAGE, 0);
        for (r, guard) in [(&self.source, 0xa5), (&self.destination, 0x5a)] {
            r.read(0, &mut data)?;
            if data[..PAGE]
                .iter()
                .chain(&data[PAGE + PAYLOAD..])
                .any(|&b| b != guard)
                || data[PAGE..PAGE + PAYLOAD]
                    .iter()
                    .enumerate()
                    .any(|(i, &b)| b != pattern(i))
            {
                return Err(Error::Refused);
            }
        }
        Ok(())
    }
    fn release(&mut self) -> Result<(), Error> {
        for binding in self.bindings.iter().rev() {
            // SAFETY: only reached after successful source BCS stop/reset,
            // pending-MI-wake/ready/GDRST/cancel checks; no other engine sees
            // this private context, ring, status or PPGTT. RAM stays pinned.
            unsafe { self.gtt.release_binding(binding) }.map_err(|_| Error::Quarantined)?;
        }
        self.bindings.clear();
        Ok(())
    }
}
fn zero_words<T: Default + Clone>(count: usize) -> Result<Vec<T>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(count).map_err(|_| Error::Refused)?;
    v.resize(count, T::default());
    Ok(v)
}
fn pattern(index: usize) -> u8 {
    (index as u8).wrapping_mul(29) ^ ((index >> 8) as u8) ^ 0x73
}

fn submit(io: &impl GtIo, memory: &Memory) -> Result<(), Error> {
    let status = memory.bindings[2].address as u32;
    // Polling selftest masks engine IRQs; USER_INTERRUPT cannot become an
    // unowned CPU IRQ. Preserve the source HWSTAM and error-clear setup.
    io.write(0x220a8, u32::MAX)?;
    io.write(0x22098, u32::MAX)?;
    io.write(0x220b4, u32::MAX)?;
    io.write(0x220b0, u32::MAX)?;
    if io.read(0x220b8)? != 0 {
        return Err(Error::Refused);
    }
    io.write(
        0x2229c,
        intel_gt::masked_enable(1 << 3) | intel_gt::masked_disable(1 << 10),
    )?;
    io.write(0x2209c, intel_gt::masked_disable(1 << 8))?;
    io.write(0x22080, status)?;
    if io.read(0x22080)? != status {
        return Err(Error::Refused);
    }
    io.write(0x220c4, (0x3fff << 16) | 6 | (6 << 7))?; // source UC index3 read/write.
    // Source CSB read/write pointer reset: Gen11 twelve slots, invalid index11.
    io.write(0x223a0, 0xffff0000 | (11 << 8) | 11)?;
    io.read(0x223a0)?;
    io.write(0x223a0, 0xffff0000 | (11 << 8) | 11)?;
    io.read(0x223a0)?;
    fence(Ordering::SeqCst);
    // Gen12 ELSQ writes port1 then port0, low then high, then explicit load.
    io.write(0x22518, 0)?;
    io.write(0x2251c, 0)?;
    io.write(0x22510, memory.descriptor as u32)?;
    io.write(0x22514, (memory.descriptor >> 32) as u32)?;
    io.write(0x22550, 1)?;
    let start = io.now_us();
    let mut value = [0u8; 4];
    for _ in 0..100_000 {
        memory.context.flush();
        memory.context.read(lrc::SCRATCH as usize, &mut value)?;
        if u32::from_le_bytes(value) == 1 {
            fence(Ordering::SeqCst);
            return Ok(());
        }
        if io.read(0x220b8)? != 0 {
            return Err(Error::Refused);
        }
        if io.now_us().saturating_sub(start) > 500_000 {
            return Err(Error::Timeout(0x22550));
        }
        io.delay_us(10);
    }
    Err(Error::Timeout(0x22550))
}

// Outer error means quiescence was not established: caller MUST retain all
// DMA owners. An inner error can be reported after safe scoped unbinding.
fn execute_and_quiesce(io: &impl GtIo, memory: &Memory) -> Result<Result<(), Error>, Error> {
    let executed = submit(io, memory);
    intel_gt::reset::stop_and_reset_bcs(io).map_err(|_| Error::Quarantined)?;
    Ok(executed.and_then(|()| {
        if memory.selftest {
            memory.verify()
        } else {
            Ok(())
        }
    }))
}

#[cfg(target_os = "none")]
pub(super) fn run(owner: &mut super::Owner, bdf: pci::Bdf) -> Result<(), Error> {
    super::super::dma::require_direct(bdf).map_err(|_| Error::Refused)?;
    intel_gt::uncore::acquire_render(&owner.bus)?;
    owner.bus.render_awake.store(true, Ordering::Release);
    // Never change shared cache policy while an abandoned firmware RCS is busy.
    let start = owner.bus.now_us();
    while owner.bus.read(0x209c)? & (1 << 9) == 0 {
        if owner.bus.now_us().saturating_sub(start) > 100_000 {
            return Err(Error::Refused);
        }
        owner.bus.delay_us(10);
    }
    bcs::prepare(&owner.bus)?;
    let gtt = super::super::shared_ggtt(bdf).map_err(|_| Error::Refused)?;
    owner.memory = Some(Memory::allocate(gtt)?);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    let ecam = pci::Ecam::platform().ok_or(Error::Refused)?;
    ecam.enable_n305_bus_master(bdf).ok_or(Error::Refused)?;
    // Breadcrumb does not establish context/page-table retirement. The
    // source stop/reset must succeed even after a possibly-landed ELSQ error.
    let verified = execute_and_quiesce(&owner.bus, memory)?;
    memory.release()?;
    owner.memory = None;
    verified
}

/// Scoped synchronous execution over existing GEM SharedPages. Caller owns all
/// GEM Arcs/reservation fences through this call; the GT owner additionally
/// retains the fixed views/page tables on any ambiguous retirement error.
#[cfg(target_os = "none")]
pub(super) fn objects(
    owner: &mut super::Owner,
    source: Arc<SharedPages>,
    destination: Arc<SharedPages>,
    operation: bcs::Copy,
) -> Result<(), Error> {
    if owner.lost || owner.memory.is_some() {
        return Err(Error::Quarantined);
    }
    super::super::dma::require_direct(owner.bdf).map_err(|_| Error::Refused)?;
    operation.validate()?;
    if Arc::ptr_eq(&source, &destination) {
        return Err(Error::Refused);
    }
    if owner.bus.read(intel_gt::uncore::GT_ACK)? & 1 == 0
        || owner.bus.read(intel_gt::uncore::RENDER_ACK)? & 1 == 0
        || owner.bus.read(0xc000)? & 1 == 0
        || owner.bus.read(0x480c)? != 0
        || owner.bus.read(0x400c)? != 5
        || owner.bus.read(0xb024)? >> 16 != 0x10
    {
        return Err(Error::Refused);
    }
    // Last job and bootstrap must already be quiescent; bounded reset also
    // establishes a fresh engine state before loading another private context.
    intel_gt::reset::stop_and_reset_bcs(&owner.bus)?;
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let memory = Memory::from_objects(gtt, source, destination, operation)?;
    owner.memory = Some(memory);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    let outcome = execute_and_quiesce(&owner.bus, memory)?;
    memory.release()?;
    owner.memory = None;
    outcome
}

#[cfg(test)]
pub(in crate::drm::intel) mod tests {
    use alloc::{boxed::Box, collections::BTreeMap};
    use core::cell::{Cell, RefCell};

    use super::*;
    struct Model<'a> {
        memory: &'a Memory,
        words: RefCell<BTreeMap<u32, u32>>,
        log: RefCell<Vec<(u32, u32)>>,
        clock: Cell<u64>,
        execute: bool,
        fail_write: Cell<Option<usize>>,
    }
    impl Model<'_> {
        fn ram(&self, physical: u64) -> Result<(&Ram, usize), Error> {
            let page = physical & !4095;
            let inside = (physical & 4095) as usize;
            for ram in [
                &self.memory.tables,
                &self.memory.source,
                &self.memory.destination,
                &self.memory.context,
                &self.memory.ring,
                &self.memory.batch,
                &self.memory.status,
            ] {
                if let Some(index) = ram.physical.iter().position(|&p| p == page) {
                    return Ok((ram, index * PAGE + inside));
                }
            }
            Err(Error::Refused)
        }
        fn physical(&self, address: u64, buf: &mut [u8]) -> Result<(), Error> {
            let (ram, offset) = self.ram(address)?;
            ram.read(offset, buf)
        }
        fn translate(&self, virtual_address: u64, write: bool) -> Result<u64, Error> {
            let mut page = self.memory.tables.physical[0];
            for shift in [39, 30, 21, 12] {
                let mut data = [0; 8];
                self.physical(page + ((virtual_address >> shift) & 511) * 8, &mut data)?;
                let entry = u64::from_le_bytes(data);
                if entry & 1 == 0 || (shift == 12 && write && entry & 2 == 0) {
                    return Err(Error::Refused);
                }
                page = entry & (((1u64 << 39) - 1) & !4095);
            }
            Ok(page + (virtual_address & 4095))
        }
        fn gpu(&self) -> Result<(), Error> {
            // This is deliberately a HOST MODEL interpreting the actual private
            // page tables/batch. It cannot establish physical GPU execution.
            let mut ctx = [0; 4];
            self.memory.context.read(PAGE + 49 * 4, &mut ctx)?;
            let high = u32::from_le_bytes(ctx);
            self.memory.context.read(PAGE + 51 * 4, &mut ctx)?;
            let low = u32::from_le_bytes(ctx);
            if (u64::from(high) << 32) | u64::from(low) != self.memory.tables.physical[0] {
                return Err(Error::Refused);
            }
            let mut words = [0u32; 14];
            for (i, w) in words.iter_mut().enumerate() {
                let mut d = [0; 4];
                self.physical(self.translate(0x30000 + i as u64 * 4, false)?, &mut d)?;
                *w = u32::from_le_bytes(d);
            }
            if words[0] != 0x11000001
                || words[1] != 0x22204
                || words[2] != 0x606
                || words[3] != 0x50800008
                || words[13] != 0x5000000
            {
                return Err(Error::Refused);
            }
            let width = words[6] & 0xffff;
            let height = words[6] >> 16;
            let pitch = words[10];
            let src = u64::from(words[11]) | (u64::from(words[12]) << 32);
            let dst = u64::from(words[7]) | (u64::from(words[8]) << 32);
            for y in 0..height {
                for x in 0..width * 4 {
                    let mut value = [0];
                    self.physical(
                        self.translate(src + u64::from(y * pitch + x), false)?,
                        &mut value,
                    )?;
                    let physical =
                        self.translate(dst + u64::from(y * (words[4] & 0xffff) + x), true)?;
                    let (ram, offset) = self.ram(physical)?;
                    ram.write(offset, &value)?;
                }
            }
            self.memory
                .context
                .write(lrc::SCRATCH as usize, &1u32.to_le_bytes())
        }
    }
    impl GtIo for Model<'_> {
        fn read(&self, r: u32) -> Result<u32, Error> {
            self.clock.set(self.clock.get() + 10_000);
            self.words
                .borrow()
                .get(&r)
                .copied()
                .ok_or(Error::Unavailable(r))
        }
        fn write(&self, r: u32, v: u32) -> Result<(), Error> {
            self.log.borrow_mut().push((r, v));
            let mut words = self.words.borrow_mut();
            if [0x2209c, 0x2229c, 0x220d0].contains(&r) {
                let old = words.get(&r).copied().unwrap_or(0);
                let value = (old & !(v >> 16)) | (v & (v >> 16));
                words.insert(
                    r,
                    if r == 0x220d0 && value & 1 != 0 {
                        value | 2
                    } else {
                        value
                    },
                );
            } else {
                words.insert(r, if r == 0x941c { 0 } else { v });
            }
            drop(words);
            if r == 0x22550 && self.execute {
                self.gpu()?;
            }
            if self.fail_write.get() == Some(self.log.borrow().len()) {
                self.fail_write.set(None);
                return Err(Error::Unavailable(r)); // write may have landed.
            }
            Ok(())
        }
        fn now_us(&self) -> u64 {
            self.clock.get()
        }
        fn delay_us(&self, n: u32) {
            self.clock.set(self.clock.get() + u64::from(n));
        }
    }
    fn memory() -> Memory {
        let array = super::super::super::gtt::mock::MockPageTable::new(65536);
        let gtt = Arc::new(Gtt::over(Box::new(array)).unwrap());
        let mut m = Memory::allocate(gtt).unwrap();
        m.bind_and_build().unwrap();
        m
    }
    fn model(memory: &Memory, execute: bool) -> Model<'_> {
        Model {
            memory,
            words: RefCell::new(BTreeMap::from([
                (0x220b8, 0),
                (0x2209c, 1 << 9),
                (0x2229c, 0),
                (0x220d0, 0),
                (0x800c, 0),
                (0xa2a0, 0),
                (0x941c, 0),
            ])),
            log: RefCell::new(Vec::new()),
            clock: Cell::new(0),
            execute,
            fail_write: Cell::new(None),
        }
    }
    pub(in crate::drm::intel) fn objects(
        source: Arc<SharedPages>,
        destination: Arc<SharedPages>,
        operation: bcs::Copy,
    ) -> Result<(), Error> {
        let array = super::super::super::gtt::mock::MockPageTable::new(65536);
        let gtt = Arc::new(Gtt::over(Box::new(array)).unwrap());
        let mut memory = Memory::from_objects(gtt, source, destination, operation)?;
        memory.bind_and_build()?;
        let io = model(&memory, true);
        let outcome = execute_and_quiesce(&io, &memory)?;
        drop(io);
        memory.release()?;
        outcome
    }
    #[test]
    fn native_copy_submission_model_walks_private_vm_compares_guards_and_retires_after_reset() {
        let _context = crate::test_support::scheduler_test_context();
        let mut memory = memory();
        let io = model(&memory, true);
        assert!(memory.verify().is_err());
        submit(&io, &memory).unwrap();
        memory.verify().unwrap();
        intel_gt::reset::stop_and_reset_bcs(&io).unwrap();
        assert_eq!(
            io.log.borrow().iter().filter(|(r, _)| *r == 0x941c).count(),
            2
        );
        drop(io);
        memory.release().unwrap();
        assert!(memory.bindings.is_empty());
    }
    #[test]
    fn missing_hardware_breadcrumb_is_not_copy_success_and_buffers_remain_owned() {
        let _context = crate::test_support::scheduler_test_context();
        let memory = memory();
        let io = model(&memory, false);
        assert_eq!(submit(&io, &memory), Err(Error::Timeout(0x22550)));
        assert!(memory.verify().is_err());
        assert_eq!(memory.bindings.len(), 3);
        assert!(Arc::strong_count(&memory.context.pages) > 1);
    }
    #[test]
    fn every_possibly_landed_submit_write_is_reset_before_scoped_unbinding() {
        let _context = crate::test_support::scheduler_test_context();
        let baseline = memory();
        let io = model(&baseline, true);
        submit(&io, &baseline).unwrap();
        let writes = io.log.borrow().len();
        drop(io);
        for prefix in 1..=writes {
            let mut memory = memory();
            let io = model(&memory, true);
            io.fail_write.set(Some(prefix));
            assert!(execute_and_quiesce(&io, &memory).unwrap().is_err());
            assert_eq!(
                io.log.borrow().iter().filter(|(r, _)| *r == 0x941c).count(),
                2
            );
            assert_eq!(memory.bindings.len(), 3);
            drop(io);
            memory.release().unwrap();
        }
    }
    #[test]
    fn ambiguous_reset_after_completed_copy_retains_every_dma_owner() {
        let _context = crate::test_support::scheduler_test_context();
        let memory = memory();
        let io = model(&memory, true);
        submit(&io, &memory).unwrap();
        memory.verify().unwrap();
        let writes = io.log.borrow().len();
        // Next invocation fails on reset's first write, even though completed
        // bytes/breadcrumb exist. Completion is NOT permission to free RAM.
        io.fail_write.set(Some(writes * 2 + 1));
        assert_eq!(execute_and_quiesce(&io, &memory), Err(Error::Quarantined));
        assert_eq!(memory.bindings.len(), 3);
        assert!(Arc::strong_count(&memory.context.pages) > 1);
    }
    #[test]
    fn private_vm_cannot_write_source_or_escape_to_unowned_system_ram() {
        let _context = crate::test_support::scheduler_test_context();
        let memory = memory();
        let io = model(&memory, false);
        assert!(io.translate(0x11000, true).is_err());
        assert!(io.translate(0x21000, true).is_ok());
        assert!(io.translate(0x50000, true).is_err());
        let physical = io.translate(0x50000, false).unwrap();
        assert_eq!(physical, memory.tables.physical[7]);
    }
}
