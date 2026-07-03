extern crate alloc;

use alloc::{format, sync::Arc};

use axdriver_base::DeviceType;
use axdriver_block::BlockDriverOps;
use axdriver_virtio::MmioTransport;
use axhal::mem::PhysAddr;
use rdrive::{
    DriverGeneric, PlatformDevice, module_driver, probe::OnProbeError, register::FdtInfo,
};

use super::PlatformDeviceBlock;
use crate::{
    dyn_drivers::{blk::maping_dev_err_to_blk_err, iomap},
    virtio::VirtIoHalImpl,
};

type Device<T> = axdriver_virtio::VirtIoBlkDev<VirtIoHalImpl, T>;

module_driver!(
    name: "Virtio Block",
    level: ProbeLevel::PostKernel,
    priority: ProbePriority::DEFAULT,
    probe_kinds: &[
        ProbeKind::Fdt {
            compatibles: &["virtio,mmio"],
            on_probe: probe
        }
    ],
);

fn probe(info: FdtInfo<'_>, plat_dev: PlatformDevice) -> Result<(), OnProbeError> {
    let base_reg = info
        .node
        .reg()
        .and_then(|mut regs| regs.next())
        .ok_or(OnProbeError::other(alloc::format!(
            "[{}] has no reg",
            info.node.name()
        )))?;

    let mmio_size = base_reg.size.unwrap_or(0x1000);
    let mmio_base = PhysAddr::from_usize(base_reg.address as usize);

    let mmio_base = iomap(mmio_base, mmio_size)?.as_ptr();

    let (ty, transport) =
        axdriver_virtio::probe_mmio_device(mmio_base, mmio_size).ok_or(OnProbeError::NotMatch)?;

    if ty != DeviceType::Block {
        return Err(OnProbeError::NotMatch);
    }

    let irq = info
        .interrupts()
        .into_iter()
        .next()
        .and_then(|cells| cells.into_iter().next())
        .map(|irq| irq as usize);

    let dev = Device::try_new_with_irq(transport, irq).map_err(|e| {
        OnProbeError::other(format!(
            "failed to initialize Virtio Block device at [PA:{mmio_base:?},): {e:?}"
        ))
    })?;

    let dev = BlockDivce {
        dev: Arc::new(spin::Mutex::new(dev)),
    };
    plat_dev.register_block(dev);
    debug!("virtio block device registered successfully");
    Ok(())
}

struct BlockDivce {
    dev: Arc<spin::Mutex<Device<MmioTransport>>>,
}

struct BlockQueue {
    raw: Arc<spin::Mutex<Device<MmioTransport>>>,
}

impl DriverGeneric for BlockDivce {
    fn name(&self) -> &str {
        "virtio-blk"
    }
}

impl rd_block::Interface for BlockDivce {
    fn create_queue(&mut self) -> Option<alloc::boxed::Box<dyn rd_block::IQueue>> {
        Some(alloc::boxed::Box::new(BlockQueue {
            raw: self.dev.clone(),
        }) as _)
    }

    fn enable_irq(&mut self) {
        self.dev.lock().enable_irq();
    }

    fn disable_irq(&mut self) {
        self.dev.lock().disable_irq();
    }

    fn is_irq_enabled(&self) -> bool {
        self.dev.lock().is_irq_enabled()
    }

    fn handle_irq(&mut self) -> rd_block::Event {
        let drained = match self.dev.lock().handle_irq() {
            Ok(drained) => drained,
            Err(err) => {
                warn!("virtio block irq handling failed: {err:?}");
                0
            }
        };
        if drained == 0 {
            return rd_block::Event::none();
        }

        let mut event = rd_block::Event::none();
        event.queue.insert(0);
        event
    }
}

impl rd_block::IQueue for BlockQueue {
    fn num_blocks(&self) -> usize {
        self.raw.lock().num_blocks() as _
    }

    fn block_size(&self) -> usize {
        self.raw.lock().block_size()
    }

    fn id(&self) -> usize {
        0
    }

    fn buff_config(&self) -> rd_block::BuffConfig {
        rd_block::BuffConfig {
            dma_mask: u64::MAX,
            align: 0x1000,
            size: self.block_size(),
        }
    }

    fn submit_request(
        &mut self,
        request: rd_block::Request<'_>,
    ) -> Result<rd_block::RequestId, rd_block::BlkError> {
        let id = request.block_id;
        match request.kind {
            rd_block::RequestKind::Read(mut buffer) => {
                self.raw
                    .lock()
                    .read_block(id as _, &mut buffer)
                    .map_err(maping_dev_err_to_blk_err)?;
                Ok(rd_block::RequestId::new(0))
            }
            rd_block::RequestKind::Write(items) => {
                self.raw
                    .lock()
                    .write_block(id as _, items)
                    .map_err(maping_dev_err_to_blk_err)?;
                Ok(rd_block::RequestId::new(0))
            }
        }
    }

    fn poll_request(&mut self, _request: rd_block::RequestId) -> Result<(), rd_block::BlkError> {
        Ok(())
    }
}
