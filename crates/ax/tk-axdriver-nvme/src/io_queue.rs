// SPDX-License-Identifier: Apache-2.0
//! Queue-owned DMA, accepted-prefix batches and task-side CQ retirement.
//!
//! Adapted to TheKernel's block boundary from TGOSKits nvme-driver's owned
//! request slots and stage/commit/drain model (Apache-2.0). Each CID retains
//! its PRP and data owners until its exact terminal CQE or proven reset.
use axdriver_block::{
    BlockAsyncOp, BlockCompletion, BlockCompletionDrain, BlockCompletionOwner,
    BlockCompletionStatus, BlockQueueRequest, BlockRequestHandle, BlockResetOutcome, BlockSegment,
    BlockSegmentDirection, BlockSubmitReport,
};

use super::*;

pub(super) struct RequestSlot<H: Hal> {
    handle: BlockRequestHandle,
    op: BlockAsyncOp,
    data: Option<Dma<H>>,
    list: Option<Dma<H>>,
    destinations: Vec<BlockSegment>,
    bytes: usize,
    completion: Option<DevResult>,
    deadline_us: Option<u64>,
}
impl<H: Hal> RequestSlot<H> {
    pub(super) fn quiesced(&mut self) {
        if let Some(data) = &mut self.data {
            data.safe = true;
        }
        if let Some(list) = &mut self.list {
            list.safe = true;
        }
    }
    fn retire(self) -> DevResult<usize> {
        let result = self.completion.ok_or(DevError::BadState)?;
        result?;
        if self.op == BlockAsyncOp::Read {
            let data = self.data.as_ref().ok_or(DevError::BadState)?;
            let mut offset = 0;
            for segment in &self.destinations {
                // SAFETY: accepted read handles retain the caller's segment
                // lease until retirement. The terminal CQE ended device writes
                // to our distinct, owned bounce buffer before this task copy.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data.pointer.as_ptr().add(offset),
                        segment.addr as *mut u8,
                        segment.len,
                    );
                }
                offset += segment.len;
            }
        }
        Ok(self.bytes)
    }
}

struct Prepared<H: Hal> {
    queue: usize,
    cid: usize,
    command: Command,
    slot: RequestSlot<H>,
}

