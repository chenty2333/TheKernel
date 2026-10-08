//! Reset, Identify, queue negotiation and synchronous NVM operations.
use alloc::vec::Vec;
use core::{
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{Ordering, fence},
};

use axdriver_block::{BaseDriverOps, BlockDriverOps, DevError, DevResult, DeviceType};

use crate::{
    Hal,
    desc::{Command, PAGE, TRANSFER, prps},
    regs::{self, Bus},
};
const DEPTH: u16 = 32;
struct Dma<H: Hal> {
    address: u64,
    pointer: NonNull<u8>,
    pages: usize,
    safe: bool,
    requester: Option<tk_vtd::PciRequester>,
    hal: PhantomData<H>,
}
// SAFETY: allocation is uniquely owned, coherent and accessed only under &mut Controller.
unsafe impl<H: Hal> Send for Dma<H> {}
// SAFETY: shared access never changes DMA memory; hardware owns it while published.
unsafe impl<H: Hal> Sync for Dma<H> {}
impl<H: Hal> Dma<H> {
    fn new(pages: usize, requester: Option<tk_vtd::PciRequester>) -> DevResult<Self> {
        let (address, pointer) = H::allocate_for(requester, pages).ok_or(DevError::NoMemory)?;
        if address == 0 || address & 4095 != 0 || pointer.as_ptr() as usize & 4095 != 0 {
            // SAFETY: unpublished allocation has no DMA owner.
            unsafe {
                H::release_for(requester, address, pointer, pages);
            }
            return Err(DevError::InvalidParam);
        }
        // SAFETY: allocation owns pages * PAGE writable bytes.
        unsafe {
            pointer.as_ptr().write_bytes(0, pages * PAGE);
        }
        Ok(Self {
            address,
            pointer,
            pages,
            safe: true,
            requester,
            hal: PhantomData,
        })
    }
}
impl<H: Hal> Drop for Dma<H> {
    fn drop(&mut self) {
        if self.safe {
            // SAFETY: controller reset proved RDY=0, or allocation was never published.
            unsafe {
                H::release_for(self.requester, self.address, self.pointer, self.pages);
            }
        }
        // If reset cannot prove DMA retirement, deliberately quarantine, never reuse.
    }
}
struct Queue<H: Hal> {
    sq: Dma<H>,
    cq: Dma<H>,
    id: u16,
    tail: u16,
    head: u16,
    phase: u16,
    cid: u16,
}
impl<H: Hal> Queue<H> {
    fn new(id: u16, requester: Option<tk_vtd::PciRequester>) -> DevResult<Self> {
        Ok(Self {
            sq: Dma::new(1, requester)?,
            cq: Dma::new(1, requester)?,
            id,
            tail: 0,
            head: 0,
            phase: 1,
            cid: 0,
        })
    }
    fn submit<B: Bus>(&mut self, bus: &mut B, stride: usize, mut cmd: Command) -> DevResult<u32> {
        let mut observed = bus.interrupt_generation();
        let deadline = bus.now_us().map(|now| now.saturating_add(5_000_000));
        self.cid = self.cid.wrapping_add(1);
        cmd.0[0] |= u32::from(self.cid) << 16;
        // SAFETY: 32 entries of 64 bytes fit the owned SQ; one synchronous command in flight.
        unsafe {
            self.sq
                .pointer
                .as_ptr()
                .add(usize::from(self.tail) * 64)
                .cast::<Command>()
                .write_volatile(cmd);
        }
        self.tail = (self.tail + 1) % DEPTH;
        fence(Ordering::SeqCst);
        bus.write32(
            regs::DBS + usize::from(self.id) * 2 * stride,
            u32::from(self.tail),
        );
        for _ in 0..500_000 {
            // SAFETY: 32 CQ entries of 16 bytes fit its allocation; volatile DMA accesses.
            let entry = unsafe {
                self.cq
                    .pointer
                    .as_ptr()
                    .add(usize::from(self.head) * 16)
                    .cast::<u32>()
            };
            // SAFETY: status is the last dword of the same CQ entry.
            let status = unsafe { entry.add(3).read_volatile() };
            if (status >> 16) & 1 == u32::from(self.phase) {
                fence(Ordering::SeqCst);
                // SAFETY: phase publication makes the remaining entry fields available.
                let (result, sq) = unsafe { (entry.read_volatile(), entry.add(2).read_volatile()) };
                self.head += 1;
                if self.head == DEPTH {
                    self.head = 0;
                    self.phase ^= 1;
                }
                bus.write32(
                    regs::DBS + (usize::from(self.id) * 2 + 1) * stride,
                    u32::from(self.head),
                );
                if status as u16 != self.cid || (sq >> 16) as u16 != self.id {
                    return Err(DevError::BadState);
                }
                return if status >> 17 == 0 {
                    Ok(result)
                } else {
                    Err(DevError::Io)
                };
            }
            if bus.read32(regs::CSTS) & 2 != 0 {
                return Err(DevError::BadState);
            }
            if deadline
                .zip(bus.now_us())
                .is_some_and(|(end, now)| now >= end)
            {
                break;
            }
            bus.wait_completion(observed);
            observed = bus.interrupt_generation();
        }
        Err(DevError::BadState)
    }
}
pub struct Controller<H: Hal, B: Bus> {
    bus: B,
    admin: Queue<H>,
    queues: Vec<Queue<H>>,
    data: Dma<H>,
    list: Dma<H>,
    stride: usize,
    timeout: u32,
    blocks: u64,
    block_size: usize,
    nsid: u32,
    allow_write: bool,
    live: bool,
    next: usize,
    max_transfer: usize,
}
impl<H: Hal, B: Bus> Controller<H, B> {
    pub fn new(mut bus: B, allow_write: bool, window_size: usize) -> DevResult<Self> {
        let requester = bus.dma_requester();
        let cap = bus.read64(regs::CAP);
        if cap == u64::MAX
            || (cap & 65535) + 1 < u64::from(DEPTH)
            || cap & (1 << 37) == 0
            || (cap >> 48) & 15 != 0
        {
            return Err(DevError::Unsupported);
        }
        let stride = 4usize << ((cap >> 32) & 15);
        if regs::DBS + 6 * stride > window_size {
            return Err(DevError::InvalidParam);
        }
        let timeout = ((((cap >> 24) & 255) as u32).max(1)) * 50_000;
        let cc = bus.read32(regs::CC);
        bus.write32(regs::CC, cc & !1);
        wait_ready(&mut bus, false, timeout)?;
        let mut this = Self {
            bus,
            admin: Queue::new(0, requester)?,
            queues: Vec::new(),
            data: Dma::new(TRANSFER / PAGE, requester)?,
            list: Dma::new(1, requester)?,
            stride,
            timeout,
            blocks: 0,
            block_size: 0,
            nsid: 0,
            allow_write,
            live: false,
            next: 0,
            max_transfer: TRANSFER,
        };
        this.admin.sq.safe = false;
        this.admin.cq.safe = false;
        this.data.safe = false;
        this.list.safe = false;
        this.bus.write32(regs::AQA, u32::from(DEPTH - 1) * 0x10001);
        this.bus.write64(regs::ASQ, this.admin.sq.address);
        this.bus.write64(regs::ACQ, this.admin.cq.address);
        this.bus.write32(regs::CC, 1 | (6 << 16) | (4 << 20));
        wait_ready(&mut this.bus, true, timeout)?;
        this.live = true;
        this.identify(0, 1)?;
        let mdts = this.bytes()[77];
        this.max_transfer = transfer_limit(mdts);
        this.identify(0, 2)?;
        let nsid = u32::from_le_bytes(this.bytes()[0..4].try_into().unwrap());
        if nsid == 0 || nsid == u32::MAX {
            return Err(DevError::Unsupported);
        }
        this.nsid = nsid;
        this.identify(nsid, 0)?;
        let data = this.bytes();
        let (blocks, block_size) = namespace_geometry(data)?;
        if this.max_transfer < block_size {
            return Err(DevError::Unsupported);
        }
        this.blocks = blocks;
        this.block_size = block_size;
        let mut request = Command::new(9, 0);
        request.0[10] = 7;
        request.0[11] = 0x10001;
        let result = this.admin_cmd(request)?;
        let count = ((result & 65535).min(result >> 16) + 1).min(2);
        for id in 1..=count as u16 {
            let mut queue = Queue::new(id, requester)?;
            queue.sq.safe = false;
            queue.cq.safe = false;
            this.queues.push(queue);
            let q = this.queues.last().unwrap();
            let mut cq = Command::new(5, 0);
            cq.pointer(6, q.cq.address);
            cq.0[10] = u32::from(id) | (u32::from(DEPTH - 1) << 16);
            cq.0[11] = 1 | if this.bus.interrupt_enabled() { 2 } else { 0 };
            this.admin_cmd(cq)?;
            let mut sq = Command::new(1, 0);
            sq.pointer(6, this.queues.last().unwrap().sq.address);
            sq.0[10] = u32::from(id) | (u32::from(DEPTH - 1) << 16);
            sq.0[11] = 1 | (u32::from(id) << 16);
            this.admin_cmd(sq)?;
        }
        Ok(this)
    }
    pub fn read_only(&self) -> bool {
        !self.allow_write
    }
    pub fn queue_count(&self) -> usize {
        self.queues.len()
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: no DMA is outstanding after a synchronous completion.
        unsafe { core::slice::from_raw_parts(self.data.pointer.as_ptr(), TRANSFER) }
    }
    fn admin_cmd(&mut self, command: Command) -> DevResult<u32> {
        let result = self.admin.submit(&mut self.bus, self.stride, command);
        if matches!(result, Err(DevError::BadState)) {
            self.live = false;
        }
        result
    }
    fn identify(&mut self, nsid: u32, cns: u32) -> DevResult {
        let mut c = Command::new(6, nsid);
        c.pointer(6, self.data.address);
        c.0[10] = cns;
        self.admin_cmd(c).map(|_| ())
    }
    fn command(&mut self, command: Command) -> DevResult {
        if !self.live {
            return Err(DevError::BadState);
        }
        let index = self.next % self.queues.len();
        self.next += 1;
        let result = self.queues[index]
            .submit(&mut self.bus, self.stride, command)
            .map(|_| ());
        if matches!(result, Err(DevError::BadState)) {
            self.live = false;
        }
        result
    }
    fn transfer(
        &mut self,
        block: u64,
        length: usize,
        write: bool,
        mut copy: impl FnMut(*mut u8, usize, usize),
    ) -> DevResult {
        if write && !self.allow_write {
            return Err(DevError::Unsupported);
        }
        if !self.live {
            return Err(DevError::BadState);
        }
        if !length.is_multiple_of(self.block_size)
            || block > self.blocks
            || (length / self.block_size) as u64 > self.blocks - block
        {
            return Err(DevError::InvalidParam);
        }
        let mut offset = 0;
        while offset < length {
            let size = (length - offset).min(self.max_transfer);
            if write {
                copy(self.data.pointer.as_ptr(), offset, size);
            }
            // SAFETY: list allocation holds PAGE/8 entries and is not currently device-owned.
            let entries = unsafe {
                core::slice::from_raw_parts_mut(self.list.pointer.as_ptr().cast::<u64>(), PAGE / 8)
            };
            let (first, second) = prps(self.data.address, size, self.list.address, entries)?;
            let lba = block + (offset / self.block_size) as u64;
            let mut c = Command::new(if write { 1 } else { 2 }, self.nsid);
            c.pointer(6, first);
            c.pointer(8, second);
            c.pointer(10, lba);
            c.0[12] = (size / self.block_size - 1) as u32;
            self.command(c)?;
            if !write {
                copy(self.data.pointer.as_ptr(), offset, size);
            }
            offset += size;
        }
        Ok(())
    }
}
/// MDTS is expressed in minimum controller pages (CAP.MPSMIN=0 here).
/// Check the multiplication too: checked_shl alone can discard value bits.
pub(super) fn transfer_limit(mdts: u8) -> usize {
    if mdts == 0 {
        return TRANSFER;
    }
    1usize
        .checked_shl(u32::from(mdts))
        .and_then(|scale| PAGE.checked_mul(scale))
        .unwrap_or(usize::MAX)
        .min(TRANSFER)
}

