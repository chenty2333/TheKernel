//! Native x86-64 ET_REL module admission.
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{
    ffi::c_char,
    sync::atomic::{AtomicBool, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::{WaitQueue, current};
use linux_raw_sys::general::{CAP_SYS_MODULE, O_ACCMODE, O_NONBLOCK, O_TRUNC, O_WRONLY};
use spin::Lazy;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, vm_load, vm_load_until_nul_bounded};

use crate::{
    file::{File, FileLike, get_typed_file},
    jit_memory::{self, ExecutableCode, MemoryError},
    mm::map_usercopy_error,
    task::AsThread,
};
const MAX: usize = 16 * 1024 * 1024;
const ARGMAX: usize = 4096;
const NAMEMAX: usize = 56;
const EH: usize = 64;
const SH: usize = 64;
const SYM: usize = 24;
const RELA: usize = 24;
const ETREL: u16 = 1;
const X64: u16 = 62;
const PROG: u32 = 1;
const SYMTAB: u32 = 2;
const STRTAB: u32 = 3;
const RELOC: u32 = 4;
const NOBITS: u32 = 8;
const ALLOC: u64 = 2;
const WRITE: u64 = 1;
const EXEC: u64 = 4;
const UNDEF: u16 = 0;
const ABS: u16 = 0xfff1;
const FUNC: u8 = 2;
const GLOBAL: u8 = 1;
const WEAK: u8 = 2;
const MODULE_INIT_IGNORE_MODVERSIONS: u32 = 1;
const MODULE_INIT_IGNORE_VERMAGIC: u32 = 2;
const MODULE_INIT_COMPRESSED_FILE: u32 = 4;
const MODULE_RELEASE: &[u8] = b"6.12.103";
// Linux limits PERF_TYPE_KPROBE function names to KSYM_NAME_LEN.  Keep the
// same bounded usercopy contract here instead of allowing an attr pointer to
// drive an unbounded scan.
pub(crate) const KPROBE_SYMBOL_MAX: usize = 128;
#[derive(Clone, Copy)]
struct S {
    n: u32,
    t: u32,
    f: u64,
    o: usize,
    z: usize,
    l: u32,
    i: u32,
    a: usize,
    e: usize,
}
#[derive(Clone, Copy)]
struct Y {
    n: u32,
    i: u8,
    s: u16,
    v: usize,
    z: usize,
}
#[derive(Clone, Copy)]
enum P {
    T(usize),
    D(usize),
    R(usize),
}
enum State {
    Coming,
    Live(Box<M>),
    Going,
}
struct Slot {
    name: String,
    state: State,
    refs: u32,
    deps: u32,
}
struct M {
    name: String,
    code: ExecutableCode,
    rodata: Option<ExecutableCode>,
    data: Option<jit_memory::WritableCode>,
    charps: Vec<Vec<u8>>,
    init: Entry,
    exit: Option<Entry>,
    dependencies: Vec<String>,
    /// Exact live-module symbols used while applying this ET_REL image.
    /// `dependencies` prevents unload; these bindings additionally fence the
    /// resolve-to-activate window against an unload/reload of the same name.
    provider_bindings: Vec<ProviderBinding>,
    exports: Vec<Export>,
}
struct Export {
    name: String,
    address: usize,
    crc: Option<u64>,
}
struct KernelExport {
    name: &'static str,
    address: usize,
    crc: u64,
}
static KERNEL_EXPORTS: Lazy<spin::Mutex<Vec<KernelExport>>> =
    Lazy::new(|| spin::Mutex::new(Vec::new()));
static KERNEL_EXPORTS_INITIALIZED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn _stext();
    fn _etext();
}

#[inline]
fn canonical_kernel_address(address: u64) -> bool {
    // The x86_64 product is built with the conventional 48-bit canonical
    // layout.  Do not turn a low user pointer, a non-canonical value, or a
    // future LA57-only spelling into a text-patch candidate.
    address >> 48 == 0xffff
}

#[inline]
fn kernel_text_contains(address: usize) -> bool {
    let start = _stext as *const () as usize;
    let end = _etext as *const () as usize;
    address >= start && address < end
}

/// True only for the legacy direct-address extension of PERF_TYPE_KPROBE.
/// Standard perf ABI callers pass a user pointer to a NUL-terminated function
/// name in config1; this discriminator prevents a low pointer from ever being
/// interpreted as an instruction address.
pub(crate) fn is_direct_kprobe_address(address: u64) -> bool {
    canonical_kernel_address(address)
}

/// Admit one final kprobe instruction address.  This is deliberately stricter
/// than the text-patch mapping check: only the linked kernel `.text` or the
/// executable allocation of a currently live module is patchable.
pub(crate) fn validate_kprobe_address(address: u64) -> AxResult<()> {
    if !canonical_kernel_address(address) {
        return Err(AxError::InvalidInput);
    }
    let address = usize::try_from(address).map_err(|_| AxError::InvalidInput)?;
    if kernel_text_contains(address) {
        return Ok(());
    }
    let modules = MODULES.lock();
    if modules.iter().any(|slot| {
        matches!(&slot.state, State::Live(module) if module.code.contains_executable_address(address))
    }) {
        return Ok(());
    }
    Err(AxError::PermissionDenied)
}

/// Return probes may only replace the ABI return word at a known function
/// entry.  An arbitrary executable byte is a valid ordinary kprobe site but
/// does not necessarily have a return address at the interrupted RSP.
pub(crate) fn validate_kretprobe_address(address: u64) -> AxResult<()> {
    validate_kprobe_address(address)?;
    if KERNEL_EXPORTS
        .lock()
        .iter()
        .any(|symbol| symbol.address == address as usize)
    {
        return Ok(());
    }
    let address = usize::try_from(address).map_err(|_| AxError::InvalidInput)?;
    if MODULES.lock().iter().any(|slot| {
        matches!(&slot.state, State::Live(module) if module.exports.iter().any(|symbol| {
            symbol.address == address && module.code.contains_executable_address(address)
        }))
    }) {
        return Ok(());
    }
    Err(AxError::InvalidInput)
}

/// Pins the live module which owns a kprobe instruction.  Kernel `.text`
/// needs no dynamic owner; a module must remain LIVE until the final INT3 has
/// been restored, so module unload observes this through its normal `refs`
/// gate.
pub(crate) fn retain_kprobe_address(address: u64) -> AxResult<()> {
    validate_kprobe_address(address)?;
    let address = usize::try_from(address).map_err(|_| AxError::InvalidInput)?;
    if kernel_text_contains(address) {
        return Ok(());
    }
    let mut modules = MODULES.lock();
    let owner = modules.iter_mut().find(|slot| {
        matches!(&slot.state, State::Live(module) if module.code.contains_executable_address(address))
    });
    let owner = owner.ok_or(AxError::PermissionDenied)?;
    owner.refs = owner.refs.checked_add(1).ok_or(AxError::NoMemory)?;
    Ok(())
}

/// Drops the pin paired with [`retain_kprobe_address`].  A live probe owns
/// the extra reference, therefore a module cannot transition to GOING before
/// this point.
pub(crate) fn release_kprobe_address(address: u64) {
    let Ok(address) = usize::try_from(address) else {
        return;
    };
    if kernel_text_contains(address) {
        return;
    }
    let mut modules = MODULES.lock();
    if let Some(owner) = modules.iter_mut().find(|slot| {
        matches!(&slot.state, State::Live(module) if module.code.contains_executable_address(address))
    }) {
        // refs includes the module's own base reference. An absent owner or a
        // base-only slot means the caller has nothing to release.
        if owner.refs > 1 {
            owner.refs -= 1;
        }
    }
}

/// Resolve an exported kernel or live-module text symbol and apply its
/// checked byte offset.  No hash or linker-name guesswork is used: names are
/// admitted only from the explicit kernel export registry or a live module's
/// own ELF export table.
pub(crate) fn resolve_kprobe_symbol(name: &str, offset: u64) -> AxResult<u64> {
    if name.is_empty() || name.len() >= KPROBE_SYMBOL_MAX || name.as_bytes().contains(&0) {
        return Err(AxError::InvalidInput);
    }

    let kernel_address = KERNEL_EXPORTS
        .lock()
        .iter()
        .find(|symbol| symbol.name == name)
        .map(|symbol| symbol.address);
    let address = if let Some(address) = kernel_address {
        address
    } else {
        let modules = MODULES.lock();
        modules
            .iter()
            .find_map(|slot| match &slot.state {
                State::Live(module) => module
                    .exports
                    .iter()
                    .find(|symbol| {
                        symbol.name == name
                            && module.code.contains_executable_address(symbol.address)
                    })
                    .map(|symbol| symbol.address),
                State::Coming | State::Going => None,
            })
            .ok_or(LinuxError::ENOENT)?
    };
    let address = address
        .checked_add(usize::try_from(offset).map_err(|_| AxError::InvalidInput)?)
        .ok_or(AxError::InvalidInput)?;
    validate_kprobe_address(address as u64)?;
    Ok(address as u64)
}

/// Native module ABI version exported to ET_REL modules.
///
/// Keep this a C entry point rather than a link-time constant: an ET_REL
/// object can use the same relocation machinery for both data-free queries
/// and callable kernel services.
#[unsafe(no_mangle)]
pub extern "C" fn thekernel_module_abi_version() -> u32 {
    1
}

