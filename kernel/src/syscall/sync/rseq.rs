//! x86-64 `rseq(2)` registration lifecycle.

use alloc::sync::Arc;

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use memory_addr::VirtAddr;
use thekernel_linux_rseq::{
    ErrnoClass, RSEQ_CPU_ID_UNINITIALIZED, RseqError, RseqRegistrationOperation,
    RseqRegistrationRequest,
};
use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext};

use crate::{
    mm::{AddrSpace, AddressSpaceUserMemory},
    task::{AsThread, Thread},
};

const CPU_ID_START_OFFSET: usize = 0;
const CPU_ID_OFFSET: usize = 4;
const RSEQ_CS_OFFSET: usize = 8;
const FLAGS_OFFSET: usize = 16;
const NODE_ID_OFFSET: usize = 20;
const MM_CID_OFFSET: usize = 24;

fn map_rseq_error(error: RseqError) -> AxError {
    match error.errno() {
        ErrnoClass::InvalidArgument => AxError::InvalidInput,
        ErrnoClass::PermissionDenied => LinuxError::EPERM.into(),
        ErrnoClass::Busy => LinuxError::EBUSY.into(),
        ErrnoClass::Fault => LinuxError::EFAULT.into(),
        ErrnoClass::Stale => LinuxError::EAGAIN.into(),
        ErrnoClass::Overflow => LinuxError::EOVERFLOW.into(),
    }
}

fn map_rseq_usercopy_error(_error: UserCopyError) -> AxError {
    // Linux's rseq syscall reports a failed user write as EFAULT. In
    // particular, do not leak the provider's allocation/access distinction
    // through this ABI boundary.
    LinuxError::EFAULT.into()
}

fn decode_request(
    area_address: usize,
    area_length: u32,
    flags: u32,
    signature: u32,
) -> AxResult<(RseqRegistrationOperation, RseqRegistrationRequest)> {
    let operation = RseqRegistrationOperation::from_flags(flags).map_err(map_rseq_error)?;
    let request = RseqRegistrationRequest::new(area_address as u64, area_length, signature);
    request.validate().map_err(map_rseq_error)?;
    Ok((operation, request))
}

/// Checks one rseq registration range against the selected address space.
///
/// This is the `access_ok` range check for registration. The subsequent
/// initialization writes are still required: an address inside the VA range
/// but outside a mapped, writable VMA must fail with `EFAULT` before the
/// thread-local registration is published.
fn access_ok_range(aspace: &AddrSpace, area_address: usize, area_length: u32) -> AxResult<()> {
    let area_length = usize::try_from(area_length).map_err(|_| LinuxError::EFAULT)?;
    let start = VirtAddr::from(area_address);
    if aspace.contains_range(start, area_length) {
        Ok(())
    } else {
        Err(LinuxError::EFAULT.into())
    }
}

fn access_ok(
    aspace: &Arc<Mutex<AddrSpace>>,
    area_address: usize,
    area_length: u32,
) -> AxResult<()> {
    let aspace = aspace.lock();
    access_ok_range(&aspace, area_address, area_length)
}

fn write_u32<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    area_address: usize,
    offset: usize,
    value: u32,
) -> Result<(), UserCopyError> {
    let address = area_address
        .checked_add(offset)
        .ok_or(UserCopyError::BadAddress)?;
    memory.write_bytes(address, &value.to_ne_bytes())
}

fn write_u64<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    area_address: usize,
    offset: usize,
    value: u64,
) -> Result<(), UserCopyError> {
    let address = area_address
        .checked_add(offset)
        .ok_or(UserCopyError::BadAddress)?;
    memory.write_bytes(address, &value.to_ne_bytes())
}

fn field_present(area_length: u32, offset: usize, size: usize) -> bool {
    usize::try_from(area_length)
        .ok()
        .and_then(|length| offset.checked_add(size).map(|end| end <= length))
        .unwrap_or(false)
}

