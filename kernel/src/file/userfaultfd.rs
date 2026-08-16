//! Linux v6.12 userfaultfd open-file-description adapter.
//!
//! The initial bounded profile handles user-mode 4 KiB anonymous-private
//! MISSING faults. It owns API negotiation, registration, readiness/event
//! delivery, COPY/ZEROPAGE/WAKE resolution, and lock-external waiter wakeups;
//! optional WP, MINOR, lifecycle-event, shmem, and hugetlb features remain
//! unadvertised.

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
};
use core::{
    mem::{self, MaybeUninit},
    slice,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex as BlockingMutex;
use bytemuck::{Pod, Zeroable};
use linux_raw_sys::{
    general::{
        UFFD_EVENT_PAGEFAULT, UFFD_PAGEFAULT_FLAG_WRITE, uffdio_api, uffdio_copy, uffdio_range,
        uffdio_register, uffdio_zeropage,
    },
    ioctl::{
        UFFDIO_API as UFFDIO_API_CMD, UFFDIO_COPY as UFFDIO_COPY_CMD,
        UFFDIO_REGISTER as UFFDIO_REGISTER_CMD, UFFDIO_UNREGISTER as UFFDIO_UNREGISTER_CMD,
        UFFDIO_WAKE as UFFDIO_WAKE_CMD, UFFDIO_ZEROPAGE as UFFDIO_ZEROPAGE_CMD,
    },
};
use memory_addr::PAGE_SIZE_4K;
use thekernel_linux_mm::{
    FaultAccess, FaultDisposition, FaultHandlerId, MmError, PageRange, UffdApiNegotiation,
    UffdApiState, UffdCopyMode, UffdCopyRequest, UffdCreateFlags, UffdIoctls, UffdRegisterMode,
    UffdResolverOutcome, UffdResolverResult, UffdZeroPageMode, UffdZeroPageRequest, UserRange,
};

use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    file::{FileLike, IoDst, IoctlContext, Kstat, anon_inode_stat},
    mm::{
        AddrSpace, DeliveredUffdEvent, PreparedCowPage, UffdAddressSpaceState,
        UffdIcacheSynchronization, UffdPagePublication, UffdPollSet, UserMemoryCapability,
        map_usercopy_error, uffd_policy_error,
    },
    readiness::block_on_poll_io,
    task::{AsThread, has_pending_sigkill},
};

const UFFD_MSG_SIZE: usize = 32;
const UFFD_API_SIZE: usize = mem::size_of::<UffdApiRaw>();

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdApiRaw {
    api: u64,
    features: u64,
    ioctls: u64,
}

