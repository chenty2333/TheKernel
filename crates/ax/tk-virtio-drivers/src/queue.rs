#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "alloc")]
pub mod owning;

#[cfg(feature = "alloc")]
use alloc::{boxed::Box, vec::Vec};
#[cfg(test)]
use core::cmp::min;
#[cfg(test)]
use core::ptr;
use core::{
    convert::TryInto,
    hint::spin_loop,
    mem::{size_of, take},
    ptr::NonNull,
    sync::atomic::{AtomicU16, Ordering, fence},
};

use bitflags::bitflags;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::{
    Error, PAGE_SIZE, Result, align_up,
    hal::{BufferDirection, Dma, Hal, PhysAddr},
    nonnull_slice_from_raw_parts, pages,
    stats::{io_counters_enabled, record_queue_sync_wait},
    transport::Transport,
};

#[inline]
fn dma_sync_barrier() {}

/// The mechanism for bulk data transport on virtio devices.
///
/// Each device can have zero or more virtqueues.
///
/// * `SIZE`: The size of the queue. This is both the number of descriptors, and the number of slots
///   in the available and used rings. It must be a power of 2 and fit in a [`u16`].
#[derive(Debug)]
pub struct VirtQueue<H: Hal, const SIZE: usize> {
    /// DMA guard
    layout: VirtQueueLayout<H>,
    /// Descriptor table
    ///
    /// The device may be able to modify this, even though it's not supposed to, so we shouldn't
    /// trust values read back from it. Use `desc_shadow` instead to keep track of what we wrote to
    /// it.
    desc: NonNull<[Descriptor]>,
    /// Available ring
    ///
    /// The device may be able to modify this, even though it's not supposed to, so we shouldn't
    /// trust values read back from it. The only field we need to read currently is `idx`, so we
    /// have `avail_idx` below to use instead.
    avail: NonNull<AvailRing<SIZE>>,
    /// Used ring
    used: NonNull<UsedRing<SIZE>>,

    /// The index of queue
    queue_idx: u16,
    /// The number of descriptors currently in use.
    num_used: u16,
    /// The head desc index of the free list.
    free_head: u16,
    /// Our trusted copy of `desc` that the device can't access.
    desc_shadow: [Descriptor; SIZE],
    /// Our trusted copy of `avail.idx`.
    avail_idx: u16,
    /// End of the publication interval covered by the previous kick check.
    last_kick_avail_idx: u16,
    last_used_idx: u16,
    /// Whether the `VIRTIO_F_EVENT_IDX` feature has been negotiated.
    event_idx: bool,
    /// Whether used-buffer notifications are currently enabled.
    ///
    /// With `VIRTIO_F_EVENT_IDX`, the suppression state lives in
    /// `AvailRing::used_event` rather than in `AvailRing::flags`.  Keep the
    /// driver's state here so popping or publishing a descriptor cannot
    /// accidentally re-enable interrupts after a terminal disable.
    dev_notify_enabled: bool,
    #[cfg(feature = "alloc")]
    indirect: bool,
    #[cfg(feature = "alloc")]
    indirect_lists: [Option<NonNull<[Descriptor]>>; SIZE],
}

/// A descriptor payload that has already been mapped from a pinned physical
/// range.  This type is crate-private so callers cannot bypass the block
/// driver's validation and mapping lifetime rules.
#[derive(Clone, Copy)]
pub(crate) struct PhysicalBuffer {
    pub(crate) addr: PhysAddr,
    pub(crate) len: usize,
}

impl<H: Hal, const SIZE: usize> VirtQueue<H, SIZE> {
    const SIZE_OK: () = assert!(SIZE.is_power_of_two() && SIZE <= u16::MAX as usize);

    /// Creates a new VirtQueue.
    ///
    /// * `indirect`: Whether to use indirect descriptors. This should be set if the
    ///   `VIRTIO_F_INDIRECT_DESC` feature has been negotiated with the device.
    /// * `event_idx`: Whether to use the `used_event` and `avail_event` fields for notification
    ///   suppression. This should be set if the `VIRTIO_F_EVENT_IDX` feature has been negotiated
    ///   with the device.
    pub fn new<T: Transport>(
        transport: &mut T,
        idx: u16,
        indirect: bool,
        event_idx: bool,
    ) -> Result<Self> {
        #[allow(clippy::let_unit_value)]
        let _ = Self::SIZE_OK;

        if transport.queue_used(idx) {
            return Err(Error::AlreadyUsed);
        }
        if transport.max_queue_size(idx) < SIZE as u32 {
            return Err(Error::InvalidParam);
        }
        let size = SIZE as u16;

        let layout = if transport.requires_legacy_layout() {
            VirtQueueLayout::allocate_legacy(size)?
        } else {
            VirtQueueLayout::allocate_flexible(size)?
        };

        transport.queue_set(
            idx,
            size.into(),
            layout.descriptors_paddr(),
            layout.driver_area_paddr(),
            layout.device_area_paddr(),
        );

        let desc =
            nonnull_slice_from_raw_parts(layout.descriptors_vaddr().cast::<Descriptor>(), SIZE);
        let avail = layout.avail_vaddr().cast();
        let used = layout.used_vaddr().cast();

        let mut desc_shadow: [Descriptor; SIZE] = FromZeroes::new_zeroed();
        // Link descriptors together.
        for i in 0..(size - 1) {
            desc_shadow[i as usize].next = i + 1;
            // Safe because `desc` is properly aligned, dereferenceable, initialised, and the device
            // won't access the descriptors for the duration of this unsafe block.
            unsafe {
                (*desc.as_ptr())[i as usize].next = i + 1;
            }
        }

        #[cfg(feature = "alloc")]
        const NONE: Option<NonNull<[Descriptor]>> = None;
        Ok(VirtQueue {
            layout,
            desc,
            avail,
            used,
            queue_idx: idx,
            num_used: 0,
            free_head: 0,
            desc_shadow,
            avail_idx: 0,
            last_kick_avail_idx: 0,
            last_used_idx: 0,
            event_idx,
            dev_notify_enabled: true,
            #[cfg(feature = "alloc")]
            indirect,
            #[cfg(feature = "alloc")]
            indirect_lists: [NONE; SIZE],
        })
    }