/// Initializes the kernel-owned fields before publishing a registration.
///
/// The provider performs the actual writable user-memory check for every
/// field. Keep the writes in Linux's order, starting with `rseq_cs`, so a
/// reused area cannot retain a stale critical-section pointer if registration
/// succeeds. Fields outside the requested registration length are not touched
/// (the current ABI requires the 32-byte base area, but this keeps extension
/// length handling explicit).
fn initialize_rseq_area<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    area_address: usize,
    area_length: u32,
) -> Result<(), UserCopyError> {
    if field_present(area_length, RSEQ_CS_OFFSET, core::mem::size_of::<u64>()) {
        write_u64(memory, area_address, RSEQ_CS_OFFSET, 0)?;
    }
    if field_present(area_length, FLAGS_OFFSET, core::mem::size_of::<u32>()) {
        write_u32(memory, area_address, FLAGS_OFFSET, 0)?;
    }
    if field_present(
        area_length,
        CPU_ID_START_OFFSET,
        core::mem::size_of::<u32>(),
    ) {
        write_u32(
            memory,
            area_address,
            CPU_ID_START_OFFSET,
            RSEQ_CPU_ID_UNINITIALIZED,
        )?;
    }
    if field_present(area_length, CPU_ID_OFFSET, core::mem::size_of::<u32>()) {
        write_u32(
            memory,
            area_address,
            CPU_ID_OFFSET,
            RSEQ_CPU_ID_UNINITIALIZED,
        )?;
    }
    if field_present(area_length, NODE_ID_OFFSET, core::mem::size_of::<u32>()) {
        write_u32(memory, area_address, NODE_ID_OFFSET, 0)?;
    }
    if field_present(area_length, MM_CID_OFFSET, core::mem::size_of::<u32>()) {
        write_u32(memory, area_address, MM_CID_OFFSET, 0)?;
    }
    Ok(())
}

/// Clears the Linux-visible unregister fields in their required order.
fn clear_rseq_area<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    area_address: usize,
) -> Result<(), UserCopyError> {
    write_u32(memory, area_address, CPU_ID_START_OFFSET, 0)?;
    write_u32(
        memory,
        area_address,
        CPU_ID_OFFSET,
        RSEQ_CPU_ID_UNINITIALIZED,
    )?;
    write_u32(memory, area_address, NODE_ID_OFFSET, 0)?;
    write_u32(memory, area_address, MM_CID_OFFSET, 0)
}

fn register(
    thr: &Thread,
    aspace: &Arc<Mutex<AddrSpace>>,
    request: RseqRegistrationRequest,
) -> AxResult<isize> {
    let plan = thr
        .with_rseq_state(|state| state.prepare_register(request))
        .map_err(map_rseq_error)?;

    match access_ok(
        aspace,
        request.area_address() as usize,
        request.area_length(),
    ) {
        Ok(()) => {
            let mut provider = AddressSpaceUserMemory::new(aspace.clone());
            let mut memory = UserMemoryContext::new(&mut provider);
            match initialize_rseq_area(
                &mut memory,
                request.area_address() as usize,
                request.area_length(),
            ) {
                Ok(()) => {
                    // Do not expose the registration to scheduler/return
                    // paths until every initial user write has succeeded.
                    thr.with_rseq_state(|state| state.commit_register(plan));
                    Ok(0)
                }
                Err(error) => {
                    thr.with_rseq_state(|state| state.cancel_register(plan));
                    Err(map_rseq_usercopy_error(error))
                }
            }
        }
        Err(error) => {
            thr.with_rseq_state(|state| state.cancel_register(plan));
            Err(error)
        }
    }
}

