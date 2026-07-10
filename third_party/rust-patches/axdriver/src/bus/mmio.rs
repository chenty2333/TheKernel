#[allow(unused_imports)]
use crate::{AllDevices, drivers::BusProbeResult, prelude::*};

impl AllDevices {
    pub(crate) fn probe_bus_devices(&mut self) {
        // TODO: parse device tree
        #[cfg(feature = "virtio")]
        for reg in axconfig::devices::VIRTIO_MMIO_RANGES {
            for_each_drivers!(type Driver, {
                match Driver::probe_mmio(reg.0, reg.1) {
                    BusProbeResult::NotMatched => {}
                    BusProbeResult::Claimed => continue,
                    BusProbeResult::Device(dev) => {
                        info!(
                            "registered a new {:?} device at [PA:{:#x}, PA:{:#x}): {:?}",
                            dev.device_type(),
                            reg.0, reg.0 + reg.1,
                            dev.device_name(),
                        );
                        self.add_device(dev);
                        continue;
                    }
                }
            });
        }
    }
}
