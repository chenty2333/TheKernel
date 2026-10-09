//! Defines types and probe methods of all supported devices.

#![allow(unused_imports, dead_code)]

use axdriver_base::DeviceType;
#[cfg(feature = "bus-pci")]
use axdriver_pci::{DeviceFunction, DeviceFunctionInfo, PciRoot};

pub use super::dummy::*;
use crate::AxDeviceEnum;
#[cfg(feature = "virtio")]
use crate::virtio::{self, VirtIoDevMeta};

pub(crate) enum BusProbeResult {
    NotMatched,
    Claimed,
    Device(AxDeviceEnum),
    Devices(alloc::vec::Vec<AxDeviceEnum>),
}

pub trait DriverProbe {
    fn probe_global() -> Option<AxDeviceEnum> {
        None
    }

    #[cfg(bus = "mmio")]
    fn probe_mmio(_mmio_base: usize, _mmio_size: usize) -> BusProbeResult {
        BusProbeResult::NotMatched
    }

    #[cfg(bus = "pci")]
    fn probe_pci(
        _root: &mut PciRoot,
        _bdf: DeviceFunction,
        _dev_info: &DeviceFunctionInfo,
    ) -> BusProbeResult {
        BusProbeResult::NotMatched
    }
}

#[cfg(net_dev = "virtio-net")]
register_net_driver!(
    <virtio::VirtIoNet as VirtIoDevMeta>::Driver,
    <virtio::VirtIoNet as VirtIoDevMeta>::Device
);

#[cfg(block_dev = "virtio-blk")]
register_block_driver!(
    <virtio::VirtIoBlk as VirtIoDevMeta>::Driver,
    <virtio::VirtIoBlk as VirtIoDevMeta>::Device
);

#[cfg(display_dev = "virtio-gpu")]
register_display_driver!(
    <virtio::VirtIoGpu as VirtIoDevMeta>::Driver,
    <virtio::VirtIoGpu as VirtIoDevMeta>::Device
);

#[cfg(input_dev = "virtio-input")]
register_input_driver!(
    <virtio::VirtIoInput as VirtIoDevMeta>::Driver,
    <virtio::VirtIoInput as VirtIoDevMeta>::Device
);

#[cfg(vsock_dev = "virtio-socket")]
register_vsock_driver!(
    <virtio::VirtIoSocket as VirtIoDevMeta>::Driver,
    <virtio::VirtIoSocket as VirtIoDevMeta>::Device
);

#[cfg(net_dev = "n305-net")]
register_net_driver!(IgcDriver, alloc::boxed::Box<dyn axdriver_net::NetDriverOps>);
#[cfg(net_dev = "rtl8125")]
register_net_driver!(Rtl8125Driver, axdriver_net::rtl8125::nic::RtlNic<crate::rtl8125::PlatformHal, crate::rtl8125::Window, 256>);
#[cfg(any(net_dev = "rtl8125", net_dev = "n305-net"))]
pub struct Rtl8125Driver;
#[cfg(any(net_dev = "rtl8125", net_dev = "n305-net"))]
impl DriverProbe for Rtl8125Driver {
    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        info: &DeviceFunctionInfo,
    ) -> BusProbeResult {
        crate::rtl8125::probe(root, bdf, info)
    }
}