fn unregister(
    thr: &Thread,
    aspace: Arc<Mutex<AddrSpace>>,
    request: RseqRegistrationRequest,
) -> AxResult<isize> {
    let plan = thr
        .with_rseq_state(|state| state.prepare_unregister(request))
        .map_err(map_rseq_error)?;

    let mut provider = AddressSpaceUserMemory::new(aspace);
    let mut memory = UserMemoryContext::new(&mut provider);
    match clear_rseq_area(&mut memory, request.area_address() as usize) {
        Ok(()) => {
            thr.with_rseq_state(|state| state.commit_unregister(plan));
            Ok(0)
        }
        Err(error) => {
            thr.with_rseq_state(|state| state.cancel_unregister(plan));
            Err(map_rseq_usercopy_error(error))
        }
    }
}

/// Implements the x86-64 Linux v6.6 `rseq(void *, u32, int, u32)` ABI.
///
/// This phase owns registration/unregistration and thread-local lifecycle
/// state. It deliberately does not claim Linux's scheduler/signal final-return
/// restart gate.
pub fn sys_rseq(
    aspace: Arc<Mutex<AddrSpace>>,
    area_address: usize,
    area_length: u32,
    flags: u32,
    signature: u32,
) -> AxResult<isize> {
    let (operation, request) = decode_request(area_address, area_length, flags, signature)?;
    let curr = current();
    let thr = curr.as_thread();
    match operation {
        RseqRegistrationOperation::Register => register(thr, &aspace, request),
        RseqRegistrationOperation::Unregister => unregister(thr, aspace, request),
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::mem::MaybeUninit;

    use thekernel_linux_rseq::{RSEQ_AREA_SIZE, RseqRegistrationState};
    use thekernel_linux_usercopy::VmResult;

    use super::*;

    struct TestMemory {
        bytes: alloc::vec::Vec<u8>,
        fail_at: Option<usize>,
    }

    // SAFETY: the fixture bounds every read/write and initializes every read
    // destination byte before returning success.
    unsafe impl UserMemory for TestMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            let end = start
                .checked_add(dst.len())
                .ok_or(UserCopyError::BadAddress)?;
            if end > self.bytes.len() {
                return Err(UserCopyError::BadAddress);
            }
            for (dst, src) in dst.iter_mut().zip(&self.bytes[start..end]) {
                dst.write(*src);
            }
            Ok(())
        }

        fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
            let end = start
                .checked_add(src.len())
                .ok_or(UserCopyError::BadAddress)?;
            if self.fail_at == Some(start) || end > self.bytes.len() {
                return Err(UserCopyError::BadAddress);
            }
            self.bytes[start..end].copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn decode_preserves_null_for_access_ok_fault() {
        let (_, request) = decode_request(0, RSEQ_AREA_SIZE as u32, 0, 7).unwrap();
        assert_eq!(request.area_address(), 0);
    }

    #[test]
    fn access_ok_range_only_checks_va_bounds_before_user_write() {
        let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
        assert!(access_ok_range(&aspace, 0x4000, RSEQ_AREA_SIZE as u32).is_ok());
    }

    #[test]
    fn access_ok_rejects_null_registration_range() {
        let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
        assert_eq!(
            access_ok_range(&aspace, 0, RSEQ_AREA_SIZE as u32),
            Err(LinuxError::EFAULT.into())
        );
    }

    #[test]
    fn registration_initializes_linux_owned_fields() {
        let mut provider = TestMemory {
            bytes: vec![0xaa; RSEQ_AREA_SIZE],
            fail_at: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        initialize_rseq_area(&mut memory, 0, RSEQ_AREA_SIZE as u32).unwrap();
        assert_eq!(
            &provider.bytes[0..4],
            &RSEQ_CPU_ID_UNINITIALIZED.to_ne_bytes()
        );
        assert_eq!(
            &provider.bytes[4..8],
            &RSEQ_CPU_ID_UNINITIALIZED.to_ne_bytes()
        );
        assert_eq!(&provider.bytes[8..16], &0u64.to_ne_bytes());
        assert_eq!(&provider.bytes[16..20], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[20..24], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[24..28], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[28..32], &[0xaa; 4]);
    }

    #[test]
    fn registration_initialization_respects_field_length() {
        let mut provider = TestMemory {
            bytes: vec![0xaa; RSEQ_AREA_SIZE],
            fail_at: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        initialize_rseq_area(&mut memory, 0, NODE_ID_OFFSET as u32 + 4).unwrap();
        assert_eq!(&provider.bytes[8..16], &0u64.to_ne_bytes());
        assert_eq!(
            &provider.bytes[0..4],
            &RSEQ_CPU_ID_UNINITIALIZED.to_ne_bytes()
        );
        assert_eq!(
            &provider.bytes[4..8],
            &RSEQ_CPU_ID_UNINITIALIZED.to_ne_bytes()
        );
        assert_eq!(&provider.bytes[16..20], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[20..24], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[24..28], &[0xaa; 4]);
    }

    #[test]
    fn registration_initialization_rejects_unmapped_range() {
        let aspace = alloc::sync::Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap(),
        ));
        let mut provider = AddressSpaceUserMemory::new(aspace);
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            initialize_rseq_area(&mut memory, 0x4000, RSEQ_AREA_SIZE as u32),
            Err(UserCopyError::AccessDenied)
        );
    }

    #[test]
    fn registration_initialization_stops_at_first_user_fault() {
        let mut provider = TestMemory {
            bytes: vec![0xaa; RSEQ_AREA_SIZE],
            fail_at: Some(RSEQ_CS_OFFSET),
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            initialize_rseq_area(&mut memory, 0, RSEQ_AREA_SIZE as u32),
            Err(UserCopyError::BadAddress)
        );
        assert_eq!(&provider.bytes, &[0xaa; RSEQ_AREA_SIZE]);
    }

    #[test]
    fn failed_registration_side_effect_cancels_pending_state() {
        let request = RseqRegistrationRequest::new(0x1000, RSEQ_AREA_SIZE as u32, 7);
        let mut state = RseqRegistrationState::new();
        let plan = state.prepare_register(request).unwrap();
        let mut provider = TestMemory {
            bytes: vec![0xaa; RSEQ_AREA_SIZE],
            fail_at: Some(RSEQ_CS_OFFSET),
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert!(initialize_rseq_area(&mut memory, 0, request.area_length()).is_err());
        state.cancel_register(plan);
        assert!(!state.is_registered());
        assert!(!state.has_pending_operation());
    }

    #[test]
    fn unregister_writes_fields_in_linux_order() {
        let mut provider = TestMemory {
            bytes: vec![0xaa; RSEQ_AREA_SIZE],
            fail_at: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        clear_rseq_area(&mut memory, 0).unwrap();
        assert_eq!(&provider.bytes[0..4], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[4..8], &u32::MAX.to_ne_bytes());
        assert_eq!(&provider.bytes[20..24], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[24..28], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[8..20], &[0xaa; 12]);
        assert_eq!(&provider.bytes[28..32], &[0xaa; 4]);
    }

    #[test]
    fn unregister_stops_at_first_fault() {
        let mut provider = TestMemory {
            bytes: vec![0xaa; RSEQ_AREA_SIZE],
            fail_at: Some(4),
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            clear_rseq_area(&mut memory, 0),
            Err(UserCopyError::BadAddress)
        );
        assert_eq!(&provider.bytes[0..4], &0u32.to_ne_bytes());
        assert_eq!(&provider.bytes[4..8], &[0xaa; 4]);
    }

    #[test]
    fn rseq_errno_classes_match_linux_registration_contract() {
        assert_eq!(
            map_rseq_error(RseqError::AlreadyRegistered),
            LinuxError::EBUSY.into()
        );
        assert_eq!(
            map_rseq_error(RseqError::SignatureMismatch),
            LinuxError::EPERM.into()
        );
        assert_eq!(
            map_rseq_usercopy_error(UserCopyError::BadAddress),
            LinuxError::EFAULT.into()
        );
    }
}
