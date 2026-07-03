//! [ArceOS](https://github.com/arceos-org/arceos) device drivers.
//!
//! # Usage
//!
//! All detected devices are composed into a large struct [`AllDevices`]
//! and returned by the [`init_drivers`] function. The upperlayer subsystems
//! (e.g., the network stack) may unpack the struct to get the specified device
//! driver they want.
//!
//! For each device category (i.e., net, block, display, etc.), an unified type
//! is used to represent all devices in that category. Currently, there are 3
//! categories: [`AxNetDevice`], [`AxBlockDevice`], and [`AxDisplayDevice`].
//!
//! # Concepts
//!
//! This crate supports two device models depending on the `dyn` feature:
//!
//! - **Static**: The type of all devices is static, it is determined at compile
//!   time by corresponding cargo features. For example, [`AxNetDevice`] will be
//!   an alias of [`VirtioNetDev`] if the `virtio-net` feature is enabled. This
//!   model provides the best performance as it avoids dynamic dispatch. But on
//!   limitation, only one device instance is supported for each device category.
//! - **Dynamic**: All device instance is using [trait objects] and wrapped in a
//!   `Box<dyn Trait>`. For example, [`AxNetDevice`] will be [`Box<dyn NetDriverOps>`].
//!   When call a method provided by the device, it uses [dynamic dispatch][dyn]
//!   that may introduce a little overhead. But on the other hand, it is more
//!   flexible, multiple instances of each device category are supported.
//!
//! # Supported Devices
//!
//! | Device Category | Cargo Feature | Description |
//! |-|-|-|
//! | Block | `ramdisk` | A RAM disk that stores data in a vector |
//! | Block | `virtio-blk` | VirtIO block device |
//! | Network | `virtio-net` | VirtIO network device |
//! | Display | `virtio-gpu` | VirtIO graphics device |
//!
//! # Other Cargo Features
//!
//! - `dyn`: use the dynamic device model (see above).
//! - `bus-mmio`: use device tree to probe all MMIO devices.
//! - `bus-pci`: use PCI bus to probe all PCI devices. This feature is
//!   enabled by default.
//! - `virtio`: use VirtIO devices. This is enabled if any of `virtio-blk`,
//!   `virtio-net` or `virtio-gpu` is enabled.
//! - `net`: use network devices. This is enabled if any feature of network
//!   devices is selected. If this feature is enabled without any network device
//!   features, a dummy struct is used for [`AxNetDevice`].
//! - `block`: use block storage devices. Similar to the `net` feature.
//! - `display`: use graphics display devices. Similar to the `net` feature.
//!
//! [`VirtioNetDev`]: axdriver_virtio::VirtIoNetDev
//! [`Box<dyn NetDriverOps>`]: axdriver_net::NetDriverOps
//! [trait objects]: https://doc.rust-lang.org/book/ch17-02-trait-objects.html
//! [dyn]: https://doc.rust-lang.org/std/keyword.dyn.html

#![no_std]
#![feature(doc_cfg)]
#![feature(used_with_arg)]
#![feature(associated_type_defaults)]

#[macro_use]
extern crate log;

#[cfg(feature = "dyn")]
extern crate alloc;

#[macro_use]
mod macros;

#[cfg(not(feature = "dyn"))]
mod bus;
mod drivers;
mod dummy;
mod structs;

#[cfg(feature = "virtio")]
mod virtio;

#[cfg(feature = "ixgbe")]
mod ixgbe;

#[cfg(feature = "dyn")]
mod dyn_drivers;

pub mod prelude;

#[cfg(feature = "virtio-blk")]
pub use axdriver_virtio::{AsyncBlockWaitPolicy, VirtioIoCounters};

#[allow(unused_imports)]
use self::prelude::*;
#[cfg(feature = "block")]
pub use self::structs::AxBlockDevice;
#[cfg(feature = "display")]
pub use self::structs::AxDisplayDevice;
#[cfg(feature = "net")]
pub use self::structs::AxNetDevice;
pub use self::structs::{AxDeviceContainer, AxDeviceEnum};

