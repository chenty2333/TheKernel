//! x86-64 `rseq(2)` registration lifecycle.

use alloc::sync::Arc;

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use memory_addr::VirtAddr;
use thekernel_linux_rseq::{
    ErrnoClass, RseqError, RseqRegistrationOperation, RseqRegistrationRequest,
};
use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext};

use crate::{
    mm::{AddrSpace, AddressSpaceUserMemory},
    task::{AsThread, Thread},
};

const CPU_ID_START_OFFSET: usize = 0;
const CPU_ID_OFFSET: usize = 4;
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
    // A null address intentionally survives this pure check. The explicit
    // address-space access check below owns its EFAULT classification.
    request.validate().map_err(map_rseq_error)?;
    Ok((operation, request))
}

/// Checks one rseq registration range against the selected address space.
///
/// This is an access check only: it examines the address-space VA bounds and
/// never requires an existing VMA, populates pages, or writes the user's rseq
/// area. Linux's registration path intentionally permits an as-yet-unmapped
/// address inside the user range; the first actual access is later gated by
/// the scheduler/return implementation.
fn access_ok_range(aspace: &AddrSpace, area_address: usize, area_length: u32) -> AxResult<()> {
    let area_length = usize::try_from(area_length).map_err(|_| LinuxError::EFAULT)?;
    // Linux x86_64's registration geometry permits a NULL area pointer to
    // reach the registration lifecycle. The first real CPU-field write is
    // performed by the return gate (or unregister usercopy) and reports the
    // resulting EFAULT there; registration itself must not pre-classify it as
    // an address-space-range failure.
    if area_address == 0 {
        return Ok(());
    }
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

/// Clears the Linux-visible unregister fields in their required order.
fn clear_rseq_area<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    area_address: usize,
) -> Result<(), UserCopyError> {
    write_u32(memory, area_address, CPU_ID_START_OFFSET, 0)?;
    write_u32(memory, area_address, CPU_ID_OFFSET, u32::MAX)?;
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
            // Registration intentionally does not initialize or overwrite the
            // user's area. The final CPU-ID publication belongs to the future
            // scheduler/return gate, not this phase.
            thr.with_rseq_state(|state| state.commit_register(plan));
            Ok(0)
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

    use thekernel_linux_rseq::RSEQ_AREA_SIZE;
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
    fn access_ok_allows_unmapped_address_inside_aspace_range() {
        let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
        assert!(access_ok_range(&aspace, 0x4000, RSEQ_AREA_SIZE as u32).is_ok());
    }

    #[test]
    fn access_ok_accepts_null_registration_range_for_later_user_fault() {
        let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
        assert!(access_ok_range(&aspace, 0, RSEQ_AREA_SIZE as u32).is_ok());
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
