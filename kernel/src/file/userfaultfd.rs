//! Dormant Linux userfaultfd open-file-description adapter.
//!
//! The public syscall remains unavailable until registration, fault waiting,
//! resolution, lifecycle races, and cross-architecture contracts close.  This
//! module establishes only the bounded OFD/API/readiness shell.

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
};
use core::{
    mem,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex as BlockingMutex;
use bytemuck::{Pod, Zeroable, pod_read_unaligned};
use linux_raw_sys::{
    general::{UFFD_EVENT_PAGEFAULT, UFFD_PAGEFAULT_FLAG_WRITE, uffdio_api},
    ioctl::UFFDIO_API as UFFDIO_API_CMD,
};
use starry_vm::{VmMutPtr, VmPtr};
use thekernel_linux_mm::{
    FaultAccess, FaultHandlerId, MmError, UffdApiNegotiation, UffdApiState, UffdCreateFlags,
};

use crate::{
    file::{FileLike, IoDst, Kstat, anon_inode_stat},
    mm::{AddrSpace, DeliveredUffdEvent, UffdAddressSpaceState, UffdPollSet, uffd_policy_error},
    readiness::block_on_poll_io,
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
    const fn handler(&self) -> FaultHandlerId {
        self.handler
    }

    fn claim(&self) -> AxResult<Option<DeliveredUffdEvent>> {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                let Some(aspace) = aspace.upgrade() else {
                    return Ok(None);
                };
                let result = aspace.lock().claim_uffd_event(self.handler);
                result
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let Some(state) = state.upgrade() else {
                    return Ok(None);
                };
                let result = state.lock().claim_next(self.handler);
                result
            }
        }
    }

    fn pending(&self) -> AxResult<bool> {
        match &self.binding {
            UffdHandlerBinding::AddressSpace(aspace) => {
                let Some(aspace) = aspace.upgrade() else {
                    return Ok(false);
                };
                let result = aspace.lock().uffd_handler_pending(self.handler);
                result
            }
            #[cfg(test)]
            UffdHandlerBinding::Standalone(state) => {
                let Some(state) = state.upgrade() else {
                    return Ok(false);
                };
                let result = state.lock().pending(self.handler);
                result
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
                let readiness = state.lock().detach_handler(self.handler);
                match readiness {
                    Ok(readiness) => {
                        readiness.wake();
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
        let readiness = Arc::try_new(UffdPollSet::new()).map_err(|_| AxError::NoMemory)?;
        let state = Arc::try_new(BlockingMutex::new(*UffdAddressSpaceState::try_new_boxed()?))
            .map_err(|_| AxError::NoMemory)?;
        let handler = state.lock().attach_handler(readiness.clone())?;
        let binding = AttachedUffdHandler {
            binding: UffdHandlerBinding::Standalone(Arc::downgrade(&state)),
            handler,
        };
        let file = Arc::try_new(Self {
            binding,
            api: BlockingMutex::new(UffdApiState::new()),
            api_initialized: AtomicBool::new(false),
            nonblocking: AtomicBool::new(flags.nonblocking()),
            readiness,
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok((state, file))
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

    fn ioctl_api(&self, arg: usize) -> AxResult<usize> {
        let user = arg as *mut [u8; UFFD_API_SIZE];
        let request: UffdApiRaw =
            pod_read_unaligned(&(user as *const [u8; UFFD_API_SIZE]).vm_read()?);
        let cleared = UffdApiRaw::default();

        let (negotiation, response) = match self.prepare_api(request) {
            Ok(prepared) => prepared,
            Err(error) => {
                user.vm_write(bytemuck::cast(cleared))?;
                return Err(uffd_policy_error(error));
            }
        };
        // UFFDIO_API is a copyout-before-commit transaction.  An EFAULT leaves
        // the context uninitialized so userspace may retry.
        user.vm_write(bytemuck::cast(response))?;
        if let Err(error) = self.commit_api(negotiation) {
            user.vm_write(bytemuck::cast(cleared))?;
            return Err(uffd_policy_error(error));
        }
        Ok(0)
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

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        if cmd == UFFDIO_API_CMD {
            self.ioctl_api(arg)
        } else {
            // Linux v6.12 returns EINVAL both before initialization and for an
            // unsupported command after initialization.
            Err(AxError::InvalidInput)
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

    use alloc::sync::Arc;
    use std::sync::{Mutex, MutexGuard, Once};

    use axio::{IoBufMut, Write};
    use linux_raw_sys::general::{O_NONBLOCK, O_RDONLY};
    use thekernel_linux_mm::{
        FaultKey, FaultRequest, FaultType, MappingAccess, MappingKind, MappingSnapshot, UFFD_API,
        UFFD_O_NONBLOCK, UFFD_USER_MODE_ONLY,
    };

    use super::*;
    use crate::file::{FileDescription, FileHandle};

    static INIT: Once = Once::new();
    static SERIAL: Mutex<()> = Mutex::new(());

    fn test_context() -> MutexGuard<'static, ()> {
        let guard = SERIAL.lock().expect("userfaultfd test lock poisoned");
        INIT.call_once(|| axtask::init_scheduler().expect("test scheduler initialization failed"));
        guard
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

    fn request(handler: FaultHandlerId, page: usize) -> FaultRequest {
        let snapshot = MappingSnapshot::from_raw(
            1,
            2,
            1,
            page,
            4096,
            4096,
            MappingAccess::new(true, true, false).bits(),
            MappingKind::AnonymousPrivate,
            true,
            false,
        )
        .unwrap();
        FaultRequest::new(
            FaultKey::from_address(snapshot, page, FaultAccess::Read).unwrap(),
            handler,
            FaultType::Missing,
        )
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