/// Cooperatively yield the current module task.
#[unsafe(no_mangle)]
pub extern "C" fn thekernel_module_yield() {
    axtask::yield_now();
}

/// Return the Linux PID associated with the caller's process.
#[unsafe(no_mangle)]
pub extern "C" fn thekernel_module_current_pid() -> u32 {
    current().as_thread().proc_data.proc.pid() as u32
}

/// Return the logical CPU on which the module is executing.
#[unsafe(no_mangle)]
pub extern "C" fn thekernel_module_current_cpu() -> u32 {
    current().cpu_id()
}

/// Return the monotonic kernel clock in nanoseconds.
#[unsafe(no_mangle)]
pub extern "C" fn thekernel_module_monotonic_time_ns() -> u64 {
    axhal::time::monotonic_time_nanos()
}

/// Publish the fixed v1 native-module ABI before userspace can load ET_REL
/// images.  The batch is preflighted and reserved under one lock so a failed
/// bootstrap never exposes a partial kernel export table.
pub(crate) fn init_kernel_module_exports() -> AxResult<()> {
    if KERNEL_EXPORTS_INITIALIZED.load(Ordering::Acquire) {
        return Ok(());
    }

    let exports = [
        (
            "thekernel_module_abi_version",
            thekernel_module_abi_version as *const () as usize,
        ),
        ("thekernel_module_yield", thekernel_module_yield as *const () as usize),
        (
            "thekernel_module_current_pid",
            thekernel_module_current_pid as *const () as usize,
        ),
        (
            "thekernel_module_current_cpu",
            thekernel_module_current_cpu as *const () as usize,
        ),
        (
            "thekernel_module_monotonic_time_ns",
            thekernel_module_monotonic_time_ns as *const () as usize,
        ),
    ];
    let mut registry = KERNEL_EXPORTS.lock();
    if KERNEL_EXPORTS_INITIALIZED.load(Ordering::Relaxed) {
        return Ok(());
    }
    if exports
        .iter()
        .any(|(name, _)| registry.iter().any(|export| export.name == *name))
    {
        return Err(LinuxError::EEXIST.into());
    }
    registry
        .try_reserve(exports.len())
        .map_err(|_| AxError::NoMemory)?;
    for (name, address) in exports {
        registry.push(KernelExport {
            name,
            address,
            // The native v1 surface deliberately has no generated
            // modversion CRC yet.  A module without __versions is admitted;
            // a consumer that supplies a CRC must explicitly match zero.
            crc: 0,
        });
    }
    KERNEL_EXPORTS_INITIALIZED.store(true, Ordering::Release);
    Ok(())
}

/// Makes a stable kernel entry point visible to relocatable modules.
///
/// Registration is deliberately explicit: modules never infer exports from
/// Rust linker names, and a `__versions` consumer must match this CRC.
pub fn register_module_export(name: &'static str, address: usize, crc: u64) -> AxResult<()> {
    if name.is_empty() || name.len() > NAMEMAX || address == 0 {
        return Err(AxError::InvalidInput);
    }
    let mut exports = KERNEL_EXPORTS.lock();
    if exports.iter().any(|export| export.name == name) {
        return Err(LinuxError::EEXIST.into());
    }
    exports.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    exports.push(KernelExport { name, address, crc });
    Ok(())
}
#[derive(Clone, Copy)]
struct Entry {
    offset: usize,
    size: usize,
}
static MODULES: Lazy<spin::Mutex<Vec<Slot>>> = Lazy::new(|| spin::Mutex::new(Vec::new()));
struct LoadFlight {
    key: LoadKey,
    state: spin::Mutex<Option<i32>>,
    done: WaitQueue,
}
struct LoadKey {
    ofd: u64,
    uargs_hash: u64,
    uargs: Vec<u8>,
    flags: u32,
}
impl LoadKey {
    fn new(ofd: u64, uargs: Vec<u8>, flags: u32) -> Self {
        Self {
            ofd,
            uargs_hash: uargs_hash(&uargs),
            uargs,
            flags,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.ofd == other.ofd
            && self.flags == other.flags
            && self.uargs_hash == other.uargs_hash
            // The hash only avoids most byte comparisons; exact bytes make
            // equal hashes collision-safe.
            && self.uargs == other.uargs
    }
}
static LOAD_FLIGHTS: Lazy<spin::Mutex<Vec<Arc<LoadFlight>>>> =
    Lazy::new(|| spin::Mutex::new(Vec::new()));

fn uargs_hash(bytes: &[u8]) -> u64 {
    // This is an index, not an identity: LoadKey::matches always verifies
    // complete bytes before sharing a flight.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^ (bytes.len() as u64)
}

fn load_flight(key: LoadKey) -> AxResult<(Arc<LoadFlight>, bool)> {
    let mut flights = LOAD_FLIGHTS.lock();
    if let Some(flight) = flights.iter().find(|flight| flight.key.matches(&key)) {
        return Ok((flight.clone(), false));
    }
    flights.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    let flight = Arc::try_new(LoadFlight {
        key,
        state: spin::Mutex::new(None),
        done: WaitQueue::new(),
    })
    .map_err(|_| AxError::NoMemory)?;
    flights.push(flight.clone());
    Ok((flight, true))
}

fn complete_flight(flight: &Arc<LoadFlight>, result: AxResult<isize>) -> AxResult<isize> {
    let code = match result {
        Ok(_) => 0,
        Err(error) => LinuxError::from(error).code(),
    };
    *flight.state.lock() = Some(code);
    LOAD_FLIGHTS
        .lock()
        .retain(|candidate| !Arc::ptr_eq(candidate, flight));
    flight.done.notify_all(false);
    if code == 0 {
        Ok(0)
    } else {
        Err(LinuxError::try_from(code)
            .unwrap_or(LinuxError::EINVAL)
            .into())
    }
}

fn await_flight(flight: Arc<LoadFlight>) -> AxResult<isize> {
    flight
        .done
        .wait_until(|| flight.state.lock().is_some())
        .map_err(AxError::from)?;
    let code = flight
        .state
        .lock()
        .expect("load flight completed without result");
    if code == 0 {
        Ok(0)
    } else {
        Err(LinuxError::try_from(code)
            .unwrap_or(LinuxError::EINVAL)
            .into())
    }
}
fn no<T>() -> AxResult<T> {
    Err(LinuxError::ENOEXEC.into())
}
fn copy_vec(bytes: &[u8]) -> AxResult<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len())
        .map_err(|_| AxError::NoMemory)?;
    out.extend_from_slice(bytes);
    Ok(out)
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}
fn gzip_decode(input: &[u8]) -> AxResult<Vec<u8>> {
    if input.len() < 18 || input[..3] != [0x1f, 0x8b, 8] {
        return no();
    }
    let flags = input[3];
    if flags & 0xe0 != 0 {
        return no();
    }
    let mut at = 10usize;
    if flags & 4 != 0 {
        let len = usize::from(u16x(input, at)?);
        at = at.checked_add(2 + len).ok_or(AxError::InvalidExecutable)?;
    }
    for bit in [8u8, 16] {
        if flags & bit != 0 {
            let end = input
                .get(at..)
                .ok_or(AxError::InvalidExecutable)?
                .iter()
                .position(|byte| *byte == 0)
                .ok_or(AxError::InvalidExecutable)?;
            at = at.checked_add(end + 1).ok_or(AxError::InvalidExecutable)?;
        }
    }
    if flags & 2 != 0 {
        at = at.checked_add(2).ok_or(AxError::InvalidExecutable)?;
    }
    let trailer = input
        .len()
        .checked_sub(8)
        .ok_or(AxError::InvalidExecutable)?;
    if at > trailer {
        return no();
    }
    // A gzip member contains a raw DEFLATE stream (not a zlib wrapper).  The
    // bounded form is important here: module loading must never let an
    // attacker turn a small compressed object into an unbounded allocation.
    let output = miniz_oxide::inflate::decompress_to_vec_with_limit(&input[at..trailer], MAX)
        .map_err(|_| AxError::InvalidExecutable)?;
    if crc32(&output) != u32x(input, trailer)? || output.len() as u32 != u32x(input, trailer + 4)? {
        return no();
    }
    Ok(output)
}
fn zstd_decode(input: &[u8]) -> AxResult<Vec<u8>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(MAX)
        .map_err(|_| AxError::NoMemory)?;
    output.resize(MAX, 0);
    let written = ruzstd::decoding::FrameDecoder::new()
        .decode_all(input, &mut output)
        .map_err(|_| AxError::InvalidExecutable)?;
    output.truncate(written);
    Ok(output)
}
fn decode_module_image(input: &[u8], flags: u32) -> AxResult<Vec<u8>> {
    if flags & MODULE_INIT_COMPRESSED_FILE == 0 {
        return copy_vec(input);
    }
    if input.starts_with(&[0x1f, 0x8b]) {
        return gzip_decode(input);
    }
    if input.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return zstd_decode(input);
    }
    // XZ is deliberately recognized separately: the bounded in-tree decoder
    // is selected by its stream magic and never attempts to interpret another
    // compressed format as an ELF object.
    if input.starts_with(b"\xfd7zXZ\0") {
        return xz_decode(input);
    }
    no()
}
fn xz_decode(input: &[u8]) -> AxResult<Vec<u8>> {
    // Linux's XZ module path has to accept ordinary LZMA2 streams, including
    // the BCJ/CRC64 forms emitted by distribution module packagers.  The
    // decoder is pure Rust/no_std.  Its dictionary is bounded by the module
    // image limit, so neither an XZ header nor its compressed size can make
    // the loader retain unbounded memory.
    let mut dictionary = Vec::new();
    dictionary
        .try_reserve_exact(MAX)
        .map_err(|_| AxError::NoMemory)?;
    dictionary.extend(core::iter::repeat_n(0, MAX));
    let mut decoder = xz4rust::XzDecoder::with_fixed_size_dict(&mut dictionary);
    let mut output = Vec::new();
    let mut offset = 0usize;
    let mut scratch = [0u8; 8192];
    loop {
        let result = decoder
            .decode(
                input.get(offset..).ok_or(AxError::InvalidExecutable)?,
                &mut scratch,
            )
            .map_err(|_| AxError::InvalidExecutable)?;
        let consumed = result.input_consumed();
        let produced = result.output_produced();
        if produced > MAX.saturating_sub(output.len()) {
            return no();
        }
        output
            .try_reserve(produced)
            .map_err(|_| AxError::NoMemory)?;
        output.extend_from_slice(&scratch[..produced]);
        offset = offset
            .checked_add(consumed)
            .ok_or(AxError::InvalidExecutable)?;
        if result.is_end_of_stream() {
            // XZ permits zero padding between concatenated streams.  Module
            // images are one object: only padding after the verified stream
            // is allowed, never an unnoticed second payload.
            if input
                .get(offset..)
                .ok_or(AxError::InvalidExecutable)?
                .iter()
                .any(|byte| *byte != 0)
            {
                return no();
            }
            return Ok(output);
        }
        if consumed == 0 && produced == 0 {
            return no();
        }
    }
}
fn copy_name(name: &str) -> AxResult<String> {
    let mut out = String::new();
    out.try_reserve_exact(name.len())
        .map_err(|_| AxError::NoMemory)?;
    // Capacity was fallibly reserved above; copying cannot allocate.
    out.push_str(name);
    Ok(out)
}
fn u16x(b: &[u8], o: usize) -> AxResult<u16> {
    b.get(o..o.checked_add(2).ok_or(AxError::InvalidExecutable)?)
        .and_then(|x| x.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(AxError::InvalidExecutable)
}
fn u32x(b: &[u8], o: usize) -> AxResult<u32> {
    b.get(o..o.checked_add(4).ok_or(AxError::InvalidExecutable)?)
        .and_then(|x| x.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(AxError::InvalidExecutable)
}
fn u64x(b: &[u8], o: usize) -> AxResult<u64> {
    b.get(o..o.checked_add(8).ok_or(AxError::InvalidExecutable)?)
        .and_then(|x| x.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(AxError::InvalidExecutable)
}
fn sl(b: &[u8], o: usize, z: usize) -> AxResult<&[u8]> {
    b.get(o..o.checked_add(z).ok_or(AxError::InvalidExecutable)?)
        .ok_or(AxError::InvalidExecutable)
}
fn cs(b: &[u8], o: usize) -> AxResult<&[u8]> {
    let x = b.get(o..).ok_or(AxError::InvalidExecutable)?;
    Ok(&x[..x
        .iter()
        .position(|x| *x == 0)
        .ok_or(AxError::InvalidExecutable)?])
}
fn modinfo_dependencies(modinfo: &[u8]) -> AxResult<Vec<String>> {
    let mut out = Vec::new();
    let Some(value) = modinfo
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"depends="))
    else {
        return Ok(out);
    };
    if value.is_empty() {
        return Ok(out);
    }
    for name in value.split(|byte| *byte == b',') {
        if name.is_empty() || name.len() > NAMEMAX {
            return no();
        }
        let name = core::str::from_utf8(name).map_err(|_| AxError::IllegalBytes)?;
        if out.iter().any(|existing| existing == name) {
            continue;
        }
        out.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        out.push(copy_name(name)?);
    }
    Ok(out)
}
fn modinfo_value<'a>(modinfo: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    modinfo
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(key))
}