const _: [(); mem::size_of::<uffdio_api>()] = [(); mem::size_of::<UffdApiRaw>()];
const _: [(); mem::align_of::<uffdio_api>()] = [(); mem::align_of::<UffdApiRaw>()];
const _: [(); mem::offset_of!(uffdio_api, api)] = [(); mem::offset_of!(UffdApiRaw, api)];
const _: [(); mem::offset_of!(uffdio_api, features)] = [(); mem::offset_of!(UffdApiRaw, features)];
const _: [(); mem::offset_of!(uffdio_api, ioctls)] = [(); mem::offset_of!(UffdApiRaw, ioctls)];

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdRangeRaw {
    start: u64,
    len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdRegisterInputRaw {
    range: UffdRangeRaw,
    mode: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdRegisterRaw {
    range: UffdRangeRaw,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdCopyInputRaw {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdCopyRaw {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdZeroPageInputRaw {
    range: UffdRangeRaw,
    mode: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct UffdZeroPageRaw {
    range: UffdRangeRaw,
    mode: u64,
    zeropage: i64,
}

const UFFD_RANGE_SIZE: usize = mem::size_of::<UffdRangeRaw>();
const UFFD_REGISTER_INPUT_SIZE: usize = mem::size_of::<UffdRegisterInputRaw>();
const UFFD_REGISTER_IOCTLS_OFFSET: usize = mem::offset_of!(UffdRegisterRaw, ioctls);
const UFFD_COPY_INPUT_SIZE: usize = mem::size_of::<UffdCopyInputRaw>();
const UFFD_COPY_OUTPUT_OFFSET: usize = mem::offset_of!(UffdCopyRaw, copy);
const UFFD_ZEROPAGE_INPUT_SIZE: usize = mem::size_of::<UffdZeroPageInputRaw>();
const UFFD_ZEROPAGE_OUTPUT_OFFSET: usize = mem::offset_of!(UffdZeroPageRaw, zeropage);

const _: [(); mem::size_of::<uffdio_range>()] = [(); mem::size_of::<UffdRangeRaw>()];
const _: [(); mem::align_of::<uffdio_range>()] = [(); mem::align_of::<UffdRangeRaw>()];
const _: [(); mem::offset_of!(uffdio_range, start)] = [(); mem::offset_of!(UffdRangeRaw, start)];
const _: [(); mem::offset_of!(uffdio_range, len)] = [(); mem::offset_of!(UffdRangeRaw, len)];
const _: [(); mem::size_of::<uffdio_register>()] = [(); mem::size_of::<UffdRegisterRaw>()];
const _: [(); mem::align_of::<uffdio_register>()] = [(); mem::align_of::<UffdRegisterRaw>()];
const _: [(); mem::offset_of!(uffdio_register, range)] =
    [(); mem::offset_of!(UffdRegisterRaw, range)];
const _: [(); mem::offset_of!(uffdio_register, mode)] =
    [(); mem::offset_of!(UffdRegisterRaw, mode)];
const _: [(); mem::offset_of!(uffdio_register, ioctls)] =
    [(); mem::offset_of!(UffdRegisterRaw, ioctls)];
const _: [(); UFFD_REGISTER_INPUT_SIZE] = [(); UFFD_REGISTER_IOCTLS_OFFSET];
const _: [(); mem::size_of::<uffdio_copy>()] = [(); mem::size_of::<UffdCopyRaw>()];
const _: [(); mem::align_of::<uffdio_copy>()] = [(); mem::align_of::<UffdCopyRaw>()];
const _: [(); mem::offset_of!(uffdio_copy, dst)] = [(); mem::offset_of!(UffdCopyRaw, dst)];
const _: [(); mem::offset_of!(uffdio_copy, src)] = [(); mem::offset_of!(UffdCopyRaw, src)];
const _: [(); mem::offset_of!(uffdio_copy, len)] = [(); mem::offset_of!(UffdCopyRaw, len)];
const _: [(); mem::offset_of!(uffdio_copy, mode)] = [(); mem::offset_of!(UffdCopyRaw, mode)];
const _: [(); mem::offset_of!(uffdio_copy, copy)] = [(); mem::offset_of!(UffdCopyRaw, copy)];
const _: [(); UFFD_COPY_INPUT_SIZE] = [(); UFFD_COPY_OUTPUT_OFFSET];
const _: [(); mem::size_of::<uffdio_zeropage>()] = [(); mem::size_of::<UffdZeroPageRaw>()];
const _: [(); mem::align_of::<uffdio_zeropage>()] = [(); mem::align_of::<UffdZeroPageRaw>()];
const _: [(); mem::offset_of!(uffdio_zeropage, range)] =
    [(); mem::offset_of!(UffdZeroPageRaw, range)];
const _: [(); mem::offset_of!(uffdio_zeropage, mode)] =
    [(); mem::offset_of!(UffdZeroPageRaw, mode)];
const _: [(); mem::offset_of!(uffdio_zeropage, zeropage)] =
    [(); mem::offset_of!(UffdZeroPageRaw, zeropage)];
const _: [(); UFFD_ZEROPAGE_INPUT_SIZE] = [(); UFFD_ZEROPAGE_OUTPUT_OFFSET];

#[derive(Clone, Copy, Debug)]
struct UffdResolverProgress {
    completed: usize,
    lower_error: Option<AxError>,
}

#[derive(Clone, Copy, Debug)]
enum UffdResolverData {
    Copy(UserRange),
    Zero,
}

impl UffdResolverData {
    const fn disposition(self) -> FaultDisposition {
        match self {
            Self::Copy(_) => FaultDisposition::Supply,
            Self::Zero => FaultDisposition::ZeroFill,
        }
    }

    fn prepare(
        self,
        offset: usize,
        prepared: &mut PreparedCowPage,
        source_capability: Option<&UserMemoryCapability>,
    ) -> AxResult {
        match self {
            Self::Copy(source) => {
                let source = source
                    .start()
                    .checked_add(offset)
                    .ok_or(AxError::BadState)?;
                let source_capability = source_capability.ok_or(AxError::BadState)?;
                // SAFETY: read_bytes returns success only after writing the
                // complete PAGE_SIZE_4K destination slice.  This copy is
                // performed before the target address-space lock is taken.
                unsafe {
                    prepared.prepare_uninitialized(|destination| {
                        source_capability
                            .read_bytes(source, destination)
                            .map_err(map_usercopy_error)?;
                        Ok(())
                    })
                }
            }
            Self::Zero => prepared.prepare_zeroed(),
        }
    }
}

fn read_user_pod<T: Pod>(context: &IoctlContext, address: usize) -> AxResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let bytes = unsafe {
        slice::from_raw_parts_mut(
            value.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            mem::size_of::<T>(),
        )
    };
    context
        .user_memory()
        .read_bytes(address, bytes)
        .map_err(map_usercopy_error)?;
    // SAFETY: read_bytes initialized the complete Pod representation.
    Ok(unsafe { value.assume_init() })
}

fn write_user_bytes(context: &IoctlContext, address: usize, bytes: &[u8]) -> AxResult {
    context
        .user_memory()
        .write_bytes(address, bytes)
        .map_err(map_usercopy_error)
}

enum UffdHandlerBinding {
    // Linux keeps the userfaultfd context but not the old image's live VMA
    // ownership across exec.  A weak binding lets the old AddrSpace retire;
    // the OFD then remains a valid, initialized, inert context.
    AddressSpace(Weak<axsync::Mutex<AddrSpace>>),
    #[cfg(test)]
    Standalone(Weak<BlockingMutex<UffdAddressSpaceState>>),
}

struct AttachedUffdHandler {
    binding: UffdHandlerBinding,
    handler: FaultHandlerId,
}

impl AttachedUffdHandler {
    #[cfg(test)]
    const fn handler(&self) -> FaultHandlerId {
        self.handler
    }

    fn claim(&self) -> AxResult<Option<DeliveredUffdEvent>> {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                let Some(aspace) = aspace.upgrade() else {
                    return Ok(None);
                };
                aspace.lock().claim_uffd_event(self.handler)
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let Some(state) = state.upgrade() else {
                    return Ok(None);
                };
                state.lock().claim_next(self.handler)
            }
        }
    }

    fn pending(&self) -> AxResult<bool> {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                let Some(aspace) = aspace.upgrade() else {
                    return Ok(false);
                };
                aspace.lock().uffd_handler_pending(self.handler)
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let Some(state) = state.upgrade() else {
                    return Ok(false);
                };
                state.lock().pending(self.handler)
            }
        }
    }

    fn register_range(
        &self,
        api: &UffdApiState,
        range: PageRange,
        mode: UffdRegisterMode,
    ) -> AxResult<UffdIoctls> {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                // Linux reports ENOMEM when mmget_not_zero() cannot retain the
                // old mm for REGISTER/UNREGISTER.
                let aspace = aspace.upgrade().ok_or(AxError::NoMemory)?;
                aspace
                    .lock()
                    .register_uffd_range(api, self.handler, range, mode)
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let state = state.upgrade().ok_or(AxError::NoMemory)?;
                state
                    .lock()
                    .register_test_range(api, self.handler, range, mode)
            }
        }
    }

    fn unregister_range(&self, api: &UffdApiState, range: PageRange) -> AxResult {
        let deferred = match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                let aspace = aspace.upgrade().ok_or(AxError::NoMemory)?;
                let result = aspace.lock().unregister_uffd_range(api, range);
                result?
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let state = state.upgrade().ok_or(AxError::NoMemory)?;
                let result = state.lock().unregister_test_range(api, range);
                result?
            }
        };
        // Fault waiters and fd poll registrations may re-enter the address
        // space. Their wake ownership is therefore consumed only after the
        // address-space/state guard above has been dropped.
        deferred.finish();
        Ok(())
    }

    fn resolver_target(&self) -> AxResult<Arc<axsync::Mutex<AddrSpace>>> {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => aspace
                .upgrade()
                .ok_or_else(|| AxError::from(LinuxError::ESRCH)),
            #[cfg(test)]
            UffdHandlerBinding::Standalone(_) => Err(AxError::OperationNotSupported),
        }
    }

    fn wake_retained(&self, aspace: &Arc<axsync::Mutex<AddrSpace>>, range: PageRange) -> AxResult {
        let wake = {
            let mut locked = aspace.lock();
            locked.wake_uffd_handler_range(self.handler, range)?
        };
        wake.finish();
        Ok(())
    }

    fn wake_range(&self, range: PageRange) -> AxResult {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                let Some(aspace) = aspace.upgrade() else {
                    // UFFDIO_WAKE does not retain or inspect the old mm. A
                    // valid range with no remaining waiter is a successful
                    // no-op after exec/teardown.
                    return Ok(());
                };
                self.wake_retained(&aspace, range)
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let Some(state) = state.upgrade() else {
                    return Ok(());
                };
                let wake = state.lock().wake_test_range(self.handler, range)?;
                wake.finish();
                Ok(())
            }
        }
    }
}

impl Drop for AttachedUffdHandler {
    fn drop(&mut self) {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                let Some(aspace) = aspace.upgrade() else {
                    return;
                };
                let detached = {
                    let mut aspace = aspace.lock();
                    aspace.detach_uffd_handler(self.handler)
                };
                match detached {
                    Ok(detached) => detached.finish(),
                    Err(error) => warn!("userfaultfd final detach failed: {error:?}"),
                }
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let Some(state) = state.upgrade() else {
                    return;
                };
                let deferred = state.lock().detach_handler(self.handler);
                match deferred {
                    Ok(deferred) => {
                        deferred.finish();
                    }
                    Err(error) => warn!("userfaultfd test detach failed: {error:?}"),
                }
            }
        }
    }
}

