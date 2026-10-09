//! xHCI Root Hub 实现
//!
//! 实现 xHCI 控制器的 Root Hub 功能，遵循 xHCI 规范第 4.19 章。

use alloc::{sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

use futures::{FutureExt, future::BoxFuture, task::AtomicWaker};
use usb_if::{err::USBError, host::hub::Speed};

use super::reg::{MemMapper, PortStatusRegisters, XhciRegisters};
use crate::{
    backend::kmod::hub::{HubInfo, HubOp, PortChangeInfo, PortEvent, PortState},
    osal::Kernel,
};

const PORT_POWER_SETTLE: core::time::Duration = core::time::Duration::from_millis(100);
const PORT_RESET_POLL: core::time::Duration = core::time::Duration::from_millis(1);
const PORT_RESET_POLLS: usize = 1_000;

pub struct PortChangeWaker {
    ports: Arc<UnsafeCell<Vec<Port>>>,
}

unsafe impl Send for PortChangeWaker {}
unsafe impl Sync for PortChangeWaker {}

impl PortChangeWaker {
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(port_num: u8) -> Self {
        let mut ports = Vec::with_capacity(port_num as usize);
        for i in 0..port_num {
            ports.push(Port {
                port_id: i + 1,
                change_waker: AtomicWaker::new(),
                changed: AtomicBool::new(false),
                state: PortState::Uninit,
            });
        }
        Self {
            ports: Arc::new(UnsafeCell::new(ports)),
        }
    }

    pub fn set_port_changed(&self, port_id: u8) {
        let ports = unsafe { &*self.ports.get() };
        let Some(idx) = port_id.checked_sub(1).map(usize::from) else {
            warn!("xhci: ignoring invalid port status event port={port_id}");
            return;
        };
        let Some(port) = ports.get(idx) else {
            warn!("xhci: ignoring out-of-range port status event port={port_id}");
            return;
        };
        debug!("Setting port {} changed", port_id);
        port.changed.store(true, Ordering::Release);
        port.change_waker.wake();
    }
}

pub struct Port {
    port_id: u8,
    change_waker: AtomicWaker,
    changed: AtomicBool,
    state: PortState,
}

/// xHCI Root Hub
///
/// Root Hub 是集成在 xHCI 控制器中的虚拟 Hub。
pub struct XhciRootHub {
    portsc: PortStatusRegisters<MemMapper>,
    kernel: Kernel,
    ports: Arc<UnsafeCell<Vec<Port>>>,
}

unsafe impl Send for XhciRootHub {}

impl XhciRootHub {
    fn ports(&self) -> &[Port] {
        unsafe { &*self.ports.get() }
    }

    fn ports_mut(&mut self) -> &mut [Port] {
        unsafe { &mut *self.ports.get() }
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::{
        alloc::Layout,
        future::Future,
        num::NonZeroUsize,
        pin::pin,
        ptr::NonNull,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
        time::Duration,
    };

    use dma_api::{
        DmaAllocHandle, DmaCoherency, DmaConstraints, DmaDeviceInfo, DmaDirection, DmaDomainId,
        DmaError, DmaMapHandle, DmaOp,
    };
    use usb_if::host::hub::Speed;

    use super::*;
    use crate::{
        backend::kmod::hub::{HubInfo, HubOp, PortEvent, UsbTt},
        osal::KernelOp,
    };

    const TEST_PORTSC_OFFSET: usize = 0x440;
    const PORT_CONNECT: u32 = 1 << 0;
    const PORT_ENABLED: u32 = 1 << 1;
    const PORT_RESET: u32 = 1 << 4;
    const PORT_POWER: u32 = 1 << 9;

    struct ResetCompletingKernel {
        portsc: AtomicUsize,
        reset_waits: AtomicUsize,
        reset_remaining: AtomicUsize,
    }

    static TEST_KERNEL: ResetCompletingKernel = ResetCompletingKernel {
        portsc: AtomicUsize::new(0),
        reset_waits: AtomicUsize::new(0),
        reset_remaining: AtomicUsize::new(0),
    };

    impl KernelOp for ResetCompletingKernel {
        fn delay(&self, duration: Duration) {
            let address = self.portsc.load(Ordering::Acquire);
            if address == 0 {
                return;
            }
            if duration == PORT_POWER_SETTLE {
                // The device's connect status arrives after xHCI has restored
                // port power, rather than being visible at the first read.
                // SAFETY: the test installs a live aligned u32 in its fake
                // xHCI MMIO allocation before root-hub initialization.
                unsafe {
                    let portsc = address as *mut u32;
                    portsc.write_volatile(portsc.read_volatile() | PORT_CONNECT);
                }
                return;
            }
            if duration != PORT_RESET_POLL {
                return;
            }
            self.reset_waits.fetch_add(1, Ordering::Relaxed);
            // Model xHC-owned completion of a port reset during the bounded
            // wait. xHC holds PR for the USB reset signaling interval (about
            // 50 ms) and clears it when the port is enabled. This is the race
            // the previous immediate one-shot scan missed.
            // SAFETY: the test installs a live aligned u32 in its fake xHCI
            // MMIO allocation before calling the root-hub initialization.
            unsafe {
                let portsc = address as *mut u32;
                let status = portsc.read_volatile();
                if status & PORT_RESET != 0
                    && self.reset_remaining.fetch_sub(1, Ordering::AcqRel) <= 1
                {
                    portsc.write_volatile((status & !PORT_RESET) | PORT_ENABLED);
                }
            }
        }
    }

    impl DmaOp for ResetCompletingKernel {
        fn page_size(&self) -> usize {
            4096
        }

        unsafe fn alloc_contiguous(&self, _: DmaConstraints, _: Layout) -> Option<DmaAllocHandle> {
            None
        }

        unsafe fn dealloc_contiguous(&self, _: DmaAllocHandle) {}

        unsafe fn alloc_coherent(&self, _: DmaConstraints, _: Layout) -> Option<DmaAllocHandle> {
            None
        }

        unsafe fn dealloc_coherent(&self, _: DmaAllocHandle) -> Result<(), DmaError> {
            Ok(())
        }

        unsafe fn map_streaming(
            &self,
            _: DmaConstraints,
            _: NonNull<u8>,
            _: NonZeroUsize,
            _: DmaDirection,
        ) -> Result<DmaMapHandle, DmaError> {
            Err(DmaError::NoMemory)
        }

        unsafe fn unmap_streaming(&self, _: DmaMapHandle) {}
    }

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("root-hub test future unexpectedly pending"),
        }
    }

    #[test]
    fn boot_probe_observes_a_device_after_the_async_port_reset_completes() {
        #[repr(align(64))]
        struct FakeXhci([u32; 320]);

        let mut regs = Box::new(FakeXhci([0; 320]));
        regs.0[0] = 0x40; // CAPLENGTH
        regs.0[1] = (1 << 24) | 1; // one device slot and one root port
        regs.0[TEST_PORTSC_OFFSET / 4] = 1 << 10; // initially disconnected, full speed
        TEST_KERNEL.portsc.store(
            (&mut regs.0[TEST_PORTSC_OFFSET / 4] as *mut u32) as usize,
            Ordering::Release,
        );
        TEST_KERNEL.reset_waits.store(0, Ordering::Relaxed);
        TEST_KERNEL.reset_remaining.store(50, Ordering::Relaxed);

        let mmio = NonNull::new(regs.0.as_mut_ptr().cast::<u8>()).unwrap();
        let kernel = Kernel::new(
            DmaDeviceInfo::new(
                DmaDomainId::Direct,
                DmaCoherency::Coherent,
                DmaConstraints::new(u64::MAX),
            ),
            &TEST_KERNEL,
        );
        let mut hub = XhciRootHub::new(XhciRegisters::new(mmio), kernel).unwrap();
        let info = HubInfo {
            parent: None,
            slot_id: 0,
            hub_depth: -1,
            speed: Speed::Full,
            port_id: 0,
            tt: UsbTt {
                multi: false,
                think_time_ns: 0,
            },
        };

        block_on_ready(hub.init(info)).unwrap();
        let changes = block_on_ready(hub.changed_ports()).unwrap();

        assert!(TEST_KERNEL.reset_waits.load(Ordering::Relaxed) >= 50);
        assert_ne!(regs.0[TEST_PORTSC_OFFSET / 4] & PORT_POWER, 0);
        assert!(matches!(
            changes.as_slice(),
            [PortEvent::Connected(change)] if change.root_port_id == 1
        ));

        TEST_KERNEL.portsc.store(0, Ordering::Release);
    }
}

