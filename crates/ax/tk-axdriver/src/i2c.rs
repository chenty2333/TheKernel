//! Default-enabled Intel LPSS I2C PCI binding.
use alloc::{string::String, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence},
    time::Duration,
};

use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use spin::Mutex;
use tk_i2c::{
    Backend, Ig4, IicError, IicMessage,
    pci::{PciAttachment, PciResources, ig4iic_pci_attach, ig4iic_pci_probe},
    reg::*,
};

static CONTROLLERS: Mutex<Vec<Controller>> = Mutex::new(Vec::new());
const MAX_I2C_CONTROLLERS: usize = 8;

struct IrqState {
    base: AtomicUsize,
    mask: AtomicU32,
    generation: AtomicU64,
}

impl IrqState {
    const fn new() -> Self {
        Self {
            base: AtomicUsize::new(0),
            mask: AtomicU32::new(0),
            generation: AtomicU64::new(0),
        }
    }
}

static IRQ_STATE: [IrqState; MAX_I2C_CONTROLLERS] =
    [const { IrqState::new() }; MAX_I2C_CONTROLLERS];

// upstream: ig4_iic.c ig4iic_intr()
fn acknowledge_irq(slot: usize) {
    let state = &IRQ_STATE[slot];
    let base = state.base.load(Ordering::Acquire);
    if base == 0 || state.mask.load(Ordering::Acquire) == 0 {
        return;
    }
    // SAFETY: the registry pins each mapped BAR while its IRQ owner is live.
    let status = unsafe { ((base + IG4_REG_INTR_STAT as usize) as *const u32).read_volatile() };
    if status != 0 {
        // Match ig4iic_intr(): mask the asserted controller before waking the
        // transfer waiter; wait_intr() clears and reconciles the mask in task context.
        unsafe { ((base + IG4_REG_INTR_MASK as usize) as *mut u32).write_volatile(0) };
        state.mask.store(0, Ordering::Release);
        state.generation.fetch_add(1, Ordering::Release);
    }
}

macro_rules! irq_handlers {
    ($($handler:ident => $slot:expr),+ $(,)?) => { $(fn $handler() { acknowledge_irq($slot); })+ };
}
irq_handlers!(irq0=>0,irq1=>1,irq2=>2,irq3=>3,irq4=>4,irq5=>5,irq6=>6,irq7=>7);
fn handler_for(slot: usize) -> Option<fn()> {
    Some(match slot {
        0 => irq0,
        1 => irq1,
        2 => irq2,
        3 => irq3,
        4 => irq4,
        5 => irq5,
        6 => irq6,
        7 => irq7,
        _ => return None,
    })
}

struct MsiOwner {
    config: usize,
    capability: usize,
    vector: usize,
    slot: usize,
    msix_restore: Option<(usize, u16)>,
}
// SAFETY: the capability mapping is permanent ECAM; vector ownership is
// synchronized by the PCI attach/detach path before the BAR is released.
unsafe impl Send for MsiOwner {}
impl Drop for MsiOwner {
    fn drop(&mut self) {
        // SAFETY: config is a mapped ECAM page and this owner disables its own MSI capability.
        unsafe {
            let control = (self.config + self.capability + 2) as *mut u16;
            control.write_volatile(control.read_volatile() & !1);
            if let Some((offset, value)) = self.msix_restore {
                ((self.config + offset + 2) as *mut u16).write_volatile(value);
            }
        }
        let _ = axhal::irq::unregister(self.vector);
        IRQ_STATE[self.slot].base.store(0, Ordering::Release);
        IRQ_STATE[self.slot].mask.store(0, Ordering::Release);
    }
}

