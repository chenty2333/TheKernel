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
