//! Connected PCI-side controller state over the source-derived rings/CSR/firmware code.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

use crate::{
    ApmError, AttachAllocationError, CsrAccess, DeviceFamily, DmaAllocator, DmaError, DmaRegion,
    FirmwareDmaImages, FirmwareImage, HostCommand, InterruptMasks, IwxAttachResources,
    IwxRegisters, PnvmDmaImage, ProcessedRxMpdu, RegisterError, RxBaTable, RxDuplicateState,
    allocate_attach_resources, initialize_firmware_sections, initialize_init_firmware_sections,
    post_alive as configure_post_alive, send_host_command, start_gen2_context, start_gen3_context,
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

#[derive(Debug, PartialEq, Eq)]
pub enum SyncCommandError<E> {
    Command(crate::CommandError),
    Receive(RxServiceError<E>),
    Timeout,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PnvmLoadError<E> {
    Firmware(crate::FirmwareError),
    Dma(DmaError),
    Context(crate::ContextError),
    Register(RegisterError),
    Wait(E),
    NotCompleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopDeviceError {
    Ring(crate::RingError),
    Prepare(ApmError),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ControllerUcodeStartError<F, P> {
    Firmware(ControllerError<F>),
    Pnvm(PnvmLoadError<P>),
    PostAlive(crate::IctError),
}

/// Owned state shared by firmware commands, interrupt dispatch, and network datapath.
pub struct IwxController<B: CsrAccess, A: DmaAllocator> {
    pub registers: IwxRegisters<B>,
    pub allocator: A,
    pub resources: IwxAttachResources<A::Region>,
    pub family: DeviceFamily,
    pub interrupt_masks: InterruptMasks,
    pub command_slots: crate::CommandSlots,
    pub pnvm_dma: Option<PnvmDmaImage<A::Region>>,
    pub rx_replay_windows: [crate::CcmpReplayWindow; 9],
    pub rx_duplicates: RxDuplicateState,
    pub rx_ba_sessions: RxBaTable<ProcessedRxMpdu>,
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
            pnvm_dma: None,
            rx_replay_windows: [crate::CcmpReplayWindow::new(); 9],
            rx_duplicates: RxDuplicateState::new(),
            rx_ba_sessions: RxBaTable::default(),
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

    /// Stop uCode, reset DMA rings, release PNVM and re-prepare card/RF-kill state.
    // upstream: if_iwx.c iwx_stop_device()
    pub fn stop_device(&mut self) -> Result<bool, StopDeviceError> {
        crate::disable_interrupts(&mut self.registers, &self.interrupt_masks);
        let _ = crate::disable_rx_dma(&mut self.registers);
        self.resources
            .rx_queue
            .reset()
            .map_err(StopDeviceError::Ring)?;
        for ring in &mut self.resources.tx_queues {
            ring.reset().map_err(StopDeviceError::Ring)?;
        }
        self.registers.force_clear_nic_locks();
        let _ = crate::apm_stop(&mut self.registers);
        crate::software_reset(&mut self.registers);
        crate::configure_msix_hardware(&mut self.registers, &self.interrupt_masks, true);
        crate::disable_interrupts(&mut self.registers, &self.interrupt_masks);
        crate::enable_rfkill_interrupts(&mut self.registers, &mut self.interrupt_masks);
        self.hardware_rfkill = crate::hardware_rfkill(&mut self.registers);
        crate::prepare_card_hw(&mut self.registers).map_err(StopDeviceError::Prepare)?;
        self.pnvm_dma.take();
        Ok(self.hardware_rfkill)
    }

    /// Apply firmware/stepping NIC setup before firmware DMA publication.
    // upstream: if_iwx.c iwx_nic_init()
    pub fn initialize_nic(
        &mut self,
        firmware_phy_config: u32,
        hardware_revision: u32,
    ) -> Result<(), ApmError> {
        crate::initialize_nic(&mut self.registers, firmware_phy_config, hardware_revision)
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

    /// Build and submit a single-payload PDU through the active TX command ring.
    // upstream: if_iwx.c iwx_send_cmd_pdu()
    pub fn send_command_pdu(
        &mut self,
        command_id: u32,
        flags: u32,
        payload: &[u8],
        response_capacity: usize,
        external: Option<&mut A::Region>,
    ) -> Result<crate::CommandTicket, crate::CommandError> {
        let parts = [payload];
        let command = HostCommand {
            id: command_id,
            flags,
            response_capacity,
            parts: &parts,
        };
        self.send_command(&command, external)
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

    /// Submit a response-bearing command and pump RX notifications until its ACK or timeout.
    // upstream: if_iwx.c iwx_send_cmd() synchronous response wait
    pub fn send_command_wait<E>(
        &mut self,
        command: &HostCommand<'_>,
        external: Option<&mut A::Region>,
        mut dispatch: impl FnMut(&crate::RxPacket<'_>, crate::RxMbufPlan) -> Result<bool, E>,
    ) -> Result<crate::CompletedCommand, SyncCommandError<E>> {
        let ticket = self
            .send_command(command, external)
            .map_err(SyncCommandError::Command)?;
        if ticket.asynchronous {
            return Err(SyncCommandError::Command(
                crate::CommandError::InvalidResponse,
            ));
        }
        let mut elapsed = 0u64;
        while elapsed < 1_000_000_000 {
            self.process_rx_notifications(|packet, plan| dispatch(packet, plan))
                .map_err(SyncCommandError::Receive)?;
            if self
                .command_slots
                .is_acknowledged(ticket.index, ticket.generation)
            {
                if self.generation != ticket.generation {
                    return Err(SyncCommandError::Command(
                        crate::CommandError::GenerationChanged,
                    ));
                }
                return self
                    .command_slots
                    .take_completed(ticket.index, ticket.generation)
                    .map_err(SyncCommandError::Command);
            }
            self.registers.delay_us(1000);
            elapsed += 1_000_000;
        }
        if self.generation == ticket.generation {
            self.command_slots
                .abandon_wait_response(ticket.index, ticket.generation)
                .map_err(SyncCommandError::Command)?;
        }
        Err(SyncCommandError::Timeout)
    }

    /// Submit a pure wire-layout command through the same ACK/response waiter.
    pub fn send_encoded_command_wait<E>(
        &mut self,
        command: &crate::EncodedCommand,
        external: Option<&mut A::Region>,
        dispatch: impl FnMut(&crate::RxPacket<'_>, crate::RxMbufPlan) -> Result<bool, E>,
    ) -> Result<crate::CompletedCommand, SyncCommandError<E>> {
        let payload = command
            .bytes
            .get(crate::HOST_COMMAND_HEADER_BYTES..)
            .ok_or(SyncCommandError::Command(
                crate::CommandError::InvalidResponse,
            ))?;
        let parts = [payload];
        let host = HostCommand {
            id: command.original_id,
            flags: command.flags,
            response_capacity: command.response_capacity,
            parts: &parts,
        };
        self.send_command_wait(&host, external, dispatch)
    }

    /// Send a PDU and return its firmware response status word.
    // upstream: if_iwx.c iwx_send_cmd_pdu_status()
    pub fn send_command_pdu_status<E>(
        &mut self,
        command_id: u32,
        flags: u32,
        payload: &[u8],
        response_capacity: usize,
        external: Option<&mut A::Region>,
        dispatch: impl FnMut(&crate::RxPacket<'_>, crate::RxMbufPlan) -> Result<bool, E>,
    ) -> Result<(crate::CompletedCommand, u32), SyncCommandError<E>> {
        let parts = [payload];
        let command = HostCommand {
            id: command_id,
            flags,
            response_capacity,
            parts: &parts,
        };
        let completed = self.send_command_wait(&command, external, dispatch)?;
        let status =
            crate::command_response_status(completed.response.as_deref().unwrap_or(&[]), false)
                .map_err(SyncCommandError::Command)?;
        Ok((completed, status))
    }

    /// Decode an RX_MPDU descriptor and apply the source decrypt/replay/duplicate gates.
    // upstream: if_iwx.c iwx_rx_mpdu_mq() / iwx_rx_hwdecrypt() / iwx_detect_duplicate()
    pub fn process_rx_mpdu(
        &mut self,
        payload: &[u8],
        monitor_mode: bool,
        decrypt_policy: crate::HardwareDecryptPolicy,
    ) -> Result<crate::RxMpduOutcome, crate::RxMpduProcessError> {
        crate::process_rx_mpdu(
            payload,
            self.family,
            monitor_mode,
            decrypt_policy,
            &mut self.rx_replay_windows,
            &mut self.rx_duplicates,
        )
    }

    /// Apply BAID/TID matching, duplicate/old-sequence decisions, buffering and NSSN release.
    // upstream: if_iwx.c iwx_rx_reorder() / iwx_release_frames()
    pub fn reorder_rx_mpdu(
        &mut self,
        frame: ProcessedRxMpdu,
        now_usec: u64,
    ) -> Result<Option<crate::RxReorderOutcome<ProcessedRxMpdu>>, crate::BaError> {
        const REORDER_NSSN_MASK: u32 = 0x0000_0fff;
        const REORDER_SN_MASK: u32 = 0x00ff_f000;
        const REORDER_BAID_MASK: u32 = 0x7f00_0000;
        const REORDER_OLD_SN: u32 = 1 << 31;
        const REORDER_SN_SHIFT: u32 = 12;
        const REORDER_BAID_SHIFT: u32 = 24;

        let reorder = frame.metadata.reorder_data;
        let baid = ((reorder & REORDER_BAID_MASK) >> REORDER_BAID_SHIFT) as u8;
        if baid == crate::INVALID_BAID {
            return Ok(None);
        }
        self.rx_ba_sessions.reorder_mpdu(
            baid,
            frame.reorder_tid,
            ((reorder & REORDER_SN_MASK) >> REORDER_SN_SHIFT) as u16,
            (reorder & REORDER_NSSN_MASK) as u16,
            frame.metadata.is_amsdu(),
            frame.metadata.last_amsdu_subframe(),
            reorder & REORDER_OLD_SN != 0,
            frame.metadata.status & crate::RX_MPDU_STATUS_DUPLICATE != 0,
            now_usec,
            frame,
        )
    }

    /// Publish Init/regular firmware context and wait for the matching ALIVE event.
    // upstream: if_iwx.c iwx_load_firmware()
    pub fn boot_firmware<E>(
        &mut self,
        firmware: &FirmwareImage,
        init_ucode: bool,
        imr_enabled: bool,
        mut wait_for_alive: impl FnMut(&mut Self, u64) -> Result<bool, E>,
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
        crate::enable_firmware_load_interrupts(&mut self.registers, &mut self.interrupt_masks);
        self.publish_firmware_context(&mut images, init, imr_enabled)?;
        let alive = wait_for_alive(self, crate::FIRMWARE_ALIVE_TIMEOUT_NS);
        if !matches!(alive, Ok(true)) {
            images.free_paging();
        }
        iml_dma.take();
        images.free_firmware_sections();
        match alive {
            Ok(true) => Ok(()),
            Ok(false) => Err(ControllerError::FirmwareNotAlive),
            Err(error) => Err(ControllerError::Wait(error)),
        }
    }

    fn publish_firmware_context<E>(
        &mut self,
        images: &mut FirmwareDmaImages<A::Region>,
        iml: Option<(u64, u32)>,
        imr_enabled: bool,
    ) -> Result<(), ControllerError<E>> {
        let family = self.family;
        let queues = crate::ContextQueueAddresses {
            free_rbd: self.resources.rx_queue.free_descriptors.device_address(),
            used_rbd: self.resources.rx_queue.used_descriptors.device_address(),
            rx_status: self.resources.rx_queue.status.device_address(),
            command_queue: self.resources.tx_queues[0].descriptors.device_address(),
        };
        let scratch_region = self.resources.prph_scratch.as_mut();
        let prph_info = self.resources.prph_info.as_ref();
        if family >= DeviceFamily::Ax210 {
            let scratch = crate::build_gen3_prph_scratch(0, queues.free_rbd, images, imr_enabled)
                .map_err(ControllerError::Context)?;
            let scratch_region =
                scratch_region.ok_or(ControllerError::Dma(DmaError::RegionTooSmall))?;
            scratch_region
                .write_at(0, &scratch)
                .map_err(ControllerError::Dma)?;
            let info_region = prph_info.ok_or(ControllerError::Dma(DmaError::RegionTooSmall))?;
            let context = crate::build_gen3_context(
                queues,
                info_region.device_address(),
                scratch_region.device_address(),
                scratch.len(),
            )
            .map_err(ControllerError::Context)?;
            self.resources
                .context_info
                .write_at(0, &context)
                .map_err(ControllerError::Dma)?;
            let (iml_address, iml_size) =
                iml.ok_or(ControllerError::Dma(DmaError::RegionTooSmall))?;
            start_gen3_context(
                &mut self.registers,
                self.resources.context_info.device_address(),
                iml_address,
                iml_size,
            )
            .map_err(ControllerError::Register)?;
        } else {
            let context =
                crate::build_gen2_context(0, queues, images).map_err(ControllerError::Context)?;
            self.resources
                .context_info
                .write_at(0, &context)
                .map_err(ControllerError::Dma)?;
            start_gen2_context(
                &mut self.registers,
                self.resources.context_info.device_address(),
            )
            .map_err(ControllerError::Register)?;
        }
        Ok(())
    }

    /// Stage the SKU-matched platform NVM and ring the PNVM doorbell until completion.
    // upstream: if_iwx.c iwx_load_pnvm() / iwx_ctxt_info_gen3_set_pnvm()
    pub fn load_pnvm<E>(
        &mut self,
        firmware: &FirmwareImage,
        external_pnvm: Option<&[u8]>,
        sku_id: [u32; 3],
        mac_type: u16,
        rf_type: u16,
        mut wait_for_complete: impl FnMut(&mut Self, u64) -> Result<bool, E>,
    ) -> Result<(), PnvmLoadError<E>> {
        if self.family < DeviceFamily::Ax210 || sku_id == [0; 3] {
            return Ok(());
        }
        let source = firmware.pnvm.as_deref().or(external_pnvm);
        let staged = if self.pnvm_dma.is_none() {
            let selected = if let Some(source) = source {
                crate::select_pnvm(source, sku_id, mac_type, rf_type)
                    .map_err(PnvmLoadError::Firmware)?
            } else {
                None
            };
            selected
                .as_ref()
                .map(|pnvm| crate::setup_pnvm(&mut self.allocator, pnvm, true))
                .transpose()
                .map_err(PnvmLoadError::Dma)?
        } else {
            None
        };
        let current = staged.as_ref().or(self.pnvm_dma.as_ref());
        if let Some(pnvm) = current {
            let scratch = self
                .resources
                .prph_scratch
                .as_mut()
                .ok_or(PnvmLoadError::Dma(DmaError::RegionTooSmall))?;
            let mut bytes = alloc::vec::Vec::new();
            bytes
                .try_reserve_exact(crate::PRPH_SCRATCH_BYTES)
                .map_err(|_| PnvmLoadError::Dma(DmaError::AllocationFailed))?;
            bytes.resize(crate::PRPH_SCRATCH_BYTES, 0);
            scratch.read_at(0, &mut bytes).map_err(PnvmLoadError::Dma)?;
            crate::set_gen3_pnvm(&mut bytes, pnvm).map_err(PnvmLoadError::Context)?;
            scratch.write_at(0, &bytes).map_err(PnvmLoadError::Dma)?;
        }
        if staged.is_some() {
            self.pnvm_dma = staged;
        }
        self.registers.nic_lock().map_err(PnvmLoadError::Register)?;
        let write_result = self.registers.write_umac_prph(0x00a0_5c04, 1 << 20);
        self.registers.nic_unlock();
        write_result.map_err(PnvmLoadError::Register)?;
        let completed = wait_for_complete(self, 2_000_000_000).map_err(PnvmLoadError::Wait)?;
        if completed {
            Ok(())
        } else {
            Err(PnvmLoadError::NotCompleted)
        }
    }

    /// Read/start regular uCode, load PNVM on Gen3, then perform post-ALIVE setup.
    // upstream: if_iwx.c iwx_load_ucode_wait_alive()
    pub fn load_ucode_wait_alive<F, P>(
        &mut self,
        firmware: &FirmwareImage,
        external_pnvm: Option<&[u8]>,
        sku_id: [u32; 3],
        mac_type: u16,
        rf_type: u16,
        imr_enabled: bool,
        wait_alive: impl FnMut(&mut Self, u64) -> Result<bool, F>,
        wait_pnvm: impl FnMut(&mut Self, u64) -> Result<bool, P>,
    ) -> Result<(), ControllerUcodeStartError<F, P>> {
        self.boot_firmware(firmware, false, imr_enabled, wait_alive)
            .map_err(ControllerUcodeStartError::Firmware)?;
        if self.family >= DeviceFamily::Ax210 {
            self.load_pnvm(
                firmware,
                external_pnvm,
                sku_id,
                mac_type,
                rf_type,
                wait_pnvm,
            )
            .map_err(ControllerUcodeStartError::Pnvm)?;
        }
        self.post_alive(firmware)
            .map_err(ControllerUcodeStartError::PostAlive)?;
        Ok(())
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
    use core::cell::{Cell, RefCell};

    use super::*;
    use crate::{DmaRegion, IoBarrier, firmware::test_image};

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
        fn read32(&mut self, offset: u32) -> u32 {
            if offset == 0x024 { 1 } else { 0 }
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

    #[test]
    fn synchronous_command_pumps_rx_until_descriptor_acknowledgement() {
        let allocator = Allocator(Cell::new(0x200000));
        let mut controller =
            IwxController::attach(Bus::default(), allocator, DeviceFamily::Ax210, 0x300000, 9)
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
        controller.resources.rx_queue.buffers[0]
            .write_at(0, &[4, 0, 0, 0, 0x44, 0, 0, 0])
            .unwrap();
        let empty: [&[u8]; 0] = [];
        let command = HostCommand {
            id: 0x44,
            flags: 0,
            response_capacity: 0,
            parts: &empty,
        };

        let completed = controller
            .send_command_wait(&command, None, |_, _| Ok::<_, ()>(true))
            .unwrap();
        assert!(completed.response.is_none());
        assert!(completed.external_payload_released);
        assert_eq!(controller.command_slots.queued(), 0);
    }

    #[test]
    fn gen3_pnvm_request_waits_for_completion_even_when_no_sku_section_matches() {
        let allocator = Allocator(Cell::new(0x300000));
        let mut controller =
            IwxController::attach(Bus::default(), allocator, DeviceFamily::Ax210, 0x300000, 11)
                .unwrap();
        let firmware = FirmwareImage::parse(&test_image(&[])).unwrap();
        let mut waited = false;
        controller
            .load_pnvm(&firmware, None, [1, 2, 3], 0x43, 0x10d, |_, timeout| {
                assert_eq!(timeout, 2_000_000_000);
                waited = true;
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert!(waited);
        assert!(controller.pnvm_dma.is_none());
    }

    #[test]
    fn regular_ucode_start_keeps_alive_pnvm_post_alive_order_by_family() {
        let section = |offset: u32, payload: &[u8]| {
            let mut bytes = offset.to_le_bytes().to_vec();
            bytes.extend_from_slice(payload);
            bytes
        };
        let lmac = section(0x1000, &[1]);
        let separator = section(0xffff_cccc, &[]);
        let umac = section(0x2000, &[2]);
        let paging_separator = section(0xaaaa_bbbb, &[]);
        let paging = section(0x3000, &[3]);
        let firmware = FirmwareImage::parse(&test_image(&[
            (19, &lmac),
            (19, &separator),
            (19, &umac),
            (19, &paging_separator),
            (19, &paging),
        ]))
        .unwrap();
        let allocator = Allocator(Cell::new(0x400000));
        let mut controller =
            IwxController::attach(Bus::default(), allocator, DeviceFamily::Family22000, 0, 1)
                .unwrap();
        let order = RefCell::new(alloc::vec::Vec::new());
        controller
            .load_ucode_wait_alive(
                &firmware,
                None,
                [0; 3],
                0,
                0,
                false,
                |_, timeout| {
                    assert_eq!(timeout, crate::FIRMWARE_ALIVE_TIMEOUT_NS);
                    order.borrow_mut().push(1);
                    Ok::<_, ()>(true)
                },
                |_, _| {
                    order.borrow_mut().push(2);
                    Ok::<_, ()>(true)
                },
            )
            .unwrap();
        assert_eq!(order.into_inner(), [1]);
    }

    #[test]
    fn controller_routes_parsed_mpdu_to_matching_rx_ba_session() {
        let allocator = Allocator(Cell::new(0x500000));
        let mut controller =
            IwxController::attach(Bus::default(), allocator, DeviceFamily::Ax210, 0x300000, 13)
                .unwrap();
        controller
            .rx_ba_sessions
            .start(1, 3, 0, 64, 20_000, 0)
            .unwrap();
        let frame = crate::ProcessedRxMpdu {
            metadata: crate::RxMpduMetadata {
                descriptor_bytes: 68,
                frame_bytes: 26,
                status: 0,
                reorder_data: (1 << 24) | 1,
                phy_info: 0,
                mac_flags2: 0,
                amsdu_info: 0,
                rate_n_flags: 0,
                channel_index: 1,
                energy_a: 0,
                energy_b: 0,
                device_timestamp: 0,
            },
            frame: alloc::vec![0; 26],
            hardware_decrypted: false,
            same_sequence: false,
            tid_index: 3,
            reorder_tid: 3,
        };
        let result = controller.reorder_rx_mpdu(frame, 1).unwrap().unwrap();
        assert!(!result.consumed);
        assert_eq!(result.frames.len(), 1);
        assert!(result.ampdu_done);
    }
}