fn validate_module_identity(modinfo: &[u8], flags: u32) -> AxResult<()> {
    if flags & MODULE_INIT_IGNORE_VERMAGIC == 0 {
        let vermagic = modinfo_value(modinfo, b"vermagic=").ok_or(LinuxError::ENOEXEC)?;
        // The suffix records configuration ABI tokens; this kernel has no
        // build-time compatibility aliases, so the release token must match
        // exactly before loading relocatable code.
        if vermagic.split(|byte| *byte == b' ').next() != Some(MODULE_RELEASE) {
            return Err(LinuxError::ENOEXEC.into());
        }
    }
    Ok(())
}

fn validate_modversions(image: &[u8], sections: &[S], names: &[u8], flags: u32) -> AxResult<()> {
    if flags & MODULE_INIT_IGNORE_MODVERSIONS != 0 {
        return Ok(());
    }
    for section in sections {
        if cs(names, section.n as usize)? != b"__versions" {
            continue;
        }
        // x86-64 `struct modversion_info` is an unsigned-long CRC followed
        // by a 56-byte symbol name.  Reject malformed records before symbol
        // resolution; version matching itself is performed by the export
        // resolver at each undefined relocation.
        if section.z % 64 != 0 {
            return no();
        }
        for record in sl(image, section.o, section.z)?.chunks_exact(64) {
            if record[8..].iter().all(|byte| *byte == 0) {
                return no();
            }
            let _ = cs(&record[8..], 0)?;
        }
    }
    Ok(())
}
fn version_for(image: &[u8], sections: &[S], names: &[u8], symbol: &[u8]) -> AxResult<Option<u64>> {
    for section in sections {
        if cs(names, section.n as usize)? != b"__versions" {
            continue;
        }
        if section.z % 64 != 0 {
            return no();
        }
        for record in sl(image, section.o, section.z)?.chunks_exact(64) {
            if cs(&record[8..], 0)? == symbol {
                return Ok(Some(u64::from_le_bytes(record[..8].try_into().unwrap())));
            }
        }
    }
    Ok(None)
}

struct ExternalSymbol {
    address: usize,
    /// A live module provider must remain pinned until the consumer's exit.
    /// Kernel exports have static lifetime and therefore carry no owner.
    provider: Option<String>,
}

struct ProviderBinding {
    provider: String,
    symbol: String,
    address: usize,
}

fn resolve_external_symbol(name: &[u8], required_crc: Option<u64>) -> AxResult<ExternalSymbol> {
    let name = core::str::from_utf8(name).map_err(|_| AxError::InvalidExecutable)?;
    if let Some(symbol) = KERNEL_EXPORTS
        .lock()
        .iter()
        .find(|symbol| symbol.name == name)
    {
        if required_crc.is_some_and(|crc| crc != symbol.crc) {
            return no();
        }
        return Ok(ExternalSymbol {
            address: symbol.address,
            provider: None,
        });
    }
    let modules = MODULES.lock();
    for slot in modules.iter() {
        let State::Live(module) = &slot.state else {
            continue;
        };
        if let Some(symbol) = module.exports.iter().find(|symbol| symbol.name == name) {
            if let Some(crc) = required_crc {
                if symbol.crc != Some(crc) {
                    return no();
                }
            }
            return Ok(ExternalSymbol {
                address: symbol.address,
                provider: Some(copy_name(&slot.name)?),
            });
        }
    }
    Err(LinuxError::ENOENT.into())
}