pub(crate) struct UserfaultFile {
    binding: AttachedUffdHandler,
    api: BlockingMutex<UffdApiState>,
    api_initialized: AtomicBool,
    // This is the backend mirror of the authoritative OFD status bit.  The
    // FileDescription transition lock updates it before publishing F_SETFL or
    // FIONBIO, and dup/fork retain the same FileDescription and mirror.
    nonblocking: AtomicBool,
    readiness: Arc<UffdPollSet>,
}

impl UserfaultFile {
    pub(crate) fn try_new(
        aspace: Arc<axsync::Mutex<AddrSpace>>,
        flags: UffdCreateFlags,
    ) -> AxResult<Arc<Self>> {
        let readiness = Arc::try_new(UffdPollSet::new()).map_err(|_| AxError::NoMemory)?;
        let mut candidate = None;
        let handler = loop {
            let needs_state = aspace.lock().needs_uffd_state();
            if needs_state && candidate.is_none() {
                candidate = Some(UffdAddressSpaceState::try_new_boxed()?);
            }

            let attached = {
                let mut locked = aspace.lock();
                locked.attach_uffd_handler(&mut candidate, readiness.clone())
            };
            match attached {
                Ok(handler) => break handler,
                Err(AxError::WouldBlock) => continue,
                Err(error) => {
                    drop(candidate);
                    return Err(error);
                }
            }
        };
        // A concurrent first binder may have installed its own candidate.
        drop(candidate);

        let binding = AttachedUffdHandler {
            binding: UffdHandlerBinding::AddressSpace(Arc::downgrade(&aspace)),
            handler,
        };
        // If the final Arc allocation fails, dropping `binding` rolls the
        // attached handler back.  Its wake and state destruction happen after
        // the address-space guard has been released.
        Arc::try_new(Self {
            binding,
            api: BlockingMutex::new(UffdApiState::new()),
            api_initialized: AtomicBool::new(false),
            nonblocking: AtomicBool::new(flags.nonblocking()),
            readiness,
        })
        .map_err(|_| AxError::NoMemory)
    }

    #[cfg(test)]
    fn try_new_for_test(
        flags: UffdCreateFlags,
    ) -> AxResult<(Arc<BlockingMutex<UffdAddressSpaceState>>, Arc<Self>)> {
        let state = Arc::try_new(BlockingMutex::new(*UffdAddressSpaceState::try_new_boxed()?))
            .map_err(|_| AxError::NoMemory)?;
        let file = Self::try_new_for_test_in_state(&state, flags)?;
        Ok((state, file))
    }

