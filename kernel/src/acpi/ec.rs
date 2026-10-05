//! EC operation regions and bounded S0 polling queries; no suspend/wake claim.
use alloc::{boxed::Box, ffi::CString, format, string::String, vec::Vec};
use core::{
    ffi::{c_char, c_void},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use axsync::Mutex;
use tk_acpica::{BAD_PARAMETER, Engine, Node, OK, Status};
struct Ec {
    path: String,
    data: u16,
    status: u16,
    global_lock: AtomicBool,
    mutex: Mutex<()>,
}
static CONTROLLERS: Mutex<Vec<&'static Ec>> = Mutex::new(Vec::new());
static STOP: AtomicBool = AtomicBool::new(false);
unsafe extern "C" {
    fn tk_acpi_install_ec(path: *const c_char, context: *mut c_void) -> Status;
    fn AcpiAcquireGlobalLock(timeout: u16, handle: *mut u32) -> Status;
    fn AcpiReleaseGlobalLock(handle: u32) -> Status;
    fn AcpiSetGpe(device: *mut c_void, gpe: u32, action: u8) -> Status;
}
struct Ports<'a>(&'a Ec);
impl tk_acpica::ec::Io for Ports<'_> {
    fn status(&mut self) -> Result<u8, Status> {
        super::native::read_ec_port(self.0.status)
    }
    fn read_data(&mut self) -> Result<u8, Status> {
        super::native::read_ec_port(self.0.data)
    }
    fn command(&mut self, value: u8) -> Result<(), Status> {
        super::native::write_ec_port(self.0.status, value)
    }
    fn write_data(&mut self, value: u8) -> Result<(), Status> {
        super::native::write_ec_port(self.0.data, value)
    }
    fn now_us(&self) -> u64 {
        axhal::time::monotonic_time_nanos() / 1000
    }
    fn stall_us(&mut self, micros: u32) {
        super::native::stall_ec(micros)
    }
}
struct GlobalLock(Option<u32>);
impl GlobalLock {
    fn acquire(required: bool) -> Result<Self, Status> {
        if !required {
            return Ok(Self(None));
        }
        let mut handle = 0;
        // SAFETY: called only through a live ACPICA operation-region/poll owner.
        let status = unsafe { AcpiAcquireGlobalLock(100, &mut handle) };
        if status != OK {
            return Err(status);
        }
        Ok(Self(Some(handle)))
    }
}
impl Drop for GlobalLock {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            // SAFETY: matching handle returned by successful acquisition on this task.
            unsafe {
                let _ = AcpiReleaseGlobalLock(handle);
            }
        }
    }
}
static BOOT: Mutex<Option<&'static Ec>> = Mutex::new(None);
pub fn bootstrap(engine: &Engine) -> Result<(), Status> {
    for index in 0..engine.table_count()? {
        let Ok(table) = engine.table(index) else {
            continue;
        };
        if !table.starts_with(b"ECDT") {
            continue;
        }
        let boot = tk_acpica::ec::ecdt(&table)?;
        let ec = Box::leak(
            Box::try_new(Ec {
                path: boot.path,
                data: boot.data,
                status: boot.command,
                // ECDT has no _GLK bit; namespace _GLK is adopted after loading.
                global_lock: AtomicBool::new(false),
                mutex: Mutex::new(()),
            })
            .map_err(|_| tk_acpica::NO_MEMORY)?,
        );
        let root = c"\\";
        // SAFETY: static validated ports/context; install before loading AML.
        let status =
            unsafe { tk_acpi_install_ec(root.as_ptr(), (ec as *const Ec).cast_mut().cast()) };
        if status != OK {
            return Err(status);
        }
        *BOOT.lock() = Some(ec);
        info!("acpica: ECDT early EC region installed; hardware-unverified");
        break;
    }
    Ok(())
}
pub fn install(engine: &Engine, nodes: &[Node]) -> Result<usize, Status> {
    for node in nodes.iter().filter(|n| n.kind == 6) {
        if engine.hardware_id(&node.path).as_deref() != Ok("PNP0C09") {
            continue;
        }
        let resources = engine
            .resources(&node.path, false)
            .and_then(|b| tk_acpica::resources::parse(&b));
        let Ok(resources) = resources else {
            warn!("acpica: EC resources unsupported; controller not activated");
            continue;
        };
        if resources.io.len() != 2
            || resources
                .io
                .iter()
                .any(|(port, len)| *port == 0 || *len != 1)
        {
            warn!("acpica: EC requires two single-byte I/O resources");
            continue;
        }
        if let Some(boot) = *BOOT.lock() {
            if engine.resolve("\\", &boot.path)? != node.path {
                // A root bootstrap handler cannot safely represent another EC.
                return Err(tk_acpica::SUPPORT);
            }
            if boot.data != resources.io[0].0 || boot.status != resources.io[1].0 {
                return Err(BAD_PARAMETER);
            }
            boot.global_lock.store(
                engine.integer(&format!("{}._GLK", node.path)).unwrap_or(0) != 0,
                Ordering::Release,
            );
            CONTROLLERS
                .lock()
                .try_reserve(1)
                .map_err(|_| tk_acpica::NO_MEMORY)?;
            CONTROLLERS.lock().push(boot);
            continue;
        }
        let ec = Box::try_new(Ec {
            path: node.path.clone(),
            data: resources.io[0].0,
            status: resources.io[1].0,
            global_lock: AtomicBool::new(
                engine.integer(&format!("{}._GLK", node.path)).unwrap_or(0) != 0,
            ),
            mutex: Mutex::new(()),
        })
        .map_err(|_| tk_acpica::NO_MEMORY)?;
        let ec = Box::leak(ec);
        let path = CString::new(ec.path.as_bytes()).map_err(|_| BAD_PARAMETER)?;
        // SAFETY: context is static; source resource ranges were checked. ACPICA
        // invokes the region callback only from live interpreter operations.
        let status =
            unsafe { tk_acpi_install_ec(path.as_ptr(), (ec as *const Ec).cast_mut().cast()) };
        if status != OK {
            warn!("acpica: EC region handler installation failed {status:#x}");
            continue;
        }
        CONTROLLERS
            .lock()
            .try_reserve(1)
            .map_err(|_| tk_acpica::NO_MEMORY)?;
        CONTROLLERS.lock().push(ec);
    }
    if BOOT.lock().is_some() && CONTROLLERS.lock().is_empty() {
        return Err(tk_acpica::SUPPORT);
    }
    Ok(CONTROLLERS.lock().len())
}
pub fn activate(engine: &Engine) -> Result<(), Status> {
    let controllers = CONTROLLERS.lock();
    for ec in controllers.iter() {
        // Polling owns queries; mask the associated *global* EC GPE rather than
        // enabling an unhandled source. GPE-block-device packages are rejected.
        let gpe = engine.integer(&format!("{}._GPE", ec.path))?;
        if gpe > u32::MAX.into() {
            return Err(BAD_PARAMETER);
        }
        // SAFETY: live ACPICA instance; only this controller's global GPE.
        let status = unsafe { AcpiSetGpe(core::ptr::null_mut(), gpe as u32, 1) };
        if status != OK {
            warn!("acpica: EC GPE could not be masked {status:#x}");
            return Err(status);
        }
    }
    if !controllers.is_empty() {
        info!(
            "acpica: EC controllers={} polling-S0-only; wake not implemented; hardware-unverified",
            controllers.len()
        );
        STOP.store(false, Ordering::Release);
        axtask::spawn_raw(poll, "acpi_ec_poll".into(), axconfig::TASK_STACK_SIZE)
            .map_err(|_| tk_acpica::NO_MEMORY)?;
    }
    Ok(())
}
pub fn stop() {
    STOP.store(true, Ordering::Release);
}
fn poll() {
    while !STOP.load(Ordering::Acquire) {
        let controllers = CONTROLLERS.lock().clone();
        for ec in controllers {
            for _ in 0..64 {
                let query = super::with_engine(|engine| {
                    let query = {
                        let _transaction = ec.mutex.lock();
                        let _global = GlobalLock::acquire(ec.global_lock.load(Ordering::Acquire))?;
                        tk_acpica::ec::query(&mut Ports(ec))?
                    };
                    if let Some(value) = query {
                        let _ = engine.evaluate(&format!("{}._Q{value:02X}", ec.path), &[]);
                    }
                    Ok::<_, Status>(query)
                });
                if !matches!(query, Some(Ok(Some(_)))) {
                    break;
                }
            }
        }
        if axtask::sleep(Duration::from_millis(25)).is_err() {
            return;
        }
    }
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tk_acpi_ec_access(
    function: u32,
    address: u64,
    width: u32,
    value: *mut u64,
    context: *mut c_void,
) -> Status {
    if context.is_null() || value.is_null() || function > 1 {
        return BAD_PARAMETER;
    }
    // SAFETY: C bridge retains a leaked Ec context and supplies writable value.
    let (ec, value) = unsafe { (&*context.cast::<Ec>(), &mut *value) };
    let _transaction = ec.mutex.lock();
    let _global = match GlobalLock::acquire(ec.global_lock.load(Ordering::Acquire)) {
        Ok(g) => g,
        Err(e) => return e,
    };
    match tk_acpica::ec::transfer(&mut Ports(ec), function == 1, address, width, value) {
        Ok(()) => OK,
        Err(e) => e,
    }
}