/// Snapshot of VirtIO I/O counters.
#[cfg(not(feature = "virtio-blk"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtioIoCounters {
    /// Number of synchronous queue waits.
    pub queue_sync_waits: u64,
    /// Number of `can_pop` polling iterations in synchronous queue waits.
    pub queue_sync_wait_polls: u64,
    /// Number of synchronous queue waits that completed without polling.
    pub queue_sync_wait_immediate: u64,
    /// Number of explicit queue notify calls.
    pub queue_notify_calls: u64,
    /// Total VirtIO block requests.
    pub blk_requests: u64,
    /// VirtIO block read requests.
    pub blk_read_requests: u64,
    /// VirtIO block write requests.
    pub blk_write_requests: u64,
    /// VirtIO block flush requests.
    pub blk_flush_requests: u64,
    /// Block-layer data fences that wait for earlier async writes.
    pub blk_data_fences: u64,
    /// Metadata or persistence fences that require a device flush boundary.
    pub blk_metadata_fences: u64,
    /// Flush boundaries skipped because the device did not negotiate FLUSH.
    pub blk_flush_unsupported: u64,
    /// Bytes requested through VirtIO block reads.
    pub blk_read_bytes: u64,
    /// Bytes requested through VirtIO block writes.
    pub blk_write_bytes: u64,
    /// VirtIO block vectored read requests.
    pub blk_vectored_read_requests: u64,
    /// VirtIO block vectored write requests.
    pub blk_vectored_write_requests: u64,
    /// Non-empty data segments in VirtIO block vectored requests.
    pub blk_vectored_segments: u64,
    /// Maximum number of outstanding pending block requests observed.
    pub blk_pending_max_depth: u64,
    /// Number of pending-submit attempts that found the VirtQueue full.
    pub blk_pending_queue_full: u64,
    /// Number of non-empty pending completion drain batches.
    pub blk_pending_drain_batches: u64,
    /// Number of pending requests completed by drain batches.
    pub blk_pending_drained_requests: u64,
    /// Whether the async block queue is enabled at runtime.
    pub blk_async_enabled: u64,
    /// Experimental RISC-V/default async queue depth cap.
    pub blk_async_depth: u64,
    /// Experimental LoongArch64 async queue depth cap.
    pub blk_async_la_depth: u64,
    /// Runtime async wait policy.
    pub blk_async_wait_policy: u64,
    /// Whether adaptive queue-depth tuning is enabled.
    pub blk_async_adaptive_enabled: u64,
    /// Current adaptive queue-depth cap.
    pub blk_async_adaptive_depth: u64,
    /// Number of adaptive depth increases.
    pub blk_async_adaptive_increases: u64,
    /// Number of adaptive depth decreases.
    pub blk_async_adaptive_decreases: u64,
    /// Number of successful completion events considered by adaptive tuning.
    pub blk_async_adaptive_good_events: u64,
    /// Number of queue-pressure events considered by adaptive tuning.
    pub blk_async_adaptive_pressure_events: u64,
    /// Whether async vectored-write request merging is enabled.
    pub blk_async_merge_write_enabled: u64,
    /// Number of vectored-write calls observed by the merge path.
    pub blk_async_merge_write_calls: u64,
    /// Number of input data segments offered to the merge path.
    pub blk_async_merge_write_input_segments: u64,
    /// Number of output block requests produced by the merge path.
    pub blk_async_merge_write_output_requests: u64,
    /// Number of block requests avoided versus one segment per request.
    pub blk_async_merge_write_saved_requests: u64,
    /// Maximum data segments allowed in one merged write request.
    pub blk_async_merge_write_max_segments: u64,
    /// Flush requests submitted through the async queue.
    pub blk_async_flush_requests: u64,
    /// Flush requests completed through the async queue.
    pub blk_async_flush_completions: u64,
    /// Number of async-capable operations forced through synchronous fallback.
    pub blk_async_fallback_sync: u64,
    /// Number of async submit batches.
    pub blk_async_submit_batches: u64,
    /// Number of requests submitted through async batches.
    pub blk_async_submit_requests: u64,
    /// Bytes submitted through async batches.
    pub blk_async_submit_bytes: u64,
    /// Number of batches that submitted only a prefix of requested work.
    pub blk_async_submit_partial_batches: u64,
    /// Number of completion drain batches for async requests.
    pub blk_async_completion_batches: u64,
    /// Number of async requests completed.
    pub blk_async_completed_requests: u64,
    /// Bytes completed through async requests.
    pub blk_async_completed_bytes: u64,
    /// Maximum observed async queue depth.
    pub blk_async_max_depth: u64,
    /// Current async queue depth at snapshot time.
    pub blk_async_current_depth: u64,
    /// Maximum observed VirtIO descriptor use by async requests.
    pub blk_async_desc_in_use_max: u64,
    /// Current descriptor budget exposed to async admission.
    pub blk_async_desc_budget: u64,
    /// Number of descriptor/request admission stalls.
    pub blk_async_admission_stalls: u64,
    /// Number of async queue-full events.
    pub blk_async_queue_full: u64,
    /// Number of async queue notify calls.
    pub blk_async_notify_calls: u64,
    /// Number of short-spin iterations in async waits.
    pub blk_async_wait_spins: u64,
    /// Number of async waits satisfied during the short-spin phase.
    pub blk_async_wait_spin_hits: u64,
    /// Number of async waits that yielded.
    pub blk_async_wait_yields: u64,
    /// Number of async waits that slept on a completion event.
    pub blk_async_wait_sleeps: u64,
    /// Number of async completion wakeups.
    pub blk_async_wait_wakeups: u64,
    /// Number of async wait timeout/fallback wakeups.
    pub blk_async_wait_timeouts: u64,
    /// Number of completion drains from the interrupt path.
    pub blk_async_interrupt_drains: u64,
    /// Number of no-timeout IRQ-first waits entered.
    pub blk_async_irq_first_waits: u64,
    /// Number of IRQ-first waits that fell back to the hybrid policy.
    pub blk_async_irq_first_fallbacks: u64,
    /// Number of devices armed for IRQ-first waits.
    pub blk_async_irq_first_arms: u64,
    /// Number of IRQ-first fallbacks because no usable IRQ wait was armed.
    pub blk_async_irq_first_fallback_unarmed: u64,
    /// Number of IRQ-first fallbacks because the current context cannot block.
    pub blk_async_irq_first_fallback_cannot_block: u64,
    /// Number of IRQ-first fallbacks because the block device has no IRQ.
    pub blk_async_irq_first_fallback_no_irq: u64,
    /// Number of IRQ-first fallbacks because IRQ handler registration failed.
    pub blk_async_irq_first_fallback_register_failed: u64,
    /// Number of IRQ-first fallbacks because the driver was built without IRQ support.
    pub blk_async_irq_first_fallback_feature_disabled: u64,
    /// Number of async submit errors.
    pub blk_async_submit_errors: u64,
    /// Number of async completion errors.
    pub blk_async_completion_errors: u64,
    /// Number of leaked async request resources detected.
    pub blk_async_resource_leaks: u64,
}