fn module_exports(
    image: &[u8],
    tab: S,
    strings: &[u8],
    sections: &[S],
    places: &[Option<P>],
    tb: usize,
    db: usize,
    rb: usize,
) -> AxResult<Vec<Export>> {
    let mut exports = Vec::new();
    for index in 0..tab.z / SYM {
        let symbol = sy(image, tab, index)?;
        let binding = symbol.i >> 4;
        if binding != GLOBAL && binding != WEAK
            || symbol.s == UNDEF
            || symbol.s == ABS
            || symbol.n == 0
        {
            continue;
        }
        let section = *sections
            .get(symbol.s as usize)
            .ok_or(AxError::InvalidExecutable)?;
        if symbol.v > section.z || symbol.z > section.z - symbol.v {
            return no();
        }
        let place = places
            .get(symbol.s as usize)
            .ok_or(AxError::InvalidExecutable)?
            .ok_or(AxError::InvalidExecutable)?;
        let name_bytes = cs(strings, symbol.n as usize)?;
        if name_bytes.is_empty() || name_bytes.len() > 255 {
            return no();
        }
        let name = core::str::from_utf8(name_bytes).map_err(|_| AxError::InvalidExecutable)?;
        if exports.iter().any(|export: &Export| export.name == name) {
            return no();
        }
        exports.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        exports.push(Export {
            name: copy_name(name)?,
            address: pa(place, tb, db, rb)?
                .checked_add(symbol.v)
                .ok_or(AxError::InvalidExecutable)?,
            crc: version_for(image, sections, strings, name_bytes)?,
        });
    }
    Ok(exports)
}
fn al(x: usize, a: usize) -> AxResult<usize> {
    if a == 0 || !a.is_power_of_two() {
        return no();
    }
    x.checked_add(a - 1)
        .map(|x| x & !(a - 1))
        .ok_or(AxError::InvalidExecutable)
}
fn cap() -> bool {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_MODULE)
}
fn me(e: MemoryError) -> AxError {
    match e {
        MemoryError::Unavailable(x) | MemoryError::Quarantined(x) | MemoryError::Retained(x) => x,
    }
}
fn sh(b: &[u8]) -> AxResult<Vec<S>> {
    if b.len() < EH
        || &b[..4] != b"\x7fELF"
        || b[4] != 2
        || b[5] != 1
        || b[6] != 1
        || u16x(b, 16)? != ETREL
        || u16x(b, 18)? != X64
    {
        return no();
    }
    let o = usize::try_from(u64x(b, 40)?).map_err(|_| AxError::InvalidExecutable)?;
    let e = usize::from(u16x(b, 58)?);
    let n = usize::from(u16x(b, 60)?);
    if e != SH || n == 0 {
        return no();
    }
    sl(b, o, e.checked_mul(n).ok_or(AxError::InvalidExecutable)?)?;
    let mut sections = Vec::new();
    sections
        .try_reserve_exact(n)
        .map_err(|_| AxError::NoMemory)?;
    for j in 0..n {
        let p = o + j * SH;
        sections.push(S {
            n: u32x(b, p)?,
            t: u32x(b, p + 4)?,
            f: u64x(b, p + 8)?,
            o: usize::try_from(u64x(b, p + 24)?).map_err(|_| AxError::InvalidExecutable)?,
            z: usize::try_from(u64x(b, p + 32)?).map_err(|_| AxError::InvalidExecutable)?,
            l: u32x(b, p + 40)?,
            i: u32x(b, p + 44)?,
            a: usize::try_from(u64x(b, p + 48)?).map_err(|_| AxError::InvalidExecutable)?,
            e: usize::try_from(u64x(b, p + 56)?).map_err(|_| AxError::InvalidExecutable)?,
        });
    }
    Ok(sections)
}
fn sy(b: &[u8], t: S, j: usize) -> AxResult<Y> {
    if t.e != SYM || j >= t.z / SYM {
        return no();
    }
    let p = t.o.checked_add(j * SYM).ok_or(AxError::InvalidExecutable)?;
    sl(b, p, SYM)?;
    Ok(Y {
        n: u32x(b, p)?,
        i: b[p + 4],
        s: u16x(b, p + 6)?,
        v: usize::try_from(u64x(b, p + 8)?).map_err(|_| AxError::InvalidExecutable)?,
        z: usize::try_from(u64x(b, p + 16)?).map_err(|_| AxError::InvalidExecutable)?,
    })
}
fn pa(p: P, tb: usize, db: usize, rb: usize) -> AxResult<usize> {
    match p {
        P::T(x) => tb.checked_add(x),
        P::D(x) => db.checked_add(x),
        P::R(x) => rb.checked_add(x),
    }
    .ok_or(AxError::InvalidExecutable)
}
fn module_entry(y: Y, sec: S, p: Option<P>) -> AxResult<Entry> {
    let Some(P::T(offset)) = p else { return no() };
    if y.z == 0 || y.v >= sec.z || y.z > sec.z - y.v {
        return no();
    }
    Ok(Entry {
        offset: offset.checked_add(y.v).ok_or(AxError::InvalidExecutable)?,
        size: y.z,
    })
}
fn put(t: &mut [u8], d: &mut [u8], r: &mut [u8], p: P, v: &[u8]) -> AxResult<()> {
    match p {
        P::T(x) => t.get_mut(x..x + v.len()),
        P::D(x) => d.get_mut(x..x + v.len()),
        P::R(x) => r.get_mut(x..x + v.len()),
    }
    .ok_or(AxError::InvalidExecutable)?
    .copy_from_slice(v);
    Ok(())
}
fn rel(
    b: &[u8],
    ss: &[S],
    ps: &[Option<P>],
    section_names: &[u8],
    symbol_names: &[u8],
    t: &mut [u8],
    d: &mut [u8],
    r: &mut [u8],
    tb: usize,
    db: usize,
    rb: usize,
) -> AxResult<Vec<ProviderBinding>> {
    // A module provider is discovered by real relocation resolution, not by
    // a caller-controlled modinfo string.  Keep every distinct binding; the
    // activation transaction turns this exact set into unload-preventing
    // dependency references before module init can execute.
    let mut providers = Vec::new();
    for reloc in ss.iter().filter(|x| x.t == RELOC) {
        let dst = ps
            .get(reloc.i as usize)
            .ok_or(AxError::InvalidExecutable)?
            .ok_or(AxError::InvalidExecutable)?;
        let dst_section = *ss.get(reloc.i as usize).ok_or(AxError::InvalidExecutable)?;
        let tab = *ss.get(reloc.l as usize).ok_or(AxError::InvalidExecutable)?;
        if tab.t != SYMTAB || reloc.e != RELA || reloc.z % RELA != 0 {
            return no();
        }
        for j in 0..reloc.z / RELA {
            let q = reloc
                .o
                .checked_add(j.checked_mul(RELA).ok_or(AxError::InvalidExecutable)?)
                .ok_or(AxError::InvalidExecutable)?;
            sl(b, q, RELA)?;
            let off = usize::try_from(u64x(b, q)?).map_err(|_| AxError::InvalidExecutable)?;
            let inf = u64x(b, q + 8)?;
            let y = sy(
                b,
                tab,
                usize::try_from(inf >> 32).map_err(|_| AxError::InvalidExecutable)?,
            )?;
            let external = y.s == UNDEF;
            let s = if external {
                let symbol_name = cs(symbol_names, y.n as usize)?;
                match resolve_external_symbol(
                    symbol_name,
                    version_for(b, ss, section_names, symbol_name)?,
                ) {
                    Ok(resolved) => {
                        if let Some(provider) = resolved.provider
                            && !providers.iter().any(|existing: &ProviderBinding| {
                                existing.provider == provider
                                    && existing.address == resolved.address
                                    && existing.symbol.as_bytes() == symbol_name
                            })
                        {
                            providers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                            providers.push(ProviderBinding {
                                provider,
                                symbol: copy_name(
                                    core::str::from_utf8(symbol_name)
                                        .map_err(|_| AxError::InvalidExecutable)?,
                                )?,
                                address: resolved.address,
                            });
                        }
                        resolved.address as i128
                    }
                    Err(_) if y.i >> 4 == WEAK => 0,
                    Err(error) => return Err(error),
                }
            } else {
                let sec = *ss.get(y.s as usize).ok_or(AxError::InvalidExecutable)?;
                let sp = ps
                    .get(y.s as usize)
                    .ok_or(AxError::InvalidExecutable)?
                    .ok_or(AxError::InvalidExecutable)?;
                if y.v > sec.z || y.z > sec.z - y.v {
                    return no();
                }
                pa(sp, tb, db, rb)?
                    .checked_add(y.v)
                    .ok_or(AxError::InvalidExecutable)? as i128
            };
            let width = match inf as u32 {
                1 => 8,
                2 | 4 | 10 | 11 => 4,
                _ => return no(),
            };
            if off.checked_add(width).ok_or(AxError::InvalidExecutable)? > dst_section.z {
                return no();
            }
            let dp = match dst {
                P::T(x) => P::T(x.checked_add(off).ok_or(AxError::InvalidExecutable)?),
                P::D(x) => P::D(x.checked_add(off).ok_or(AxError::InvalidExecutable)?),
                P::R(x) => P::R(x.checked_add(off).ok_or(AxError::InvalidExecutable)?),
            };
            let a = u64x(b, q + 16)? as i64 as i128;
            let p = pa(dp, tb, db, rb)? as i128;
            match inf as u32 {
                1 => put(
                    t,
                    d,
                    r,
                    dp,
                    &u64::try_from(s + a)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                2 | 4 => put(
                    t,
                    d,
                    r,
                    dp,
                    &i32::try_from(s + a - p)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                10 => put(
                    t,
                    d,
                    r,
                    dp,
                    &u32::try_from(s + a)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                11 => put(
                    t,
                    d,
                    r,
                    dp,
                    &i32::try_from(s + a)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                _ => unreachable!(),
            }
        }
    }
    Ok(providers)
}
struct ParamArg {
    name: Vec<u8>,
    value: Vec<u8>,
    bare: bool,
}
fn args(b: &[u8]) -> AxResult<Vec<ParamArg>> {
    let (mut i, mut stop) = (0, false);
    let mut out = Vec::new();
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1
        }
        if i == b.len() {
            break;
        }
        let mut x = Vec::new();
        let mut q = 0;
        while i < b.len() && (q != 0 || !b[i].is_ascii_whitespace()) {
            let z = b[i];
            i += 1;
            if q != 0 {
                if z == q {
                    q = 0
                } else if z == b'\\' && i < b.len() {
                    x.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    x.push(b[i]);
                    i += 1
                } else {
                    x.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    x.push(z)
                }
            } else if z == b'\'' || z == b'"' {
                q = z
            } else {
                x.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                x.push(z)
            }
        }
        if q != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        if x == b"--" {
            stop = true;
            continue;
        }
        if stop {
            continue;
        }
        let (mut n, value, bare) = if let Some(e) = x.iter().position(|x| *x == b'=') {
            (copy_vec(&x[..e])?, copy_vec(&x[e + 1..])?, false)
        } else {
            (x, Vec::new(), true)
        };
        while n.first() == Some(&b'-') {
            n.remove(0);
        }
        if n.is_empty() {
            return Err(LinuxError::EINVAL.into());
        }
        out.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        out.push(ParamArg {
            name: n,
            value,
            bare,
        })
    }
    Ok(out)
}
fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x == y || (*x == b'-' && *y == b'_') || (*x == b'_' && *y == b'-'))
}
fn num(x: &[u8], sg: bool, bits: u32) -> AxResult<u64> {
    let s = core::str::from_utf8(x).map_err(|_| LinuxError::EINVAL)?;
    if sg {
        let neg = s.starts_with('-');
        let t = s.trim_start_matches(['+', '-']);
        let (t, r) = if let Some(t) = t.strip_prefix("0x") {
            (t, 16)
        } else if t.len() > 1 && t.starts_with('0') {
            (&t[1..], 8)
        } else {
            (t, 10)
        };
        let mut v = i128::from_str_radix(t, r).map_err(|_| LinuxError::EINVAL)?;
        if neg {
            v = -v
        };
        if v < -(1i128 << (bits - 1)) || v > (1i128 << (bits - 1)) - 1 {
            return Err(LinuxError::EINVAL.into());
        }
        Ok(v as u64)
    } else {
        let t = s.strip_prefix('+').unwrap_or(s);
        let (t, r) = if let Some(t) = t.strip_prefix("0x") {
            (t, 16)
        } else if t.len() > 1 && t.starts_with('0') {
            (&t[1..], 8)
        } else {
            (t, 10)
        };
        let v = u128::from_str_radix(t, r).map_err(|_| LinuxError::EINVAL)?;
        if v > (1u128 << bits) - 1 {
            return Err(LinuxError::EINVAL.into());
        }
        Ok(v as u64)
    }
}
struct DataView<'a> {
    ro: &'a [u8],
    ro_base: usize,
    rw: &'a [u8],
    rw_base: usize,
}
impl DataView<'_> {
    fn get(&self, p: usize, n: usize) -> AxResult<&[u8]> {
        let end = p.checked_add(n).ok_or(AxError::InvalidExecutable)?;
        for (base, bytes) in [(self.ro_base, self.ro), (self.rw_base, self.rw)] {
            if p >= base
                && end
                    <= base
                        .checked_add(bytes.len())
                        .ok_or(AxError::InvalidExecutable)?
            {
                return Ok(&bytes[p - base..end - base]);
            }
        }
        no()
    }
    fn u16(&self, p: usize) -> AxResult<u16> {
        Ok(u16::from_le_bytes(self.get(p, 2)?.try_into().unwrap()))
    }
    fn u32(&self, p: usize) -> AxResult<u32> {
        Ok(u32::from_le_bytes(self.get(p, 4)?.try_into().unwrap()))
    }
    fn u64(&self, p: usize) -> AxResult<u64> {
        Ok(u64::from_le_bytes(self.get(p, 8)?.try_into().unwrap()))
    }
    fn cstr(&self, p: usize) -> AxResult<&[u8]> {
        let bytes = if p >= self.ro_base
            && p < self
                .ro_base
                .checked_add(self.ro.len())
                .ok_or(AxError::InvalidExecutable)?
        {
            &self.ro[p - self.ro_base..]
        } else if p >= self.rw_base
            && p < self
                .rw_base
                .checked_add(self.rw.len())
                .ok_or(AxError::InvalidExecutable)?
        {
            &self.rw[p - self.rw_base..]
        } else {
            return no();
        };
        Ok(&bytes[..bytes
            .iter()
            .position(|x| *x == 0)
            .ok_or(AxError::InvalidExecutable)?])
    }
    fn rw_offset(&self, p: usize, n: usize) -> AxResult<usize> {
        let end = p.checked_add(n).ok_or(AxError::InvalidExecutable)?;
        if p < self.rw_base
            || end
                > self
                    .rw_base
                    .checked_add(self.rw.len())
                    .ok_or(AxError::InvalidExecutable)?
        {
            return no();
        }
        Ok(p - self.rw_base)
    }
}
fn params(
    ro: &[u8],
    ro_base: usize,
    d: &mut [u8],
    d_base: usize,
    ss: &[S],
    ps: &[Option<P>],
    which: Option<usize>,
    av: &[ParamArg],
) -> AxResult<Vec<Vec<u8>>> {
    let Some(i) = which else {
        return Ok(Vec::new());
    };
    let s = ss[i];
    let Some(p) = ps[i] else { return no() };
    if matches!(p, P::T(_)) {
        return no();
    }
    let base = pa(p, 0, d_base, ro_base)?;
    // Decode the relocated records from a stable snapshot while writes go to
    // their final RW mapping.  The snapshot is not module-owned storage.
    let old = copy_vec(d)?;
    let view = DataView {
        ro,
        ro_base,
        rw: &old,
        rw_base: d_base,
    };
    if s.z < 16 || view.u32(base)? != 1 || view.u32(base + 12)? != 0 {
        return no();
    }
    let r = (|| {
        let rs = view.u32(base + 4)? as usize;
        let n = view.u32(base + 8)? as usize;
        if rs != 40 || base + 16 + n.checked_mul(rs).ok_or(AxError::InvalidExecutable)? > base + s.z
        {
            return no();
        }
        let mut cp = Vec::new();
        for arg in av {
            let k = &arg.name;
            let v = &arg.value;
            let mut seen = false;
            for j in 0..n {
                let q = base + 16 + j * rs;
                let np = usize::try_from(view.u64(q)?).map_err(|_| AxError::InvalidExecutable)?;
                if !eq(k, view.cstr(np)?) {
                    continue;
                }
                seen = true;
                let ap =
                    usize::try_from(view.u64(q + 8)?).map_err(|_| AxError::InvalidExecutable)?;
                let cnt =
                    usize::try_from(view.u64(q + 16)?).map_err(|_| AxError::InvalidExecutable)?;
                let kind = view.u16(q + 24)?;
                let fl = view.u16(q + 26)?;
                let cap = view.u32(q + 28)? as usize;
                if fl & !1 != 0 || view.u32(q + 32)? != 0 || cap == 0 || (kind == 5 && fl & 1 != 0)
                {
                    return no();
                }
                if arg.bare && kind != 0 {
                    return Err(LinuxError::EINVAL.into());
                }
                let value_count = if arg.bare {
                    1
                } else if fl & 1 != 0 {
                    v.split(|x| *x == b',').count()
                } else {
                    1
                };
                if value_count > cap {
                    return Err(LinuxError::EINVAL.into());
                }
                let w = match kind {
                    0 => 1,
                    1 | 2 => 4,
                    3 | 4 | 6 => 8,
                    5 => cap,
                    _ => return no(),
                };
                let ao = view.rw_offset(
                    ap,
                    if kind == 5 {
                        cap
                    } else {
                        w.checked_mul(cap).ok_or(AxError::InvalidExecutable)?
                    },
                )?;
                if cnt != 0 {
                    view.rw_offset(cnt, 4)?;
                }
                for i in 0..value_count {
                    let x = if arg.bare {
                        b"1".as_slice()
                    } else if fl & 1 != 0 {
                        v.split(|x| *x == b',').nth(i).unwrap()
                    } else {
                        v
                    };
                    let z = ao + i * w;
                    match kind {
                        0 => {
                            d[z] = match x {
                                b"1" | b"y" | b"Y" | b"yes" | b"true" | b"on" => 1,
                                b"0" | b"n" | b"N" | b"no" | b"false" | b"off" => 0,
                                _ => return Err(LinuxError::EINVAL.into()),
                            }
                        }
                        1 => d[z..z + 4].copy_from_slice(&(num(x, true, 32)? as u32).to_le_bytes()),
                        2 => {
                            d[z..z + 4].copy_from_slice(&(num(x, false, 32)? as u32).to_le_bytes())
                        }
                        3 => d[z..z + 8].copy_from_slice(&num(x, true, 64)?.to_le_bytes()),
                        4 => d[z..z + 8].copy_from_slice(&num(x, false, 64)?.to_le_bytes()),
                        5 => {
                            if x.len() >= cap {
                                return Err(LinuxError::EINVAL.into());
                            }
                            d[z..z + cap].fill(0);
                            d[z..z + x.len()].copy_from_slice(x)
                        }
                        6 => {
                            let mut s = Vec::new();
                            s.try_reserve_exact(x.len() + 1)
                                .map_err(|_| AxError::NoMemory)?;
                            s.extend_from_slice(x);
                            s.push(0);
                            d[z..z + 8].copy_from_slice(&(s.as_ptr() as u64).to_le_bytes());
                            cp.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                            cp.push(s)
                        }
                        _ => unreachable!(),
                    }
                }
                if cnt != 0 {
                    let x = view.rw_offset(cnt, 4)?;
                    d[x..x + 4].copy_from_slice(&(value_count as u32).to_le_bytes())
                }
            }
            if !seen {
                return Err(LinuxError::EINVAL.into());
            }
        }
        Ok(cp)
    })();
    if r.is_err() {
        d.copy_from_slice(&old)
    }
    r
}
fn prep(b: &[u8], av: &[ParamArg], flags: u32) -> AxResult<M> {
    let ss = sh(b)?;
    let names = *ss
        .get(usize::from(u16x(b, 62)?))
        .ok_or(AxError::InvalidExecutable)?;
    if names.t != STRTAB {
        return no();
    }
    let names = sl(b, names.o, names.z)?;
    for s in &ss {
        cs(names, s.n as usize)?;
    }
    validate_modversions(b, &ss, names, flags)?;
    let param_section = ss
        .iter()
        .position(|s| cs(names, s.n as usize).is_ok_and(|x| x == b".thekernel.param.v1"));
    let tab = *ss
        .iter()
        .find(|x| x.t == SYMTAB)
        .ok_or(AxError::InvalidExecutable)?;
    let strtab = *ss.get(tab.l as usize).ok_or(AxError::InvalidExecutable)?;
    if tab.e != SYM || tab.z % SYM != 0 || strtab.t != STRTAB {
        return no();
    }
    let mut ps = Vec::new();
    ps.try_reserve_exact(ss.len())
        .map_err(|_| AxError::NoMemory)?;
    ps.extend(core::iter::repeat_n(None, ss.len()));
    let (mut tl, mut dl, mut rl) = (0, 0, 0);
    for (i, s) in ss.iter().enumerate() {
        if s.f & ALLOC == 0 {
            continue;
        }
        if s.f & (EXEC | WRITE) == EXEC | WRITE {
            return no();
        }
        if s.f & EXEC != 0 {
            tl = al(tl, s.a.max(1))?;
            ps[i] = Some(P::T(tl));
            tl = tl.checked_add(s.z).ok_or(AxError::InvalidExecutable)?
        } else if s.f & WRITE != 0 {
            dl = al(dl, s.a.max(1))?;
            ps[i] = Some(P::D(dl));
            dl = dl.checked_add(s.z).ok_or(AxError::InvalidExecutable)?
        } else {
            rl = al(rl, s.a.max(1))?;
            ps[i] = Some(P::R(rl));
            rl = rl.checked_add(s.z).ok_or(AxError::InvalidExecutable)?
        }
    }
    if tl == 0 {
        return no();
    }
    if tl
        .checked_add(dl)
        .and_then(|x| x.checked_add(rl))
        .ok_or(AxError::InvalidExecutable)?
        > MAX
    {
        return Err(AxError::NoMemory);
    }
    let mut text = jit_memory::prepare(tl).map_err(me)?;
    let mut data = if dl == 0 {
        None
    } else {
        Some(jit_memory::prepare_module_data(dl).map_err(me)?)
    };
    let mut rodata = if rl == 0 {
        None
    } else {
        Some(jit_memory::prepare_module_data(rl).map_err(me)?)
    };
    for (i, s) in ss.iter().enumerate() {
        let Some(p) = ps[i] else { continue };
        if s.t != PROG && s.t != NOBITS {
            return no();
        }
        if s.t == PROG {
            match p {
                P::T(o) => text.bytes_mut()[o..o + s.z].copy_from_slice(sl(b, s.o, s.z)?),
                P::D(o) => {
                    data.as_mut().unwrap().bytes_mut()[o..o + s.z].copy_from_slice(sl(b, s.o, s.z)?)
                }
                P::R(o) => rodata.as_mut().unwrap().bytes_mut()[o..o + s.z]
                    .copy_from_slice(sl(b, s.o, s.z)?),
            }
        }
    }
    let (mut init, mut exit) = (None, None);
    for j in 0..tab.z / SYM {
        let y = sy(b, tab, j)?;
        if y.s == UNDEF {
            continue;
        }
        let sec = *ss.get(y.s as usize).ok_or(AxError::InvalidExecutable)?;
        if y.v > sec.z || y.z > sec.z - y.v {
            return no();
        }
        if y.i & 15 == FUNC {
            match cs(sl(b, strtab.o, strtab.z)?, y.n as usize)? {
                b"thekernel_module_init" => init = Some(module_entry(y, sec, ps[y.s as usize])?),
                b"thekernel_module_exit" => exit = Some(module_entry(y, sec, ps[y.s as usize])?),
                _ => {}
            }
        }
    }
    let init = init.ok_or(AxError::InvalidExecutable)?;
    let mi = ss
        .iter()
        .find(|s| cs(names, s.n as usize).is_ok_and(|x| x == b".modinfo"))
        .ok_or(AxError::InvalidExecutable)?;
    validate_module_identity(sl(b, mi.o, mi.z)?, flags)?;
    let mn = sl(b, mi.o, mi.z)?
        .split(|x| *x == 0)
        .find_map(|x| x.strip_prefix(b"name="))
        .ok_or(AxError::InvalidExecutable)?;
    if mn.is_empty() || mn.len() > NAMEMAX {
        return no();
    }
    core::str::from_utf8(mn).map_err(|_| AxError::IllegalBytes)?;
    let name = String::from_utf8(copy_vec(mn)?).map_err(|_| AxError::IllegalBytes)?;
    let mut dependencies = modinfo_dependencies(sl(b, mi.o, mi.z)?)?;
    let tb = text.code_address();
    let db = data
        .as_ref()
        .map_or(0, jit_memory::WritableCode::code_address);
    let rb = rodata
        .as_ref()
        .map_or(0, jit_memory::WritableCode::code_address);
    let symbol_dependencies = {
        let d = data
            .as_mut()
            .map(jit_memory::WritableCode::bytes_mut)
            .unwrap_or(&mut []);
        let r = rodata
            .as_mut()
            .map(jit_memory::WritableCode::bytes_mut)
            .unwrap_or(&mut []);
        rel(
            b,
            &ss,
            &ps,
            names,
            sl(b, strtab.o, strtab.z)?,
            text.bytes_mut(),
            d,
            r,
            tb,
            db,
            rb,
        )?
    };
    // Retain symbol providers even if the producer omitted (or the object
    // forged) its `depends=` metadata.  The explicit metadata remains part
    // of the load contract too, so modules that declare an ordering-only
    // dependency keep that provider pinned as before.
    for binding in &symbol_dependencies {
        if !dependencies
            .iter()
            .any(|dependency| dependency == &binding.provider)
        {
            dependencies.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            dependencies.push(copy_name(&binding.provider)?);
        }
    }
    let charps = {
        let ro = rodata
            .as_mut()
            .map(jit_memory::WritableCode::bytes_mut)
            .unwrap_or(&mut []);
        let d = data
            .as_mut()
            .map(jit_memory::WritableCode::bytes_mut)
            .unwrap_or(&mut []);
        params(ro, rb, d, db, &ss, &ps, param_section, av)?
    };
    let code = text.publish(init.offset).map_err(me)?;
    let rodata = rodata
        .map(jit_memory::WritableCode::publish_readonly)
        .transpose()
        .map_err(me)?;
    Ok(M {
        name,
        code,
        rodata,
        data,
        charps,
        init,
        exit,
        dependencies,
        provider_bindings: symbol_dependencies,
        exports: module_exports(b, tab, sl(b, strtab.o, strtab.z)?, &ss, &ps, tb, db, rb)?,
    })
}
fn activate(x: M) -> AxResult<isize> {
    let x = Box::try_new(x).map_err(|_| AxError::NoMemory)?;
    let n = copy_name(&x.name)?;
    let slot_name = copy_name(&n)?;
    {
        let mut v = MODULES.lock();
        if v.iter().any(|x| x.name == n) {
            return Err(LinuxError::EEXIST.into());
        }
        v.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        // Resolve and retain all providers while COMING is unpublished.  A
        // provider cannot be removed between this check and module init.
        // Validate the complete dependency closure before changing a single
        // provider reference.  Otherwise a later missing/COMING dependency
        // would leak the references taken for earlier entries in `depends=`.
        for dependency in &x.dependencies {
            let provider = v
                .iter()
                .find(|slot| slot.name == *dependency)
                .ok_or(LinuxError::ENOENT)?;
            if !matches!(provider.state, State::Live(_)) {
                return Err(LinuxError::EBUSY.into());
            }
            if provider.deps == u32::MAX {
                return Err(AxError::NoMemory);
            }
        }
        // A dependency name alone is not enough: the address already written
        // into this image must still be owned by that same live export.  This
        // closes the interval between relocation and COMING publication in
        // which a provider could otherwise unload and another module with the
        // same name could be loaded at a different executable address.
        for binding in &x.provider_bindings {
            let provider = v
                .iter()
                .find(|slot| slot.name == binding.provider)
                .ok_or(LinuxError::ENOENT)?;
            let State::Live(module) = &provider.state else {
                return Err(LinuxError::EBUSY.into());
            };
            if !module
                .exports
                .iter()
                .any(|export| export.name == binding.symbol && export.address == binding.address)
            {
                return Err(LinuxError::ENOENT.into());
            }
        }
        for dependency in &x.dependencies {
            let provider = v
                .iter_mut()
                .find(|slot| slot.name == *dependency)
                .expect("dependency checked before retention");
            provider.deps = provider.deps.checked_add(1).ok_or(AxError::NoMemory)?;
        }
        v.push(Slot {
            name: slot_name,
            state: State::Coming,
            refs: 1,
            deps: 0,
        })
    }
    let r = x
        .code
        .execute_module_entry(x.init.offset, x.init.size)
        .ok_or(AxError::InvalidExecutable)?;
    let mut v = MODULES.lock();
    let i = v
        .iter()
        .position(|x| x.name == n)
        .ok_or(AxError::BadState)?;
    if let Err(error) = module_init_result(r) {
        v.remove(i);
        for dependency in &x.dependencies {
            if let Some(provider) = v.iter_mut().find(|slot| slot.name == *dependency) {
                provider.deps = provider.deps.saturating_sub(1);
            }
        }
        return Err(error);
    }
    v[i].state = State::Live(x);
    Ok(0)
}
fn module_init_result(result: i32) -> AxResult<()> {
    if result < 0 {
        Err(result
            .checked_neg()
            .and_then(|code| LinuxError::try_from(code).ok())
            .unwrap_or(LinuxError::EINVAL)
            .into())
    } else {
        // Linux do_init_module() warns but makes the module live for a
        // positive init return; only negative values fail the load.
        Ok(())
    }
}
fn ua<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    p: *const c_char,
) -> AxResult<(Vec<u8>, Vec<ParamArg>)> {
    let raw = if p.is_null() {
        None
    } else {
        Some(vm_load_until_nul_bounded(m, p.cast(), ARGMAX).map_err(map_usercopy_error)?)
    };
    parse_uargs(raw)
}