    #[cfg(test)]
    fn try_new_for_test_in_state(
        state: &Arc<BlockingMutex<UffdAddressSpaceState>>,
        flags: UffdCreateFlags,
    ) -> AxResult<Arc<Self>> {
        let readiness = Arc::try_new(UffdPollSet::new()).map_err(|_| AxError::NoMemory)?;
        let handler = state.lock().attach_handler(readiness.clone())?;
        let binding = AttachedUffdHandler {
            binding: UffdHandlerBinding::Standalone(Arc::downgrade(state)),
            handler,
        };
        Arc::try_new(Self {
            binding,
            api: BlockingMutex::new(UffdApiState::new()),
            api_initialized: AtomicBool::new(false),
            nonblocking: AtomicBool::new(flags.nonblocking()),
            readiness,
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn initialized(&self) -> bool {
        self.api_initialized.load(Ordering::Acquire)
    }

    fn prepare_api(
        &self,
        request: UffdApiRaw,
    ) -> Result<(UffdApiNegotiation, UffdApiRaw), MmError> {
        let negotiation = self.api.lock().prepare_raw(request.api, request.features)?;
        let response = negotiation.response();
        Ok((
            negotiation,
            UffdApiRaw {
                api: response.api(),
                features: response.features().bits(),
                ioctls: response.ioctls().bits(),
            },
        ))
    }

    fn commit_api(&self, negotiation: UffdApiNegotiation) -> Result<(), MmError> {
        self.api.lock().commit(negotiation)?;
        self.api_initialized.store(true, Ordering::Release);
        self.readiness.wake();
        Ok(())
    }

    fn ioctl_api(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let request: UffdApiRaw = read_user_pod(context, arg)?;
        let cleared = UffdApiRaw::default();

        let (negotiation, response) = match self.prepare_api(request) {
            Ok(prepared) => prepared,
            Err(error) => {
                write_user_bytes(context, arg, bytemuck::bytes_of(&cleared))?;
                return Err(uffd_policy_error(error));
            }
        };
        // UFFDIO_API is a copyout-before-commit transaction.  An EFAULT leaves
        // the context uninitialized so userspace may retry.
        write_user_bytes(context, arg, bytemuck::bytes_of(&response))?;
        if let Err(error) = self.commit_api(negotiation) {
            write_user_bytes(context, arg, bytemuck::bytes_of(&cleared))?;
            return Err(uffd_policy_error(error));
        }
        Ok(0)
    }

    fn api_snapshot(&self) -> AxResult<UffdApiState> {
        if !self.initialized() {
            return Err(AxError::InvalidInput);
        }
        // Clone under the OFD mutex, then release it before acquiring the
        // address-space mutex. API state is one-way after initialization.
        Ok(self.api.lock().clone())
    }

    fn checked_range(raw: UffdRangeRaw) -> AxResult<PageRange> {
        let start = usize::try_from(raw.start).map_err(|_| AxError::InvalidInput)?;
        let length = usize::try_from(raw.len).map_err(|_| AxError::InvalidInput)?;
        let range = PageRange::new(start, length, PAGE_SIZE_4K).map_err(uffd_policy_error)?;
        let user_end = USER_SPACE_BASE
            .checked_add(USER_SPACE_SIZE)
            .ok_or(AxError::InvalidInput)?;
        // Current Linux 6.12.y accepts a geometrically valid range beginning
        // below mmap_min_addr.  The later VMA scan publishes only mapped
        // intersections, so this gate owns page geometry and TASK_SIZE only.
        // Keep those failures ahead of a retired-mm ENOMEM.
        if range.end() > user_end {
            return Err(AxError::InvalidInput);
        }
        Ok(range)
    }

    fn register_with_usercopy(
        &self,
        copyin: impl FnOnce() -> AxResult<UffdRegisterInputRaw>,
        copyout: impl FnOnce(u64) -> AxResult,
    ) -> AxResult<usize> {
        // Linux rejects every non-API ioctl on an uninitialized context before
        // touching its command-specific user pointer.
        let api = self.api_snapshot()?;
        let request = copyin()?;
        let mode = UffdRegisterMode::from_bits(request.mode).map_err(uffd_policy_error)?;
        let range = Self::checked_range(request.range)?;
        let ioctls = self.binding.register_range(&api, range, mode)?;
        // Linux commits the registration before this lone 8-byte copyout.
        // EFAULT is returned without rolling the table back.
        copyout(ioctls.bits())?;
        Ok(0)
    }

    fn ioctl_register(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        self.register_with_usercopy(
            || read_user_pod(context, arg),
            |ioctls| {
                let output = arg
                    .checked_add(UFFD_REGISTER_IOCTLS_OFFSET)
                    .ok_or(AxError::BadAddress)?;
                write_user_bytes(context, output, &ioctls.to_ne_bytes())
            },
        )
    }

    fn unregister_with_usercopy(
        &self,
        copyin: impl FnOnce() -> AxResult<UffdRangeRaw>,
    ) -> AxResult<usize> {
        // Keep the initialization gate ahead of the command-specific copyin,
        // matching userfaultfd_ioctl() ordering.
        let api = self.api_snapshot()?;
        let request = copyin()?;
        let range = Self::checked_range(request)?;
        self.binding.unregister_range(&api, range)?;
        Ok(0)
    }

    fn ioctl_unregister(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        self.unregister_with_usercopy(|| read_user_pod(context, arg))
    }

    fn checked_copy_request(request: UffdCopyInputRaw) -> AxResult<UffdCopyRequest> {
        let destination = Self::checked_range(UffdRangeRaw {
            start: request.dst,
            len: request.len,
        })?;
        let source_start = usize::try_from(request.src).map_err(|_| AxError::InvalidInput)?;
        let source_len = usize::try_from(request.len).map_err(|_| AxError::InvalidInput)?;
        let user_end = USER_SPACE_BASE
            .checked_add(USER_SPACE_SIZE)
            .ok_or(AxError::InvalidInput)?;
        let source = UserRange::new_bounded(source_start, source_len, user_end)
            .map_err(uffd_policy_error)?;
        let mode = UffdCopyMode::from_bits(request.mode).map_err(uffd_policy_error)?;
        UffdCopyRequest::new(source, destination, mode).map_err(uffd_policy_error)
    }

    fn checked_zeropage_request(request: UffdZeroPageInputRaw) -> AxResult<UffdZeroPageRequest> {
        let destination = Self::checked_range(request.range)?;
        let mode = UffdZeroPageMode::from_bits(request.mode).map_err(uffd_policy_error)?;
        Ok(UffdZeroPageRequest::new(destination, mode))
    }

    fn install_resolver_pages(
        caller_task: &axtask::AxTaskRef,
        target: &Arc<axsync::Mutex<AddrSpace>>,
        destination: PageRange,
        data: UffdResolverData,
        source_capability: Option<&UserMemoryCapability>,
    ) -> AxResult<UffdResolverProgress> {
        // The raw geometry/mode gates have already passed. Destination
        // VMA/registration failure comes from Linux's mfill operation, so it
        // is a zero-progress lower error that must be written to the signed
        // output field.
        let preflight = {
            let locked = target.lock();
            locked.preflight_uffd_resolver_range(destination)
        };
        let lease = match preflight {
            Ok(lease) => lease,
            Err(error) => {
                return Ok(UffdResolverProgress {
                    completed: 0,
                    lower_error: Some(error),
                });
            }
        };

        let mut progress = UffdResolverProgress {
            completed: 0,
            lower_error: None,
        };
        let mut prepared = match PreparedCowPage::try_new() {
            Ok(prepared) => prepared,
            Err(error) => {
                progress.lower_error = Some(error);
                return Ok(progress);
            }
        };

        while progress.completed < destination.len() {
            // The at-most-three table pages are retained and reused across the
            // range. Replenishment and data preparation both happen without an
            // address-space guard.
            if let Err(error) = prepared.reserve_max_table_frames() {
                progress.lower_error = Some(error);
                break;
            }
            if let Err(error) = data.prepare(progress.completed, &mut prepared, source_capability) {
                progress.lower_error = Some(error);
                break;
            }
            let page = destination
                .subrange(progress.completed, PAGE_SIZE_4K)
                .map_err(|_| AxError::BadState)?;
            let mut icache_synchronization: Option<UffdIcacheSynchronization> = None;
            loop {
                let publication = {
                    let mut locked = target.lock();
                    locked.publish_prepared_uffd_page(
                        lease,
                        page,
                        data.disposition(),
                        &mut prepared,
                        icache_synchronization.take(),
                    )
                };
                match publication {
                    Ok(publication @ UffdPagePublication::NeedsIcacheSynchronization) => {
                        // The initialized frame is not reachable yet. Remote
                        // maintenance stays outside the address-space mutex;
                        // the retry revalidates all publication authority.
                        icache_synchronization = Some(publication.synchronize());
                    }
                    Ok(UffdPagePublication::Published) => {
                        progress.completed = progress
                            .completed
                            .checked_add(PAGE_SIZE_4K)
                            .ok_or(AxError::BadState)?;
                        break;
                    }
                    Err(error) => {
                        progress.lower_error = Some(error);
                        break;
                    }
                }
            }
            if progress.lower_error.is_some() {
                break;
            }

            if progress.completed < destination.len() {
                // Match Linux's per-page cond_resched/fatal-signal boundary.
                // This runs after all MM ownership has left the critical
                // section, and a completed prefix remains reportable as
                // positive progress.
                axtask::resched_if_needed();
                if has_pending_sigkill(caller_task.as_thread()) {
                    progress.lower_error = Some(AxError::Interrupted);
                    break;
                }
            }
        }
        // Unused table reservations and a failed/unpublished data frame are
        // reclaimed before usercopy, with no MM lock held.
        drop(prepared);
        Ok(progress)
    }

    fn reject_copy_wp_after_target_preflight(
        target: &Arc<axsync::Mutex<AddrSpace>>,
        destination: PageRange,
    ) -> UffdResolverProgress {
        // UFFDIO_COPY_MODE_WP is a recognized Linux bit, not malformed raw
        // input. This MISSING-only profile cannot install a UFFD-WP PTE, but
        // Linux resolves target-mm/range errors before reporting that mode
        // mismatch through the signed `uffdio_copy.copy` field.
        let preflight = {
            let locked = target.lock();
            locked
                .preflight_uffd_resolver_range(destination)
                .map(|_lease| ())
        };
        Self::copy_wp_progress(preflight)
    }

    fn copy_wp_progress(preflight: AxResult) -> UffdResolverProgress {
        let lower_error = preflight.err().unwrap_or(AxError::InvalidInput);
        UffdResolverProgress {
            completed: 0,
            lower_error: Some(lower_error),
        }
    }

    fn finish_resolver_with(
        result: UffdResolverResult,
        lower_error: Option<AxError>,
        copyout: impl FnOnce(i64) -> AxResult,
        wake: impl FnOnce(PageRange) -> AxResult,
    ) -> AxResult<usize> {
        // Linux stores the signed result before waking. If this copyout fails,
        // installed pages and deferred broker completions remain intact and a
        // later explicit WAKE can recover the waiter.
        copyout(result.reported_bytes())?;
        if let Some(range) = result.wake_range() {
            wake(range)?;
        }
        match result.outcome() {
            UffdResolverOutcome::Complete if lower_error.is_none() => Ok(0),
            UffdResolverOutcome::Retry if lower_error.is_some() => {
                Err(AxError::from(LinuxError::EAGAIN))
            }
            UffdResolverOutcome::Failed => {
                let error = lower_error.ok_or(AxError::BadState)?;
                Err(error)
            }
            UffdResolverOutcome::Complete | UffdResolverOutcome::Retry => Err(AxError::BadState),
        }
    }

    fn finish_resolver(
        &self,
        target: &Arc<axsync::Mutex<AddrSpace>>,
        result: UffdResolverResult,
        lower_error: Option<AxError>,
        copyout: impl FnOnce(i64) -> AxResult,
    ) -> AxResult<usize> {
        Self::finish_resolver_with(result, lower_error, copyout, |range| {
            self.binding.wake_retained(target, range)
        })
    }

    fn copy_with_usercopy(
        &self,
        context: &IoctlContext,
        copyin: impl FnOnce() -> AxResult<UffdCopyInputRaw>,
        copyout: impl FnOnce(i64) -> AxResult,
    ) -> AxResult<usize> {
        let _api = self.api_snapshot()?;
        let request = Self::checked_copy_request(copyin()?)?;
        let target = self.binding.resolver_target()?;
        let progress = if request.mode().write_protect() {
            Self::reject_copy_wp_after_target_preflight(&target, request.destination())
        } else {
            Self::install_resolver_pages(
                context.caller_task(),
                &target,
                request.destination(),
                UffdResolverData::Copy(request.source()),
                Some(context.user_memory()),
            )?
        };
        let result = if progress.completed == 0 {
            let error = progress.lower_error.ok_or(AxError::BadState)?;
            UffdResolverResult::failure(LinuxError::from(error).code())
                .map_err(uffd_policy_error)?
        } else {
            UffdResolverResult::for_copy(request, progress.completed).map_err(uffd_policy_error)?
        };
        self.finish_resolver(&target, result, progress.lower_error, copyout)
    }

    fn ioctl_copy(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        self.copy_with_usercopy(
            context,
            || read_user_pod(context, arg),
            |result| {
                let output = arg
                    .checked_add(UFFD_COPY_OUTPUT_OFFSET)
                    .ok_or(AxError::BadAddress)?;
                write_user_bytes(context, output, &result.to_ne_bytes())
            },
        )
    }

    fn zeropage_with_usercopy(
        &self,
        context: &IoctlContext,
        copyin: impl FnOnce() -> AxResult<UffdZeroPageInputRaw>,
        copyout: impl FnOnce(i64) -> AxResult,
    ) -> AxResult<usize> {
        let _api = self.api_snapshot()?;
        let request = Self::checked_zeropage_request(copyin()?)?;
        let target = self.binding.resolver_target()?;
        let progress = Self::install_resolver_pages(
            context.caller_task(),
            &target,
            request.destination(),
            UffdResolverData::Zero,
            None,
        )?;
        let result = if progress.completed == 0 {
            let error = progress.lower_error.ok_or(AxError::BadState)?;
            UffdResolverResult::failure(LinuxError::from(error).code())
                .map_err(uffd_policy_error)?
        } else {
            UffdResolverResult::for_zeropage(request, progress.completed)
                .map_err(uffd_policy_error)?
        };
        self.finish_resolver(&target, result, progress.lower_error, copyout)
    }

    fn ioctl_zeropage(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        self.zeropage_with_usercopy(
            context,
            || read_user_pod(context, arg),
            |result| {
                let output = arg
                    .checked_add(UFFD_ZEROPAGE_OUTPUT_OFFSET)
                    .ok_or(AxError::BadAddress)?;
                write_user_bytes(context, output, &result.to_ne_bytes())
            },
        )
    }

    fn wake_with_usercopy(
        &self,
        copyin: impl FnOnce() -> AxResult<UffdRangeRaw>,
    ) -> AxResult<usize> {
        let _api = self.api_snapshot()?;
        let range = Self::checked_range(copyin()?)?;
        self.binding.wake_range(range)?;
        Ok(0)
    }

    fn ioctl_wake(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        self.wake_with_usercopy(|| read_user_pod(context, arg))
    }

    fn encode_pagefault(event: DeliveredUffdEvent) -> [u8; UFFD_MSG_SIZE] {
        let request = event.request();
        let mut message = [0u8; UFFD_MSG_SIZE];
        message[0] = UFFD_EVENT_PAGEFAULT as u8;
        let flags = if request.key().access() == FaultAccess::Write {
            UFFD_PAGEFAULT_FLAG_WRITE as u64
        } else {
            0
        };
        message[8..16].copy_from_slice(&flags.to_ne_bytes());
        message[16..24].copy_from_slice(&(request.key().page_address().get() as u64).to_ne_bytes());
        message
    }

    fn claim_delivery(&self) -> AxResult<Option<DeliveredUffdEvent>> {
        // A non-CLOEXEC OFD remains valid after exec, but its old mm has no
        // live mappings and therefore no deliverable faults.
        self.binding.claim()
    }

    fn has_pending(&self) -> AxResult<bool> {
        self.binding.pending()
    }

    fn read_ready(&self, dst: &mut IoDst) -> AxResult<usize> {
        let mut written = 0usize;
        while dst.remaining_mut() >= UFFD_MSG_SIZE {
            let Some(event) = self.claim_delivery()? else {
                break;
            };
            // Claim precedes copyout.  A usercopy fault leaves the broker entry
            // Delivered, but this OFD deliberately does not replay it or make
            // it poll-readable again; only still-Pending entries are readable.
            let message = Self::encode_pagefault(event);
            match dst.write(&message) {
                Ok(copied) if copied == UFFD_MSG_SIZE => written += copied,
                Ok(_) => {
                    return if written == 0 {
                        Err(AxError::BadAddress)
                    } else {
                        Ok(written)
                    };
                }
                Err(error) => {
                    return if written == 0 {
                        Err(error)
                    } else {
                        Ok(written)
                    };
                }
            }
        }
        if written == 0 {
            Err(AxError::WouldBlock)
        } else {
            Ok(written)
        }
    }
}

impl FileLike for UserfaultFile {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if dst.remaining_mut() < UFFD_MSG_SIZE {
            return Err(AxError::InvalidInput);
        }
        if !self.initialized() {
            return Err(AxError::InvalidInput);
        }
        let nonblocking = self.nonblocking();
        // The shared readiness helper owns the check -> arm -> check contract;
        // each broker claim is already serialized by the address-space lock,
        // so concurrent readers need no OFD-wide mutex held across sleep.
        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            self.read_ready(dst)
        })
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[userfaultfd]".into())
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            UFFDIO_API_CMD => self.ioctl_api(context, arg),
            UFFDIO_REGISTER_CMD => self.ioctl_register(context, arg),
            UFFDIO_UNREGISTER_CMD => self.ioctl_unregister(context, arg),
            UFFDIO_WAKE_CMD => self.ioctl_wake(context, arg),
            UFFDIO_COPY_CMD => self.ioctl_copy(context, arg),
            UFFDIO_ZEROPAGE_CMD => self.ioctl_zeropage(context, arg),
            // Linux v6.12 returns EINVAL both before initialization and for an
            // unsupported command after initialization.
            _ => Err(AxError::InvalidInput),
        }
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.nonblocking.store(nonblocking, Ordering::Release);
        // Linux's userfaultfd poll contract itself changes when O_NONBLOCK is
        // toggled, so wake retained poll/epoll registrations after commit.
        self.readiness.wake();
        Ok(())
    }
}