/// Runtime wait policy for the async block queue.
#[cfg(not(feature = "virtio-blk"))]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u64)]
pub enum AsyncBlockWaitPolicy {
    /// Drain, spin briefly, then yield/sleep through the shared completion path.
    Hybrid         = 0,
    /// Force submit-one/wait-one fallback through the owned-request path.
    Sync           = 1,
    /// Prefer IRQ wakeups and use the hybrid fallback unless IRQ wait is armed.
    InterruptFirst = 2,
}

/// Enables or disables VirtIO I/O counters.
pub fn set_virtio_io_counters_enabled(enabled: bool) {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::set_virtio_io_counters_enabled(enabled);
    #[cfg(not(feature = "virtio-blk"))]
    let _ = enabled;
}

/// Resets VirtIO I/O counters.
pub fn reset_virtio_io_counters() {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::reset_virtio_io_counters();
}

/// Enables or disables async block queue behavior for VirtIO block devices.
pub fn set_virtio_async_block_enabled(enabled: bool) {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::set_virtio_async_block_enabled(enabled);
    #[cfg(not(feature = "virtio-blk"))]
    let _ = enabled;
}

/// Returns whether async block queue behavior is enabled.
pub fn virtio_async_block_enabled() -> bool {
    #[cfg(feature = "virtio-blk")]
    {
        axdriver_virtio::virtio_async_block_enabled()
    }
    #[cfg(not(feature = "virtio-blk"))]
    {
        false
    }
}