    /// Add buffers to the virtqueue, return a token.
    ///
    /// The buffers must not be empty.
    ///
    /// Ref: linux virtio_ring.c virtqueue_add
    ///
    /// # Safety
    ///
    /// The input and output buffers must remain valid and not be accessed until a call to
    /// `pop_used` with the returned token succeeds.
    pub unsafe fn add<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        // SAFETY: This method preserves the original add contract: it publishes
        // the descriptor chain before returning, and the caller must keep the
        // buffers alive until the matching `pop_used`.
        let head = unsafe { self.add_unpublished(inputs, outputs) }?;
        self.publish_unpublished(head);
        Ok(head)
    }

    /// Add buffers to the descriptor table without publishing them in the
    /// available ring yet.
    ///
    /// This is used by block pending-completion paths which must install their
    /// token metadata before a fast device can observe and complete the chain.
    ///
    /// # Safety
    ///
    /// The input and output buffers must remain valid and not be accessed until
    /// a call to `pop_used` for the returned token succeeds. The caller must
    /// publish the returned token with [`publish_unpublished`](Self::publish_unpublished).
    pub unsafe fn add_unpublished<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(Error::InvalidParam);
        }
        let descriptors_needed = inputs.len() + outputs.len();
        // Only consider indirect descriptors if the alloc feature is enabled, as they require
        // allocation.
        #[cfg(feature = "alloc")]
        if self.num_used as usize + 1 > SIZE
            || descriptors_needed > SIZE
            || (!self.indirect && self.num_used as usize + descriptors_needed > SIZE)
        {
            return Err(Error::QueueFull);
        }
        #[cfg(not(feature = "alloc"))]
        if self.num_used as usize + descriptors_needed > SIZE {
            return Err(Error::QueueFull);
        }

        #[cfg(feature = "alloc")]
        let head = if self.indirect && descriptors_needed > 1 {
            self.add_indirect(inputs, outputs)
        } else {
            self.add_direct(inputs, outputs)
        };
        #[cfg(not(feature = "alloc"))]
        let head = self.add_direct(inputs, outputs);

        Ok(head)
    }

    /// Adds a direct descriptor chain containing normal virtual header/status
    /// buffers and already-mapped physical payload buffers.
    ///
    /// This is intentionally crate-private.  The block device validates and
    /// maps the physical ranges before constructing [`PhysicalBuffer`] values;
    /// this method only installs the resulting device addresses and never
    /// calls [`Hal::share`] or creates a Rust slice for a physical payload.
    ///
    /// # Safety
    ///
    /// Virtual buffers and physical mappings must remain valid and untouched
    /// until [`Self::pop_used_physical`] succeeds. The physical input ranges
    /// are device-readable and the physical output ranges are device-writable;
    /// their owner must keep the ranges pinned, avoid CPU access during DMA,
    /// and unmap them only after the matching pop. The physical mapping owner
    /// is responsible for unmapping after that point.
    pub(crate) unsafe fn add_unpublished_physical<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        physical_inputs: &[PhysicalBuffer],
        physical_outputs: &[PhysicalBuffer],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        let descriptors_needed =
            inputs.len() + physical_inputs.len() + physical_outputs.len() + outputs.len();
        #[cfg(feature = "alloc")]
        let queue_descriptors = if self.indirect && descriptors_needed > 1 {
            1
        } else {
            descriptors_needed
        };
        #[cfg(not(feature = "alloc"))]
        let queue_descriptors = descriptors_needed;
        if descriptors_needed == 0
            || descriptors_needed > SIZE
            || usize::from(self.num_used) + queue_descriptors > SIZE
        {
            return Err(if descriptors_needed == 0 {
                Error::InvalidParam
            } else {
                Error::QueueFull
            });
        }
        if inputs
            .iter()
            .any(|input| input.is_empty() || input.len() > u32::MAX as usize)
            || outputs
                .iter()
                .any(|output| output.is_empty() || output.len() > u32::MAX as usize)
            || physical_inputs
                .iter()
                .chain(physical_outputs)
                .any(|buffer| buffer.addr == 0 || buffer.len == 0 || buffer.len > u32::MAX as usize)
        {
            return Err(Error::InvalidParam);
        }

        #[cfg(feature = "alloc")]
        if self.indirect && descriptors_needed > 1 {
            return self.add_indirect_physical(inputs, physical_inputs, physical_outputs, outputs);
        }

        let head = self.free_head;
        let mut current = head;
        let mut last = head;

        for input in inputs {
            // SAFETY: The caller promises the virtual header remains valid
            // until the matching physical pop.
            unsafe {
                self.desc_shadow[usize::from(current)].set_buf::<H>(
                    (*input).into(),
                    BufferDirection::DriverToDevice,
                    DescFlags::NEXT,
                );
            }
            last = current;
            current = self.desc_shadow[usize::from(current)].next;
            self.write_desc(last);
        }

        for buffer in physical_inputs {
            self.install_physical_descriptor(current, *buffer, false);
            last = current;
            current = self.desc_shadow[usize::from(current)].next;
        }

        for buffer in physical_outputs {
            self.install_physical_descriptor(current, *buffer, true);
            last = current;
            current = self.desc_shadow[usize::from(current)].next;
        }

        for output in outputs {
            // SAFETY: The caller promises the virtual status remains valid
            // until the matching physical pop.
            unsafe {
                self.desc_shadow[usize::from(current)].set_buf::<H>(
                    (*output).into(),
                    BufferDirection::DeviceToDriver,
                    DescFlags::NEXT,
                );
            }
            last = current;
            current = self.desc_shadow[usize::from(current)].next;
            self.write_desc(last);
        }

        self.desc_shadow[usize::from(last)]
            .flags
            .remove(DescFlags::NEXT);
        self.write_desc(last);
        self.num_used = self
            .num_used
            .checked_add(descriptors_needed as u16)
            .expect("virtqueue descriptor count overflow");
        self.free_head = current;
        Ok(head)
    }

    #[cfg(feature = "alloc")]
    fn add_indirect_physical<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        physical_inputs: &[PhysicalBuffer],
        physical_outputs: &[PhysicalBuffer],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        let descriptors_needed =
            inputs.len() + physical_inputs.len() + physical_outputs.len() + outputs.len();
        let head = self.free_head;
        let mut indirect_list = Self::try_new_box_slice_zeroed(descriptors_needed)?;
        let mut index = 0usize;
        for input in inputs {
            // SAFETY: the caller keeps virtual buffers valid until the used
            // entry is popped.
            unsafe {
                indirect_list[index].set_buf::<H>(
                    (*input).into(),
                    BufferDirection::DriverToDevice,
                    DescFlags::NEXT,
                );
            }
            index += 1;
        }
        for buffer in physical_inputs {
            Self::install_indirect_physical_descriptor(&mut indirect_list[index], *buffer, false);
            index += 1;
        }
        for buffer in physical_outputs {
            Self::install_indirect_physical_descriptor(&mut indirect_list[index], *buffer, true);
            index += 1;
        }
        for output in outputs {
            // SAFETY: the caller keeps virtual buffers valid until the used
            // entry is popped.
            unsafe {
                indirect_list[index].set_buf::<H>(
                    (*output).into(),
                    BufferDirection::DeviceToDriver,
                    DescFlags::NEXT,
                );
            }
            index += 1;
        }
        // Physical descriptors do not pass through `set_buf`, so their
        // `next` fields were zeroed by allocation. Build the complete table
        // links explicitly before publishing the indirect head.
        for (entry, next) in indirect_list
            .iter_mut()
            .zip((1..descriptors_needed).map(|next| next as u16))
        {
            entry.next = next;
        }
        indirect_list[descriptors_needed - 1].next = 0;
        indirect_list[descriptors_needed - 1]
            .flags
            .remove(DescFlags::NEXT);

        assert!(self.indirect_lists[usize::from(head)].is_none());
        self.indirect_lists[usize::from(head)] = Some(indirect_list.as_mut().into());
        let direct_desc = &mut self.desc_shadow[usize::from(head)];
        self.free_head = direct_desc.next;
        // SAFETY: the indirect list remains owned by the queue until the
        // matching pop/discard path recycles it.
        unsafe {
            direct_desc.set_buf::<H>(
                Box::leak(indirect_list).as_bytes().into(),
                BufferDirection::DriverToDevice,
                DescFlags::INDIRECT,
            );
        }
        self.write_desc(head);
        self.num_used = self
            .num_used
            .checked_add(1)
            .expect("virtqueue descriptor count overflow");
        Ok(head)
    }

    #[cfg(feature = "alloc")]
    fn install_indirect_physical_descriptor(
        descriptor: &mut Descriptor,
        buffer: PhysicalBuffer,
        device_writes: bool,
    ) {
        descriptor.addr = buffer.addr as u64;
        descriptor.len = buffer.len as u32;
        descriptor.flags = DescFlags::NEXT
            | if device_writes {
                DescFlags::WRITE
            } else {
                DescFlags::empty()
            };
    }

    #[cfg(feature = "alloc")]
    fn try_new_box_slice_zeroed(len: usize) -> Result<Box<[Descriptor]>> {
        let mut descriptors = Vec::new();
        descriptors.try_reserve(len).map_err(|_| Error::DmaError)?;
        descriptors.resize_with(len, Descriptor::new_zeroed);
        Ok(descriptors.into_boxed_slice())
    }

    fn install_physical_descriptor(
        &mut self,
        index: u16,
        buffer: PhysicalBuffer,
        device_writes: bool,
    ) {
        debug_assert!(buffer.addr != 0 && buffer.len != 0 && buffer.len <= u32::MAX as usize);
        let descriptor = &mut self.desc_shadow[usize::from(index)];
        descriptor.addr = buffer.addr as u64;
        descriptor.len = buffer.len as u32;
        descriptor.flags = DescFlags::NEXT
            | if device_writes {
                DescFlags::WRITE
            } else {
                DescFlags::empty()
            };
        self.write_desc(index);
    }

    /// Publishes a descriptor chain previously returned by
    /// [`add_unpublished`](Self::add_unpublished) to the available ring.
    pub fn publish_unpublished(&mut self, head: u16) {
        let avail_slot = self.avail_idx & (SIZE as u16 - 1);
        // Safe because self.avail is properly aligned, dereferenceable and initialised.
        unsafe {
            (*self.avail.as_ptr()).ring[avail_slot as usize] = head;
        }

        // Write barrier so that device sees changes to descriptor table and available ring before
        // change to available index.
        dma_sync_barrier();
        fence(Ordering::SeqCst);

        // increase head of avail ring
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // Safe because self.avail is properly aligned, dereferenceable and initialised.
        unsafe {
            (*self.avail.as_ptr())
                .idx
                .store(self.avail_idx, Ordering::Release);
        }

        // A queue may continue to be drained/refilled while callbacks are
        // disabled. Extend the EVENT_IDX holdoff after publishing so the
        // newly admitted descriptor cannot re-enable used interrupts.
        if self.event_idx && !self.dev_notify_enabled {
            self.refresh_used_event_suppression();
        }
    }

    fn add_direct<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> u16 {
        // allocate descriptors from free list
        let head = self.free_head;
        let mut last = self.free_head;

        for (buffer, direction) in InputOutputIter::new(inputs, outputs) {
            assert_ne!(buffer.len(), 0);

            // Write to desc_shadow then copy.
            let desc = &mut self.desc_shadow[usize::from(self.free_head)];
            // Safe because our caller promises that the buffers live at least until `pop_used`
            // returns them.
            unsafe {
                desc.set_buf::<H>(buffer, direction, DescFlags::NEXT);
            }
            last = self.free_head;
            self.free_head = desc.next;

            self.write_desc(last);
        }

        // set last_elem.next = NULL
        self.desc_shadow[usize::from(last)]
            .flags
            .remove(DescFlags::NEXT);
        self.write_desc(last);

        self.num_used = self
            .num_used
            .checked_add((inputs.len() + outputs.len()) as u16)
            .expect("virtqueue descriptor count overflow");

        head
    }

    #[cfg(feature = "alloc")]
    fn add_indirect<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> u16 {
        let head = self.free_head;

        // Allocate and fill in indirect descriptor list.
        let mut indirect_list = Descriptor::new_box_slice_zeroed(inputs.len() + outputs.len());
        for (i, (buffer, direction)) in InputOutputIter::new(inputs, outputs).enumerate() {
            let desc = &mut indirect_list[i];
            // Safe because our caller promises that the buffers live at least until `pop_used`
            // returns them.
            unsafe {
                desc.set_buf::<H>(buffer, direction, DescFlags::NEXT);
            }
            desc.next = (i + 1) as u16;
        }
        indirect_list
            .last_mut()
            .unwrap()
            .flags
            .remove(DescFlags::NEXT);

        // Need to store pointer to indirect_list too, because direct_desc.set_buf will only store
        // the physical DMA address which might be different.
        assert!(self.indirect_lists[usize::from(head)].is_none());
        self.indirect_lists[usize::from(head)] = Some(indirect_list.as_mut().into());

        // Write a descriptor pointing to indirect descriptor list. We use Box::leak to prevent the
        // indirect list from being freed when this function returns; recycle_descriptors is instead
        // responsible for freeing the memory after the buffer chain is popped.
        let direct_desc = &mut self.desc_shadow[usize::from(head)];
        self.free_head = direct_desc.next;
        unsafe {
            direct_desc.set_buf::<H>(
                Box::leak(indirect_list).as_bytes().into(),
                BufferDirection::DriverToDevice,
                DescFlags::INDIRECT,
            );
        }
        self.write_desc(head);
        self.num_used = self
            .num_used
            .checked_add(1)
            .expect("virtqueue descriptor count overflow");

        head
    }

    /// Add the given buffers to the virtqueue, notifies the device, blocks until the device uses
    /// them, then pops them.
    ///
    /// This assumes that the device isn't processing any other buffers at the same time.
    ///
    /// The buffers must not be empty.
    pub fn add_notify_wait_pop<'a>(
        &mut self,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
        transport: &mut impl Transport,
    ) -> Result<u32> {
        // Safe because we don't return until the same token has been popped, so the buffers remain
        // valid and are not otherwise accessed until then.
        let token = unsafe { self.add(inputs, outputs) }?;
        let count_io_stats = io_counters_enabled();
        let mut notified = false;

        // Notify the queue.
        if self.should_notify() {
            dma_sync_barrier();
            transport.notify(self.queue_idx);
            notified = true;
        }

        // Wait until there is at least one element in the used ring.
        if count_io_stats {
            let mut wait_polls = 0u64;
            while !self.can_pop() {
                wait_polls = wait_polls.saturating_add(1);
                spin_loop();
            }
            record_queue_sync_wait(wait_polls, notified);
        } else {
            while !self.can_pop() {
                spin_loop();
            }
        }

        // Safe because these are the same buffers as we passed to `add` above and they are still
        // valid.
        unsafe { self.pop_used(token, inputs, outputs) }
    }

    /// Advise the device whether used buffer notifications are needed.
    ///
    /// See Virtio v1.1 2.6.7 Used Buffer Notification Suppression
    pub fn set_dev_notify(&mut self, enable: bool) {
        let avail_ring_flags = if enable { 0x0000 } else { 0x0001 };
        self.dev_notify_enabled = enable;
        if self.event_idx {
            // The device applies `vring_need_event(used_event, new, old)`;
            // an interrupt is requested only once `new` advances *past*
            // `used_event`.  Set the event at the furthest used index that
            // the currently outstanding chains can reach. This suppresses
            // every completion the device can produce without a new driver
            // submission, including a u16 wrap. `publish_unpublished` and
            // the pop paths refresh this bound while notifications remain
            // disabled.
            let used_event = if enable {
                self.last_used_idx
            } else {
                self.suppressed_used_event()
            };
            // Safe because self.avail points to a valid, aligned,
            // initialised, dereferenceable instance of AvailRing.
            unsafe {
                (*self.avail.as_ptr())
                    .used_event
                    .store(used_event, Ordering::Release)
            }
        } else {
            // Safe because self.avail points to a valid, aligned, initialised, dereferenceable, readable
            // instance of AvailRing.
            unsafe {
                (*self.avail.as_ptr())
                    .flags
                    .store(avail_ring_flags, Ordering::Release)
            }
        }
        if enable {
            // Publish the rearm before the caller checks used.idx again.
            // Release/Acquire alone permits StoreLoad reordering on x86:
            // the device can still see suppression while our check sees
            // no completion, leaving both sides waiting. This is the
            // virtio_mb between Linux enable_cb_prepare and virtqueue_poll.
            fence(Ordering::SeqCst);
        }
    }

    /// Compute the next used-event threshold while callbacks are disabled.
    ///
    /// A split virtqueue can expose at most `num_used` additional used
    /// entries before the driver must reclaim or stop submitting descriptors.
    /// The event index is therefore placed at that upper bound (or one entry
    /// ahead for an empty queue); virtio's wrapping arithmetic makes this
    /// correct when the 16-bit used index crosses zero.
    #[inline]
    fn suppressed_used_event(&self) -> u16 {
        let used_idx = unsafe { (*self.used.as_ptr()).idx.load(Ordering::Acquire) };
        used_idx.wrapping_add(self.num_used.max(1))
    }

    #[inline]
    fn refresh_used_event_suppression(&self) {
        debug_assert!(self.event_idx);
        let used_event = self.suppressed_used_event();
        // Safe because self.avail points to a valid, aligned, initialised,
        // dereferenceable instance of AvailRing.
        unsafe {
            (*self.avail.as_ptr())
                .used_event
                .store(used_event, Ordering::Release)
        }
    }

    /// Returns whether the driver should notify the device after adding a new buffer to the
    /// virtqueue.
    ///
    /// This will be false if the device has supressed notifications.
    pub fn should_notify(&mut self) -> bool {
        let old = self.last_kick_avail_idx;
        self.last_kick_avail_idx = self.avail_idx;
        // StoreLoad ordering is required between publishing avail.idx and
        // reading device suppression: a Release store and Acquire load alone
        // can miss the device re-enabling kicks, even on x86. This is the
        // virtio_mb in Linux virtqueue_kick_prepare_split.
        fence(Ordering::SeqCst);
        if self.event_idx {
            // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
            // instance of UsedRing.
            let avail_event = unsafe { (*self.used.as_ptr()).avail_event.load(Ordering::Acquire) };
            self.avail_idx.wrapping_sub(avail_event).wrapping_sub(1)
                < self.avail_idx.wrapping_sub(old)
        } else {
            // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
            // instance of UsedRing.
            unsafe { (*self.used.as_ptr()).flags.load(Ordering::Acquire) & 0x0001 == 0 }
        }
    }

    /// Copies the descriptor at the given index from `desc_shadow` to `desc`, so it can be seen by
    /// the device.
    fn write_desc(&mut self, index: u16) {
        let index = usize::from(index);
        // Safe because self.desc is properly aligned, dereferenceable and initialised, and nothing
        // else reads or writes the descriptor during this block.
        unsafe {
            (*self.desc.as_ptr())[index] = self.desc_shadow[index].clone();
        }
    }

    /// Returns whether there is a used element that can be popped.
    pub fn can_pop(&self) -> bool {
        // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
        // instance of UsedRing.
        dma_sync_barrier();
        self.last_used_idx != unsafe { (*self.used.as_ptr()).idx.load(Ordering::Acquire) }
    }

    #[cfg(test)]
    pub(crate) fn set_used_for_test(&mut self, slot: usize, id: u32, len: u32, idx: u16) {
        assert!(slot < SIZE);
        // Safe because tests exclusively own the fake device's used ring.
        unsafe {
            (*self.used.as_ptr()).ring[slot] = UsedElem { id, len };
            (*self.used.as_ptr()).idx.store(idx, Ordering::Release);
        }
    }

    /// Returns whether this queue has no descriptor chains awaiting device
    /// completion or driver-side reaping.
    ///
    /// This intentionally checks the driver's descriptor accounting rather
    /// than [`Self::available_desc`], whose indirect-descriptor mode can hide
    /// outstanding chains behind one table descriptor.
    pub(crate) fn is_empty(&self) -> bool {
        self.num_used == 0
    }

    /// Returns the number of descriptor-table entries currently owned by the
    /// device or awaiting driver-side reaping. In indirect mode this is one
    /// entry per indirect chain.
    pub(crate) fn outstanding_descriptor_count(&self) -> usize {
        usize::from(self.num_used)
    }

    /// Returns whether an indirect descriptor allocation is still owned by the
    /// queue.  The descriptor count alone is not sufficient after a failed
    /// reset/rollback: an indirect table remains a DMA owner until its exact
    /// chain has been recycled.
    pub(crate) fn has_live_indirect_lists(&self) -> bool {
        #[cfg(feature = "alloc")]
        {
            self.indirect_lists.iter().any(Option::is_some)
        }
        #[cfg(not(feature = "alloc"))]
        {
            false
        }
    }

    /// Returns the descriptor index (a.k.a. token) of the next used element without popping it, or
    /// `None` if the used ring is empty.
    pub fn peek_used(&self) -> Option<u16> {
        if self.can_pop() {
            let last_used_slot = self.last_used_idx & (SIZE as u16 - 1);
            // Safe because self.used points to a valid, aligned, initialised, dereferenceable,
            // readable instance of UsedRing.
            Some(unsafe { (*self.used.as_ptr()).ring[last_used_slot as usize].id as u16 })
        } else {
            None
        }
    }

    /// Returns the number of free descriptors.
    pub fn available_desc(&self) -> usize {
        #[cfg(feature = "alloc")]
        if self.indirect {
            return if usize::from(self.num_used) == SIZE {
                0
            } else {
                SIZE
            };
        }

        SIZE - usize::from(self.num_used)
    }

    /// Unshares buffers in the list starting at descriptor index `head` and adds them to the free
    /// list. Unsharing may involve copying data back to the original buffers, so they must be
    /// passed in too.
    ///
    /// This will push all linked descriptors at the front of the free list.
    ///
    /// # Safety
    ///
    /// The buffers in `inputs` and `outputs` must match the set of buffers originally added to the
    /// queue by `add`.
    unsafe fn recycle_descriptors<'a, 'b>(
        &mut self,
        head: u16,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) {
        let original_free_head = self.free_head;
        self.free_head = head;

        let head_desc = &mut self.desc_shadow[usize::from(head)];
        if head_desc.flags.contains(DescFlags::INDIRECT) {
            #[cfg(feature = "alloc")]
            {
                // Find the indirect descriptor list, unshare it and move its descriptor to the free
                // list.
                let indirect_list = self.indirect_lists[usize::from(head)].take().unwrap();
                // SAFETY: We allocated the indirect list in `add_indirect`, and the device has
                // finished accessing it by this point.
                let mut indirect_list = unsafe { Box::from_raw(indirect_list.as_ptr()) };
                let paddr = head_desc.addr;
                head_desc.unset_buf();
                self.num_used -= 1;
                head_desc.next = original_free_head;

                unsafe {
                    H::unshare(
                        paddr as usize,
                        indirect_list.as_bytes_mut().into(),
                        BufferDirection::DriverToDevice,
                    );
                }

                // Unshare the buffers in the indirect descriptor list, and free it.
                assert_eq!(indirect_list.len(), inputs.len() + outputs.len());
                for (i, (buffer, direction)) in InputOutputIter::new(inputs, outputs).enumerate() {
                    assert_ne!(buffer.len(), 0);

                    // SAFETY: The caller ensures that the buffer is valid and matches the
                    // descriptor from which we got `paddr`.
                    unsafe {
                        // Unshare the buffer (and perhaps copy its contents back to the original
                        // buffer).
                        H::unshare(indirect_list[i].addr as usize, buffer, direction);
                    }
                }
                drop(indirect_list);
            }
        } else {
            let mut next = Some(head);

            for (buffer, direction) in InputOutputIter::new(inputs, outputs) {
                assert_ne!(buffer.len(), 0);

                let desc_index = next.expect("Descriptor chain was shorter than expected.");
                let desc = &mut self.desc_shadow[usize::from(desc_index)];

                let paddr = desc.addr;
                desc.unset_buf();
                self.num_used -= 1;
                next = desc.next();
                if next.is_none() {
                    desc.next = original_free_head;
                }

                self.write_desc(desc_index);

                // SAFETY: The caller ensures that the buffer is valid and matches the descriptor
                // from which we got `paddr`.
                unsafe {
                    // Unshare the buffer (and perhaps copy its contents back to the original buffer).
                    H::unshare(paddr as usize, buffer, direction);
                }
            }

            if next.is_some() {
                panic!("Descriptor chain was longer than expected.");
            }
        }
    }

    /// Recycles a direct chain containing virtual header/status buffers and
    /// mapped physical payload buffers. Physical payloads are deliberately
    /// not passed to [`Hal::unshare`]; their owner performs the matching
    /// physical unmap after this method returns.
    unsafe fn recycle_physical_descriptors<'a, 'b>(
        &mut self,
        head: u16,
        inputs: &'a [&'b [u8]],
        physical_inputs: &[PhysicalBuffer],
        physical_outputs: &[PhysicalBuffer],
        outputs: &'a mut [&'b mut [u8]],
    ) {
        let original_free_head = self.free_head;
        self.free_head = head;

        #[cfg(feature = "alloc")]
        if self.desc_shadow[usize::from(head)]
            .flags
            .contains(DescFlags::INDIRECT)
        {
            let head_desc = &mut self.desc_shadow[usize::from(head)];
            let indirect_list = self.indirect_lists[usize::from(head)]
                .take()
                .expect("missing physical indirect descriptor list");
            // SAFETY: the device has consumed the indirect list because the
            // used entry is being reaped (or the caller explicitly discarded
            // an unpublished chain).
            let mut indirect_list = unsafe { Box::from_raw(indirect_list.as_ptr()) };
            let paddr = head_desc.addr;
            head_desc.unset_buf();
            self.num_used -= 1;
            head_desc.next = original_free_head;
            unsafe {
                H::unshare(
                    paddr as usize,
                    indirect_list.as_bytes_mut().into(),
                    BufferDirection::DriverToDevice,
                );
            }

            let expected =
                inputs.len() + physical_inputs.len() + physical_outputs.len() + outputs.len();
            assert_eq!(indirect_list.len(), expected);
            let mut index = 0usize;
            for input in inputs {
                unsafe {
                    H::unshare(
                        indirect_list[index].addr as usize,
                        (*input).into(),
                        BufferDirection::DriverToDevice,
                    );
                }
                index += 1;
            }
            index += physical_inputs.len() + physical_outputs.len();
            for output in outputs {
                unsafe {
                    H::unshare(
                        indirect_list[index].addr as usize,
                        (*output).into(),
                        BufferDirection::DeviceToDriver,
                    );
                }
                index += 1;
            }
            self.write_desc(head);
            return;
        }

        let mut next = Some(head);

        for input in inputs {
            // SAFETY: The descriptor chain was built from these exact
            // buffers and the device has completed it.
            unsafe {
                self.recycle_one_physical_descriptor(
                    &mut next,
                    original_free_head,
                    Some(((*input).into(), BufferDirection::DriverToDevice)),
                );
            }
        }
        for _ in physical_inputs {
            // SAFETY: The physical mapping remains owned by the caller and is
            // unmapped after the queue has released the descriptor.
            unsafe {
                self.recycle_one_physical_descriptor(&mut next, original_free_head, None);
            }
        }
        for _ in physical_outputs {
            // SAFETY: The physical mapping remains owned by the caller and is
            // unmapped after the queue has released the descriptor.
            unsafe {
                self.recycle_one_physical_descriptor(&mut next, original_free_head, None);
            }
        }
        for output in outputs {
            // SAFETY: The descriptor chain was built from these exact
            // buffers and the device has completed it.
            unsafe {
                self.recycle_one_physical_descriptor(
                    &mut next,
                    original_free_head,
                    Some(((*output).into(), BufferDirection::DeviceToDriver)),
                );
            }
        }
        assert!(next.is_none(), "Descriptor chain was longer than expected.");
    }

    /// Discards a descriptor chain which was prepared but never published.
    /// No used-ring index is advanced; all queue-owned virtual and indirect
    /// mappings are released and the caller may then release physical DMA
    /// mappings.
    pub(crate) unsafe fn discard_unpublished_physical<'a, 'b>(
        &mut self,
        head: u16,
        inputs: &'a [&'b [u8]],
        physical_inputs: &[PhysicalBuffer],
        physical_outputs: &[PhysicalBuffer],
        outputs: &'a mut [&'b mut [u8]],
    ) {
        // SAFETY: the caller proves this chain was installed by
        // add_unpublished_physical and is not visible to the device.
        unsafe {
            self.recycle_physical_descriptors(
                head,
                inputs,
                physical_inputs,
                physical_outputs,
                outputs,
            );
        }
    }

    /// Discards a normal virtual-buffer chain which was prepared but never
    /// published.  This is the rollback counterpart to [`Self::add_unpublished`]
    /// and deliberately does not advance the used ring.
    pub(crate) unsafe fn discard_unpublished<'a, 'b>(
        &mut self,
        head: u16,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) {
        // SAFETY: the caller proves this chain was installed by
        // add_unpublished and has not been exposed to the device.
        unsafe { self.recycle_descriptors(head, inputs, outputs) };
    }

    /// Recycles a published descriptor chain after the transport has already
    /// proved that the device is quiescent.  No used-ring index is consumed;
    /// this is the reset/teardown counterpart to [`Self::pop_used`].
    pub(crate) unsafe fn discard_quiesced<'a, 'b>(
        &mut self,
        head: u16,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) {
        // SAFETY: the caller proved quiescence and supplies the exact buffers
        // used to install this chain.
        unsafe { self.recycle_descriptors(head, inputs, outputs) };
    }

    unsafe fn recycle_one_physical_descriptor(
        &mut self,
        next: &mut Option<u16>,
        original_free_head: u16,
        virtual_buffer: Option<(NonNull<[u8]>, BufferDirection)>,
    ) {
        let desc_index = next
            .take()
            .expect("Descriptor chain was shorter than expected.");
        let desc = &mut self.desc_shadow[usize::from(desc_index)];
        let paddr = desc.addr as usize;
        let following = desc.next();
        desc.unset_buf();
        self.num_used -= 1;
        *next = following;
        if following.is_none() {
            desc.next = original_free_head;
        }
        self.write_desc(desc_index);
        if let Some((buffer, direction)) = virtual_buffer {
            // SAFETY: The caller supplied the exact virtual buffer used when
            // the descriptor was installed, and the device is quiescent.
            unsafe { H::unshare(paddr, buffer, direction) };
        }
    }

    /// If the given token is next on the device used queue, pops it and returns the total buffer
    /// length which was used (written) by the device.
    ///
    /// Ref: linux virtio_ring.c virtqueue_get_buf_ctx
    ///
    /// # Safety
    ///
    /// The buffers in `inputs` and `outputs` must match the set of buffers originally added to the
    /// queue by `add` when it returned the token being passed in here.
    pub unsafe fn pop_used<'a>(
        &mut self,
        token: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> Result<u32> {
        if !self.can_pop() {
            return Err(Error::NotReady);
        }

        // Get the index of the start of the descriptor chain for the next element in the used ring.
        let last_used_slot = self.last_used_idx & (SIZE as u16 - 1);
        let index;
        let len;
        // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
        // instance of UsedRing.
        dma_sync_barrier();
        unsafe {
            let raw_index = (*self.used.as_ptr()).ring[last_used_slot as usize].id;
            if raw_index > u32::from(u16::MAX) || raw_index as usize >= SIZE {
                return Err(Error::WrongToken);
            }
            index = raw_index as u16;
            len = (*self.used.as_ptr()).ring[last_used_slot as usize].len;
        }

        if index != token {
            // The device used a different descriptor chain to the one we were expecting.
            return Err(Error::WrongToken);
        }

        // Safe because the caller ensures the buffers are valid and match the descriptor.
        unsafe {
            self.recycle_descriptors(index, inputs, outputs);
        }
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        if self.event_idx {
            if self.dev_notify_enabled {
                unsafe {
                    (*self.avail.as_ptr())
                        .used_event
                        .store(self.last_used_idx, Ordering::Release);
                }
                // Flush the new event threshold before the next used-ring
                // check, matching Linux's virtio_store_mb on get_buf.
                fence(Ordering::SeqCst);
            } else {
                self.refresh_used_event_suppression();
            }
        }

        Ok(len)
    }

    /// Pops a completed direct chain created by
    /// [`Self::add_unpublished_physical`].
    ///
    /// # Safety
    ///
    /// The buffers and physical descriptors must exactly match the submitted
    /// chain. The physical mappings must remain active until this succeeds.
    /// Physical inputs are device-readable and physical outputs are
    /// device-writable; concurrent CPU/device access races on contents.
    pub(crate) unsafe fn pop_used_physical<'a, 'b>(
        &mut self,
        token: u16,
        inputs: &'a [&'b [u8]],
        physical_inputs: &[PhysicalBuffer],
        physical_outputs: &[PhysicalBuffer],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u32> {
        if !self.can_pop() {
            return Err(Error::NotReady);
        }

        let last_used_slot = self.last_used_idx & (SIZE as u16 - 1);
        let (index, len) = unsafe {
            let used = &*self.used.as_ptr();
            let raw_index = used.ring[last_used_slot as usize].id;
            if raw_index > u32::from(u16::MAX) || raw_index as usize >= SIZE {
                return Err(Error::WrongToken);
            }
            (raw_index as u16, used.ring[last_used_slot as usize].len)
        };
        if index != token {
            return Err(Error::WrongToken);
        }

        // SAFETY: The caller guarantees these are the exact submitted
        // buffers and all device access has completed.
        unsafe {
            self.recycle_physical_descriptors(
                index,
                inputs,
                physical_inputs,
                physical_outputs,
                outputs,
            );
        }
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        if self.event_idx {
            if self.dev_notify_enabled {
                unsafe {
                    (*self.avail.as_ptr())
                        .used_event
                        .store(self.last_used_idx, Ordering::Release);
                }
                // Physical chains use the same event-index wake protocol.
                fence(Ordering::SeqCst);
            } else {
                self.refresh_used_event_suppression();
            }
        }
        Ok(len)
    }
}