impl Pollable for UserfaultFile {
    fn poll(&self) -> IoEvents {
        // Linux v6.12 deliberately reports POLLERR until UFFDIO_API commits,
        // and also for a blocking userfaultfd OFD.
        if !self.initialized() || !self.nonblocking() {
            return IoEvents::ERROR;
        }
        match self.has_pending() {
            Ok(true) => IoEvents::READABLE,
            Ok(false) => IoEvents::empty(),
            Err(_) => IoEvents::ERROR,
        }
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(IoEvents::READABLE | IoEvents::ERROR) {
            PollRegistration::single(&self.readiness, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::{Arc, Weak as ArcWeak};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{
        sync::MutexGuard,
        task::{Wake, Waker},
    };

    use axio::{IoBufMut, Write};
    use linux_raw_sys::general::{O_NONBLOCK, O_RDONLY};
    use thekernel_linux_mm::{
        FaultDisposition, FaultKey, FaultRequest, FaultType, MappingAccess, MappingKind,
        MappingSnapshot, UFFD_API, UFFD_O_NONBLOCK, UFFD_USER_MODE_ONLY,
    };

    use super::*;
    use crate::file::{FileDescription, FileHandle};

    fn test_context() -> MutexGuard<'static, ()> {
        crate::test_support::scheduler_test_context()
    }

    fn ioctl_context() -> IoctlContext {
        let aspace = axtask::current().as_thread().proc_data.aspace().clone();
        IoctlContext::new(aspace)
    }

    struct TestDst {
        remaining: usize,
        fail: bool,
        bytes: alloc::vec::Vec<u8>,
    }

    impl TestDst {
        fn success(remaining: usize) -> Self {
            Self {
                remaining,
                fail: false,
                bytes: alloc::vec::Vec::new(),
            }
        }

        fn fault(remaining: usize) -> Self {
            Self {
                remaining,
                fail: true,
                bytes: alloc::vec::Vec::new(),
            }
        }
    }

    impl Write for TestDst {
        fn write(&mut self, buf: &[u8]) -> AxResult<usize> {
            if self.fail {
                return Err(AxError::BadAddress);
            }
            self.bytes.extend_from_slice(buf);
            self.remaining -= buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> AxResult<()> {
            Ok(())
        }
    }

    impl IoBufMut for TestDst {
        fn remaining_mut(&self) -> usize {
            self.remaining
        }
    }

    fn flags(nonblocking: bool) -> UffdCreateFlags {
        UffdCreateFlags::from_bits(
            UFFD_USER_MODE_ONLY | if nonblocking { UFFD_O_NONBLOCK } else { 0 },
        )
        .unwrap()
    }

    fn new_file(
        nonblocking: bool,
    ) -> (
        Arc<BlockingMutex<UffdAddressSpaceState>>,
        Arc<UserfaultFile>,
    ) {
        UserfaultFile::try_new_for_test(flags(nonblocking)).unwrap()
    }

    fn initialize(file: &UserfaultFile) {
        let (negotiation, response) = file
            .prepare_api(UffdApiRaw {
                api: UFFD_API,
                features: 0,
                ioctls: u64::MAX,
            })
            .unwrap();
        assert_eq!(response.api, UFFD_API);
        assert_eq!(response.features, 0);
        file.commit_api(negotiation).unwrap();
    }

    fn snapshot(start: usize, length: usize) -> MappingSnapshot {
        MappingSnapshot::from_raw(
            1,
            2,
            1,
            start,
            length,
            PAGE_SIZE_4K,
            MappingAccess::new(true, true, false).bits(),
            MappingKind::AnonymousPrivate,
            true,
            false,
        )
        .unwrap()
    }

    fn request(handler: FaultHandlerId, page: usize) -> FaultRequest {
        FaultRequest::new(
            FaultKey::from_address(snapshot(page, PAGE_SIZE_4K), page, FaultAccess::Read).unwrap(),
            handler,
            FaultType::Missing,
        )
    }

    fn register_input(start: usize, length: usize) -> UffdRegisterInputRaw {
        UffdRegisterInputRaw {
            range: UffdRangeRaw {
                start: start as u64,
                len: length as u64,
            },
            mode: UffdRegisterMode::MISSING.bits(),
        }
    }

    fn range(start: usize, length: usize) -> UffdRangeRaw {
        UffdRangeRaw {
            start: start as u64,
            len: length as u64,
        }
    }

    fn copy_input(dst: usize, src: usize, length: usize, mode: u64) -> UffdCopyInputRaw {
        UffdCopyInputRaw {
            dst: dst as u64,
            src: src as u64,
            len: length as u64,
            mode,
        }
    }

    fn dead_address_space_file() -> UserfaultFile {
        let mut api = UffdApiState::new();
        let negotiation = api.prepare_raw(UFFD_API, 0).unwrap();
        api.commit(negotiation).unwrap();
        UserfaultFile {
            binding: AttachedUffdHandler {
                binding: UffdHandlerBinding::AddressSpace(Weak::new()),
                handler: FaultHandlerId::new(1).unwrap(),
            },
            api: BlockingMutex::new(api),
            api_initialized: AtomicBool::new(true),
            nonblocking: AtomicBool::new(true),
            readiness: Arc::new(UffdPollSet::new()),
        }
    }

    struct StateLockWakeProbe {
        state: ArcWeak<BlockingMutex<UffdAddressSpaceState>>,
        calls: AtomicUsize,
        all_lock_external: AtomicBool,
    }

    impl StateLockWakeProbe {
        fn new(state: &Arc<BlockingMutex<UffdAddressSpaceState>>) -> Self {
            Self {
                state: Arc::downgrade(state),
                calls: AtomicUsize::new(0),
                all_lock_external: AtomicBool::new(true),
            }
        }

        fn observe(&self) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let lock_external = self
                .state
                .upgrade()
                .is_none_or(|state| state.try_lock().is_some());
            if !lock_external {
                self.all_lock_external.store(false, Ordering::Relaxed);
            }
        }
    }

    impl Wake for StateLockWakeProbe {
        fn wake(self: Arc<Self>) {
            self.observe();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.observe();
        }
    }

    #[test]
    fn api_prepare_does_not_commit_until_copyout_boundary() {
        let _context = test_context();
        let (_aspace, file) = new_file(true);
        let request = UffdApiRaw {
            api: UFFD_API,
            features: 0,
            ioctls: u64::MAX,
        };
        let (abandoned, response) = file.prepare_api(request).unwrap();
        assert_eq!(response.api, UFFD_API);
        assert!(!file.initialized());
        let _ = abandoned;

        let (committed, _) = file.prepare_api(request).unwrap();
        file.commit_api(committed).unwrap();
        assert!(file.initialized());
        assert!(file.prepare_api(request).is_err());
    }

    #[test]
    fn poll_matches_linux_initialization_and_nonblocking_contract() {
        let _context = test_context();
        let (_aspace, file) = new_file(false);
        assert_eq!(file.poll(), IoEvents::ERROR);
        initialize(&file);
        assert_eq!(file.poll(), IoEvents::ERROR);

        let description = FileDescription::new_with_flags(file.clone(), O_RDONLY).unwrap();
        let handle: FileHandle<dyn FileLike> = FileHandle::from_description_for_test(description);
        let status = handle.set_nonblocking_status(true).unwrap();
        assert_eq!(status.raw(), O_RDONLY | O_NONBLOCK);
        assert!(file.nonblocking());
        assert_eq!(file.poll(), IoEvents::empty());

        let status = handle.set_nonblocking_status(false).unwrap();
        assert_eq!(status.raw(), O_RDONLY);
        assert!(!file.nonblocking());
        assert_eq!(file.poll(), IoEvents::ERROR);
    }

    #[test]
    fn resolver_before_read_clears_authoritative_readiness() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        let handler = file.binding.handler();
        let admission = state
            .lock()
            .admit_test_request(handler, request(handler, 0x4000));
        assert_eq!(file.poll(), IoEvents::READABLE);

        state.lock().complete_test_request(admission.request());
        assert_eq!(file.poll(), IoEvents::empty());
        let mut dst = TestDst::success(UFFD_MSG_SIZE);
        assert_eq!(file.read_ready(&mut dst), Err(AxError::WouldBlock));
    }

    #[test]
    fn failed_copyout_does_not_replay_a_delivered_event() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        let handler = file.binding.handler();
        state
            .lock()
            .admit_test_request(handler, request(handler, 0x5000));
        assert_eq!(file.poll(), IoEvents::READABLE);

        let mut faulting = TestDst::fault(UFFD_MSG_SIZE);
        assert_eq!(file.read_ready(&mut faulting), Err(AxError::BadAddress));
        assert_eq!(file.poll(), IoEvents::empty());

        let mut retry = TestDst::success(UFFD_MSG_SIZE);
        assert_eq!(file.read_ready(&mut retry), Err(AxError::WouldBlock));
        assert!(retry.bytes.is_empty());
    }

