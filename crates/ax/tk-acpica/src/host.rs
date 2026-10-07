//! Offline-only, simulated hardware backend for table/AML tests.
//! Never maps /dev/mem or accesses host ports/PCI. Firmware is read from an
//! explicit directory outside the repository, and only counts/status are logged.
use std::{
    boxed::Box,
    collections::BTreeMap,
    ffi::c_void,
    fs,
    path::Path,
    string::String,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
    vec::Vec,
};

use crate::{
    BAD_PARAMETER, LIMIT, OK, Status,
    backend::{Backend, BackendRegistration, IrqHandler, PciId, Work},
};
static START: OnceLock<Instant> = OnceLock::new();
pub struct OfflineBackend {
    root: u64,
    tables: Vec<Box<[u8]>>,
    regions: Mutex<BTreeMap<(u64, usize), Box<[u64]>>>,
    active: AtomicUsize,
}
fn table(signature: &[u8; 4], mut bytes: Vec<u8>) -> Box<[u8]> {
    bytes[..4].copy_from_slice(signature);
    let len = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    bytes[8] = 6;
    bytes[10..16].copy_from_slice(b"TKTEST");
    bytes[16..24].copy_from_slice(b"OFFLINE ");
    bytes[9] = 0;
    bytes[9] = 0u8.wrapping_sub(bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    bytes.into_boxed_slice()
}
impl OfflineBackend {
    pub fn from_directory(path: &Path) -> Result<Self, std::io::Error> {
        let mut entries: Vec<_> = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();
        let mut aml = Vec::new();
        for path in entries {
            // Ignore MSDM and every other unrelated firmware table, even when
            // the directory happens to contain them. Never log table contents.
            let filename = path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_ascii_uppercase();
            if !(filename.starts_with("DSDT") || filename.starts_with("SSDT")) {
                continue;
            }
            if filename.ends_with(".DSL") || filename.ends_with(".ASL") {
                continue;
            }
            let b = fs::read(path)?;
            if b.len() < 36
                || b.len() > 1024 * 1024
                || u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize != b.len()
                || b.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0
                || !matches!(&b[..4], b"DSDT" | b"SSDT")
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid AML table length/signature/checksum",
                ));
            }
            aml.push(b.into_boxed_slice());
        }
        Self::from_tables(aml)
    }
    pub fn from_tables(mut aml: Vec<Box<[u8]>>) -> Result<Self, std::io::Error> {
        let dsdt = aml
            .iter()
            .find(|b| &b[..4] == b"DSDT")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "DSDT missing"))?;
        let mut fadt = std::vec![0;276];
        fadt[140..148].copy_from_slice(&(dsdt.as_ptr() as u64).to_le_bytes());
        fadt[112..116].copy_from_slice(&(1u32 << 20).to_le_bytes());
        let fadt = table(b"FACP", fadt);
        let addresses = std::iter::once(fadt.as_ptr() as u64).chain(
            aml.iter()
                .filter(|t| &t[..4] == b"SSDT")
                .map(|t| t.as_ptr() as u64),
        );
        let mut xsdt = std::vec![0;36];
        for address in addresses {
            xsdt.extend_from_slice(&address.to_le_bytes());
        }
        let xsdt = table(b"XSDT", xsdt);
        let mut rsdp = std::vec![0u8;36];
        rsdp[..8].copy_from_slice(b"RSD PTR ");
        rsdp[15] = 2;
        rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
        rsdp[24..32].copy_from_slice(&(xsdt.as_ptr() as u64).to_le_bytes());
        rsdp[8] = 0u8.wrapping_sub(rsdp[..20].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
        rsdp[32] = 0u8.wrapping_sub(rsdp.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
        let rsdp = rsdp.into_boxed_slice();
        let root = rsdp.as_ptr() as u64;
        aml.extend([fadt, xsdt, rsdp]);
        Ok(Self {
            root,
            tables: aml,
            regions: Mutex::new(BTreeMap::new()),
            active: AtomicUsize::new(0),
        })
    }
    pub fn register(self) -> &'static BackendRegistration {
        Box::leak(Box::new(BackendRegistration(Box::leak(Box::new(self)))))
    }
}
// SAFETY: only owned immutable firmware buffers or private zeroed simulation
// buffers are mapped; no real hardware access. Deferred callbacks execute on
// host threads and active accounting is drained on termination.
unsafe impl Backend for OfflineBackend {
    fn root_pointer(&self) -> u64 {
        self.root
    }
    fn map(&self, address: u64, size: usize) -> *mut c_void {
        if size == 0 || size > 1024 * 1024 || address.checked_add(size as u64).is_none() {
            return std::ptr::null_mut();
        }
        for table in &self.tables {
            let start = table.as_ptr() as u64;
            if address >= start && address + size as u64 <= start + table.len() as u64 {
                return address as *mut c_void;
            }
        }
        // Unknown SystemMemory operation regions are *simulated*. This is not
        // evidence of actual EC/NVS/MMIO behaviour or successful native _INI.
        let mut regions = self.regions.lock().unwrap();
        if regions.len() >= 4096 {
            return std::ptr::null_mut();
        }
        let data = regions
            .entry((address, size))
            .or_insert_with(|| std::vec![0u64;size.div_ceil(8)].into_boxed_slice());
        data.as_mut_ptr().cast()
    }
    fn unmap(&self, _p: *mut c_void, _size: usize) {}
    fn physical_address(&self, p: *mut c_void) -> Result<u64, Status> {
        Ok(p as u64)
    }
    fn readable(&self, _p: *mut c_void, _size: usize) -> bool {
        false
    }
    fn writable(&self, _p: *mut c_void, _size: usize) -> bool {
        false
    }
    fn timer_100ns(&self) -> u64 {
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64 / 100
    }
    fn sleep(&self, millis: u64) -> bool {
        std::thread::sleep(Duration::from_millis(millis.min(100)));
        true
    }
    fn stall(&self, micros: u32) {
        std::thread::sleep(Duration::from_micros(u64::from(micros).min(100_000)));
    }
    fn thread_id(&self) -> u64 {
        std::thread_local! {static ID:usize={static NEXT:AtomicUsize=AtomicUsize::new(1);NEXT.fetch_add(1,Ordering::Relaxed)};}
        ID.with(|id| *id as u64)
    }
    fn irq_save(&self) -> usize {
        0
    }
    fn irq_restore(&self, _flags: usize) {}
    fn execute(&self, _kind: u32, function: Work, context: *mut c_void) -> Status {
        self.active.fetch_add(1, Ordering::AcqRel);
        let context = context as usize;
        let active = &self.active as *const AtomicUsize as usize;
        match std::thread::Builder::new().spawn(move || {
            // SAFETY: backend remains static, termination waits for this worker;
            // ACPICA owns callback/context until completion.
            unsafe {
                function(context as *mut c_void);
                (*(active as *const AtomicUsize)).fetch_sub(1, Ordering::Release);
            }
        }) {
            Ok(_) => OK,
            Err(_) => {
                self.active.fetch_sub(1, Ordering::Release);
                LIMIT
            }
        }
    }
    fn remove_irq(&self, _irq: u32, _handler: IrqHandler) -> Status {
        OK
    }
    fn quiesce(&self) {
        self.wait_events();
    }
    fn wait_events(&self) {
        while self.active.load(Ordering::Acquire) != 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn read_port(&self, _address: u16, width: u32) -> Result<u32, Status> {
        if matches!(width, 8 | 16 | 32) {
            Ok(0)
        } else {
            Err(BAD_PARAMETER)
        }
    }
    fn write_port(&self, _a: u16, _w: u32, _v: u32) -> Status {
        OK
    }
    fn read_pci(&self, _id: PciId, _reg: u32, _width: u32) -> Result<u64, Status> {
        Ok(0)
    }
    fn write_pci(&self, _id: PciId, _reg: u32, _width: u32, _value: u64) -> Status {
        OK
    }
    fn log(&self, message: &[u8]) {
        std::eprintln!("{}", String::from_utf8_lossy(message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Mode, Value};
    unsafe extern "C" {
        fn tk_acpi_table_references(index: u32) -> u32;
        fn tk_acpi_table(index: u32, out: *mut u8, capacity: usize, used: *mut usize) -> Status;
    }
    static NOTIFIED: AtomicUsize = AtomicUsize::new(0);
    fn notification(path: &str, value: u32) {
        assert_eq!(path, "\\_SB_.PWRB");
        NOTIFIED.store(value as usize, Ordering::Release);
    }
    #[test]
    fn real_interpreter_namespace_evaluation_and_notify() {
        let data = include_bytes!("../tests/synthetic.aml")
            .to_vec()
            .into_boxed_slice();
        let backend = OfflineBackend::from_tables(std::vec![data])
            .unwrap()
            .register();
        // SAFETY: single instance with only simulated hardware and authored AML.
        let mut e = unsafe { Engine::initialize(backend, Mode::Offline) }.unwrap();
        // Every owned copy, including output-limit failure, balances the C
        // descriptor's validation reference rather than pinning it forever.
        let count = e.table_count().unwrap();
        assert!(count > 0);
        for index in 0..count {
            // SAFETY: live offline engine; test-only observer takes table mutex.
            let before = unsafe { tk_acpi_table_references(index) };
            assert_ne!(before, u32::MAX);
            for _ in 0..8 {
                assert!(!e.table(index).unwrap().is_empty());
                let mut byte = 0;
                let mut used = usize::MAX;
                // SAFETY: writable one-byte buffer and live engine, bounded C API.
                assert_eq!(
                    unsafe { tk_acpi_table(index, &mut byte, 1, &mut used) },
                    LIMIT
                );
                assert_eq!(used, 0);
                // SAFETY: same synchronized host-only observer.
                assert_eq!(unsafe { tk_acpi_table_references(index) }, before);
            }
        }
        e.install_notify(notification).unwrap();
        e.initialize_objects().unwrap();
        assert_eq!(
            e.evaluate("\\_S5", &[]).unwrap(),
            Value::Package(std::vec![Value::Integer(5), Value::Integer(5)])
        );
        assert_eq!(e.integer("\\_SB.PWRB._STA").unwrap(), 15);
        assert!(
            e.namespace()
                .unwrap()
                .iter()
                .any(|n| n.path == "\\_SB_.PWRB" && n.kind == 6)
        );
        assert!(matches!(
            e.evaluate("\\_SB.PCI0._PRT", &[]),
            Ok(Value::Package(_))
        ));
        e.evaluate("\\_SB.PWRB.TEST", &[]).unwrap();
        backend.0.wait_events();
        assert_eq!(NOTIFIED.load(Ordering::Acquire), 0x80);
        assert!(e.evaluate("\\DOES_NOT_EXIST", &[]).is_err());
        drop(e);
    }
}