// SAFETY: None of the virt queue resources are tied to a particular thread.
unsafe impl<H: Hal, const SIZE: usize> Send for VirtQueue<H, SIZE> {}

// SAFETY: A `&VirtQueue` only allows reading from the various pointers it contains, so there is no
// data race.
unsafe impl<H: Hal, const SIZE: usize> Sync for VirtQueue<H, SIZE> {}

/// The inner layout of a VirtQueue.
///
/// Ref: 2.6 Split Virtqueues
#[derive(Debug)]
enum VirtQueueLayout<H: Hal> {
    Legacy {
        dma: Dma<H>,
        avail_offset: usize,
        used_offset: usize,
    },
    Modern {
        /// The region used for the descriptor area and driver area.
        driver_to_device_dma: Dma<H>,
        /// The region used for the device area.
        device_to_driver_dma: Dma<H>,
        /// The offset from the start of the `driver_to_device_dma` region to the driver area
        /// (available ring).
        avail_offset: usize,
    },
}

impl<H: Hal> VirtQueueLayout<H> {
    /// Allocates a single DMA region containing all parts of the virtqueue, following the layout
    /// required by legacy interfaces.
    ///
    /// Ref: 2.6.2 Legacy Interfaces: A Note on Virtqueue Layout
    fn allocate_legacy(queue_size: u16) -> Result<Self> {
        let (desc, avail, used) = queue_part_sizes(queue_size);
        let size = align_up(desc + avail) + align_up(used);
        // Allocate contiguous pages.
        let dma = Dma::new(size / PAGE_SIZE, BufferDirection::Both)?;
        Ok(Self::Legacy {
            dma,
            avail_offset: desc,
            used_offset: align_up(desc + avail),
        })
    }