impl HubOp for XhciRootHub {
    fn changed_ports(&mut self) -> BoxFuture<'_, Result<Vec<PortEvent>, USBError>> {
        self._changed_ports().boxed()
    }

    fn init(&mut self, info: HubInfo) -> BoxFuture<'_, Result<HubInfo, USBError>> {
        async {
            let mut info = info;
            info.speed = Speed::SuperSpeedPlus;
            info!("xhci: initializing {} root ports", self.portsc.len());

            for idx in 0..self.portsc.len() {
                self.portsc.update_volatile_at(idx, |portsc| {
                    // Reassert power after firmware handoff even when PP
                    // reads high, matching Linux's root-hub power-on path.
                    trace!("Powering on port {}", idx + 1);
                    portsc.set_port_power();
                });
            }

            // Port power and connection status are not necessarily observable
            // immediately after setting PP (for example, after firmware has
            // handed over an integrated xHCI controller).  The previous
            // one-shot scan reset ports and immediately scanned them, so a
            // normal in-progress PR bit made every attached device disappear
            // from the boot-time enumeration pass.
            self.kernel.delay(PORT_POWER_SETTLE);

            let mut resetting = Vec::new();
            for idx in 0..self.portsc.len() {
                let status = self.portsc.read_volatile_at(idx);
                if !status.current_connect_status() {
                    continue;
                }
                // Initial enumeration uses the normal xHCI port reset (PR).
                // Warm reset (WPR) is a separate USB 3 link-recovery signal,
                // not a substitute for the initial attach reset.
                info!(
                    "xhci: port {} connected speed={} powered={}; starting reset",
                    idx + 1,
                    status.port_speed(),
                    status.port_power()
                );
                self.portsc.update_volatile_at(idx, |portsc| {
                    portsc.set_0_port_enabled_disabled();
                    portsc.set_port_reset();
                });
                resetting.push(idx);
            }

            // xHCI completes port resets asynchronously.  Wait for the
            // hardware-owned PR bit to clear before the single boot scan;
            // bound each controller-wide wait so a bad port cannot stall
            // platform bring-up forever.
            for _ in 0..PORT_RESET_POLLS {
                let mut pending = Vec::new();
                for idx in core::mem::take(&mut resetting) {
                    let status = self.portsc.read_volatile_at(idx);
                    if status.port_reset() {
                        pending.push(idx);
                    } else {
                        self.ports_mut()[idx].state = PortState::Reseted;
                        info!(
                            "xhci: port {} reset complete connected={} enabled={} speed={}",
                            idx + 1,
                            status.current_connect_status(),
                            status.port_enabled_disabled(),
                            status.port_speed()
                        );
                    }
                }
                if pending.is_empty() {
                    break;
                }
                resetting = pending;
                self.kernel.delay(PORT_RESET_POLL);
            }
            for &idx in &resetting {
                let status = self.portsc.read_volatile_at(idx);
                if status.port_reset() {
                    warn!(
                        "xhci: port {} reset timed out; skipping it for boot enumeration \
                         PORTSC={:?}",
                        idx + 1,
                        status
                    );
                }
            }

            Ok(info)
        }
        .boxed()
    }

    fn slot_id(&self) -> u8 {
        0
    }
}

