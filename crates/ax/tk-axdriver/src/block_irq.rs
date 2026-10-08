//! PCI message/INTx completion notification for block frontends.
//!
//! Controllers supply a bounded status-acknowledgment function over their
//! permanently mapped register window. The platform callback acknowledges the
//! device first, then increments a generation and invokes the optional block
//! completion notifier. MSI-X is preferred, MSI is second, and firmware-routed
//! shared INTx is the final interrupt mode; callers retain polling when no
//! route can be admitted.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use axdriver_block::BlockCompletionNotifier;
use axdriver_pci::{BarInfo, DeviceFunction, PciRoot};
use spin::Mutex;

/// Device-specific bounded IRQ acknowledgment. Return true only for a cause
/// owned by this controller.
pub type Acknowledger = fn(usize) -> bool;
type Notifier = fn(usize);

const ENDPOINT_COUNT: usize = 16;
const PCI_CAP_MSI: u8 = 0x05;
const PCI_CAP_MSIX: u8 = 0x11;
const PCI_CAP_LIMIT: u8 = 0xf4;
const PCI_COMMAND: u8 = 0x04;
const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;
const PCI_INTERRUPT_LINE_PIN: u8 = 0x3c;
const IO_APIC_VECTOR_BASE: u32 = 0x20;
const NOTIFIER_COUNT: usize = 8;

struct NotifierSlot {
    callback: AtomicUsize,
    context: AtomicUsize,
}

impl NotifierSlot {
    const fn new() -> Self {
        Self {
            callback: AtomicUsize::new(0),
            context: AtomicUsize::new(0),
        }
    }
}

struct Endpoint {
    active: AtomicBool,
    readers: AtomicUsize,
    vector: AtomicUsize,
    context: AtomicUsize,
    acknowledge: AtomicUsize,
    generation: AtomicU64,
    notifiers: [NotifierSlot; NOTIFIER_COUNT],
}

impl Endpoint {
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
            vector: AtomicUsize::new(0),
            context: AtomicUsize::new(0),
            acknowledge: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            notifiers: [const { NotifierSlot::new() }; NOTIFIER_COUNT],
        }
    }
}

static ENDPOINTS: [Endpoint; ENDPOINT_COUNT] = [const { Endpoint::new() }; ENDPOINT_COUNT];
static NEXT_ENDPOINT: AtomicUsize = AtomicUsize::new(0);
static ADMISSION: Mutex<()> = Mutex::new(());

fn reserve(context: usize, acknowledge: Acknowledger) -> Option<usize> {
    let _admission = ADMISSION.lock();
    let index = NEXT_ENDPOINT.fetch_add(1, Ordering::AcqRel);
    let endpoint = ENDPOINTS.get(index)?;
    endpoint.context.store(context, Ordering::Relaxed);
    endpoint
        .acknowledge
        .store(acknowledge as usize, Ordering::Relaxed);
    Some(index)
}