    /// Allocates separate DMA regions for the the different parts of the virtqueue, as supported by
    /// non-legacy interfaces.
    ///
    /// This is preferred over `allocate_legacy` where possible as it reduces memory fragmentation
    /// and allows the HAL to know which DMA regions are used in which direction.
    fn allocate_flexible(queue_size: u16) -> Result<Self> {
        let (desc, avail, used) = queue_part_sizes(queue_size);
        let driver_to_device_dma = Dma::new(pages(desc + avail), BufferDirection::DriverToDevice)?;
        let device_to_driver_dma = Dma::new(pages(used), BufferDirection::DeviceToDriver)?;
        Ok(Self::Modern {
            driver_to_device_dma,
            device_to_driver_dma,
            avail_offset: desc,
        })
    }

    /// Returns the physical address of the descriptor area.
    fn descriptors_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy { dma, .. } => dma.paddr(),
            Self::Modern {
                driver_to_device_dma,
                ..
            } => driver_to_device_dma.paddr(),
        }
    }

    /// Returns a pointer to the descriptor table (in the descriptor area).
    fn descriptors_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy { dma, .. } => dma.vaddr(0),
            Self::Modern {
                driver_to_device_dma,
                ..
            } => driver_to_device_dma.vaddr(0),
        }
    }

    /// Returns the physical address of the driver area.
    fn driver_area_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy {
                dma, avail_offset, ..
            } => dma.paddr() + avail_offset,
            Self::Modern {
                driver_to_device_dma,
                avail_offset,
                ..
            } => driver_to_device_dma.paddr() + avail_offset,
        }
    }

    /// Returns a pointer to the available ring (in the driver area).
    fn avail_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy {
                dma, avail_offset, ..
            } => dma.vaddr(*avail_offset),
            Self::Modern {
                driver_to_device_dma,
                avail_offset,
                ..
            } => driver_to_device_dma.vaddr(*avail_offset),
        }
    }

    /// Returns the physical address of the device area.
    fn device_area_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy {
                used_offset, dma, ..
            } => dma.paddr() + used_offset,
            Self::Modern {
                device_to_driver_dma,
                ..
            } => device_to_driver_dma.paddr(),
        }
    }

    /// Returns a pointer to the used ring (in the driver area).
    fn used_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy {
                dma, used_offset, ..
            } => dma.vaddr(*used_offset),
            Self::Modern {
                device_to_driver_dma,
                ..
            } => device_to_driver_dma.vaddr(0),
        }
    }
}

