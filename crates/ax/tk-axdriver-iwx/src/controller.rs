//! Connected PCI-side controller state over the source-derived rings/CSR/firmware code.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

use crate::{
    ApmError, AttachAllocationError, CsrAccess, DeviceFamily, DmaAllocator, DmaError, DmaRegion,
    FirmwareDmaImages, FirmwareImage, HostCommand, InterruptMasks, IwxAttachResources,
    IwxRegisters, RegisterError, allocate_attach_resources, initialize_firmware_sections,
    initialize_init_firmware_sections, load_firmware, post_alive as configure_post_alive,
    send_host_command, start_gen2_context, start_gen3_context,
};

#[derive(Debug, PartialEq, Eq)]
pub enum ControllerError<E> {
    Attach(AttachAllocationError),
    Hardware(ApmError),
    Dma(DmaError),
    Context(crate::ContextError),
    Register(RegisterError),
    Wait(E),
    FirmwareNotAlive,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RxServiceError<E> {
    Notifications(crate::NotificationRingError),
    Ring(crate::RingError),
    Dma(DmaError),
    Command(crate::CommandError),
    Dispatch(E),
    AllocationFailed,
}

/// Owned state shared by firmware commands, interrupt dispatch, and network datapath.
pub struct IwxController<B: CsrAccess, A: DmaAllocator> {
    pub registers: IwxRegisters<B>,
    pub allocator: A,
    pub resources: IwxAttachResources<A::Region>,
    pub family: DeviceFamily,
    pub interrupt_masks: InterruptMasks,
    pub command_slots: crate::CommandSlots,
    pub generation: u32,
    pub hardware_rfkill: bool,
}

impl<B: CsrAccess, A: DmaAllocator> IwxController<B, A> {
    /// Create the CSR/DMA/ring aggregate that the PCI attach path retains.
    // upstream: if_iwx.c iwx_attach()
    pub fn attach(
        bus: B,
        mut allocator: A,
        family: DeviceFamily,
        umac_prph_offset: u32,
        generation: u32,
    ) -> Result<Self, AttachAllocationError> {
        let resources = allocate_attach_resources(&mut allocator, family)?;
        Ok(Self {
            registers: IwxRegisters::new(bus, family, umac_prph_offset),
            allocator,
            resources,
            family,
            interrupt_masks: InterruptMasks::default(),
            command_slots: crate::CommandSlots::new(0, generation),
            generation,
            hardware_rfkill: false,
        })
    }

    /// Start APM/NIC hardware and retain the active-low RF-kill state.
    // upstream: if_iwx.c iwx_start_hw() / iwx_nic_init()
    pub fn start_hardware(&mut self, integrated_22000: bool) -> Result<bool, ApmError> {
        self.hardware_rfkill = crate::start_hardware(
            &mut self.registers,
            &mut self.interrupt_masks,
            integrated_22000,
        )?;
        Ok(self.hardware_rfkill)
    }

    /// Submit host commands through the attached command ring and HBUS doorbell.
    // upstream: if_iwx.c iwx_send_cmd()
    pub fn send_command(
        &mut self,
        command: &HostCommand<'_>,
        external: Option<&mut A::Region>,
    ) -> Result<crate::CommandTicket, crate::CommandError> {
        let Some(command_ring) = self.resources.tx_queues.get_mut(0) else {
            return Err(crate::CommandError::InvalidIndex);
        };
        send_host_command(
            &mut self.registers,
            command_ring,
            &mut self.command_slots,
            self.generation,
            command,
            external,
        )
    }

