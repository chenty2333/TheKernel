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
                retry_attempts: 0,
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
    retry_attempts: u8,
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
    const PORT_SPEED_FULL: u32 = 1 << 10;
    const PORT_POWER: u32 = 1 << 9;
    const PORT_WARM_RESET: u32 = 1 << 31;

    struct ResetCompletingKernel {
        portsc: AtomicUsize,
        reset_waits: AtomicUsize,
        reset_remaining: AtomicUsize,
        connect_after_power_settle: AtomicBool,
    }

    static TEST_KERNEL: ResetCompletingKernel = ResetCompletingKernel {
        portsc: AtomicUsize::new(0),
        reset_waits: AtomicUsize::new(0),
        reset_remaining: AtomicUsize::new(0),
        connect_after_power_settle: AtomicBool::new(true),
    };
    static TEST_LOCKED: AtomicBool = AtomicBool::new(false);

    struct TestLock;

    impl TestLock {
        fn lock() -> Self {
            while TEST_LOCKED
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                core::hint::spin_loop();
            }
            Self
        }
    }

    impl Drop for TestLock {
        fn drop(&mut self) {
            TEST_LOCKED.store(false, Ordering::Release);
        }
    }

    impl KernelOp for ResetCompletingKernel {
        fn delay(&self, duration: Duration) {
            let address = self.portsc.load(Ordering::Acquire);
            if address == 0 {
                return;
            }
            if duration == PORT_POWER_SETTLE {
                if !self.connect_after_power_settle.load(Ordering::Acquire) {
                    return;
                }
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
                if status & (PORT_RESET | PORT_WARM_RESET) != 0
                    && self.reset_remaining.fetch_sub(1, Ordering::AcqRel) <= 1
                {
                    portsc
                        .write_volatile((status & !(PORT_RESET | PORT_WARM_RESET)) | PORT_ENABLED);
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
        let _test_lock = TestLock::lock();
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
        TEST_KERNEL
            .connect_after_power_settle
            .store(true, Ordering::Release);

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

        TEST_KERNEL.reset_remaining.store(50, Ordering::Relaxed);
        let retry = block_on_ready(hub.retry_connected_port(1)).unwrap();
        assert!(matches!(retry, Some(change) if change.root_port_id == 1));
        assert_eq!(TEST_KERNEL.reset_waits.load(Ordering::Relaxed), 100);
        assert!(
            block_on_ready(hub.retry_connected_port(1))
                .unwrap()
                .is_none()
        );

        TEST_KERNEL.portsc.store(0, Ordering::Release);
    }

    #[test]
    fn root_port_absent_at_boot_is_reset_before_a_later_hotplug_is_enumerated() {
        let _test_lock = TestLock::lock();
        #[repr(align(64))]
        struct FakeXhci([u32; 320]);

        let mut regs = Box::new(FakeXhci([0; 320]));
        regs.0[0] = 0x40;
        regs.0[1] = (1 << 24) | 1;
        regs.0[TEST_PORTSC_OFFSET / 4] = PORT_SPEED_FULL;
        TEST_KERNEL.portsc.store(
            (&mut regs.0[TEST_PORTSC_OFFSET / 4] as *mut u32) as usize,
            Ordering::Release,
        );
        TEST_KERNEL.reset_waits.store(0, Ordering::Relaxed);
        TEST_KERNEL.reset_remaining.store(3, Ordering::Relaxed);
        TEST_KERNEL
            .connect_after_power_settle
            .store(false, Ordering::Release);

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
        assert!(block_on_ready(hub.changed_ports()).unwrap().is_empty());
        assert_eq!(hub.ports()[0].state, PortState::Uninit);

        // The cable arrives after the boot scan. The hotplug scan must issue
        // PR and wait for its hardware completion before publishing Connected.
        regs.0[TEST_PORTSC_OFFSET / 4] = PORT_POWER | PORT_CONNECT | PORT_SPEED_FULL;
        let connected = block_on_ready(hub.changed_ports()).unwrap();
        assert_eq!(TEST_KERNEL.reset_waits.load(Ordering::Relaxed), 3);
        assert_ne!(regs.0[TEST_PORTSC_OFFSET / 4] & PORT_ENABLED, 0);
        assert!(matches!(
            connected.as_slice(),
            [PortEvent::Connected(change)] if change.root_port_id == 1
        ));

        // A disconnect returns the port to Uninit, so a later device receives
        // a new reset rather than reusing stale Reseted state.
        regs.0[TEST_PORTSC_OFFSET / 4] = PORT_POWER | PORT_SPEED_FULL;
        let disconnected = block_on_ready(hub.changed_ports()).unwrap();
        assert!(matches!(
            disconnected.as_slice(),
            [PortEvent::Disconnected { port_id: 1 }]
        ));
        regs.0[TEST_PORTSC_OFFSET / 4] = PORT_POWER | PORT_CONNECT | PORT_SPEED_FULL;
        TEST_KERNEL.reset_remaining.store(1, Ordering::Relaxed);
        let reconnected = block_on_ready(hub.changed_ports()).unwrap();
        assert_eq!(TEST_KERNEL.reset_waits.load(Ordering::Relaxed), 4);
        assert!(matches!(
            reconnected.as_slice(),
            [PortEvent::Connected(change)] if change.root_port_id == 1
        ));

        TEST_KERNEL.portsc.store(0, Ordering::Release);
        TEST_KERNEL
            .connect_after_power_settle
            .store(true, Ordering::Release);
    }
}

impl HubOp for XhciRootHub {
    fn changed_ports(&mut self) -> BoxFuture<'_, Result<Vec<PortEvent>, USBError>> {
        self._changed_ports().boxed()
    }

    fn retry_connected_port(
        &mut self,
        port_id: u8,
    ) -> BoxFuture<'_, Result<Option<PortChangeInfo>, USBError>> {
        self._retry_connected_port(port_id).boxed()
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
                    self.ports_mut()[idx].state = PortState::ResetFailed;
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

    async fn _retry_connected_port(
        &mut self,
        port_id: u8,
    ) -> Result<Option<PortChangeInfo>, USBError> {
        let Some(index) = port_id.checked_sub(1).map(usize::from) else {
            return Err(USBError::InvalidParameter);
        };
        if index >= self.portsc.len() {
            return Err(USBError::InvalidParameter);
        }
        if self.ports()[index].state != PortState::Probed || self.ports()[index].retry_attempts != 0
        {
            return Ok(None);
        }

        let before = self.portsc.read_volatile_at(index);
        if !before.current_connect_status() {
            return Ok(None);
        }
        self.ports_mut()[index].retry_attempts = 1;
        let warm_reset = matches!(before.port_speed(), 4 | 5);
        self.portsc.update_volatile_at(index, |portsc| {
            portsc.set_0_port_enabled_disabled();
            if warm_reset {
                // USB 3.x recovery uses WPR; initial enumeration above uses
                // the normal PR signal. Do not conflate the two controls.
                portsc.set_warm_port_reset();
            } else {
                portsc.set_port_reset();
            }
        });

        for _ in 0..PORT_RESET_POLLS {
            let status = self.portsc.read_volatile_at(index);
            let reset_active = if warm_reset {
                status.warm_port_reset()
            } else {
                status.port_reset()
            };
            if !reset_active {
                if status.current_connect_status() && status.port_enabled_disabled() {
                    info!("xhci: port {port_id} recovered after one bounded reset retry");
                    return Ok(Some(PortChangeInfo {
                        root_port_id: port_id,
                        port_id,
                        port_speed: Speed::from_xhci_portsc(status.port_speed()),
                    }));
                }
                warn!(
                    "xhci: port {port_id} retry reset completed without an enabled device \
                     PORTSC={status:?}"
                );
                return Ok(None);
            }
            self.kernel.delay(PORT_RESET_POLL);
        }

        let status = self.portsc.read_volatile_at(index);
        warn!("xhci: port {port_id} bounded retry reset timed out PORTSC={status:?}");
        Ok(None)
    }

    fn handle_disconnected(&mut self) -> Vec<PortEvent> {
        let disconnected = self
            .ports()
            .iter()
            .filter(|port| matches!(port.state, PortState::Probed | PortState::ResetFailed))
            .filter_map(|port| {
                let index = usize::from(port.port_id - 1);
                (!self.portsc.read_volatile_at(index).current_connect_status())
                    .then_some(port.port_id)
            })
            .collect::<Vec<_>>();
        for port_id in &disconnected {
            let port = &mut self.ports_mut()[usize::from(*port_id - 1)];
            port.state = PortState::Uninit;
            port.retry_attempts = 0;
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
            let i = (id - 1) as usize;
            let before = self.portsc.read_volatile_at(i);
            if !before.current_connect_status() {
                // An absent port is still eligible for a future connection.
                // Leave it Uninit so the first later attachment gets a reset.
                continue;
            }

            // USB 2.x attachment uses PR; a connected USB 3.x link uses WPR.
            // Both are hardware-completed asynchronously and must be observed
            // before the Core attempts Address Device at address zero.
            let warm_reset = matches!(before.port_speed(), 4 | 5);
            let reset_active = if warm_reset {
                before.warm_port_reset()
            } else {
                before.port_reset()
            };
            if !reset_active {
                info!("xhci: port {id} hotplug connected; starting bounded reset");
                self.portsc.update_volatile_at(i, |portsc| {
                    portsc.set_0_port_enabled_disabled();
                    if warm_reset {
                        portsc.set_warm_port_reset();
                    } else {
                        portsc.set_port_reset();
                    }
                });
            }

            let mut completed = false;
            for _ in 0..PORT_RESET_POLLS {
                let status = self.portsc.read_volatile_at(i);
                let reset_active = if warm_reset {
                    status.warm_port_reset()
                } else {
                    status.port_reset()
                };
                if reset_active {
                    self.kernel.delay(PORT_RESET_POLL);
                    continue;
                }
                if status.current_connect_status() && status.port_enabled_disabled() {
                    self.ports_mut()[i].state = PortState::Reseted;
                    info!("xhci: port {id} hotplug reset complete; device enabled");
                } else {
                    self.ports_mut()[i].state = PortState::ResetFailed;
                    warn!(
                        "xhci: port {id} hotplug reset completed without an enabled device \
                         PORTSC={status:?}"
                    );
                }
                completed = true;
                break;
            }
            if !completed {
                let status = self.portsc.read_volatile_at(i);
                self.ports_mut()[i].state = PortState::ResetFailed;
                warn!(
                    "xhci: port {id} hotplug reset timed out; waiting for unplug before retry \
                     PORTSC={status:?}"
                );
            }
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
            if !portsc.current_connect_status() {
                self.ports_mut()[i].state = PortState::Uninit;
                self.ports_mut()[i].retry_attempts = 0;
                continue;
            }
            if !portsc.port_enabled_disabled() {
                self.ports_mut()[i].state = PortState::Uninit;
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