/// Returns the size in bytes of the descriptor table, available ring and used ring for a given
/// queue size.
///
/// Ref: 2.6 Split Virtqueues
fn queue_part_sizes(queue_size: u16) -> (usize, usize, usize) {
    assert!(
        queue_size.is_power_of_two(),
        "queue size should be a power of 2"
    );
    let queue_size = queue_size as usize;
    let desc = size_of::<Descriptor>() * queue_size;
    let avail = size_of::<u16>() * (3 + queue_size);
    let used = size_of::<u16>() * 3 + size_of::<UsedElem>() * queue_size;
    (desc, avail, used)
}

#[repr(C, align(16))]
#[derive(AsBytes, Clone, Debug, FromBytes, FromZeroes)]
pub(crate) struct Descriptor {
    addr: u64,
    len: u32,
    flags: DescFlags,
    next: u16,
}

impl Descriptor {
    /// Sets the buffer address, length and flags, and shares it with the device.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the buffer lives at least as long as the descriptor is active.
    unsafe fn set_buf<H: Hal>(
        &mut self,
        buf: NonNull<[u8]>,
        direction: BufferDirection,
        extra_flags: DescFlags,
    ) {
        // Safe because our caller promises that the buffer is valid.
        unsafe {
            self.addr = H::share(buf, direction) as u64;
        }
        self.len = buf.len().try_into().unwrap();
        self.flags = extra_flags
            | match direction {
                BufferDirection::DeviceToDriver => DescFlags::WRITE,
                BufferDirection::DriverToDevice => DescFlags::empty(),
                BufferDirection::Both => {
                    panic!("Buffer passed to device should never use BufferDirection::Both.")
                }
            };
    }