    /// Drain completed FH RX buffers, retain command replies, and dispatch firmware/data packets.
    // upstream: if_iwx.c iwx_notif_intr() / iwx_rx_pkt()
    pub fn process_rx_notifications<E>(
        &mut self,
        mut dispatch: impl FnMut(&crate::RxPacket<'_>, crate::RxMbufPlan) -> Result<bool, E>,
    ) -> Result<usize, RxServiceError<E>> {
        use core::cell::RefCell;

        let batch = crate::drain_rx_notifications(&mut self.resources.rx_queue, self.family)
            .map_err(RxServiceError::Notifications)?;
        self.registers
            .write_csr(batch.return_register, batch.return_value);
        let completion_count = batch.completions.len();
        for completion in batch.completions {
            if completion.fragmented {
                continue;
            }
            let mut buffer = alloc::vec::Vec::new();
            buffer
                .try_reserve_exact(crate::RX_BUFFER_SIZE)
                .map_err(|_| RxServiceError::AllocationFailed)?;
            buffer.resize(crate::RX_BUFFER_SIZE, 0);
            self.resources
                .rx_queue
                .read_buffer(completion.buffer_id, 0, &mut buffer)
                .map_err(RxServiceError::Ring)?;

            let generation = self.generation;
            let family = self.family;
            let (rx_queue, allocator, command_slots) = (
                &mut self.resources.rx_queue,
                &mut self.allocator,
                &mut self.command_slots,
            );
            let command_slots = RefCell::new(command_slots);
            crate::process_rx_buffer(
                &buffer,
                family >= DeviceFamily::Ax210,
                completion.buffer_id,
                |_, _| false,
                |index| rx_queue.refill_buffer(allocator, index).is_ok(),
                |packet, plan| {
                    if matches!(
                        crate::decode_firmware_event(packet),
                        crate::FirmwareEvent::CommandResponse(_)
                    ) {
                        match command_slots.borrow_mut().receive_response(
                            packet.command_queue_id(),
                            usize::from(packet.index),
                            generation,
                            packet.payload,
                            packet.command_failed,
                        ) {
                            Ok(())
                            | Err(crate::CommandError::NoResponseSlot)
                            | Err(crate::CommandError::InvalidResponse) => {}
                            Err(error) => return Err(RxServiceError::Command(error)),
                        }
                        return Ok(true);
                    }
                    dispatch(packet, plan).map_err(RxServiceError::Dispatch)
                },
                |queue, index, _code| {
                    command_slots
                        .borrow_mut()
                        .command_done(queue, usize::from(index), generation)
                        .map_err(RxServiceError::Command)
                        .map(|_| ())
                },
            )
            .map_err(|error| match error {
                crate::RxBufferError::Dispatch(error)
                | crate::RxBufferError::CommandDone(error) => error,
            })?;
        }
        Ok(completion_count)
    }

    /// Publish Init/regular firmware context and wait for the matching ALIVE event.
    // upstream: if_iwx.c iwx_load_firmware()
    pub fn boot_firmware<E>(
        &mut self,
        firmware: &FirmwareImage,
        init_ucode: bool,
        imr_enabled: bool,
        mut wait_for_alive: impl FnMut(u64) -> Result<bool, E>,
    ) -> Result<(), ControllerError<E>> {
        let mut images = if init_ucode {
            initialize_init_firmware_sections(&mut self.allocator, firmware)
        } else {
            initialize_firmware_sections(&mut self.allocator, firmware)
        }
        .map_err(ControllerError::Dma)?;
        let mut iml_dma = None;
        let init = if init_ucode && self.family >= DeviceFamily::Ax210 {
            let iml = firmware
                .iml
                .as_ref()
                .ok_or(ControllerError::Dma(DmaError::RegionTooSmall))?;
            let mut region = self
                .allocator
                .allocate_aligned(iml.len(), 4096)
                .map_err(ControllerError::Dma)?;
            region.write_at(0, iml).map_err(ControllerError::Dma)?;
            let iml_address = region.device_address();
            let iml_size = u32::try_from(iml.len())
                .map_err(|_| ControllerError::Dma(DmaError::RegionTooSmall))?;
            iml_dma = Some(region);
            Some((iml_address, iml_size))
        } else {
            None
        };
        let family = self.family;
        let queues = crate::ContextQueueAddresses {
            free_rbd: self.resources.rx_queue.free_descriptors.device_address(),
            used_rbd: self.resources.rx_queue.used_descriptors.device_address(),
            rx_status: self.resources.rx_queue.status.device_address(),
            command_queue: self.resources.tx_queues[0].descriptors.device_address(),
        };
        let context_info = &mut self.resources.context_info;
        let mut prph_scratch = self.resources.prph_scratch.as_mut();
        let prph_info = self.resources.prph_info.as_ref();
        let mut initialize_context =
            |family: DeviceFamily, images: &mut FirmwareDmaImages<A::Region>| {
                if family >= DeviceFamily::Ax210 {
                    let scratch =
                        crate::build_gen3_prph_scratch(0, queues.free_rbd, images, imr_enabled)
                            .map_err(ControllerError::Context)?;
                    let scratch_region = prph_scratch
                        .as_deref_mut()
                        .ok_or(ControllerError::Dma(DmaError::RegionTooSmall))?;
                    scratch_region
                        .write_at(0, &scratch)
                        .map_err(ControllerError::Dma)?;
                    let info_region =
                        prph_info.ok_or(ControllerError::Dma(DmaError::RegionTooSmall))?;
                    let context = crate::build_gen3_context(
                        queues,
                        info_region.device_address(),
                        scratch_region.device_address(),
                        scratch.len(),
                    )
                    .map_err(ControllerError::Context)?;
                    context_info
                        .write_at(0, &context)
                        .map_err(ControllerError::Dma)?;
                    let (iml_address, iml_size) =
                        init.ok_or(ControllerError::Dma(DmaError::RegionTooSmall))?;
                    start_gen3_context(
                        &mut self.registers,
                        context_info.device_address(),
                        iml_address,
                        iml_size,
                    )
                    .map_err(ControllerError::Register)?;
                } else {
                    let context = crate::build_gen2_context(0, queues, images)
                        .map_err(ControllerError::Context)?;
                    context_info
                        .write_at(0, &context)
                        .map_err(ControllerError::Dma)?;
                    start_gen2_context(&mut self.registers, context_info.device_address())
                        .map_err(ControllerError::Register)?;
                }
                Ok::<(), ControllerError<E>>(())
            };
        let alive = load_firmware::<A, ControllerError<E>>(
            family,
            &mut images,
            &mut iml_dma,
            |family, images| (&mut initialize_context)(family, images),
            |timeout| wait_for_alive(timeout).map_err(ControllerError::Wait),
        );
        match alive {
            Ok(()) => Ok(()),
            Err(crate::FirmwareLoadError::Context(error)) => Err(error),
            Err(crate::FirmwareLoadError::Wait(ControllerError::Wait(error))) => {
                Err(ControllerError::Wait(error))
            }
            Err(crate::FirmwareLoadError::Wait(error)) => Err(error),
            Err(crate::FirmwareLoadError::FirmwareNotAlive) => {
                Err(ControllerError::FirmwareNotAlive)
            }
        }
    }

    /// Configure the ICT and firmware-versioned TX rate format after ALIVE.
    // upstream: if_iwx.c iwx_post_alive()
    pub fn post_alive(
        &mut self,
        firmware: &FirmwareImage,
    ) -> Result<crate::PostAliveState, crate::IctError> {
        let (ict, state) = configure_post_alive(
            &mut self.allocator,
            &mut self.registers,
            &mut self.interrupt_masks,
            firmware,
            None,
        )?;
        self.resources.ict = ict;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::Cell;

    use super::*;
    use crate::{DmaRegion, IoBarrier};

    struct Region {
        address: u64,
        bytes: Vec<u8>,
    }
    impl DmaRegion for Region {
        fn device_address(&self) -> u64 {
            self.address
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), DmaError> {
            self.write_at(0, bytes)
        }
        fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DmaError> {
            self.bytes
                .get_mut(offset..offset + bytes.len())
                .ok_or(DmaError::RegionTooSmall)?
                .copy_from_slice(bytes);
            Ok(())
        }
        fn read_at(&self, offset: usize, bytes: &mut [u8]) -> Result<(), DmaError> {
            bytes.copy_from_slice(
                self.bytes
                    .get(offset..offset + bytes.len())
                    .ok_or(DmaError::RegionTooSmall)?,
            );
            Ok(())
        }
    }

    struct Allocator(Cell<u64>);
    impl DmaAllocator for Allocator {
        type Region = Region;
        fn allocate(&mut self, size: usize) -> Result<Region, DmaError> {
            let address = self.0.get();
            self.0
                .set(address + size.max(1).div_ceil(4096) as u64 * 4096);
            Ok(Region {
                address,
                bytes: alloc::vec![0; size],
            })
        }
    }

    #[derive(Default)]
    struct Bus {
        writes: Vec<(u32, u32)>,
    }
    impl CsrAccess for Bus {
        fn read32(&mut self, _: u32) -> u32 {
            0
        }
        fn write32(&mut self, offset: u32, value: u32) {
            self.writes.push((offset, value));
        }
        fn write8(&mut self, offset: u32, value: u8) {
            self.writes.push((offset, u32::from(value)));
        }
        fn barrier(&mut self, _: IoBarrier) {}
        fn delay_us(&mut self, _: u32) {}
    }

    #[test]
    fn controller_drains_completed_buffer_and_dispatches_notification() {
        let allocator = Allocator(Cell::new(0x100000));
        let mut controller =
            IwxController::attach(Bus::default(), allocator, DeviceFamily::Ax210, 0x300000, 7)
                .unwrap();
        controller
            .resources
            .rx_queue
            .status
            .write_at(0, &[1, 0])
            .unwrap();
        controller
            .resources
            .rx_queue
            .used_descriptors
            .write_at(4, &[0, 0])
            .unwrap();
        let packet = [6, 0, 0, 0, 0x99, 0, 0, 0x80, 0xaa, 0xbb];
        controller.resources.rx_queue.buffers[0]
            .write_at(0, &packet)
            .unwrap();

        let mut seen = 0;
        let completed = controller
            .process_rx_notifications(|packet, _| {
                assert_eq!(packet.command_id(), 0x99);
                assert_eq!(packet.payload, [0xaa, 0xbb]);
                seen += 1;
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert_eq!(completed, 1);
        assert_eq!(seen, 1);
        assert!(!controller.registers.into_inner().writes.is_empty());
    }
}