fn parse_uargs(raw: Option<Vec<u8>>) -> AxResult<(Vec<u8>, Vec<ParamArg>)> {
    // load_module() uses strndup_user(), so NULL is a user-copy fault rather
    // than an empty parameter string. An empty string must be readable NUL.
    let raw = raw.ok_or(LinuxError::EFAULT)?;
    let parsed = args(&raw)?;
    Ok((raw, parsed))
}
pub fn sys_init_module<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    p: *const u8,
    n: usize,
    a: *const c_char,
) -> AxResult<isize> {
    if !cap() {
        return Err(AxError::OperationNotPermitted);
    }
    if n == 0 || n > MAX {
        return Err(AxError::InvalidInput);
    }
    let (_, a) = ua(m, a)?;
    activate(prep(&vm_load(m, p, n).map_err(map_usercopy_error)?, &a, 0)?)
}
pub fn sys_finit_module<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    fd: i32,
    a: *const c_char,
    fl: u32,
) -> AxResult<isize> {
    if !cap() {
        return Err(AxError::OperationNotPermitted);
    }
    if fl
        & !(MODULE_INIT_IGNORE_MODVERSIONS
            | MODULE_INIT_IGNORE_VERMAGIC
            | MODULE_INIT_COMPRESSED_FILE)
        != 0
    {
        return Err(AxError::InvalidInput);
    }
    let (raw_args, a) = ua(m, a)?;
    let f = get_typed_file::<File>(fd)?;
    f.check_io_access()?;
    if f.status_flags() & O_ACCMODE == O_WRONLY {
        return Err(AxError::BadFileDescriptor);
    }
    let (flight, owner) = load_flight(LoadKey::new(f.open_file_description_key(), raw_args, fl))?;
    if !owner {
        return await_flight(flight);
    }
    let n = match f
        .stat()
        .and_then(|stat| usize::try_from(stat.size).map_err(|_| AxError::InvalidInput))
    {
        Ok(n) => n,
        Err(error) => return complete_flight(&flight, Err(error)),
    };
    if n == 0 || n > MAX {
        return complete_flight(&flight, Err(AxError::InvalidInput));
    }
    let mut b = Vec::new();
    if b.try_reserve_exact(n).is_err() {
        return complete_flight(&flight, Err(AxError::NoMemory));
    }
    // Capacity was fallibly reserved above; extending cannot allocate.
    b.extend(core::iter::repeat_n(0, n));
    let copied = match f.inner().read_at(&mut b, 0) {
        Ok(copied) => copied,
        Err(error) => return complete_flight(&flight, Err(error)),
    };
    if copied != n {
        return complete_flight(&flight, no());
    }
    complete_flight(
        &flight,
        decode_module_image(&b, fl)
            .and_then(|image| prep(&image, &a, fl))
            .and_then(activate),
    )
}
pub fn sys_delete_module<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    p: *const c_char,
    fl: u32,
) -> AxResult<isize> {
    if !cap() {
        return Err(AxError::OperationNotPermitted);
    }
    if fl & !(O_NONBLOCK | O_TRUNC) != 0 {
        return Err(LinuxError::EINVAL.into());
    }
    let raw = vm_load_until_nul_bounded(m, p.cast(), NAMEMAX + 1).map_err(map_usercopy_error)?;
    let n = core::str::from_utf8(&raw).map_err(|_| AxError::IllegalBytes)?;
    let force = fl & O_TRUNC != 0;
    let x = {
        let mut v = MODULES.lock();
        let i = v
            .iter()
            .position(|x| x.name == n)
            .ok_or(LinuxError::ENOENT)?;
        let live = match &v[i].state {
            State::Live(x) => x,
            _ => {
                return Err(if fl & O_NONBLOCK != 0 {
                    LinuxError::EAGAIN
                } else {
                    LinuxError::EBUSY
                }
                .into());
            }
        };
        if v[i].refs != 1 || v[i].deps != 0 {
            return Err(if fl & O_NONBLOCK != 0 {
                LinuxError::EAGAIN
            } else if force {
                LinuxError::EOPNOTSUPP
            } else {
                LinuxError::EBUSY
            }
            .into());
        }
        if live.exit.is_none() && !force {
            return Err(LinuxError::EBUSY.into());
        }
        match core::mem::replace(&mut v[i].state, State::Going) {
            State::Live(x) => x,
            _ => unreachable!(),
        }
    };
    if let Some(e) = x.exit {
        let _ = x.code.execute_module_entry(e.offset, e.size);
    };
    // Retire the executable segment before releasing the RO/RW segment
    // owners.  Even a failed retirement consumes its owner exactly once and
    // leaves any uncertain mapping retained/quarantined.
    let retired = x.code.retire().map_err(me);
    let mut v = MODULES.lock();
    if let Some(i) = v
        .iter()
        .position(|x| x.name == n && matches!(x.state, State::Going))
    {
        v.remove(i);
    };
    for dependency in &x.dependencies {
        if let Some(provider) = v.iter_mut().find(|slot| slot.name == *dependency) {
            provider.deps = provider.deps.saturating_sub(1);
        }
    }
    retired?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn assigned(name: &[u8], value: &[u8]) -> ParamArg {
        ParamArg {
            name: name.to_vec(),
            value: value.to_vec(),
            bare: false,
        }
    }

    fn param_image() -> Vec<u8> {
        let mut data = vec![0; 96];
        data[0..4].copy_from_slice(&1u32.to_le_bytes());
        data[4..8].copy_from_slice(&40u32.to_le_bytes());
        data[8..12].copy_from_slice(&1u32.to_le_bytes());
        let base = data.as_ptr() as usize;
        data[16..24].copy_from_slice(&((base + 64) as u64).to_le_bytes());
        data[24..32].copy_from_slice(&((base + 72) as u64).to_le_bytes());
        data[32..40].copy_from_slice(&((base + 80) as u64).to_le_bytes());
        data[40..42].copy_from_slice(&1u16.to_le_bytes());
        data[44..48].copy_from_slice(&1u32.to_le_bytes());
        data[64..68].copy_from_slice(b"foo\0");
        data
    }

    #[test]
    fn parameters_write_data_and_rollback_as_one_transaction() {
        let mut data = param_image();
        let section = S {
            n: 0,
            t: PROG,
            f: ALLOC,
            o: 0,
            z: data.len(),
            l: 0,
            i: 0,
            a: 1,
            e: 0,
        };
        let args = vec![assigned(b"foo", b"0x2a")];
        let base = data.as_ptr() as usize;
        params(
            &[],
            0,
            &mut data,
            base,
            &[section],
            &[Some(P::D(0))],
            Some(0),
            &args,
        )
        .unwrap();
        assert_eq!(&data[72..76], &42u32.to_le_bytes());
        let before = data.clone();
        let bad = vec![assigned(b"foo", b"7"), assigned(b"unknown", b"1")];
        let base = data.as_ptr() as usize;
        assert!(
            params(
                &[],
                0,
                &mut data,
                base,
                &[section],
                &[Some(P::D(0))],
                Some(0),
                &bad
            )
            .is_err()
        );
        assert_eq!(data, before);
    }

    #[test]
    fn module_arguments_handle_quotes_dash_and_underscore() {
        let got = args(b"--foo-bar='quoted value' answer=\"two words\" -- ignored=1").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, b"foo-bar");
        assert_eq!(got[0].value, b"quoted value");
        assert!(!got[0].bare);
        assert_eq!(got[1].name, b"answer");
        assert_eq!(got[1].value, b"two words");
        assert!(args(b" \t").unwrap().is_empty());
        assert!(args(b"foo='unterminated").is_err());
        assert!(eq(b"foo-bar", b"foo_bar"));
        assert_eq!(num(b"077", false, 32).unwrap(), 63);
    }

    #[test]
    fn bare_arguments_only_enable_boolean_parameters() {
        let section = |size| S {
            n: 0,
            t: PROG,
            f: ALLOC,
            o: 0,
            z: size,
            l: 0,
            i: 0,
            a: 1,
            e: 0,
        };
        let mut boolean = param_image();
        boolean[40..42].copy_from_slice(&0u16.to_le_bytes());
        let base = boolean.as_ptr() as usize;
        let boolean_size = boolean.len();
        params(
            &[],
            0,
            &mut boolean,
            base,
            &[section(boolean_size)],
            &[Some(P::D(0))],
            Some(0),
            &args(b"--foo").unwrap(),
        )
        .unwrap();
        assert_eq!(boolean[72], 1);

        let mut integer = param_image();
        let base = integer.as_ptr() as usize;
        let integer_size = integer.len();
        assert_eq!(
            LinuxError::from(
                params(
                    &[],
                    0,
                    &mut integer,
                    base,
                    &[section(integer_size)],
                    &[Some(P::D(0))],
                    Some(0),
                    &args(b"--foo").unwrap(),
                )
                .unwrap_err(),
            ),
            LinuxError::EINVAL
        );
    }

    #[test]
    fn parameter_names_normalize_dashes_and_last_value_wins() {
        let mut data = param_image();
        data[64..72].copy_from_slice(b"foo_bar\0");
        let section = S {
            n: 0,
            t: PROG,
            f: ALLOC,
            o: 0,
            z: data.len(),
            l: 0,
            i: 0,
            a: 1,
            e: 0,
        };
        let base = data.as_ptr() as usize;
        params(
            &[],
            0,
            &mut data,
            base,
            &[section],
            &[Some(P::D(0))],
            Some(0),
            &args(b"foo-bar=7 foo_bar=42").unwrap(),
        )
        .unwrap();
        assert_eq!(&data[72..76], &42u32.to_le_bytes());
    }

    #[test]
    fn module_uargs_require_a_nonnull_nul_terminated_pointer() {
        let null_error = match parse_uargs(None) {
            Err(error) => LinuxError::from(error),
            Ok(_) => panic!("NULL module arguments unexpectedly accepted"),
        };
        assert_eq!(null_error, LinuxError::EFAULT);
        let (raw, parsed) = parse_uargs(Some(Vec::new())).unwrap();
        assert!(raw.is_empty());
        assert!(parsed.is_empty());
    }

    #[test]
    fn load_flights_require_matching_ofd_args_and_flags() {
        let key = LoadKey::new(7, b"answer=42".to_vec(), 0);
        assert!(key.matches(&LoadKey::new(7, b"answer=42".to_vec(), 0)));
        assert!(!key.matches(&LoadKey::new(8, b"answer=42".to_vec(), 0)));
        assert!(!key.matches(&LoadKey::new(7, b"answer=43".to_vec(), 0)));
        assert!(!key.matches(&LoadKey::new(7, b"answer=42".to_vec(), 1)));

        let colliding_hash = LoadKey {
            ofd: 7,
            uargs_hash: key.uargs_hash,
            uargs: b"different".to_vec(),
            flags: 0,
        };
        assert!(!key.matches(&colliding_hash));
    }

    #[test]
    fn module_init_return_matches_linux_zero_negative_and_positive_rules() {
        assert!(module_init_result(0).is_ok());
        assert!(module_init_result(1).is_ok());
        assert_eq!(
            LinuxError::from(module_init_result(-LinuxError::ENOMEM.code()).unwrap_err()),
            LinuxError::ENOMEM
        );
    }

    #[test]
    fn relocations_use_final_addresses_across_text_rodata_and_data() {
        let mut image = vec![0; 72];
        // Rela[0]: text[0] = rodata symbol (R_X86_64_64).
        image[8..16].copy_from_slice(&((1u64 << 32) | 1).to_le_bytes());
        image[24 + 24 + 6..24 + 24 + 8].copy_from_slice(&1u16.to_le_bytes());
        image[24 + 24 + 8..24 + 24 + 16].copy_from_slice(&8u64.to_le_bytes());
        let sections = [
            S {
                n: 0,
                t: PROG,
                f: ALLOC | EXEC,
                o: 0,
                z: 8,
                l: 0,
                i: 0,
                a: 1,
                e: 0,
            },
            S {
                n: 0,
                t: PROG,
                f: ALLOC,
                o: 0,
                z: 8,
                l: 0,
                i: 0,
                a: 1,
                e: 0,
            },
            S {
                n: 0,
                t: RELOC,
                f: 0,
                o: 0,
                z: RELA,
                l: 3,
                i: 0,
                a: 8,
                e: RELA,
            },
            S {
                n: 0,
                t: SYMTAB,
                f: 0,
                o: 24,
                z: 48,
                l: 0,
                i: 0,
                a: 8,
                e: SYM,
            },
        ];
        let places = [Some(P::T(0)), Some(P::R(0)), None, None];
        let (mut text, mut data, mut rodata) = (vec![0; 8], vec![0; 8], vec![0; 8]);
        rel(
            &image,
            &sections,
            &places,
            b"",
            b"",
            &mut text,
            &mut data,
            &mut rodata,
            0x1000,
            0x2000,
            0x3000,
        )
        .unwrap();
        assert_eq!(u64::from_le_bytes(text[..8].try_into().unwrap()), 0x3008);

        // The same symbol resolved PC-relatively from RW data must use the
        // final bases, rather than a temporary combined buffer address.
        image[8..16].copy_from_slice(&((1u64 << 32) | 2).to_le_bytes());
        let places = [Some(P::D(0)), Some(P::R(0)), None, None];
        rel(
            &image,
            &sections,
            &places,
            b"",
            b"",
            &mut text,
            &mut data,
            &mut rodata,
            0x1000,
            0x2000,
            0x3000,
        )
        .unwrap();
        assert_eq!(i32::from_le_bytes(data[..4].try_into().unwrap()), 0x1008);
    }

    #[test]
    fn relocations_add_nonzero_defined_symbol_offsets_for_all_forms() {
        let mut image = vec![0; RELA * 5 + SYM * 2];
        for (j, kind) in [1u32, 2, 4, 10, 11].into_iter().enumerate() {
            let q = j * RELA;
            image[q..q + 8].copy_from_slice(&((j * 8) as u64).to_le_bytes());
            image[q + 8..q + 16].copy_from_slice(&((1u64 << 32) | kind as u64).to_le_bytes());
            image[q + 16..q + 24].copy_from_slice(&7i64.to_le_bytes());
        }
        let sym = RELA * 5 + SYM;
        image[sym + 6..sym + 8].copy_from_slice(&1u16.to_le_bytes());
        image[sym + 8..sym + 16].copy_from_slice(&3u64.to_le_bytes());
        image[sym + 16..sym + 24].copy_from_slice(&1u64.to_le_bytes());
        let sections = [
            S {
                n: 0,
                t: PROG,
                f: ALLOC | EXEC,
                o: 0,
                z: 40,
                l: 0,
                i: 0,
                a: 1,
                e: 0,
            },
            S {
                n: 0,
                t: PROG,
                f: ALLOC,
                o: 0,
                z: 8,
                l: 0,
                i: 0,
                a: 1,
                e: 0,
            },
            S {
                n: 0,
                t: RELOC,
                f: 0,
                o: 0,
                z: RELA * 5,
                l: 3,
                i: 0,
                a: 8,
                e: RELA,
            },
            S {
                n: 0,
                t: SYMTAB,
                f: 0,
                o: RELA * 5,
                z: SYM * 2,
                l: 0,
                i: 0,
                a: 8,
                e: SYM,
            },
        ];
        let mut text = vec![0; 40];
        rel(
            &image,
            &sections,
            &[Some(P::T(0)), Some(P::R(0)), None, None],
            b"",
            b"",
            &mut text,
            &mut [],
            &mut [],
            0x1000,
            0,
            0x3000,
        )
        .unwrap();
        assert_eq!(u64::from_le_bytes(text[..8].try_into().unwrap()), 0x300a);
        assert_eq!(i32::from_le_bytes(text[8..12].try_into().unwrap()), 0x2002);
        assert_eq!(i32::from_le_bytes(text[16..20].try_into().unwrap()), 0x1ffa);
        assert_eq!(u32::from_le_bytes(text[24..28].try_into().unwrap()), 0x300a);
        assert_eq!(i32::from_le_bytes(text[32..36].try_into().unwrap()), 0x300a);
    }

    #[test]
    fn module_entry_rejects_empty_and_exclusive_end_symbols() {
        let section = S {
            n: 0,
            t: PROG,
            f: ALLOC | EXEC,
            o: 0,
            z: 8,
            l: 0,
            i: 0,
            a: 1,
            e: 0,
        };
        let base = Y {
            n: 0,
            i: FUNC,
            s: 0,
            v: 7,
            z: 1,
        };
        let entry = module_entry(base, section, Some(P::T(16))).unwrap();
        assert_eq!(entry.offset, 23);
        assert!(module_entry(Y { z: 0, ..base }, section, Some(P::T(16))).is_err());
        assert!(module_entry(Y { v: 8, z: 0, ..base }, section, Some(P::T(16))).is_err());
        assert!(module_entry(Y { v: 7, z: 2, ..base }, section, Some(P::T(16))).is_err());
    }

    #[test]
    fn parameter_targets_must_be_in_final_rw_segment() {
        let mut ro = param_image();
        let mut rw = vec![0; 8];
        let base = ro.as_ptr() as usize;
        // `arg` points into rodata, which must be rejected even though the
        // parameter record and name themselves may live there.
        ro[24..32].copy_from_slice(&((base + 72) as u64).to_le_bytes());
        let section = S {
            n: 0,
            t: PROG,
            f: ALLOC,
            o: 0,
            z: ro.len(),
            l: 0,
            i: 0,
            a: 1,
            e: 0,
        };
        let rw_base = rw.as_ptr() as usize;
        assert!(
            params(
                &ro,
                base,
                &mut rw,
                rw_base,
                &[section],
                &[Some(P::R(0))],
                Some(0),
                &[assigned(b"foo", b"1")]
            )
            .is_err()
        );
    }
}