    /// Sets the buffer address and length to 0.
    ///
    /// This must only be called once the device has finished using the descriptor.
    fn unset_buf(&mut self) {
        self.addr = 0;
        self.len = 0;
    }

    /// Returns the index of the next descriptor in the chain if the `NEXT` flag is set, or `None`
    /// if it is not (and thus this descriptor is the end of the chain).
    fn next(&self) -> Option<u16> {
        if self.flags.contains(DescFlags::NEXT) {
            Some(self.next)
        } else {
            None
        }
    }
}

/// Descriptor flags
#[derive(AsBytes, Copy, Clone, Debug, Default, Eq, FromBytes, FromZeroes, PartialEq)]
#[repr(transparent)]
struct DescFlags(u16);

bitflags! {
    impl DescFlags: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
    }
}

/// The driver uses the available ring to offer buffers to the device:
/// each ring entry refers to the head of a descriptor chain.
/// It is only written by the driver and read by the device.
#[repr(C)]
#[derive(Debug)]
struct AvailRing<const SIZE: usize> {
    flags: AtomicU16,
    /// A driver MUST NOT decrement the idx.
    idx: AtomicU16,
    ring: [u16; SIZE],
    /// Only used if `VIRTIO_F_EVENT_IDX` is negotiated.
    used_event: AtomicU16,
}

/// The used ring is where the device returns buffers once it is done with them:
/// it is only written to by the device, and read by the driver.
#[repr(C)]
#[derive(Debug)]
struct UsedRing<const SIZE: usize> {
    flags: AtomicU16,
    idx: AtomicU16,
    ring: [UsedElem; SIZE],
    /// Only used if `VIRTIO_F_EVENT_IDX` is negotiated.
    avail_event: AtomicU16,
}

#[repr(C)]
#[derive(Debug)]
struct UsedElem {
    id: u32,
    len: u32,
}

struct InputOutputIter<'a, 'b> {
    inputs: &'a [&'b [u8]],
    outputs: &'a mut [&'b mut [u8]],
}

impl<'a, 'b> InputOutputIter<'a, 'b> {
    fn new(inputs: &'a [&'b [u8]], outputs: &'a mut [&'b mut [u8]]) -> Self {
        Self { inputs, outputs }
    }
}

impl<'a, 'b> Iterator for InputOutputIter<'a, 'b> {
    type Item = (NonNull<[u8]>, BufferDirection);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(input) = take_first(&mut self.inputs) {
            Some(((*input).into(), BufferDirection::DriverToDevice))
        } else {
            let output = take_first_mut(&mut self.outputs)?;
            Some(((*output).into(), BufferDirection::DeviceToDriver))
        }
    }
}

// TODO: Use `slice::take_first` once it is stable
// (https://github.com/rust-lang/rust/issues/62280).
fn take_first<'a, T>(slice: &mut &'a [T]) -> Option<&'a T> {
    let (first, rem) = slice.split_first()?;
    *slice = rem;
    Some(first)
}

// TODO: Use `slice::take_first_mut` once it is stable
// (https://github.com/rust-lang/rust/issues/62280).
fn take_first_mut<'a, T>(slice: &mut &'a mut [T]) -> Option<&'a mut T> {
    let (first, rem) = take(slice).split_first_mut()?;
    *slice = rem;
    Some(first)
}