/// Sets the default/RISC-V async block queue depth cap.
pub fn set_virtio_async_block_depth(depth: u64) {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::set_virtio_async_block_depth(depth);
    #[cfg(not(feature = "virtio-blk"))]
    let _ = depth;
}

/// Sets the LoongArch64 async block queue depth cap.
pub fn set_virtio_async_block_la_depth(depth: u64) {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::set_virtio_async_block_la_depth(depth);
    #[cfg(not(feature = "virtio-blk"))]
    let _ = depth;
}

/// Enables or disables adaptive async block queue-depth tuning.
pub fn set_virtio_async_block_adaptive_enabled(enabled: bool) {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::set_virtio_async_block_adaptive_enabled(enabled);
    #[cfg(not(feature = "virtio-blk"))]
    let _ = enabled;
}

/// Resets adaptive async block queue-depth state.
pub fn reset_virtio_async_block_adaptive_depth() {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::reset_virtio_async_block_adaptive_depth();
}

/// Enables or disables async vectored-write request merging.
pub fn set_virtio_async_block_merge_write_enabled(enabled: bool) {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::set_virtio_async_block_merge_write_enabled(enabled);
    #[cfg(not(feature = "virtio-blk"))]
    let _ = enabled;
}

/// Sets the async block wait policy.
pub fn set_virtio_async_block_wait_policy(policy: AsyncBlockWaitPolicy) {
    #[cfg(feature = "virtio-blk")]
    axdriver_virtio::set_virtio_async_block_wait_policy(policy);
    #[cfg(not(feature = "virtio-blk"))]
    let _ = policy;
}

/// Returns the async block wait policy.
pub fn virtio_async_block_wait_policy() -> AsyncBlockWaitPolicy {
    #[cfg(feature = "virtio-blk")]
    {
        axdriver_virtio::virtio_async_block_wait_policy()
    }
    #[cfg(not(feature = "virtio-blk"))]
    {
        AsyncBlockWaitPolicy::Hybrid
    }
}

/// Returns a snapshot of VirtIO I/O counters.
pub fn virtio_io_counters_snapshot() -> VirtioIoCounters {
    #[cfg(feature = "virtio-blk")]
    {
        axdriver_virtio::virtio_io_counters_snapshot()
    }
    #[cfg(not(feature = "virtio-blk"))]
    {
        VirtioIoCounters::default()
    }
}

/// A structure that contains all device drivers, organized by their category.
#[derive(Default)]
pub struct AllDevices {
    /// All network device drivers.
    #[cfg(feature = "net")]
    pub net: AxDeviceContainer<AxNetDevice>,
    /// All block device drivers.
    #[cfg(feature = "block")]
    pub block: AxDeviceContainer<AxBlockDevice>,
    /// All graphics device drivers.
    #[cfg(feature = "display")]
    pub display: AxDeviceContainer<AxDisplayDevice>,
    /// All input device drivers.
    #[cfg(feature = "input")]
    pub input: AxDeviceContainer<AxInputDevice>,
    /// All vsock device drivers.
    #[cfg(feature = "vsock")]
    pub vsock: AxDeviceContainer<AxVsockDevice>,
}

