//! Native OSL adapter. Enabled only by acpi=acpica; no native hardware claim.
use core::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axhal::mem::{pa, va};
use kspin::SpinNoIrq;
use tk_acpica::{
    LIMIT, NO_MEMORY, OK, SUPPORT, Status,
    backend::{Backend, BackendRegistration, IrqHandler, PciId, Work},
};

pub static REGISTRATION: BackendRegistration = BackendRegistration(&NATIVE);
static NATIVE: Native = Native;
struct Native;
#[derive(Clone, Copy)]
struct Item {
    function: Work,
    context: usize,
}
struct Queue {
    items: [Option<Item>; 128],
    head: usize,
    len: usize,
}
impl Queue {
    const fn new() -> Self {
        Self {
            items: [None; 128],
            head: 0,
            len: 0,
        }
    }
    fn push(&mut self, item: Item) -> bool {
        if self.len == self.items.len() {
            return false;
        }
        self.items[(self.head + self.len) % self.items.len()] = Some(item);
        self.len += 1;
        true
    }
    fn pop(&mut self) -> Option<Item> {
        if self.len == 0 {
            return None;
        }
        let item = self.items[self.head].take();
        self.head = (self.head + 1) % self.items.len();
        self.len -= 1;
        item
    }
}
static QUEUE: SpinNoIrq<Queue> = SpinNoIrq::new(Queue::new());
static PENDING: AtomicU32 = AtomicU32::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static SCI_INFLIGHT: SpinNoIrq<()> = SpinNoIrq::new(());
static IRQ_FUNCTION: AtomicUsize = AtomicUsize::new(0);
static IRQ_CONTEXT: AtomicUsize = AtomicUsize::new(0);
static IRQ_VECTOR: AtomicUsize = AtomicUsize::new(0);
static IRQ_WINDOW: AtomicU64 = AtomicU64::new(0);
static IRQ_COUNT: AtomicU32 = AtomicU32::new(0);
static STORM: AtomicBool = AtomicBool::new(false);