fn deliver(index: usize, vector: usize) -> bool {
    let Some(endpoint) = ENDPOINTS.get(index) else {
        return false;
    };
    if !endpoint.active.load(Ordering::Acquire) {
        return false;
    }
    endpoint.readers.fetch_add(1, Ordering::SeqCst);
    if !endpoint.active.load(Ordering::SeqCst) || endpoint.vector.load(Ordering::Acquire) != vector
    {
        endpoint.readers.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    let context = endpoint.context.load(Ordering::Acquire);
    let acknowledge = endpoint.acknowledge.load(Ordering::Acquire);
    if context == 0 || acknowledge == 0 {
        endpoint.readers.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    // SAFETY: reserve publishes a function pointer with the matching ABI and
    // the hardware window remains owned for the endpoint's boot lifetime.
    let acknowledge = unsafe { core::mem::transmute::<usize, Acknowledger>(acknowledge) };
    if !acknowledge(context) {
        endpoint.readers.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    endpoint.generation.fetch_add(1, Ordering::Release);
    for slot in &endpoint.notifiers {
        let notifier = slot.callback.load(Ordering::Acquire);
        if notifier != 0 {
            let notifier_context = slot.context.load(Ordering::Acquire);
            // SAFETY: the bounded callback ABI/context pair is published by
            // `install_completion_notifier`.
            let notifier = unsafe { core::mem::transmute::<usize, Notifier>(notifier) };
            notifier(notifier_context);
        }
    }
    endpoint.readers.fetch_sub(1, Ordering::AcqRel);
    true
}

fn dispatch_intx(vector: usize) -> bool {
    let mut owned = false;
    for index in 0..NEXT_ENDPOINT.load(Ordering::Acquire).min(ENDPOINT_COUNT) {
        owned |= deliver(index, vector);
    }
    owned
}

macro_rules! direct_handlers {
    ($($index:literal => $name:ident),+ $(,)?) => {
        $(fn $name() { let _ = deliver($index, ENDPOINTS[$index].vector.load(Ordering::Acquire)); })+
        const DIRECT_HANDLERS: [fn(); ENDPOINT_COUNT] = [$($name),+];
    };
}

direct_handlers!(
    0 => irq0, 1 => irq1, 2 => irq2, 3 => irq3, 4 => irq4, 5 => irq5,
    6 => irq6, 7 => irq7, 8 => irq8, 9 => irq9, 10 => irq10, 11 => irq11,
    12 => irq12, 13 => irq13, 14 => irq14, 15 => irq15,
);

/// Admitted interrupt delivery mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockIrqMode {
    /// One function-wide MSI-X table entry.
    Msix,
    /// One-message MSI.
    Msi,
    /// Firmware-routed level-triggered INTx.
    Intx,
}

/// Lifetime token for one PCI block interrupt endpoint. The endpoint uses a
/// permanently mapped controller window and a fixed IRQ slot; drivers must
/// mask their device source before dropping it or tearing down the BAR.
#[derive(Clone)]
pub struct PciBlockInterrupt {
    endpoint: usize,
    vector: usize,
    mode: BlockIrqMode,
    bdf: DeviceFunction,
    capability: Option<u8>,
    msix_table_entry: Option<usize>,
}

impl PciBlockInterrupt {
    /// Installs an interrupt endpoint and selects MSI-X, MSI, then routed
    /// INTx. `acknowledge` must clear the device cause and return true only for
    /// an owned completion. The status window/context must remain valid until
    /// device interrupts are masked and this endpoint is quiescent.
    pub fn register(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        context: usize,
        acknowledge: Acknowledger,
    ) -> Option<Self> {
        let endpoint = reserve(context, acknowledge)?;
        Self::register_msix(root, bdf, endpoint, 0)
            .or_else(|| Self::register_msi(root, bdf, endpoint))
            .or_else(|| Self::register_intx(root, bdf, endpoint))
    }

    /// Registers one SDHCI slot on its corresponding MSI-X table entry. Only
    /// slot zero may fall back to the function-wide MSI or INTx route.
    pub fn register_slot(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        table_index: usize,
        context: usize,
        acknowledge: Acknowledger,
    ) -> Option<Self> {
        let endpoint = reserve(context, acknowledge)?;
        Self::register_msix(root, bdf, endpoint, table_index).or_else(|| {
            (table_index == 0)
                .then(|| Self::register_msi(root, bdf, endpoint))
                .flatten()
                .or_else(|| {
                    (table_index == 0)
                        .then(|| Self::register_intx(root, bdf, endpoint))
                        .flatten()
                })
        })
    }

    fn register_msix(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        endpoint: usize,
        table_index: usize,
    ) -> Option<Self> {
        Self::disable_capability(root, bdf, PCI_CAP_MSIX);
        let capability = root
            .capabilities(bdf)
            .take(48)
            .find(|cap| cap.id == PCI_CAP_MSIX && cap.offset <= PCI_CAP_LIMIT)?;
        let control = capability.private_header;
        let table_info = root.read_config_dword(bdf, capability.offset.checked_add(4)?)?;
        let table_bir = (table_info & 7) as u8;
        let table_offset = (table_info & !7) as usize;
        let vectors = usize::from(control & 0x07ff) + 1;
        if table_index >= vectors {
            return None;
        }
        let table_bytes = vectors.checked_mul(16)?;
        let BarInfo::Memory { address, size, .. } = root.bar_info(bdf, table_bir).ok()? else {
            return None;
        };
        if address == 0 || table_offset.checked_add(table_bytes)? > usize::try_from(size).ok()? {
            return None;
        }
        let base = axklib::mem::iomap((address as usize).into(), usize::try_from(size).ok()?)
            .ok()?
            .as_usize();
        let table = base
            .checked_add(table_offset)?
            .checked_add(table_index.checked_mul(16)?)?;
        let (message, data, vector) = axhal::irq::allocate_msi(DIRECT_HANDLERS[endpoint])?;
        endpoint_publish_vector(endpoint, vector);
        if !root.write_config_u16(bdf, capability.offset + 2, control | 0xc000) {
            return None;
        }
        // SAFETY: the complete table was bounded within its mapped memory BAR;
        // function masking prevents delivery before entry zero is configured.
        unsafe {
            (table as *mut u32).write_volatile(message as u32);
            ((table + 4) as *mut u32).write_volatile((message >> 32) as u32);
            ((table + 8) as *mut u32).write_volatile(data);
            ((table + 12) as *mut u32).write_volatile(1);
            let _ = ((table + 12) as *const u32).read_volatile();
            ((table + 12) as *mut u32).write_volatile(0);
        }
        if !root.write_config_u16(bdf, capability.offset + 2, (control | 0x8000) & !0x4000) {
            unsafe { ((table + 12) as *mut u32).write_volatile(1) };
            Self::disable_capability(root, bdf, PCI_CAP_MSIX);
            return None;
        }
        endpoint_activate(endpoint);
        Some(Self {
            endpoint,
            vector,
            mode: BlockIrqMode::Msix,
            bdf,
            capability: Some(capability.offset),
            msix_table_entry: Some(table),
        })
    }

    fn register_msi(root: &mut PciRoot, bdf: DeviceFunction, endpoint: usize) -> Option<Self> {
        Self::disable_capability(root, bdf, PCI_CAP_MSI);
        let capability = root
            .capabilities(bdf)
            .take(48)
            .find(|cap| cap.id == PCI_CAP_MSI && cap.offset <= PCI_CAP_LIMIT)?;
        let control = capability.private_header;
        let is_64bit = control & (1 << 7) != 0;
        let data_offset = capability
            .offset
            .checked_add(if is_64bit { 12 } else { 8 })?;
        let (message, data, vector) = axhal::irq::allocate_msi(DIRECT_HANDLERS[endpoint])?;
        endpoint_publish_vector(endpoint, vector);
        let disabled = control & !1;
        root.write_config_u16(bdf, capability.offset + 2, disabled);
        let writes_ok = write_config_u32(root, bdf, capability.offset + 4, message as u32)
            && (!is_64bit
                || write_config_u32(
                    root,
                    bdf,
                    capability.offset.checked_add(8)?,
                    (message >> 32) as u32,
                ))
            && root.write_config_u16(bdf, data_offset, data as u16)
            && root.write_config_u16(bdf, capability.offset + 2, (disabled & !(7 << 4)) | 1);
        if !writes_ok {
            Self::disable_capability(root, bdf, PCI_CAP_MSI);
            return None;
        }
        endpoint_activate(endpoint);
        Some(Self {
            endpoint,
            vector,
            mode: BlockIrqMode::Msi,
            bdf,
            capability: Some(capability.offset),
            msix_table_entry: None,
        })
    }

    fn register_intx(root: &mut PciRoot, bdf: DeviceFunction, endpoint: usize) -> Option<Self> {
        let raw = root.read_config_dword(bdf, PCI_INTERRUPT_LINE_PIN)?;
        let line = raw as u8;
        let pin = (raw >> 8) as u8;
        if line == 0xff || pin == 0 || pin > 4 {
            return None;
        }
        let Some(Some((gsi, active_low))) =
            axhal::pci_firmware_irq::resolve(bdf.bus, bdf.device, bdf.function, pin)
        else {
            return None;
        };
        let vector = usize::try_from(gsi.checked_add(IO_APIC_VECTOR_BASE)?).ok()?;
        if !axhal::irq::register_shared_dispatcher(dispatch_intx)
            || !axhal::pci_firmware_irq::configure(vector, active_low)
        {
            return None;
        }
        let command = root.read_config_dword(bdf, PCI_COMMAND)? as u16;
        if !root.write_config_u16(bdf, PCI_COMMAND, command & !PCI_COMMAND_INTX_DISABLE) {
            return None;
        }
        axhal::irq::set_enable(vector, true);
        endpoint_publish_vector(endpoint, vector);
        endpoint_activate(endpoint);
        Some(Self {
            endpoint,
            vector,
            mode: BlockIrqMode::Intx,
            bdf,
            capability: None,
            msix_table_entry: None,
        })
    }

    fn disable_capability(root: &mut PciRoot, bdf: DeviceFunction, id: u8) {
        if let Some(capability) = root
            .capabilities(bdf)
            .take(48)
            .find(|cap| cap.id == id && cap.offset <= PCI_CAP_LIMIT)
        {
            let mask = if id == PCI_CAP_MSIX { 0x4000 } else { 0 };
            let enable = if id == PCI_CAP_MSIX { 0x8000 } else { 1 };
            let _ = root.write_config_u16(
                bdf,
                capability.offset + 2,
                (capability.private_header | mask) & !enable,
            );
            let _ = root.read_config_dword(bdf, capability.offset);
        }
    }

    /// The MSI/MSI-X/INTx route selected for this endpoint.
    pub const fn mode(&self) -> BlockIrqMode {
        self.mode
    }

    /// Hardware interrupt vector/CPU event number.
    pub const fn vector(&self) -> usize {
        self.vector
    }

    /// Current interrupt progress generation; callers compare with a saved
    /// value and always inspect the device queue/status after a change.
    pub fn generation(&self) -> u64 {
        ENDPOINTS[self.endpoint].generation.load(Ordering::Acquire)
    }

    /// Install the existing block completion wake callback. It runs only after
    /// the device acknowledgment succeeds and must not block or allocate.
    pub fn install_completion_notifier(
        &self,
        notifier: Option<BlockCompletionNotifier>,
        context: usize,
    ) -> bool {
        let endpoint = &ENDPOINTS[self.endpoint];
        let Some(notifier) = notifier else {
            for slot in &endpoint.notifiers {
                if slot.context.load(Ordering::Acquire) == context
                    && slot.callback.load(Ordering::Acquire) != 0
                {
                    slot.callback.store(0, Ordering::Release);
                    while endpoint.readers.load(Ordering::Acquire) != 0 {
                        core::hint::spin_loop();
                    }
                    slot.context.store(0, Ordering::Release);
                    return true;
                }
            }
            return false;
        };
        let callback = notifier as usize;
        for slot in &endpoint.notifiers {
            if slot.callback.load(Ordering::Acquire) == callback
                && slot.context.load(Ordering::Acquire) == context
            {
                return true;
            }
        }
        let Some(slot) = endpoint
            .notifiers
            .iter()
            .find(|slot| slot.callback.load(Ordering::Acquire) == 0)
        else {
            return false;
        };
        slot.context.store(context, Ordering::Relaxed);
        slot.callback.store(callback, Ordering::Release);
        true
    }

    /// Wait for IRQ progress with a bounded timeout. If the current task cannot
    /// block, the caller gets an equivalent bounded delay and can poll status.
    pub fn wait_for_generation(&self, observed: u64, timeout_us: u64) -> bool {
        if self.generation() != observed {
            return true;
        }
        if !axtask::can_block_current() {
            axhal::time::busy_wait(core::time::Duration::from_micros(timeout_us));
            return self.generation() != observed;
        }
        use core::{future::poll_fn, task::Poll, time::Duration};

        use axtask::future::{
            block_on, cancel_irq_waker, register_irq_waker, timeout, update_irq_waker,
        };

        let mut token = None;
        let _ = block_on(timeout(
            Some(Duration::from_micros(timeout_us)),
            poll_fn(|cx| {
                if self.generation() != observed {
                    return Poll::Ready(());
                }
                if let Some(existing) = token {
                    if update_irq_waker(existing, cx.waker()).is_err() {
                        return Poll::Ready(());
                    }
                } else {
                    match register_irq_waker(self.vector, cx.waker()) {
                        Ok(registered) => token = Some(registered),
                        Err(_) => return Poll::Ready(()),
                    }
                }
                if self.generation() != observed {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }),
        ));
        if let Some(token) = token {
            let _ = cancel_irq_waker(token);
        }
        self.generation() != observed
    }

    /// Disables the selected PCI message mode or masks the admitted INTx
    /// delivery. The controller must mask its own interrupt source first.
    pub fn disable_pci_delivery(&self, root: &mut PciRoot) {
        if let Some(capability) = self.capability {
            if self.mode == BlockIrqMode::Msix {
                if let Some(entry) = self.msix_table_entry {
                    // SAFETY: the table entry was mapped and bounds-checked by
                    // register_msix; masking the entry prevents further DMA IRQs.
                    unsafe { ((entry + 12) as *mut u32).write_volatile(1) };
                }
                let _ = root.write_config_u16(self.bdf, capability + 2, 0x4000);
            } else {
                let _ = root.write_config_u16(self.bdf, capability + 2, 0);
            }
        } else if let Some(command) = root.read_config_dword(self.bdf, PCI_COMMAND) {
            let _ = root.write_config_u16(
                self.bdf,
                PCI_COMMAND,
                command as u16 | PCI_COMMAND_INTX_DISABLE,
            );
            axhal::irq::set_enable(self.vector, false);
        }
        ENDPOINTS[self.endpoint]
            .active
            .store(false, Ordering::Release);
        while ENDPOINTS[self.endpoint].readers.load(Ordering::Acquire) != 0 {
            core::hint::spin_loop();
        }
    }
}

fn endpoint_publish_vector(endpoint: usize, vector: usize) {
    ENDPOINTS[endpoint].vector.store(vector, Ordering::Relaxed);
}

fn endpoint_activate(endpoint: usize) {
    ENDPOINTS[endpoint].active.store(true, Ordering::Release);
}

fn write_config_u32(root: &mut PciRoot, bdf: DeviceFunction, offset: u8, value: u32) -> bool {
    root.write_config_u16(bdf, offset, value as u16)
        && root.write_config_u16(bdf, offset + 2, (value >> 16) as u16)
}

#[cfg(test)]
fn intx_vector(line: u8, pin: u8, route: Option<(u32, bool)>) -> Option<(usize, bool)> {
    if line == 0xff || pin == 0 || pin > 4 {
        return None;
    }
    let (gsi, active_low) = route?;
    Some((
        usize::try_from(gsi.checked_add(IO_APIC_VECTOR_BASE)?).ok()?,
        active_low,
    ))
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use axdriver_pci::DeviceFunction;

    use super::{
        BlockIrqMode, PciBlockInterrupt, deliver, endpoint_activate, endpoint_publish_vector,
        intx_vector, reserve,
    };

    static ACK_COUNT: AtomicUsize = AtomicUsize::new(0);
    static NOTIFY_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn acknowledge(_context: usize) -> bool {
        ACK_COUNT.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn notify(_context: usize) {
        NOTIFY_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn intx_fallback_requires_a_valid_firmware_route() {
        assert_eq!(intx_vector(0x20, 1, Some((20, true))), Some((0x34, true)));
        assert_eq!(intx_vector(0xff, 1, Some((20, true))), None);
        assert_eq!(intx_vector(0x20, 0, Some((20, true))), None);
        assert_eq!(intx_vector(0x20, 5, Some((20, true))), None);
        assert_eq!(intx_vector(0x20, 1, None), None);
    }

    #[test]
    fn acknowledged_completion_publishes_generation_then_notifies() {
        ACK_COUNT.store(0, Ordering::Relaxed);
        NOTIFY_COUNT.store(0, Ordering::Relaxed);
        let endpoint = reserve(1, acknowledge).expect("bounded endpoint slot");
        endpoint_publish_vector(endpoint, 0x45);
        let token = PciBlockInterrupt {
            endpoint,
            vector: 0x45,
            mode: BlockIrqMode::Msi,
            bdf: DeviceFunction {
                bus: 0,
                device: 0,
                function: 0,
            },
            capability: None,
            msix_table_entry: None,
        };
        assert!(token.install_completion_notifier(Some(notify), 1));
        endpoint_activate(endpoint);
        assert_eq!(token.generation(), 0);
        assert!(deliver(endpoint, 0x45));
        assert_eq!(token.generation(), 1);
        assert_eq!(ACK_COUNT.load(Ordering::Relaxed), 1);
        assert_eq!(NOTIFY_COUNT.load(Ordering::Relaxed), 1);
        assert!(token.install_completion_notifier(None, 1));
    }
}