/// Simulates the device reading from a VirtIO queue and writing a response back, for use in tests.
///
/// The fake device always uses descriptors in order.
///
/// Returns true if a descriptor chain was available and processed, or false if no descriptors were
/// available.
#[cfg(test)]
pub(crate) fn fake_read_write_queue<const QUEUE_SIZE: usize>(
    descriptors: *const [Descriptor; QUEUE_SIZE],
    queue_driver_area: *const u8,
    queue_device_area: *mut u8,
    handler: impl FnOnce(Vec<u8>) -> Vec<u8>,
) -> bool {
    use core::{ops::Deref, slice};

    let available_ring = queue_driver_area as *const AvailRing<QUEUE_SIZE>;
    let used_ring = queue_device_area as *mut UsedRing<QUEUE_SIZE>;

    // Safe because the various pointers are properly aligned, dereferenceable, initialised, and
    // nothing else accesses them during this block.
    unsafe {
        // Make sure there is actually at least one descriptor available to read from.
        if (*available_ring).idx.load(Ordering::Acquire) == (*used_ring).idx.load(Ordering::Acquire)
        {
            return false;
        }
        // The fake device always uses descriptors in order, like VIRTIO_F_IN_ORDER, so
        // `used_ring.idx` marks the next descriptor we should take from the available ring.
        let next_slot = (*used_ring).idx.load(Ordering::Acquire) & (QUEUE_SIZE as u16 - 1);
        let head_descriptor_index = (*available_ring).ring[next_slot as usize];
        let mut descriptor = &(*descriptors)[head_descriptor_index as usize];

        let output;
        if descriptor.flags.contains(DescFlags::INDIRECT) {
            // The descriptor shouldn't have any other flags if it is indirect.
            assert_eq!(descriptor.flags, DescFlags::INDIRECT);

            // Loop through all input descriptors in the indirect descriptor list, reading data from
            // them.
            let indirect_descriptor_list: &[Descriptor] = zerocopy::Ref::new_slice(
                slice::from_raw_parts(descriptor.addr as *const u8, descriptor.len as usize),
            )
            .unwrap()
            .into_slice();
            let mut input = Vec::new();
            let mut indirect_descriptor_index = 0;
            while indirect_descriptor_index < indirect_descriptor_list.len() {
                let indirect_descriptor = &indirect_descriptor_list[indirect_descriptor_index];
                if indirect_descriptor.flags.contains(DescFlags::WRITE) {
                    break;
                }

                input.extend_from_slice(slice::from_raw_parts(
                    indirect_descriptor.addr as *const u8,
                    indirect_descriptor.len as usize,
                ));

                indirect_descriptor_index += 1;
            }
            // Let the test handle the request.
            output = handler(input);

            // Write the response to the remaining descriptors.
            let mut remaining_output = output.deref();
            while indirect_descriptor_index < indirect_descriptor_list.len() {
                let indirect_descriptor = &indirect_descriptor_list[indirect_descriptor_index];
                assert!(indirect_descriptor.flags.contains(DescFlags::WRITE));

                let length_to_write = min(remaining_output.len(), indirect_descriptor.len as usize);
                ptr::copy(
                    remaining_output.as_ptr(),
                    indirect_descriptor.addr as *mut u8,
                    length_to_write,
                );
                remaining_output = &remaining_output[length_to_write..];

                indirect_descriptor_index += 1;
            }
            assert_eq!(remaining_output.len(), 0);
        } else {
            // Loop through all input descriptors in the chain, reading data from them.
            let mut input = Vec::new();
            while !descriptor.flags.contains(DescFlags::WRITE) {
                input.extend_from_slice(slice::from_raw_parts(
                    descriptor.addr as *const u8,
                    descriptor.len as usize,
                ));

                if let Some(next) = descriptor.next() {
                    descriptor = &(*descriptors)[next as usize];
                } else {
                    break;
                }
            }
            // Let the test handle the request.
            output = handler(input);

            // Write the response to the remaining descriptors.
            let mut remaining_output = output.deref();
            if descriptor.flags.contains(DescFlags::WRITE) {
                loop {
                    assert!(descriptor.flags.contains(DescFlags::WRITE));

                    let length_to_write = min(remaining_output.len(), descriptor.len as usize);
                    ptr::copy(
                        remaining_output.as_ptr(),
                        descriptor.addr as *mut u8,
                        length_to_write,
                    );
                    remaining_output = &remaining_output[length_to_write..];

                    if let Some(next) = descriptor.next() {
                        descriptor = &(*descriptors)[next as usize];
                    } else {
                        break;
                    }
                }
            }
            assert_eq!(remaining_output.len(), 0);
        }

        // Mark the buffer as used.
        (*used_ring).ring[next_slot as usize].id = head_descriptor_index.into();
        // VirtIO's used length counts bytes written through device-writable
        // descriptors, not bytes consumed from driver-readable descriptors.
        (*used_ring).ring[next_slot as usize].len = output.len() as u32;
        (*used_ring).idx.fetch_add(1, Ordering::AcqRel);

        true
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        device::common::Feature,
        hal::fake::FakeHal,
        transport::{
            DeviceType,
            fake::{FakeTransport, QueueStatus, State},
            mmio::{MODERN_VERSION, MmioTransport, VirtIOHeader},
        },
    };

    #[test]
    fn queue_too_big() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        assert_eq!(
            VirtQueue::<FakeHal, 8>::new(&mut transport, 0, false, false).unwrap_err(),
            Error::InvalidParam
        );
        // Rejecting an oversized request must leave the queue available, and
        // a request exactly at the advertised maximum remains valid.
        assert!(!transport.queue_used(0));
        let queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert!(transport.queue_used(0));
        drop(queue);
    }

    #[test]
    fn queue_already_used() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap_err(),
            Error::AlreadyUsed
        );
    }

    #[test]
    fn add_empty() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(
            unsafe { queue.add(&[], &mut []) }.unwrap_err(),
            Error::InvalidParam
        );
    }

    #[test]
    fn pop_rejects_used_id_outside_queue() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        let input = [1u8];
        let token = unsafe { queue.add_unpublished(&[&input], &mut []) }.unwrap();
        queue.publish_unpublished(token);
        unsafe {
            (*queue.used.as_ptr()).ring[0].id = 4;
            (*queue.used.as_ptr()).idx.store(1, Ordering::Release);
        }
        assert_eq!(
            unsafe { queue.pop_used(token, &[&input], &mut []) },
            Err(Error::WrongToken)
        );
        assert_eq!(queue.outstanding_descriptor_count(), 1);
    }

    #[test]
    fn add_too_many() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(queue.available_desc(), 4);
        assert_eq!(
            unsafe { queue.add(&[&[], &[], &[]], &mut [&mut [], &mut []]) }.unwrap_err(),
            Error::QueueFull
        );
    }

    #[test]
    fn add_buffers() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(queue.available_desc(), 4);

        // Add a buffer chain consisting of two device-readable parts followed by two
        // device-writable parts.
        let token = unsafe { queue.add(&[&[1, 2], &[3]], &mut [&mut [0, 0], &mut [0]]) }.unwrap();

        assert_eq!(queue.available_desc(), 0);
        assert!(!queue.can_pop());

        // Safe because the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            let first_descriptor_index = (*queue.avail.as_ptr()).ring[0];
            assert_eq!(first_descriptor_index, token);
            assert_eq!(
                (*queue.desc.as_ptr())[first_descriptor_index as usize].len,
                2
            );
            assert_eq!(
                (*queue.desc.as_ptr())[first_descriptor_index as usize].flags,
                DescFlags::NEXT
            );
            let second_descriptor_index =
                (*queue.desc.as_ptr())[first_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[second_descriptor_index as usize].len,
                1
            );
            assert_eq!(
                (*queue.desc.as_ptr())[second_descriptor_index as usize].flags,
                DescFlags::NEXT
            );
            let third_descriptor_index =
                (*queue.desc.as_ptr())[second_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[third_descriptor_index as usize].len,
                2
            );
            assert_eq!(
                (*queue.desc.as_ptr())[third_descriptor_index as usize].flags,
                DescFlags::NEXT | DescFlags::WRITE
            );
            let fourth_descriptor_index =
                (*queue.desc.as_ptr())[third_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[fourth_descriptor_index as usize].len,
                1
            );
            assert_eq!(
                (*queue.desc.as_ptr())[fourth_descriptor_index as usize].flags,
                DescFlags::WRITE
            );
        }
    }

    #[test]
    fn physical_buffers_use_mapped_addresses_and_direction_flags() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        let request = [0u8; 16];
        let mut status = [0u8; 1];
        let read_payload = PhysicalBuffer {
            addr: 0x1234_0000,
            len: 512,
        };
        let write_payload = PhysicalBuffer {
            addr: 0x5678_0000,
            len: 512,
        };

        let token = unsafe {
            queue.add_unpublished_physical(
                &[&request],
                &[read_payload],
                &[write_payload],
                &mut [&mut status],
            )
        }
        .unwrap();
        assert!(!queue.is_empty());

        unsafe {
            let first = &(*queue.desc.as_ptr())[token as usize];
            let second = &(*queue.desc.as_ptr())[first.next as usize];
            let third = &(*queue.desc.as_ptr())[second.next as usize];
            let fourth = &(*queue.desc.as_ptr())[third.next as usize];
            assert_eq!(second.addr, read_payload.addr as u64);
            assert_eq!(third.addr, write_payload.addr as u64);
            assert!(!second.flags.contains(DescFlags::WRITE));
            assert!(third.flags.contains(DescFlags::WRITE));
            assert!(fourth.flags.contains(DescFlags::WRITE));
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn indirect_physical_chain_links_every_entry() {
        use core::ptr::slice_from_raw_parts;

        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 8);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 8>::new(&mut transport, 0, true, false).unwrap();
        let request = [0u8; 16];
        let mut status = [0u8; 1];
        let physical = [
            PhysicalBuffer {
                addr: 0x1000,
                len: 512,
            },
            PhysicalBuffer {
                addr: 0x2000,
                len: 512,
            },
            PhysicalBuffer {
                addr: 0x3000,
                len: 512,
            },
        ];
        let token = unsafe {
            queue.add_unpublished_physical(
                &[&request],
                &physical[..2],
                &physical[2..],
                &mut [&mut status],
            )
        }
        .unwrap();

        unsafe {
            let head = &(*queue.desc.as_ptr())[token as usize];
            let table = slice_from_raw_parts(head.addr as *const Descriptor, 5);
            for index in 0..4 {
                assert!(
                    (*table)[index].flags.contains(DescFlags::NEXT),
                    "entry {index} must link to the next indirect descriptor"
                );
                assert_eq!((*table)[index].next, (index + 1) as u16);
            }
            assert!(!(*table)[4].flags.contains(DescFlags::NEXT));
            assert_eq!((*table)[4].next, 0);
        }

        unsafe {
            queue.discard_unpublished_physical(
                token,
                &[&request],
                &physical[..2],
                &physical[2..],
                &mut [&mut status],
            );
        }
        assert!(queue.is_empty());
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn add_buffers_indirect() {
        use core::ptr::slice_from_raw_parts;

        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, true, false).unwrap();
        assert_eq!(queue.available_desc(), 4);

        // Add a buffer chain consisting of two device-readable parts followed by two
        // device-writable parts.
        let token = unsafe { queue.add(&[&[1, 2], &[3]], &mut [&mut [0, 0], &mut [0]]) }.unwrap();

        assert_eq!(queue.available_desc(), 4);
        assert!(!queue.can_pop());

        // Safe because the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            let indirect_descriptor_index = (*queue.avail.as_ptr()).ring[0];
            assert_eq!(indirect_descriptor_index, token);
            assert_eq!(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].len as usize,
                4 * size_of::<Descriptor>()
            );
            assert_eq!(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].flags,
                DescFlags::INDIRECT
            );

            let indirect_descriptors = slice_from_raw_parts(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].addr
                    as *const Descriptor,
                4,
            );
            assert_eq!((*indirect_descriptors)[0].len, 2);
            assert_eq!((*indirect_descriptors)[0].flags, DescFlags::NEXT);
            assert_eq!((*indirect_descriptors)[0].next, 1);
            assert_eq!((*indirect_descriptors)[1].len, 1);
            assert_eq!((*indirect_descriptors)[1].flags, DescFlags::NEXT);
            assert_eq!((*indirect_descriptors)[1].next, 2);
            assert_eq!((*indirect_descriptors)[2].len, 2);
            assert_eq!(
                (*indirect_descriptors)[2].flags,
                DescFlags::NEXT | DescFlags::WRITE
            );
            assert_eq!((*indirect_descriptors)[2].next, 3);
            assert_eq!((*indirect_descriptors)[3].len, 1);
            assert_eq!((*indirect_descriptors)[3].flags, DescFlags::WRITE);
        }
    }

    /// Tests that the queue advises the device that notifications are needed.
    #[test]
    fn set_dev_notify() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();

        // Check that the avail ring's flag is zero by default.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x0
        );

        queue.set_dev_notify(false);

        // Check that the avail ring's flag is 1 after `disable_dev_notify`.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x1
        );

        queue.set_dev_notify(true);

        // Check that the avail ring's flag is 0 after `enable_dev_notify`.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x0
        );
    }

    /// Tests that the queue notifies the device about added buffers, if it hasn't suppressed
    /// notifications.
    #[test]
    fn add_notify() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();

        // Add a buffer chain with a single device-readable part.
        unsafe { queue.add(&[&[42]], &mut []) }.unwrap();

        // Check that the transport would be notified.
        assert_eq!(queue.should_notify(), true);

        // SAFETY: the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            // Suppress notifications.
            (*queue.used.as_ptr()).flags.store(0x01, Ordering::Release);
        }

        // Check that the transport would not be notified.
        assert_eq!(queue.should_notify(), false);
    }

    /// Tests that the queue notifies the device about added buffers, if it hasn't suppressed
    /// notifications with the `avail_event` index.
    #[test]
    fn add_notify_event_idx() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: Feature::RING_EVENT_IDX.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true).unwrap();

        // Add a buffer chain with a single device-readable part.
        assert_eq!(unsafe { queue.add(&[&[42]], &mut []) }.unwrap(), 0);

        // Check that the transport would be notified.
        assert_eq!(queue.should_notify(), true);

        // SAFETY: the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            // Suppress notifications.
            (*queue.used.as_ptr())
                .avail_event
                .store(1, Ordering::Release);
        }

        // Check that the transport would not be notified.
        assert_eq!(queue.should_notify(), false);

        // Add another buffer chain.
        assert_eq!(unsafe { queue.add(&[&[42]], &mut []) }.unwrap(), 1);

        // Check that the transport should be notified again now.
        assert_eq!(queue.should_notify(), true);
    }

    #[test]
    fn notify_event_idx_tracks_wrapping_publication_batches() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: Feature::RING_EVENT_IDX.bits(),
            config_space: NonNull::from(&mut config_space),
            state,
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true).unwrap();
        // Seed an empty queue just before index wrap, then publish real chains.
        queue.avail_idx = u16::MAX - 1;
        queue.last_kick_avail_idx = u16::MAX - 1;
        unsafe {
            (*queue.used.as_ptr())
                .avail_event
                .store(u16::MAX, Ordering::Release);
            queue.add(&[&[42]], &mut []).unwrap();
        }
        assert!(!queue.should_notify());
        unsafe {
            queue.add(&[&[43]], &mut []).unwrap();
        }
        assert_eq!(queue.avail_idx, 0);
        assert!(queue.should_notify());
        assert!(
            !queue.should_notify(),
            "no new publication must not repeat a kick"
        );
        // One check covers both new chains and an event within their interval.
        unsafe {
            (*queue.used.as_ptr())
                .avail_event
                .store(0, Ordering::Release);
            queue.add(&[&[44]], &mut []).unwrap();
            queue.add(&[&[45]], &mut []).unwrap();
        }
        assert_eq!(queue.avail_idx, 2);
        assert!(queue.should_notify());
        assert!(!queue.should_notify());
    }

    /// Device-side EVENT_IDX suppression uses wrapping u16 arithmetic. A
    /// fixed sentinel (for example, `u16::MAX`) would spuriously interrupt as
    /// the used index crosses zero, while enabling callbacks must point back
    /// at the driver's next expected used entry.
    #[test]
    fn dev_notify_event_idx_suppresses_wrap_and_reenables() {
        fn device_needs_event(event_idx: u16, new_idx: u16, old_idx: u16) -> bool {
            new_idx.wrapping_sub(event_idx).wrapping_sub(1) < new_idx.wrapping_sub(old_idx)
        }

        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: Feature::RING_EVENT_IDX.bits(),
            config_space: NonNull::from(&mut config_space),
            state,
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true).unwrap();

        // Place both sides immediately before the u16 wrap. Suppression must
        // cover the first post-wrap completion, not only ordinary indices.
        unsafe {
            (*queue.used.as_ptr())
                .idx
                .store(u16::MAX, Ordering::Release);
        }
        queue.last_used_idx = u16::MAX;
        queue.set_dev_notify(false);
        let used_event = unsafe { (*queue.avail.as_ptr()).used_event.load(Ordering::Acquire) };
        assert_eq!(used_event, 0);
        assert!(!device_needs_event(used_event, 0, u16::MAX));

        // Re-enabling points the device at the next entry expected by the
        // driver, so a subsequent completion is observable again.
        queue.set_dev_notify(true);
        let used_event = unsafe { (*queue.avail.as_ptr()).used_event.load(Ordering::Acquire) };
        assert_eq!(used_event, u16::MAX);
        assert!(device_needs_event(used_event, 0, u16::MAX));
    }
}