cfg_if::cfg_if! {
    if #[cfg(block_dev = "ramdisk")] {
        pub struct RamDiskDriver;
        register_block_driver!(RamDiskDriver, axdriver_block::ramdisk::RamDisk);

        impl DriverProbe for RamDiskDriver {
            fn probe_global() -> Option<AxDeviceEnum> {
                // TODO: format RAM disk
                Some(AxDeviceEnum::from_block(
                    axdriver_block::ramdisk::RamDisk::new(0x100_0000), // 16 MiB
                ))
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(any(net_dev = "igc", net_dev = "n305-net"))] {
        pub struct IgcDriver;
        #[cfg(net_dev = "igc")]
        register_net_driver!(
            IgcDriver,
            axdriver_net::igc::IgcNic<crate::igc::IgcHalImpl, { crate::igc::QUEUE_SIZE }>
        );
        impl DriverProbe for IgcDriver {
            #[cfg(bus = "pci")]
            fn probe_pci(
                root: &mut axdriver_pci::PciRoot,
                bdf: axdriver_pci::DeviceFunction,
                dev_info: &axdriver_pci::DeviceFunctionInfo,
            ) -> BusProbeResult {
                crate::igc::probe_and_init(root, bdf, dev_info)
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(net_dev = "ixgbe")] {
        use crate::ixgbe::IxgbeHalImpl;
        pub struct IxgbeDriver;
        register_net_driver!(IxgbeDriver, axdriver_net::ixgbe::IxgbeNic<IxgbeHalImpl, 1024, 1>);
        impl DriverProbe for IxgbeDriver {
            #[cfg(bus = "pci")]
            fn probe_pci(
                root: &mut axdriver_pci::PciRoot,
                bdf: axdriver_pci::DeviceFunction,
                dev_info: &axdriver_pci::DeviceFunctionInfo,
            ) -> Option<crate::AxDeviceEnum> {
                use axdriver_net::ixgbe::{INTEL_82599, INTEL_VEND, IxgbeNic};
                if dev_info.vendor_id == INTEL_VEND && dev_info.device_id == INTEL_82599 {
                    // Intel 10Gb Network
                    info!("ixgbe PCI device found at {:?}", bdf);

                    // Initialize the device
                    // These can be changed according to the requirments specified in the ixgbe init
                    // function.
                    const QN: u16 = 1;
                    const QS: usize = 1024;
                    let bar_info = root.bar_info(bdf, 0).unwrap();
                    match bar_info {
                        axdriver_pci::BarInfo::Memory { address, size, .. } => {
                            let ixgbe_nic = IxgbeNic::<IxgbeHalImpl, QS, QN>::init(
                                axhal::mem::phys_to_virt((address as usize).into()).into(),
                                size as usize,
                            )
                            .expect("failed to initialize ixgbe device");
                            return Some(AxDeviceEnum::from_net(ixgbe_nic));
                        }
                        axdriver_pci::BarInfo::IO { .. } => {
                            warn!("ixgbe: BAR0 is of I/O type");
                            return None;
                        }
                    }
                }
                None
            }
        }
    }
}

// Appended PCI probe for Intel iwx. The full network/net80211 adapter follows.
#[cfg(feature = "iwx")]
pub struct IwxDriver;
#[cfg(feature = "iwx")]
impl DriverProbe for IwxDriver {
    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut axdriver_pci::PciRoot,
        bdf: axdriver_pci::DeviceFunction,
        info: &axdriver_pci::DeviceFunctionInfo,
    ) -> BusProbeResult {
        crate::iwx::probe(root, bdf, info)
    }
}

#[cfg(feature = "ahci-pci")]
pub struct AhciDriver;
#[cfg(feature = "ahci-pci")]
impl DriverProbe for AhciDriver {
    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut axdriver_pci::PciRoot,
        bdf: axdriver_pci::DeviceFunction,
        info: &axdriver_pci::DeviceFunctionInfo,
    ) -> BusProbeResult {
        crate::ahci::probe(root, bdf, info)
    }
}

#[cfg(feature = "sdhci-pci")]
pub struct SdhciDriver;
#[cfg(feature = "sdhci-pci")]
impl DriverProbe for SdhciDriver {
    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut axdriver_pci::PciRoot,
        bdf: axdriver_pci::DeviceFunction,
        info: &axdriver_pci::DeviceFunctionInfo,
    ) -> BusProbeResult {
        crate::sdhci::probe(root, bdf, info)
    }
}

#[cfg(feature = "e1000")]
pub struct E1000Driver;
#[cfg(feature = "e1000")]
impl DriverProbe for E1000Driver {
    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut axdriver_pci::PciRoot,
        bdf: axdriver_pci::DeviceFunction,
        info: &axdriver_pci::DeviceFunctionInfo,
    ) -> BusProbeResult {
        crate::e1000::probe(root, bdf, info)
    }
}