pub fn start_worker() -> Result<(), Status> {
    STOP.store(false, Ordering::Release);
    axtask::spawn_raw(worker, "acpi_osl".into(), axconfig::TASK_STACK_SIZE)
        .map(|_| ())
        .map_err(|_| NO_MEMORY)
}
pub fn stop_worker() {
    STOP.store(true, Ordering::Release);
}
fn worker() {
    loop {
        let item = QUEUE.lock().pop();
        if let Some(item) = item {
            // SAFETY: ACPICA owns the queued callback/context until this worker
            // returns; queue admission is allocation-free and never inline in SCI.
            unsafe {
                (item.function)(item.context as *mut c_void);
            }
            PENDING.fetch_sub(1, Ordering::Release);
        } else {
            if STOP.load(Ordering::Acquire) {
                return;
            }
            if STORM.swap(false, Ordering::AcqRel) {
                // Fail closed on sustained SCI traffic. Disable GPEs from task
                // context before re-enabling the fixed-event SCI delivery.
                super::disable_storming_gpes();
            }
            let _ = axtask::sleep(Duration::from_millis(1));
        }
    }
}
fn sci() {
    let _inflight = SCI_INFLIGHT.lock();
    let now = axhal::time::monotonic_time_nanos() / 1_000_000_000;
    if IRQ_WINDOW.swap(now, Ordering::Relaxed) != now {
        IRQ_COUNT.store(0, Ordering::Relaxed);
    }
    if IRQ_COUNT.fetch_add(1, Ordering::Relaxed) >= 1024 {
        axhal::irq::set_enable(IRQ_VECTOR.load(Ordering::Relaxed), false);
        STORM.store(true, Ordering::Release);
        return;
    }
    let callback = IRQ_FUNCTION.load(Ordering::Acquire);
    if callback != 0 {
        // SAFETY: install publishes the C signature and context before unmasking;
        // an in-flight guard synchronizes masked removal on any CPU.
        unsafe {
            let f = core::mem::transmute::<usize, IrqHandler>(callback);
            let _ = f(IRQ_CONTEXT.load(Ordering::Relaxed) as *mut c_void);
        }
    }
}
fn mapped(address: u64, size: usize) -> *mut c_void {
    let Some(end) = address.checked_add(size as u64) else {
        return core::ptr::null_mut();
    };
    if size == 0 || size > 16 * 1024 * 1024 || end > 64u64 << 40 {
        return core::ptr::null_mut();
    }
    let mut page = address as usize & !4095;
    while page < (end as usize).saturating_add(4095) & !4095 {
        let virt = axhal::mem::phys_to_virt(pa!(page));
        // Never change the attributes of an existing RAM/huge-page mapping.
        let exists = axmm::kernel_aspace().lock().query_leaf(virt).is_ok();
        if !exists && axmm::iomap(pa!(page), 4096).is_err() {
            return core::ptr::null_mut();
        }
        page += 4096;
    }
    axhal::mem::phys_to_virt(pa!(address as usize))
        .as_mut_ptr()
        .cast()
}
fn pci_address(id: PciId, reg: u32) -> Option<u64> {
    let (base, segment, begin, end) = axhal::acpi::ecam();
    if id.segment != segment || id.bus < u16::from(begin) || id.bus > u16::from(end) {
        return None;
    }
    base.checked_add(
        (u64::from(id.bus) << 20)
            | (u64::from(id.device) << 15)
            | (u64::from(id.function) << 12)
            | u64::from(reg),
    )
}
// SAFETY: native adapter is activated after allocation/scheduler setup and only
// with explicit ACPI hardware ownership. SCI is BSP-targeted and synchronized
// by masking and waiting for the in-flight callback. Mappings persist for the kernel lifetime; queue is bounded
// and drained before interpreter termination.
unsafe impl Backend for Native {
    fn root_pointer(&self) -> u64 {
        axhal::mem::virt_to_phys(va!(axhal::acpi::rsdp_pointer())).as_usize() as u64
    }
    fn map(&self, address: u64, size: usize) -> *mut c_void {
        mapped(address, size)
    }
    fn unmap(&self, _address: *mut c_void, _size: usize) {} // Permanent shared direct-map pages.
    fn physical_address(&self, address: *mut c_void) -> Result<u64, Status> {
        let v = va!(address as usize);
        axmm::kernel_aspace()
            .lock()
            .query_leaf(v)
            .map(|(p, ..)| p.as_usize() as u64)
            .map_err(|_| SUPPORT)
    }
    fn timer_100ns(&self) -> u64 {
        axhal::time::monotonic_time_nanos() / 100
    }
    fn sleep(&self, millis: u64) -> bool {
        axtask::sleep(Duration::from_millis(millis)).is_ok()
    }
    fn stall(&self, micros: u32) {
        let deadline = axhal::time::monotonic_time_nanos().saturating_add(u64::from(micros) * 1000);
        while axhal::time::monotonic_time_nanos() < deadline {
            core::hint::spin_loop();
        }
    }
    fn thread_id(&self) -> u64 {
        axtask::current().id().as_u64()
    }
    fn irq_save(&self) -> usize {
        let on = axhal::asm::irqs_enabled();
        axhal::asm::disable_irqs();
        usize::from(on)
    }
    fn irq_restore(&self, flags: usize) {
        if flags != 0 {
            axhal::asm::enable_irqs();
        }
    }
    fn install_irq(&self, irq: u32, handler: IrqHandler, context: *mut c_void) -> Status {
        if IRQ_FUNCTION.load(Ordering::Acquire) != 0 {
            return SUPPORT;
        }
        IRQ_CONTEXT.store(context as usize, Ordering::Relaxed);
        IRQ_FUNCTION.store(handler as usize, Ordering::Release);
        match axhal::acpi::install_sci(irq, sci) {
            Some(vector) => {
                IRQ_VECTOR.store(vector, Ordering::Release);
                OK
            }
            None => {
                IRQ_FUNCTION.store(0, Ordering::Release);
                SUPPORT
            }
        }
    }
    fn remove_irq(&self, _irq: u32, handler: IrqHandler) -> Status {
        if IRQ_FUNCTION.load(Ordering::Acquire) == 0 {
            return OK;
        }
        if IRQ_FUNCTION.load(Ordering::Acquire) != handler as usize {
            return SUPPORT;
        }
        axhal::acpi::remove_sci(IRQ_VECTOR.load(Ordering::Acquire));
        let _inflight = SCI_INFLIGHT.lock();
        IRQ_FUNCTION.store(0, Ordering::Release);
        OK
    }
    fn execute(&self, _kind: u32, function: Work, context: *mut c_void) -> Status {
        let mut queue = QUEUE.lock();
        if !queue.push(Item {
            function,
            context: context as usize,
        }) {
            return LIMIT;
        }
        PENDING.fetch_add(1, Ordering::Release);
        OK
    }
    fn quiesce(&self) {
        if IRQ_FUNCTION.load(Ordering::Acquire) != 0 {
            axhal::irq::set_enable(IRQ_VECTOR.load(Ordering::Acquire), false);
            {
                let _inflight = SCI_INFLIGHT.lock();
            }
        }
        self.wait_events();
    }
    fn wait_events(&self) {
        while PENDING.load(Ordering::Acquire) != 0 {
            if axtask::sleep(Duration::from_millis(1)).is_err() {
                core::hint::spin_loop();
            }
        }
    }
    fn read_port(&self, address: u16, width: u32) -> Result<u32, Status> {
        // SAFETY: OSL validated port width/range; explicit acpi=acpica owns I/O.
        unsafe {
            let value: u32;
            match width {
                8 => {
                    let v: u8;
                    core::arch::asm!("in al, dx",out("al")v,in("dx")address,options(nomem,nostack));
                    value = v.into();
                }
                16 => {
                    let v: u16;
                    core::arch::asm!("in ax, dx",out("ax")v,in("dx")address,options(nomem,nostack));
                    value = v.into();
                }
                32 => {
                    core::arch::asm!("in eax, dx",out("eax")value,in("dx")address,options(nomem,nostack));
                }
                _ => return Err(SUPPORT),
            }
            Ok(value)
        }
    }
    fn write_port(&self, address: u16, width: u32, value: u32) -> Status {
        // SAFETY: OSL validated port width/range; explicit acpi=acpica owns I/O.
        unsafe {
            match width {
                8 => {
                    core::arch::asm!("out dx, al",in("al")value as u8,in("dx")address,options(nomem,nostack))
                }
                16 => {
                    core::arch::asm!("out dx, ax",in("ax")value as u16,in("dx")address,options(nomem,nostack))
                }
                32 => {
                    core::arch::asm!("out dx, eax",in("eax")value,in("dx")address,options(nomem,nostack))
                }
                _ => return SUPPORT,
            }
        }
        OK
    }
    fn read_pci(&self, id: PciId, reg: u32, width: u32) -> Result<u64, Status> {
        let address = pci_address(id, reg).ok_or(SUPPORT)?;
        let p = mapped(address, (width / 8) as usize);
        if p.is_null() {
            return Err(NO_MEMORY);
        }
        // SAFETY: OSL validated aligned BDF/register and native ECAM admission.
        unsafe {
            Ok(match width {
                8 => p.cast::<u8>().read_volatile() as u64,
                16 => p.cast::<u16>().read_volatile() as u64,
                32 => p.cast::<u32>().read_volatile() as u64,
                _ => {
                    u64::from(p.cast::<u32>().read_volatile())
                        | (u64::from(p.cast::<u32>().add(1).read_volatile()) << 32)
                }
            })
        }
    }
    fn write_pci(&self, id: PciId, reg: u32, width: u32, value: u64) -> Status {
        let Some(address) = pci_address(id, reg) else {
            return SUPPORT;
        };
        let p = mapped(address, (width / 8) as usize);
        if p.is_null() {
            return NO_MEMORY;
        }
        // SAFETY: OSL validated aligned BDF/register and native ECAM admission.
        unsafe {
            match width {
                8 => p.cast::<u8>().write_volatile(value as u8),
                16 => p.cast::<u16>().write_volatile(value as u16),
                32 => p.cast::<u32>().write_volatile(value as u32),
                _ => {
                    p.cast::<u32>().write_volatile(value as u32);
                    p.cast::<u32>().add(1).write_volatile((value >> 32) as u32);
                }
            }
        }
        OK
    }
    fn log(&self, message: &[u8]) {
        if let Ok(text) = core::str::from_utf8(message) {
            info!("acpica: {}", text.trim_end());
        }
    }
}
pub fn reenable_sci() {
    IRQ_COUNT.store(0, Ordering::Relaxed);
    axhal::irq::set_enable(IRQ_VECTOR.load(Ordering::Acquire), true);
}
pub(super) fn read_ec_port(port: u16) -> Result<u8, Status> {
    NATIVE.read_port(port, 8).map(|v| v as u8)
}
pub(super) fn write_ec_port(port: u16, value: u8) -> Result<(), Status> {
    let status = NATIVE.write_port(port, 8, u32::from(value));
    if status == OK { Ok(()) } else { Err(status) }
}
pub(super) fn stall_ec(micros: u32) {
    NATIVE.stall(micros);
}
pub(super) fn pci_read(id: PciId, reg: u32, width: u32) -> Result<u64, Status> {
    NATIVE.read_pci(id, reg, width)
}