impl XhciRootHub {
    /// 创建新的 xHCI Root Hub
    pub fn new(reg: XhciRegisters, kernel: Kernel) -> Result<Self, USBError> {
        let portsc = reg.port_status_registers();
        let port_num = portsc.len();
        let ports = PortChangeWaker::new(port_num as _).ports.clone();

        Ok(Self {
            portsc,
            kernel,
            ports,
        })
    }

    pub fn waker(&self) -> PortChangeWaker {
        PortChangeWaker {
            ports: self.ports.clone(),
        }
    }

    async fn _changed_ports(&mut self) -> Result<Vec<PortEvent>, USBError> {
        let mut events = self.handle_disconnected();
        self.handle_uninit().await?;
        events.extend(
            self.handle_reseted()
                .await?
                .into_iter()
                .map(PortEvent::Connected),
        );
        Ok(events)
    }

    fn handle_disconnected(&mut self) -> Vec<PortEvent> {
        let disconnected = self
            .ports()
            .iter()
            .filter(|port| matches!(port.state, PortState::Probed))
            .filter_map(|port| {
                let index = usize::from(port.port_id - 1);
                (!self.portsc.read_volatile_at(index).current_connect_status())
                    .then_some(port.port_id)
            })
            .collect::<Vec<_>>();
        for port_id in &disconnected {
            self.ports_mut()[usize::from(*port_id - 1)].state = PortState::Uninit;
        }
        disconnected
            .into_iter()
            .map(|port_id| PortEvent::Disconnected { port_id })
            .collect()
    }

    async fn handle_uninit(&mut self) -> Result<(), USBError> {
        let uninited = self
            .ports()
            .iter()
            .filter(|port| matches!(port.state, PortState::Uninit))
            .map(|p| p.port_id)
            .collect::<Vec<_>>();

        for &id in &uninited {
            debug!("Waiting for port {id} reset ...");
            let i = (id - 1) as usize;

            let port = self.portsc.read_volatile_at(i);

            if port.port_reset() {
                continue;
            }

            debug!(
                "Port {} reset complete, enable={}, connect={}",
                id,
                port.port_enabled_disabled(),
                port.current_connect_status()
            );

            self.ports_mut()[i].state = PortState::Reseted;
        }

        Ok(())
    }

    async fn handle_reseted(&mut self) -> Result<Vec<PortChangeInfo>, USBError> {
        let reseted = self
            .ports()
            .iter()
            .filter(|port| matches!(port.state, PortState::Reseted))
            .map(|p| p.port_id)
            .collect::<Vec<_>>();

        let mut out = Vec::new();

        for &id in &reseted {
            let i = (id - 1) as usize;
            let portsc = self.portsc.read_volatile_at(i);
            if !portsc.current_connect_status() || !portsc.port_enabled_disabled() {
                continue;
            }
            let speed_raw = portsc.port_speed();
            let speed = Speed::from_xhci_portsc(speed_raw);
            debug!("Port {} device connected at speed {:?}", id, speed);
            debug!("Port {} : \r\n {:?}", id, portsc);
            self.ports_mut()[i].state = PortState::Probed;

            out.push(PortChangeInfo {
                root_port_id: id,
                port_id: id,
                port_speed: speed,
            });
        }

        Ok(out)
    }
}