impl<H: Hal, B: Bus> Controller<H, B> {
    fn pending_hardware(&self) -> bool {
        self.queues
            .iter()
            .flat_map(|q| q.slots.iter().flatten())
            .any(|s| s.completion.is_none())
    }
    fn flush_pending(&self) -> bool {
        self.queues
            .iter()
            .flat_map(|q| q.slots.iter().flatten())
            .any(|s| s.op == BlockAsyncOp::Flush && s.completion.is_none())
    }
    fn validate_request(&self, request: &BlockQueueRequest<'_>) -> DevResult<usize> {
        if request.handle.is_some() {
            return Err(DevError::InvalidParam);
        }
        if request.op == BlockAsyncOp::Flush {
            return if request.segments.is_empty() {
                Ok(0)
            } else {
                Err(DevError::InvalidParam)
            };
        }
        if request.op == BlockAsyncOp::Write && !self.allow_write {
            return Err(DevError::Unsupported);
        }
        let direction = if request.op == BlockAsyncOp::Read {
            BlockSegmentDirection::DeviceToMemory
        } else {
            BlockSegmentDirection::MemoryToDevice
        };
        let mut bytes = 0usize;
        for segment in request.segments {
            if segment.direction != direction
                || segment.len == 0
                || segment.addr == 0
                || segment.addr.checked_add(segment.len).is_none()
            {
                return Err(DevError::InvalidParam);
            }
            bytes = bytes
                .checked_add(segment.len)
                .ok_or(DevError::InvalidParam)?;
        }
        if bytes == 0
            || bytes > self.max_transfer
            || !bytes.is_multiple_of(self.block_size)
            || request.block_id > self.blocks
            || (bytes / self.block_size) as u64 > self.blocks - request.block_id
        {
            return Err(DevError::InvalidParam);
        }
        Ok(bytes)
    }
    pub(super) fn submit_owned_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if !self.live {
            return Err(DevError::BadState);
        }
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }
        if self.flush_pending() {
            return Ok(BlockSubmitReport {
                queue_full: true,
                ..Default::default()
            });
        }
        let capacity = self
            .queues
            .iter()
            .map(|q| (usize::from(DEPTH) - 1).saturating_sub(q.slots.iter().flatten().count()))
            .sum::<usize>();
        let limit = requests.len().min(capacity);
        if limit == 0 {
            return Ok(BlockSubmitReport {
                queue_full: true,
                ..Default::default()
            });
        }
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(limit)
            .map_err(|_| DevError::NoMemory)?;
        let mut total = 0usize;
        for request in requests.iter().take(limit) {
            if request.op == BlockAsyncOp::Flush
                && (self.pending_hardware() || !prepared.is_empty())
            {
                break;
            }
            let bytes = self.validate_request(request)?;
            let token = self
                .next_handle
                .checked_add(prepared.len() as u64)
                .ok_or(DevError::BadState)?;
            if token == u64::MAX {
                return Err(DevError::BadState);
            }
            let (queue, cid) = (0..self.queues.len())
                .find_map(|step| {
                    let index = (self.next + step) % self.queues.len();
                    let occupied = self.queues[index].slots.iter().flatten().count()
                        + prepared
                            .iter()
                            .filter(|p: &&Prepared<H>| p.queue == index)
                            .count();
                    if occupied >= usize::from(DEPTH) - 1 {
                        return None;
                    }
                    (0..usize::from(DEPTH))
                        .find(|&cid| {
                            self.queues[index].slots[cid].is_none()
                                && !prepared.iter().any(|p| p.queue == index && p.cid == cid)
                        })
                        .map(|cid| (index, cid))
                })
                .ok_or(DevError::BadState)?;
            let mut command = Command::new(
                match request.op {
                    BlockAsyncOp::Read => 2,
                    BlockAsyncOp::Write => 1,
                    BlockAsyncOp::Flush => 0,
                },
                self.nsid,
            );
            command.0[0] |= (cid as u32) << 16;
            let mut destinations = Vec::new();
            let (data, list) = if bytes != 0 {
                let data = Dma::<H>::new(bytes.div_ceil(PAGE), self.bus.dma_requester())?;
                let list = Dma::<H>::new(1, self.bus.dma_requester())?;
                // SAFETY: unpublished exclusive PRP allocation owns one page.
                let entries = unsafe {
                    core::slice::from_raw_parts_mut(list.pointer.as_ptr().cast::<u64>(), PAGE / 8)
                };
                let (prp1, prp2) = prps(data.address, bytes, list.address, entries)?;
                command.pointer(6, prp1);
                command.pointer(8, prp2);
                command.pointer(10, request.block_id);
                command.0[12] = (bytes / self.block_size - 1) as u32;
                if request.op == BlockAsyncOp::Read {
                    destinations
                        .try_reserve_exact(request.segments.len())
                        .map_err(|_| DevError::NoMemory)?;
                    destinations.extend_from_slice(request.segments);
                } else {
                    let mut offset = 0;
                    for segment in request.segments {
                        // SAFETY: the caller grants readable segments through
                        // submission; our unpublished DMA owner is disjoint and
                        // sized for their validated total. No pointer escapes.
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                segment.addr as *const u8,
                                data.pointer.as_ptr().add(offset),
                                segment.len,
                            );
                        }
                        offset += segment.len;
                    }
                }
                (Some(data), Some(list))
            } else {
                (None, None)
            };
            total += bytes;
            prepared.push(Prepared {
                queue,
                cid,
                command,
                slot: RequestSlot {
                    handle: BlockRequestHandle { raw: token },
                    op: request.op,
                    data,
                    list,
                    destinations,
                    bytes,
                    completion: None,
                    deadline_us: self.bus.now_us().map(|now| now.saturating_add(5_000_000)),
                },
            });
            if request.op == BlockAsyncOp::Flush {
                break;
            }
        }
        if prepared.is_empty() {
            return Ok(BlockSubmitReport {
                queue_full: true,
                ..Default::default()
            });
        }
        let count = prepared.len();
        // All fallible work precedes descriptor publication. A later invalid
        // request/allocation error leaves the whole offered prefix untouched.
        for (request, mut owner) in requests.iter_mut().zip(prepared) {
            let queue = &mut self.queues[owner.queue];
            if let Some(data) = &mut owner.slot.data {
                data.safe = false;
            }
            if let Some(list) = &mut owner.slot.list {
                list.safe = false;
            }
            // SAFETY: the queue admits at most DEPTH-1 live CID owners. Its
            // exclusive task owner writes the next unused 64-byte SQ entry.
            unsafe {
                queue
                    .sq
                    .pointer
                    .as_ptr()
                    .add(usize::from(queue.tail) * 64)
                    .cast::<Command>()
                    .write_volatile(owner.command);
            }
            queue.tail = (queue.tail + 1) % DEPTH;
            request.handle = Some(owner.slot.handle);
            queue.slots[owner.cid] = Some(owner.slot);
            self.next = (owner.queue + 1) % self.queues.len();
        }
        self.next_handle += count as u64;
        fence(Ordering::SeqCst);
        // One doorbell per hardware queue, never per request.
        for queue in &self.queues {
            self.bus.write32(
                regs::DBS + usize::from(queue.id) * 2 * self.stride,
                u32::from(queue.tail),
            );
        }
        Ok(BlockSubmitReport {
            submitted: count,
            bytes: total,
            queue_full: count < requests.len(),
        })
    }

    fn harvest(&mut self) -> DevResult {
        if !self.live {
            return Err(DevError::BadState);
        }
        // IRQ-owned queues inspect CQ only after an acknowledged event. The
        // explicit polling profile remains available for route-less devices.
        let generation = self.bus.interrupt_generation();
        if self.bus.requires_interrupt_event() && generation == self.observed_irq {
            return self.enforce_deadlines();
        }
        self.observed_irq = generation;
        for queue in &mut self.queues {
            let old_head = queue.head;
            for _ in 0..DEPTH {
                // SAFETY: CQ is a live, owned page containing DEPTH CQEs.
                let entry = unsafe {
                    queue
                        .cq
                        .pointer
                        .as_ptr()
                        .add(usize::from(queue.head) * 16)
                        .cast::<u32>()
                };
                // SAFETY: the last dword publishes CQE phase/command/status.
                let status = unsafe { entry.add(3).read_volatile() };
                if (status >> 16) & 1 != u32::from(queue.phase) {
                    break;
                }
                fence(Ordering::SeqCst);
                // SAFETY: matching phase publishes the remaining CQE fields.
                let sq = unsafe { entry.add(2).read_volatile() };
                let cid = usize::from(status as u16);
                let slot = queue.slots.get_mut(cid).and_then(Option::as_mut);
                let Some(slot) = slot.filter(|s| s.completion.is_none()) else {
                    self.stop_owned();
                    return Err(DevError::BadState);
                };
                if (sq >> 16) as u16 != queue.id || (sq as u16) >= DEPTH {
                    self.stop_owned();
                    return Err(DevError::BadState);
                }
                slot.quiesced();
                slot.completion = Some(if status >> 17 == 0 {
                    Ok(())
                } else {
                    Err(DevError::Io)
                });
                queue.head += 1;
                if queue.head == DEPTH {
                    queue.head = 0;
                    queue.phase ^= 1;
                }
            }
            if queue.head != old_head {
                self.bus.write32(
                    regs::DBS + (usize::from(queue.id) * 2 + 1) * self.stride,
                    u32::from(queue.head),
                );
            }
        }
        self.enforce_deadlines()
    }
    fn enforce_deadlines(&mut self) -> DevResult {
        let expired = self.bus.now_us().is_some_and(|now| {
            self.queues
                .iter()
                .flat_map(|q| q.slots.iter().flatten())
                .any(|slot| {
                    slot.completion.is_none() && slot.deadline_us.is_some_and(|end| now >= end)
                })
        });
        if expired || self.bus.read32(regs::CSTS) & 2 != 0 {
            self.stop_owned();
            return Err(DevError::BadState);
        }
        Ok(())
    }
    fn find_slot(&self, handle: BlockRequestHandle) -> Option<(usize, usize)> {
        self.queues.iter().enumerate().find_map(|(qi, q)| {
            q.slots
                .iter()
                .position(|s| s.as_ref().is_some_and(|s| s.handle == handle))
                .map(|cid| (qi, cid))
        })
    }
    pub(super) fn wait_owned(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        for (i, handle) in handles.iter().enumerate() {
            if self.find_slot(*handle).is_none() || handles[..i].contains(handle) {
                return Err(DevError::InvalidParam);
            }
        }
        let deadline = self.bus.now_us().map(|now| now.saturating_add(5_000_000));
        for _ in 0..500_000 {
            let observed = self.bus.interrupt_generation();
            self.harvest()?;
            if handles.iter().all(|h| {
                self.find_slot(*h).is_some_and(|(q, c)| {
                    self.queues[q].slots[c]
                        .as_ref()
                        .is_some_and(|s| s.completion.is_some())
                })
            }) {
                let mut error = None;
                for handle in handles {
                    let (q, c) = self.find_slot(*handle).ok_or(DevError::BadState)?;
                    let owner = self.queues[q].slots[c].take().ok_or(DevError::BadState)?;
                    if let Err(e) = owner.retire() {
                        error.get_or_insert(e);
                    }
                }
                return error.map_or(Ok(()), Err);
            }
            if deadline
                .zip(self.bus.now_us())
                .is_some_and(|(end, now)| now >= end)
            {
                break;
            }
            self.bus.wait_completion(observed);
        }
        self.stop_owned();
        Err(DevError::BadState)
    }
    pub(super) fn drain_owned(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        if output.is_empty() {
            return Ok(BlockCompletionDrain::default());
        }
        self.harvest()?;
        let mut count = 0;
        for queue in &mut self.queues {
            for slot in &mut queue.slots {
                if count == output.len() {
                    break;
                }
                if slot.as_ref().is_none_or(|s| s.completion.is_none()) {
                    continue;
                }
                let owner = slot.take().ok_or(DevError::BadState)?;
                let handle = owner.handle;
                let result = owner.retire();
                output[count] = BlockCompletion {
                    handle,
                    owner: BlockCompletionOwner::Ordinary,
                    cookie: handle.raw,
                    status: if result.is_ok() {
                        BlockCompletionStatus::Success
                    } else {
                        BlockCompletionStatus::DeviceError(1)
                    },
                    bytes: result.unwrap_or(0) as u32,
                };
                count += 1;
            }
        }
        Ok(BlockCompletionDrain {
            completed: count,
            continuation: self
                .queues
                .iter()
                .flat_map(|q| q.slots.iter().flatten())
                .any(|s| s.completion.is_some()),
        })
    }
    pub(super) fn wait_hardware_idle(&mut self) -> DevResult {
        let deadline = self.bus.now_us().map(|now| now.saturating_add(5_000_000));
        for _ in 0..500_000 {
            let observed = self.bus.interrupt_generation();
            self.harvest()?;
            if !self.pending_hardware() {
                return Ok(());
            }
            if deadline
                .zip(self.bus.now_us())
                .is_some_and(|(end, now)| now >= end)
            {
                break;
            }
            self.bus.wait_completion(observed);
        }
        self.stop_owned();
        Err(DevError::BadState)
    }
    pub(super) fn stop_owned(&mut self) -> BlockResetOutcome {
        self.live = false;
        let cc = self.bus.read32(regs::CC);
        self.bus.write32(regs::CC, cc & !1);
        let stopped = wait_ready(&mut self.bus, false, self.timeout).is_ok();
        if stopped {
            self.admin.sq.safe = true;
            self.admin.cq.safe = true;
            self.data.safe = true;
        }
        for queue in &mut self.queues {
            if stopped {
                queue.sq.safe = true;
                queue.cq.safe = true;
            }
            for slot in &mut queue.slots {
                if let Some(mut owner) = slot.take() {
                    if stopped {
                        owner.quiesced();
                    }
                    // No post-error copy into caller memory. If reset failed,
                    // dropping unsafe DMA owners deliberately retains backing.
                }
            }
        }
        if stopped {
            BlockResetOutcome::Retired
        } else {
            BlockResetOutcome::Quarantined
        }
    }
    pub(super) fn transfer_segments(
        &mut self,
        block: u64,
        address: usize,
        length: usize,
        op: BlockAsyncOp,
    ) -> DevResult {
        if !self.live {
            return Err(DevError::BadState);
        }
        if !length.is_multiple_of(self.block_size)
            || block > self.blocks
            || (length / self.block_size) as u64 > self.blocks - block
        {
            return Err(DevError::InvalidParam);
        }
        if op == BlockAsyncOp::Write && !self.allow_write {
            return Err(DevError::Unsupported);
        }
        let mut offset = 0;
        while offset < length {
            let bytes = (length - offset).min(self.max_transfer);
            let segment = BlockSegment {
                addr: address.checked_add(offset).ok_or(DevError::InvalidParam)?,
                len: bytes,
                direction: if op == BlockAsyncOp::Read {
                    BlockSegmentDirection::DeviceToMemory
                } else {
                    BlockSegmentDirection::MemoryToDevice
                },
            };
            let mut request = BlockQueueRequest {
                op,
                block_id: block + (offset / self.block_size) as u64,
                segments: core::slice::from_ref(&segment),
                handle: None,
            };
            let report = self.submit_owned_batch(core::slice::from_mut(&mut request))?;
            if report.submitted != 1 {
                return Err(DevError::Again);
            }
            self.wait_owned(&[request.handle.ok_or(DevError::BadState)?])?;
            offset += bytes;
        }
        Ok(())
    }
}