impl AllDevices {
    /// Returns the device model used, either `dyn` or `static`.
    ///
    /// See the [crate-level documentation](crate) for more details.
    pub const fn device_model() -> &'static str {
        if cfg!(feature = "dyn") {
            "dyn"
        } else {
            "static"
        }
    }

    /// Probes all supported devices.
    fn probe(&mut self) {
        #[cfg(feature = "dyn")]
        for dev in dyn_drivers::probe_all_devices() {
            self.add_device(dev);
        }
        #[cfg(not(feature = "dyn"))]
        {
            for_each_drivers!(type Driver, {
                if let Some(dev) = Driver::probe_global() {
                    info!(
                        "registered a new {:?} device: {:?}",
                        dev.device_type(),
                        dev.device_name(),
                    );
                    self.add_device(dev);
                }
            });

            self.probe_bus_devices();
        }
    }

    /// Adds one device into the corresponding container, according to its device category.
    #[allow(dead_code)]
    fn add_device(&mut self, dev: AxDeviceEnum) {
        match dev {
            #[cfg(feature = "net")]
            AxDeviceEnum::Net(dev) => self.net.push(dev),
            #[cfg(feature = "block")]
            AxDeviceEnum::Block(dev) => self.block.push(dev),
            #[cfg(feature = "display")]
            AxDeviceEnum::Display(dev) => self.display.push(dev),
            #[cfg(feature = "input")]
            AxDeviceEnum::Input(dev) => self.input.push(dev),
            #[cfg(feature = "vsock")]
            AxDeviceEnum::Vsock(dev) => self.vsock.push(dev),
        }
    }
}

/// Probes and initializes all device drivers, returns the [`AllDevices`] struct.
pub fn init_drivers() -> AllDevices {
    info!("Initialize device drivers...");
    info!("  device model: {}", AllDevices::device_model());

    let mut all_devs = AllDevices::default();
    all_devs.probe();

    #[cfg(feature = "net")]
    {
        debug!("number of NICs: {}", all_devs.net.len());
        for (i, dev) in all_devs.net.iter().enumerate() {
            assert_eq!(dev.device_type(), DeviceType::Net);
            debug!("  NIC {}: {:?}", i, dev.device_name());
        }
    }
    #[cfg(feature = "block")]
    {
        debug!("number of block devices: {}", all_devs.block.len());
        for (i, dev) in all_devs.block.iter().enumerate() {
            assert_eq!(dev.device_type(), DeviceType::Block);
            debug!("  block device {}: {:?}", i, dev.device_name());
        }
    }
    #[cfg(feature = "display")]
    {
        debug!("number of graphics devices: {}", all_devs.display.len());
        for (i, dev) in all_devs.display.iter().enumerate() {
            assert_eq!(dev.device_type(), DeviceType::Display);
            debug!("  graphics device {}: {:?}", i, dev.device_name());
        }
    }
    #[cfg(feature = "input")]
    {
        debug!("number of input devices: {}", all_devs.input.len());
        for (i, dev) in all_devs.input.iter().enumerate() {
            assert_eq!(dev.device_type(), DeviceType::Input);
            debug!("  input device {}: {:?}", i, dev.device_name());
        }
    }
    #[cfg(feature = "vsock")]
    {
        debug!("number of vsock devices: {}", all_devs.vsock.len());
        for (i, dev) in all_devs.vsock.iter().enumerate() {
            assert_eq!(dev.device_type(), DeviceType::Vsock);
            debug!("  vsock device {}: {:?}", i, dev.device_name());
        }
    }

    all_devs
}