fn install_msi(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    base: usize,
    slot: usize,
) -> Option<MsiOwner> {
    let capability = root.capabilities(bdf).take(48).find(|cap| cap.id == 5)?;
    let offset = usize::from(capability.offset);
    if !(0x40..=0xe0).contains(&offset) || !offset.is_multiple_of(4) {
        return None;
    }
    let (bus_first, bus_last) = axhal::pci::ecam_bus_range();
    if !(bus_first..=bus_last).contains(&bdf.bus) || axhal::pci::ecam_segment() != 0 {
        return None;
    }
    let ecam = axhal::pci::ecam_base()
        .checked_add(usize::from(bdf.bus - bus_first) << 20)?
        .checked_add(usize::from(bdf.device) << 15)?
        .checked_add(usize::from(bdf.function) << 12)?;
    let config = axhal::mem::phys_to_virt(ecam.into()).as_mut_ptr() as usize;
    let handler = handler_for(slot)?;
    let (message, data, vector) = axhal::irq::allocate_msi(
        tk_vtd::PciRequester {
            segment: axhal::pci::ecam_segment(),
            bus: bdf.bus,
            device: bdf.device,
            function: bdf.function,
        },
        handler,
    )?;
    // SAFETY: this capability and configuration page were validated above.
    let old_control = unsafe { ((config + offset + 2) as *const u16).read_volatile() };
    let is_64bit = old_control & 0x80 != 0;
    let data_offset = if is_64bit { 12 } else { 8 };
    // Never leave firmware-enabled MSI-X competing with the selected MSI route.
    let mut msix_restore = None;
    if let Some(msix) = root.capabilities(bdf).take(48).find(|cap| cap.id == 0x11) {
        let control = root.read_config_dword(bdf, msix.offset)?;
        let control = (control >> 16) as u16 & !(1 << 15);
        let old_control =
            unsafe { ((config + usize::from(msix.offset) + 2) as *const u16).read_volatile() };
        if old_control & (1 << 15) != 0 {
            msix_restore = Some((usize::from(msix.offset), old_control));
        }
        if !root.write_config_u16(bdf, msix.offset + 2, control) {
            let _ = axhal::irq::unregister(vector);
            return None;
        }
    }
    // Quiesce any firmware-left controller interrupt source before routing
    // MSI. Otherwise an already-pending status bit could reach the new vector
    // before IRQ_STATE has been published below.
    // SAFETY: BAR mapping is held by the caller until the MSI owner is dropped.
    unsafe {
        ((base + IG4_REG_INTR_MASK as usize) as *mut u32).write_volatile(0);
    }
    fence(Ordering::Release);
    // SAFETY: message address/data are returned for this reserved vector; the
    // capability layout is validated from its 64-bit-address control bit.
    unsafe {
        ((config + offset + 4) as *mut u32).write_volatile(message as u32);
        if is_64bit {
            ((config + offset + 8) as *mut u32).write_volatile((message >> 32) as u32);
        }
        ((config + offset + data_offset) as *mut u16).write_volatile(data as u16);
        if old_control & 0x100 != 0 {
            let mask_offset = if is_64bit { 16 } else { 12 };
            ((config + offset + mask_offset) as *mut u32).write_volatile(0);
        }
    }
    let control = (old_control & !0x70) | 1;
    if !root.write_config_u16(bdf, capability.offset + 2, control) {
        IRQ_STATE[slot].base.store(0, Ordering::Release);
        IRQ_STATE[slot].mask.store(0, Ordering::Release);
        if let Some((offset, value)) = msix_restore {
            unsafe {
                ((config + offset + 2) as *mut u16).write_volatile(value);
            }
        }
        let _ = axhal::irq::unregister(vector);
        return None;
    }
    IRQ_STATE[slot].base.store(base, Ordering::Release);
    IRQ_STATE[slot].mask.store(0, Ordering::Release);
    Some(MsiOwner {
        config,
        capability: offset,
        vector,
        slot,
        msix_restore,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpiI2cChild {
    pub path: String,
    pub hid: String,
    pub slave_address: u16,
    pub ten_bit: bool,
    pub speed_hz: u32,
    pub hid_descriptor_register: Option<u16>,
}

#[crate_interface::def_interface]
pub trait AcpiI2cSupport {
    fn controller_path(segment: u16, bus: u8, device: u8, function: u8) -> Option<String>;
    fn enumerate_children(controller_path: &str) -> Vec<AcpiI2cChild>;
    fn clock_params(controller_path: &str, method: &str) -> Option<[u64; 3]>;
}

struct Mmio {
    base: NonNull<u8>,
    size: usize,
    call_lock: bool,
    slot: usize,
    last_irq_generation: u64,
    msi: Option<MsiOwner>,
    acpi_path: Option<String>,
}
// SAFETY: the PCI-probed memory BAR remains mapped for the lifetime of the
// registry entry; each public operation holds CONTROLLERS' exclusive lock.
unsafe impl Send for Mmio {}

impl Mmio {
    fn new(
        base: usize,
        size: usize,
        slot: usize,
        msi: Option<MsiOwner>,
        acpi_path: Option<String>,
    ) -> Option<Self> {
        if size < (IG4_REG_AUTO_LTR_VALUE + 4) as usize || base & 3 != 0 {
            return None;
        }
        Some(Self {
            base: NonNull::new(base as *mut u8)?,
            size,
            call_lock: false,
            slot,
            last_irq_generation: 0,
            msi,
            acpi_path,
        })
    }
    fn valid(&self, register: u32) -> bool {
        register & 3 == 0
            && (register as usize)
                .checked_add(4)
                .is_some_and(|end| end <= self.size)
    }
}

impl Backend for Mmio {
    fn read32(&mut self, register: u32) -> u32 {
        if !self.valid(register) {
            return u32::MAX;
        }
        // SAFETY: valid checks BAR bounds/alignment and the PCI walker retains
        // the mapping until this controller is detached.
        fence(Ordering::Acquire);
        let value = unsafe {
            self.base
                .as_ptr()
                .add(register as usize)
                .cast::<u32>()
                .read_volatile()
        };
        fence(Ordering::Acquire);
        value
    }
    fn write32(&mut self, register: u32, value: u32) {
        if !self.valid(register) {
            return;
        }
        // SAFETY: same validated bounds and lifetime as read32.
        unsafe {
            self.base
                .as_ptr()
                .add(register as usize)
                .cast::<u32>()
                .write_volatile(value)
        }
        fence(Ordering::Release);
        if register == IG4_REG_INTR_MASK {
            IRQ_STATE[self.slot].mask.store(value, Ordering::Release);
        }
    }
    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(Duration::from_micros(u64::from(micros)));
    }
    fn pause_ms(&mut self, _message: &'static str, millis: u32) {
        axhal::time::busy_wait(Duration::from_millis(u64::from(millis)));
    }
    // upstream: ig4_iic.c DO_POLL, mapped to the scheduler's blocking capability.
    fn do_poll(&self) -> bool {
        !axtask::can_block_current()
    }
    // upstream: ig4_iic.c wait_intr() waits for interrupt-driven completion.
    fn wait_irq(&mut self, milliseconds: u32) {
        let Some(msi) = self.msi.as_ref() else {
            return;
        };
        let slot = msi.slot;
        let before = IRQ_STATE[slot].generation.load(Ordering::Acquire);
        if before != self.last_irq_generation {
            self.last_irq_generation = before;
            return;
        }
        use core::{future::poll_fn, task::Poll};

        use axtask::future::{
            block_on, cancel_irq_waker, register_irq_waker, timeout, update_irq_waker,
        };
        let mut token = None;
        let _ = block_on(timeout(
            Some(Duration::from_millis(u64::from(milliseconds))),
            poll_fn(|cx| {
                if IRQ_STATE[slot].generation.load(Ordering::Acquire) != before {
                    return Poll::Ready(());
                }
                if let Some(existing) = token {
                    if update_irq_waker(existing, cx.waker()).is_err() {
                        return Poll::Ready(());
                    }
                } else {
                    match register_irq_waker(msi.vector, cx.waker()) {
                        Ok(registered) => token = Some(registered),
                        Err(_) => return Poll::Ready(()),
                    }
                }
                if IRQ_STATE[slot].generation.load(Ordering::Acquire) != before {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }),
        ));
        if let Some(token) = token {
            let _ = cancel_irq_waker(token);
        }
        self.last_irq_generation = IRQ_STATE[slot].generation.load(Ordering::Acquire);
    }
    fn acpi_clock_params(&mut self, _method: &str) -> Result<[u64; 3], ()> {
        self.acpi_path
            .as_deref()
            .and_then(|path| {
                crate_interface::call_interface!(AcpiI2cSupport::clock_params, path, _method)
            })
            .ok_or(())
    }
    fn add_iicbus_child(&mut self) -> bool {
        true
    }
    fn attach_iicbus_children(&mut self) {}
    fn detach_iicbus_children(&mut self) -> Result<(), IicError> {
        Ok(())
    }
    fn setup_interrupt(&mut self) -> Result<(), IicError> {
        // MSI handler and message were installed before core attach; the owner
        // remains pinned in this backend until the translated detach path.
        if self.msi.is_some() {
            Ok(())
        } else {
            Err(IicError::HardwareUnavailable)
        }
    }
    fn teardown_interrupt(&mut self) {}
    fn suspend_iicbus_children(&mut self) -> Result<(), IicError> {
        Ok(())
    }
    fn resume_iicbus_children(&mut self) -> Result<(), IicError> {
        Ok(())
    }
    fn debug_register(&mut self, name: &'static str, value: u32) {
        log::debug!("i2c: {name}={value:#010x}");
    }
    fn debug_warning(&mut self, message: &'static str) {
        log::warn!("i2c: {message}");
    }
    fn init_locks(&mut self) {
        self.call_lock = false;
    }
    fn destroy_locks(&mut self) {
        self.call_lock = false;
    }
    fn call_lock_owned(&self) -> bool {
        self.call_lock
    }
    fn call_try_lock(&mut self) -> bool {
        if self.call_lock {
            false
        } else {
            self.call_lock = true;
            true
        }
    }
    fn call_lock(&mut self) {
        self.call_lock = true;
    }
    fn call_unlock(&mut self) {
        self.call_lock = false;
    }
    fn wake_waiters(&mut self) {}
}

struct Controller {
    core: Ig4<Mmio>,
    attachment: PciAttachment,
    acpi_path: Option<String>,
    children: Vec<AcpiI2cChild>,
}

impl PciResources for Mmio {
    fn allocate_bar0_memory(&mut self) -> bool {
        true
    }
    fn allocate_msi(&mut self) -> bool {
        self.msi.is_some()
    }
    fn allocate_irq(&mut self, _msi_rid: u8) -> bool {
        self.msi.is_some()
    }
    fn release_irq(&mut self, _rid: u8) {}
    fn release_msi(&mut self) {
        drop(self.msi.take());
    }
    fn release_bar0(&mut self) {}
}

/// Probe a PCI function, bind all devices in the translated FreeBSD table and
/// initialize their DesignWare controllers. Returns true for every matched ID
/// so a failed attach is not offered to a later driver.
pub(crate) fn probe(root: &mut PciRoot, bdf: DeviceFunction, info: &DeviceFunctionInfo) -> bool {
    let Some(matched) = ig4iic_pci_probe(info.vendor_id, info.device_id) else {
        return false;
    };
    let info = root.bar_info(bdf, 0);
    let (address, size) = match info {
        Ok(BarInfo::Memory { address, size, .. }) => (address, size as usize),
        _ => {
            log::warn!("i2c: {bdf} BAR0 is unavailable or not memory");
            return true;
        }
    };
    let Some(address) = usize::try_from(address).ok() else {
        log::warn!("i2c: {bdf} BAR0 address does not fit");
        return true;
    };
    let Some(base) = NonNull::new(axhal::mem::phys_to_virt(address.into()).as_mut_ptr()) else {
        return true;
    };
    let slot = CONTROLLERS.lock().len();
    if slot >= MAX_I2C_CONTROLLERS {
        log::warn!("i2c: controller limit {} reached", MAX_I2C_CONTROLLERS);
        return true;
    }
    let base = base.as_ptr() as usize;
    let msi = install_msi(root, bdf, base, slot);
    let segment = axhal::pci::ecam_segment();
    let acpi_path = crate_interface::call_interface!(
        AcpiI2cSupport::controller_path,
        segment,
        bdf.bus,
        bdf.device,
        bdf.function
    );
    let Some(io) = Mmio::new(base, size, slot, msi, acpi_path.clone()) else {
        log::warn!("i2c: {bdf} BAR0 too small for ig4 registers");
        return true;
    };
    let children = acpi_path.as_deref().map_or_else(Vec::new, |path| {
        crate_interface::call_interface!(AcpiI2cSupport::enumerate_children, path)
    });
    let mut controller = Controller {
        core: Ig4::new(io, matched.version, 0),
        attachment: PciAttachment::default(),
        acpi_path,
        children,
    };
    match ig4iic_pci_attach(&mut controller.core, &mut controller.attachment) {
        Ok(()) => {
            let mut devices = CONTROLLERS.lock();
            if devices.try_reserve(1).is_err() {
                let _ = tk_i2c::pci::ig4iic_pci_detach(
                    &mut controller.core,
                    &mut controller.attachment,
                );
                log::error!("i2c: unable to reserve controller registry entry for {bdf}");
            } else {
                log::info!(
                    "i2c: {} at {bdf}, {} bytes, version {:?}",
                    matched.description,
                    size,
                    matched.version
                );
                devices.push(controller);
            }
        }
        Err(error) => log::warn!(
            "i2c: {} at {bdf} attach failed: {error:?}",
            matched.description
        ),
    }
    true
}

pub fn bus_count() -> usize {
    CONTROLLERS.lock().len()
}

pub fn children(bus: usize) -> Option<(Option<String>, Vec<AcpiI2cChild>)> {
    CONTROLLERS
        .lock()
        .get(bus)
        .map(|controller| (controller.acpi_path.clone(), controller.children.clone()))
}

/// Linux i2c-dev transfers are executed as one serialized bus transaction.
pub fn transfer(bus: usize, messages: &mut [IicMessage<'_>]) -> Result<(), IicError> {
    let mut controllers = CONTROLLERS.lock();
    let controller = controllers
        .get_mut(bus)
        .ok_or(IicError::HardwareUnavailable)?;
    match controller.core.ig4iic_transfer(messages) {
        IicError::NoError => Ok(()),
        error => Err(error),
    }
}

pub fn reset(bus: usize, speed: u8, address: u8) -> Result<(), IicError> {
    let mut controllers = CONTROLLERS.lock();
    let controller = controllers
        .get_mut(bus)
        .ok_or(IicError::HardwareUnavailable)?;
    let error = controller.core.ig4iic_reset(speed, address, None);
    if error == IicError::NoError {
        Ok(())
    } else {
        Err(error)
    }
}