pub(super) fn namespace_geometry(data: &[u8]) -> DevResult<(u64, usize)> {
    if data.len() < PAGE {
        return Err(DevError::InvalidParam);
    }
    let blocks = u64::from_le_bytes(data[0..8].try_into().unwrap());
    // FLBAS[6:5] are the upper two bits of the six-bit format index.
    let format = usize::from(data[26] & 0x0f) | usize::from((data[26] & 0x60) >> 1);
    if data[25] >= 64 || format > usize::from(data[25]) {
        return Err(DevError::Unsupported);
    }
    let offset = 128 + format * 4;
    let metadata = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
    let shift = data[offset + 2];
    if blocks == 0 || metadata != 0 || data[29] & 7 != 0 || !(9..=12).contains(&shift) {
        return Err(DevError::Unsupported);
    }
    Ok((blocks, 1 << shift))
}

fn wait_ready<B: Bus>(bus: &mut B, ready: bool, timeout: u32) -> DevResult {
    for _ in 0..timeout {
        let status = bus.read32(regs::CSTS);
        if status == u32::MAX || (ready && status & 2 != 0) {
            return Err(DevError::BadState);
        }
        if (status & 1 != 0) == ready {
            return Ok(());
        }
        bus.delay_us(10);
    }
    Err(DevError::BadState)
}
impl<H: Hal, B: Bus> Drop for Controller<H, B> {
    fn drop(&mut self) {
        let cc = self.bus.read32(regs::CC);
        self.bus.write32(regs::CC, cc & !1);
        if wait_ready(&mut self.bus, false, self.timeout).is_ok() {
            self.admin.sq.safe = true;
            self.admin.cq.safe = true;
            self.data.safe = true;
            self.list.safe = true;
            for q in &mut self.queues {
                q.sq.safe = true;
                q.cq.safe = true;
            }
        }
    }
}
impl<H: Hal, B: Bus> BaseDriverOps for Controller<H, B> {
    fn device_name(&self) -> &str {
        "nvme0n1"
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}
impl<H: Hal, B: Bus> BlockDriverOps for Controller<H, B> {
    fn num_blocks(&self) -> u64 {
        self.blocks
    }
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn read_block(&mut self, block: u64, buf: &mut [u8]) -> DevResult {
        self.transfer(block, buf.len(), false, |pointer, offset, size| {
            // SAFETY: disjoint owned bounce and caller slices, both cover size bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(pointer, buf.as_mut_ptr().add(offset), size);
            }
        })
    }
    fn write_block(&mut self, block: u64, buf: &[u8]) -> DevResult {
        self.transfer(block, buf.len(), true, |pointer, offset, size| {
            // SAFETY: disjoint owned bounce and caller slices, both cover size bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr().add(offset), pointer, size);
            }
        })
    }
    fn flush(&mut self) -> DevResult {
        if !self.allow_write {
            return Ok(());
        }
        self.command(Command::new(0, self.nsid))
    }
}