    #[test]
    fn range_ioctls_reject_uninitialized_context_before_usercopy() {
        let _context = test_context();
        let (_state, file) = new_file(true);
        let ioctl_context = ioctl_context();

        assert_eq!(
            file.ioctl(&ioctl_context, UFFDIO_REGISTER_CMD, usize::MAX),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.ioctl(&ioctl_context, UFFDIO_UNREGISTER_CMD, usize::MAX),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.ioctl(&ioctl_context, UFFDIO_WAKE_CMD, usize::MAX),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.ioctl(&ioctl_context, UFFDIO_COPY_CMD, usize::MAX),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.ioctl(&ioctl_context, UFFDIO_ZEROPAGE_CMD, usize::MAX),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn resolver_layout_and_geometry_match_linux_v6_12() {
        let _context = test_context();
        assert_eq!(UFFD_COPY_INPUT_SIZE, 32);
        assert_eq!(UFFD_COPY_OUTPUT_OFFSET, 32);
        assert_eq!(mem::size_of::<UffdCopyRaw>(), 40);
        assert_eq!(UFFD_ZEROPAGE_INPUT_SIZE, 24);
        assert_eq!(UFFD_ZEROPAGE_OUTPUT_OFFSET, 24);
        assert_eq!(mem::size_of::<UffdZeroPageRaw>(), 32);

        let copy = UserfaultFile::checked_copy_request(copy_input(0x4000, 0x1801, PAGE_SIZE_4K, 0))
            .unwrap();
        assert_eq!(copy.source().start(), 0x1801);
        assert_eq!(copy.destination().start(), 0x4000);
        let copy_wp =
            UserfaultFile::checked_copy_request(copy_input(0x4000, 0x1801, PAGE_SIZE_4K, 1 << 1))
                .unwrap();
        assert!(copy_wp.mode().write_protect());
        assert_eq!(
            UserfaultFile::checked_copy_request(copy_input(0x4000, 0x1801, PAGE_SIZE_4K, 1 << 2,)),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            UserfaultFile::checked_copy_request(copy_input(0x4001, 0x1801, PAGE_SIZE_4K, 0,)),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            UserfaultFile::checked_copy_request(copy_input(
                0x4000,
                usize::MAX - PAGE_SIZE_4K + 2,
                PAGE_SIZE_4K,
                0,
            )),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            UserfaultFile::checked_zeropage_request(UffdZeroPageInputRaw {
                range: range(0x4000, PAGE_SIZE_4K),
                mode: u64::MAX,
            }),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn resolver_raw_preflight_and_retired_mm_leave_output_untouched() {
        let _test_context = test_context();
        let ioctl_context = ioctl_context();
        let (_state, file) = new_file(true);
        initialize(&file);
        let copied_out = AtomicBool::new(false);
        assert_eq!(
            file.copy_with_usercopy(
                &ioctl_context,
                || Ok(copy_input(0x4000, 0x1801, PAGE_SIZE_4K, u64::MAX)),
                |_| {
                    copied_out.store(true, Ordering::Release);
                    Ok(())
                },
            ),
            Err(AxError::InvalidInput)
        );
        assert!(!copied_out.load(Ordering::Acquire));

        let dead = dead_address_space_file();
        assert_eq!(
            dead.copy_with_usercopy(
                &ioctl_context,
                || Ok(copy_input(0x4000, 0x1801, PAGE_SIZE_4K, 0)),
                |_| {
                    copied_out.store(true, Ordering::Release);
                    Ok(())
                },
            )
            .map_err(LinuxError::from),
            Err(LinuxError::ESRCH)
        );
        assert!(!copied_out.load(Ordering::Acquire));
    }

    #[test]
    fn copy_wp_reports_post_target_errors_through_the_signed_output() {
        let _context = test_context();

        let unsupported = UserfaultFile::copy_wp_progress(Ok(()));
        assert_eq!(
            unsupported.lower_error.map(LinuxError::from),
            Some(LinuxError::EINVAL)
        );

        let target_error = AxError::from(LinuxError::ENOENT);
        let progress = UserfaultFile::copy_wp_progress(Err(target_error));
        assert_eq!(progress.completed, 0);
        assert_eq!(
            progress.lower_error.map(LinuxError::from),
            Some(LinuxError::ENOENT)
        );
        let result = UffdResolverResult::failure(LinuxError::ENOENT.code()).unwrap();
        let mut reported = None;
        assert_eq!(
            UserfaultFile::finish_resolver_with(
                result,
                progress.lower_error,
                |value| {
                    reported = Some(value);
                    Ok(())
                },
                |_| panic!("a zero-progress failure must not wake a range"),
            )
            .map_err(LinuxError::from),
            Err(LinuxError::ENOENT)
        );
        assert_eq!(reported, Some(-LinuxError::ENOENT.code() as i64));

        assert_eq!(
            UserfaultFile::finish_resolver_with(
                result,
                Some(target_error),
                |_| Err(AxError::BadAddress),
                |_| panic!("copyout failure must precede wake"),
            ),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn wake_is_a_successful_noop_after_target_mm_retires() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        drop(state);
        assert_eq!(
            file.wake_with_usercopy(|| Ok(range(0x4000, PAGE_SIZE_4K))),
            Ok(0)
        );
    }

    #[test]
    fn register_reads_only_input_prefix_and_overwrites_reused_output() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        state
            .lock()
            .set_test_snapshots(&[snapshot(0x1000, 0x3000)])
            .unwrap();

        let mut reused_output = u64::MAX;
        assert_eq!(
            file.register_with_usercopy(
                || Ok(register_input(0x1000, 0x3000)),
                |ioctls| {
                    reused_output = ioctls;
                    Ok(())
                },
            ),
            Ok(0)
        );
        assert_eq!(reused_output, UffdIoctls::MISSING_RANGE_PROFILE.bits());
        assert_eq!(UFFD_REGISTER_INPUT_SIZE, 24);
        assert_eq!(UFFD_REGISTER_IOCTLS_OFFSET, 24);
        assert_eq!(state.lock().registrations.len(), 1);
    }

    #[test]
    fn register_copyout_fault_does_not_roll_back_committed_registration() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        state
            .lock()
            .set_test_snapshots(&[snapshot(0x2000, 0x2000)])
            .unwrap();

        assert_eq!(
            file.register_with_usercopy(
                || Ok(register_input(0, 0x4000)),
                |_| Err(AxError::BadAddress),
            ),
            Err(AxError::BadAddress)
        );
        let locked = state.lock();
        assert_eq!(locked.registrations.len(), 1);
        assert_eq!(
            locked.registrations.iter().next().unwrap().range(),
            PageRange::new(0x2000, 0x2000, PAGE_SIZE_4K).unwrap()
        );
    }

    #[test]
    fn register_accepts_a_low_leading_hole_without_registering_it() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        state
            .lock()
            .set_test_snapshots(&[snapshot(USER_SPACE_BASE, PAGE_SIZE_4K)])
            .unwrap();

        assert_eq!(
            file.register_with_usercopy(
                || Ok(register_input(0, USER_SPACE_BASE + PAGE_SIZE_4K)),
                |_| Ok(()),
            ),
            Ok(0)
        );
        let locked = state.lock();
        let registration = locked.registrations.iter().next().unwrap();
        assert_eq!(
            registration.range(),
            PageRange::new(USER_SPACE_BASE, PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap()
        );
    }

    #[test]
    fn unregister_is_address_space_wide_across_userfaultfd_ofds() {
        let _context = test_context();
        let (state, first) = new_file(true);
        let second = UserfaultFile::try_new_for_test_in_state(&state, flags(true)).unwrap();
        initialize(&first);
        initialize(&second);
        state
            .lock()
            .set_test_snapshots(&[snapshot(0x3000, 0x3000)])
            .unwrap();

        first
            .register_with_usercopy(|| Ok(register_input(0x3000, 0x3000)), |_| Ok(()))
            .unwrap();
        assert_ne!(first.binding.handler(), second.binding.handler());
        assert_eq!(
            second.unregister_with_usercopy(|| Ok(range(0x3000, 0x3000))),
            Ok(0)
        );
        assert!(state.lock().registrations.is_empty());
    }

    #[test]
    fn range_geometry_is_validated_before_old_mm_retirement() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        drop(state);

        let task_size = USER_SPACE_BASE + USER_SPACE_SIZE;
        assert_eq!(
            file.register_with_usercopy(
                || Ok(register_input(task_size, PAGE_SIZE_4K)),
                |_| panic!("copyout must not run for an invalid range"),
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.unregister_with_usercopy(|| Ok(range(task_size, PAGE_SIZE_4K))),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.register_with_usercopy(
                || Ok(register_input(1, PAGE_SIZE_4K)),
                |_| panic!("copyout must not run for an invalid range"),
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.unregister_with_usercopy(|| Ok(range(1, PAGE_SIZE_4K))),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.register_with_usercopy(
                || Ok(register_input(usize::MAX - PAGE_SIZE_4K + 1, PAGE_SIZE_4K)),
                |_| panic!("copyout must not run for an overflowing range"),
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.unregister_with_usercopy(|| {
                Ok(range(usize::MAX - PAGE_SIZE_4K + 1, PAGE_SIZE_4K))
            }),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            file.register_with_usercopy(
                || Ok(register_input(0, PAGE_SIZE_4K)),
                |_| panic!("copyout must not run for a retired address space"),
            ),
            Err(AxError::NoMemory)
        );
        assert_eq!(
            file.unregister_with_usercopy(|| Ok(range(0, PAGE_SIZE_4K))),
            Err(AxError::NoMemory)
        );
    }

    #[test]
    fn unregister_wakes_fault_completion_after_releasing_state_lock() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        let handler = file.binding.handler();
        let mapping = snapshot(0x4000, 0x2000);
        state.lock().set_test_snapshots(&[mapping]).unwrap();
        file.register_with_usercopy(|| Ok(register_input(0x4000, 0x2000)), |_| Ok(()))
            .unwrap();
        let admission = state
            .lock()
            .admit_test_request(handler, request(handler, 0x4000));

        let completion = state.lock().fault_completion_for_test();
        let probe = Arc::new(StateLockWakeProbe::new(&state));
        let waker = Waker::from(probe.clone());
        completion.register(&waker).unwrap();

        assert_eq!(
            file.unregister_with_usercopy(|| Ok(range(0x4000, 0x2000))),
            Ok(0)
        );
        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        assert!(probe.all_lock_external.load(Ordering::Relaxed));
        assert_eq!(
            state
                .lock()
                .observe_test_waiter(admission.waiter())
                .unwrap(),
            axfault::WaiterObservation::Ready(FaultDisposition::Cancelled)
        );
    }

    #[test]
    fn final_ofd_detach_wakes_fault_completion_after_releasing_state_lock() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        let handler = file.binding.handler();
        let admission = state
            .lock()
            .admit_test_request(handler, request(handler, 0x7000));
        let completion = state.lock().fault_completion_for_test();
        let probe = Arc::new(StateLockWakeProbe::new(&state));
        let waker = Waker::from(probe.clone());
        completion.register(&waker).unwrap();

        drop(file);

        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        assert!(probe.all_lock_external.load(Ordering::Relaxed));
        assert_eq!(
            state
                .lock()
                .observe_test_waiter(admission.waiter())
                .unwrap(),
            axfault::WaiterObservation::Ready(FaultDisposition::HandlerDetached)
        );
    }

    #[test]
    fn old_mm_retirement_wakes_fault_completion_waiters() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        let handler = file.binding.handler();
        state
            .lock()
            .admit_test_request(handler, request(handler, 0x8000));
        let completion = state.lock().fault_completion_for_test();
        let probe = Arc::new(StateLockWakeProbe::new(&state));
        let waker = Waker::from(probe.clone());
        completion.register(&waker).unwrap();

        drop(state);

        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        assert!(probe.all_lock_external.load(Ordering::Relaxed));
        assert_eq!(file.poll(), IoEvents::empty());
    }

    #[test]
    fn final_ofd_drop_detaches_handler() {
        let _context = test_context();
        let (state, file) = new_file(true);
        let handler = file.binding.handler();
        assert_eq!(state.lock().pending(handler), Ok(false));
        drop(file);
        assert_eq!(
            state.lock().pending(handler),
            Err(AxError::BadFileDescriptor)
        );
    }

    #[test]
    fn old_mm_retirement_leaves_non_cloexec_context_inert_not_erroneous() {
        let _context = test_context();
        let (state, file) = new_file(true);
        initialize(&file);
        drop(state);

        assert!(file.initialized());
        assert_eq!(file.poll(), IoEvents::empty());
        let mut dst = TestDst::success(UFFD_MSG_SIZE);
        assert_eq!(file.read_ready(&mut dst), Err(AxError::WouldBlock));
    }
}
