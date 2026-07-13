//! Typed, allocation-free security-hook dispatch.
//!
//! Security modules are admitted fallibly during boot as complete units, then
//! frozen and published exactly once before the initial credential exists.
//! Runtime dispatch only walks that immutable declaration order: it cannot
//! allocate, register, remove, or silently skip a module.

use alloc::{sync::Arc, vec::Vec};
use core::{fmt, marker::PhantomData};

use axerrno::{AxError, AxResult};
use spin::Once;
use thekernel_linux_cred::{
    AuthorizationError, commoncap_ptrace_access as external_commoncap_ptrace_access,
    commoncap_ptrace_traceme as external_commoncap_ptrace_traceme,
    commoncap_scheduler as external_commoncap_scheduler,
};
pub(crate) use thekernel_linux_cred::{
    PtraceAccessKind, PtraceCredentialKind, SchedulerSecurityOperation,
};

use super::{ExecCredentialSecurityContext, UserNamespace, exec_cred::authorize_commoncap_exec};

const SECURITY_MODULE_LIMIT: usize = 8;
const COMMONCAP_MODULE_KEY: ModuleKey = ModuleKey(0);
const NOOP_POLICY_MODULE_KEY: ModuleKey = ModuleKey(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleKey(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleId(u8);

/// Boot-time registry construction and publication failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub(crate) enum RegistryBuildError {
    NoMemory,
    Capacity,
    DuplicateModule,
    ReservedModuleKey,
    // Commoncap/noop cannot currently fail after their zero-resource init,
    // but the registry contract preserves this class for future built-ins.
    #[allow(dead_code)]
    ModuleInitFailed,
    AlreadyPublished,
}

impl fmt::Display for RegistryBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoMemory => "security registry allocation failed",
            Self::Capacity => "security module capacity exceeded",
            Self::DuplicateModule => "duplicate security module",
            Self::ReservedModuleKey => "reserved security module key",
            Self::ModuleInitFailed => "security module initialization failed",
            Self::AlreadyPublished => "security registry already published",
        })
    }
}

/// Maps policy-neutral authorization failures at the kernel adapter boundary.
pub(crate) const fn authorization_error(error: AuthorizationError) -> AxError {
    match error {
        AuthorizationError::NotPermitted => AxError::OperationNotPermitted,
        AuthorizationError::AccessDenied => AxError::PermissionDenied,
        _ => AxError::OperationNotPermitted,
    }
}

/// Opaque identity for the address-space generation checked by one hook run.
///
/// The lifetime is tied to a borrowed owning `Arc`, so a context cannot retain
/// an identity after the corresponding pinned image handle has gone away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessImageIdentity<'a> {
    pointer: *const (),
    _image: PhantomData<&'a ()>,
}

impl<'a> ProcessImageIdentity<'a> {
    fn from_arc<T>(image: &'a Arc<T>) -> Self {
        Self {
            pointer: Arc::as_ptr(image).cast(),
            _image: PhantomData,
        }
    }
}

/// Security facts for the exact process image authorized by the caller.
///
/// `owner_user_ns` is the Linux `mm->user_ns` analogue. It intentionally need
/// not equal the target credential namespace. `identity` identifies the
/// already-pinned image generation and prevents hooks from having to resample
/// mutable process state.
#[derive(Clone, Copy)]
pub(crate) struct ProcessImageSecurityRef<'a> {
    owner_user_ns: &'a Arc<UserNamespace>,
    identity: ProcessImageIdentity<'a>,
}

impl<'a> ProcessImageSecurityRef<'a> {
    pub(crate) fn new<T>(owner_user_ns: &'a Arc<UserNamespace>, image: &'a Arc<T>) -> Self {
        Self {
            owner_user_ns,
            identity: ProcessImageIdentity::from_arc(image),
        }
    }

    pub(crate) const fn owner_user_ns(self) -> &'a Arc<UserNamespace> {
        self.owner_user_ns
    }

    pub(crate) const fn identity(self) -> ProcessImageIdentity<'a> {
        self.identity
    }
}

/// External typed security contexts specialized to TheKernel's namespace and
/// exact pinned process-image token. The registry and dispatch remain local.
pub(crate) type PtraceAccessContext<'a> =
    thekernel_linux_cred::PtraceAccessContext<'a, UserNamespace, ProcessImageSecurityRef<'a>>;
pub(crate) type PtraceTracemeContext<'a> =
    thekernel_linux_cred::PtraceTracemeContext<'a, UserNamespace, ProcessImageSecurityRef<'a>>;
pub(crate) type SecuritySchedulerContext<'a> =
    thekernel_linux_cred::SchedulerSecurityContext<'a, UserNamespace>;

/// One security module owns every hook family as one registration unit.
///
/// The defaults are explicit no-ops so a module cannot be partially inserted
/// into independent per-hook registries. Boot initialization must return an
/// owned runtime object; dropping that object rolls back all module-local boot
/// resources if a later registry step fails.
trait SecurityModule: Send + Sync + 'static {
    const KEY: ModuleKey;

    fn try_boot_init() -> Result<Self, RegistryBuildError>
    where
        Self: Sized;

    fn ptrace_access(&self, _context: &PtraceAccessContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn ptrace_traceme(&self, _context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn exec_credential(&self, _context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn scheduler(&self, _context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        Ok(())
    }
}

/// Object-safe runtime view of a source-facing module. The adapter keeps the
/// compile-time key and fallible initializer out of dispatch's trait object.
trait ErasedSecurityModule: Send + Sync {
    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()>;
    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()>;
    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()>;
    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()>;
}

impl<M: SecurityModule> ErasedSecurityModule for M {
    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        SecurityModule::ptrace_access(self, context)
    }

    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        SecurityModule::ptrace_traceme(self, context)
    }

    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        SecurityModule::exec_credential(self, context)
    }

    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        SecurityModule::scheduler(self, context)
    }
}

struct CommoncapModule;

impl SecurityModule for CommoncapModule {
    const KEY: ModuleKey = COMMONCAP_MODULE_KEY;

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        Ok(Self)
    }

    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        external_commoncap_ptrace_access(context).map_err(authorization_error)
    }

    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        external_commoncap_ptrace_traceme(context).map_err(authorization_error)
    }

    /// Validates the invariants that must still hold after commoncap's exec
    /// credential algebra has produced its proposed value. Keeping this in the
    /// mandatory module prevents an allow-by-default call-site closure from
    /// becoming the effective exec policy.
    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        authorize_commoncap_exec(context)
    }

    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        external_commoncap_scheduler(context).map_err(authorization_error)
    }
}

/// A deliberately inert second module keeps stacked dispatch exercised in the
/// production shape without selecting a mandatory access-control policy.
struct NoopPolicyModule;

impl SecurityModule for NoopPolicyModule {
    const KEY: ModuleKey = NOOP_POLICY_MODULE_KEY;

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        Ok(Self)
    }
}

struct RegisteredModule {
    id: ModuleId,
    key: ModuleKey,
    module: Arc<dyn ErasedSecurityModule>,
}

struct NeedsCommoncap;
struct HasCommoncap;

/// Fallible, bounded boot builder. Only `HasCommoncap` can be frozen.
struct SecurityRegistryBuilder<State> {
    modules: Option<Vec<RegisteredModule>>,
    _state: PhantomData<State>,
}

impl SecurityRegistryBuilder<NeedsCommoncap> {
    fn try_new() -> Result<Self, RegistryBuildError> {
        Self::try_new_with_reservation(SECURITY_MODULE_LIMIT)
    }

    fn try_new_with_reservation(reservation: usize) -> Result<Self, RegistryBuildError> {
        let mut modules = Vec::new();
        modules
            .try_reserve_exact(reservation.max(SECURITY_MODULE_LIMIT))
            .map_err(|_| RegistryBuildError::NoMemory)?;
        Ok(Self {
            modules: Some(modules),
            _state: PhantomData,
        })
    }

    fn try_register_commoncap(
        self,
    ) -> Result<SecurityRegistryBuilder<HasCommoncap>, RegistryBuildError> {
        self.try_register_commoncap_with(CommoncapModule::try_boot_init)
    }

    fn try_register_commoncap_with<F>(
        mut self,
        init: F,
    ) -> Result<SecurityRegistryBuilder<HasCommoncap>, RegistryBuildError>
    where
        F: FnOnce() -> Result<CommoncapModule, RegistryBuildError>,
    {
        debug_assert!(self.modules().is_empty());
        let module = init()?;
        self.push_commoncap(module)?;
        let modules = self.modules.take();
        Ok(SecurityRegistryBuilder {
            modules,
            _state: PhantomData,
        })
    }

    fn push_commoncap(&mut self, module: CommoncapModule) -> Result<ModuleId, RegistryBuildError> {
        try_push_registered_module(&mut self.modules, module, try_allocate_security_module)
    }
}

impl SecurityRegistryBuilder<HasCommoncap> {
    fn try_register<M: SecurityModule>(&mut self) -> Result<ModuleId, RegistryBuildError> {
        self.validate_registration(M::KEY)?;
        let module = M::try_boot_init()?;
        self.push_prevalidated(module)
    }

    #[cfg(test)]
    fn try_register_initialized<M: SecurityModule>(
        &mut self,
        module: M,
    ) -> Result<ModuleId, RegistryBuildError> {
        self.validate_registration(M::KEY)?;
        self.push_prevalidated(module)
    }

    #[cfg(test)]
    fn try_register_with_allocator<M, F>(
        &mut self,
        allocate: F,
    ) -> Result<ModuleId, RegistryBuildError>
    where
        M: SecurityModule,
        F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
    {
        self.validate_registration(M::KEY)?;
        let module = M::try_boot_init()?;
        self.push_prevalidated_with(module, allocate)
    }

    fn validate_registration(&self, key: ModuleKey) -> Result<(), RegistryBuildError> {
        if key == COMMONCAP_MODULE_KEY {
            return Err(RegistryBuildError::ReservedModuleKey);
        }
        if self.modules().iter().any(|module| module.key == key) {
            return Err(RegistryBuildError::DuplicateModule);
        }
        if self.modules().len() >= SECURITY_MODULE_LIMIT {
            return Err(RegistryBuildError::Capacity);
        }
        Ok(())
    }

    fn push_prevalidated<M: SecurityModule>(
        &mut self,
        module: M,
    ) -> Result<ModuleId, RegistryBuildError> {
        self.push_prevalidated_with(module, try_allocate_security_module)
    }

    fn push_prevalidated_with<M, F>(
        &mut self,
        module: M,
        allocate: F,
    ) -> Result<ModuleId, RegistryBuildError>
    where
        M: SecurityModule,
        F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
    {
        try_push_registered_module(&mut self.modules, module, allocate)
    }

    fn freeze(mut self) -> SecurityRegistry {
        let modules = self.modules.take().expect("registry builder was consumed");
        debug_assert!(!modules.is_empty());
        debug_assert_eq!(modules[0].key, COMMONCAP_MODULE_KEY);
        SecurityRegistry { modules }
    }
}

impl<State> SecurityRegistryBuilder<State> {
    fn modules(&self) -> &[RegisteredModule] {
        self.modules
            .as_deref()
            .expect("registry builder was consumed")
    }
}

fn try_allocate_security_module<M: SecurityModule>(
    module: M,
) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError> {
    let module: Arc<dyn ErasedSecurityModule> =
        Arc::try_new(module).map_err(|_| RegistryBuildError::NoMemory)?;
    Ok(module)
}

fn try_push_registered_module<M, F>(
    modules: &mut Option<Vec<RegisteredModule>>,
    module: M,
    allocate: F,
) -> Result<ModuleId, RegistryBuildError>
where
    M: SecurityModule,
    F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
{
    let modules = modules.as_mut().expect("registry builder was consumed");
    debug_assert!(modules.len() < SECURITY_MODULE_LIMIT);
    debug_assert!(modules.len() < modules.capacity());
    let id = ModuleId(u8::try_from(modules.len()).expect("bounded module index fits u8"));
    let key = M::KEY;
    let module = allocate(module)?;
    modules.push(RegisteredModule { id, key, module });
    Ok(id)
}

impl<State> Drop for SecurityRegistryBuilder<State> {
    fn drop(&mut self) {
        if let Some(modules) = &mut self.modules {
            while modules.pop().is_some() {}
        }
    }
}

/// Immutable, allocation-free runtime dispatch table.
struct SecurityRegistry {
    modules: Vec<RegisteredModule>,
}

impl SecurityRegistry {
    fn dispatch_ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.ptrace_access(context)?;
        }
        Ok(())
    }

    fn dispatch_ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.ptrace_traceme(context)?;
        }
        Ok(())
    }

    fn dispatch_exec_credential(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.exec_credential(context)?;
        }
        Ok(())
    }

    fn dispatch_scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.scheduler(context)?;
        }
        Ok(())
    }
}

impl Drop for SecurityRegistry {
    fn drop(&mut self) {
        while self.modules.pop().is_some() {}
    }
}

struct SecurityRegistryPublication {
    registry: Once<SecurityRegistry>,
}

impl SecurityRegistryPublication {
    const fn new() -> Self {
        Self {
            registry: Once::new(),
        }
    }

    /// Serializes construction as well as publication. `spin::Once` retries
    /// after a failed initializer and never invokes a losing caller's closure
    /// after another caller succeeds. The local flag distinguishes that first
    /// success from a later call that merely observed the published value.
    fn try_publish_with<F>(&self, build: F) -> Result<&SecurityRegistry, RegistryBuildError>
    where
        F: FnOnce() -> Result<SecurityRegistry, RegistryBuildError>,
    {
        let mut initialized_here = false;
        let registry = self.registry.try_call_once(|| {
            initialized_here = true;
            build()
        })?;
        if initialized_here {
            Ok(registry)
        } else {
            Err(RegistryBuildError::AlreadyPublished)
        }
    }

    fn get(&self) -> Option<&SecurityRegistry> {
        self.registry.get()
    }
}

static SECURITY_REGISTRY: SecurityRegistryPublication = SecurityRegistryPublication::new();

#[cfg(test)]
static TEST_SECURITY_REGISTRY: Once<SecurityRegistry> = Once::new();

fn try_build_builtin_registry() -> Result<SecurityRegistry, RegistryBuildError> {
    let mut builder = SecurityRegistryBuilder::try_new()?.try_register_commoncap()?;
    builder.try_register::<NoopPolicyModule>()?;
    Ok(builder.freeze())
}

/// Builds, freezes, and publishes the complete registry before userspace.
pub(crate) fn init() -> Result<(), RegistryBuildError> {
    SECURITY_REGISTRY.try_publish_with(try_build_builtin_registry)?;
    Ok(())
}

fn require_published_registry(registry: Option<&SecurityRegistry>) -> AxResult<&SecurityRegistry> {
    registry.ok_or(AxError::OperationNotPermitted)
}

fn registry_for_dispatch() -> AxResult<&'static SecurityRegistry> {
    match require_published_registry(SECURITY_REGISTRY.get()) {
        Ok(registry) => return Ok(registry),
        Err(error) => {
            #[cfg(not(test))]
            return Err(error);

            #[cfg(test)]
            let _ = error;
        }
    }

    // Host unit tests do not execute `entry::init`; keep their policy complete
    // without weakening the non-test boot contract or the one-shot publisher.
    #[cfg(test)]
    {
        return Ok(TEST_SECURITY_REGISTRY
            .call_once(|| try_build_builtin_registry().expect("failed to build test registry")));
    }
}

/// Runs the frozen ptrace access hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_access(context: &PtraceAccessContext<'_>) -> AxResult<()> {
    registry_for_dispatch()?.dispatch_ptrace_access(context)
}

/// Runs the frozen traceme hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_traceme(context: &PtraceTracemeContext<'_>) -> AxResult<()> {
    registry_for_dispatch()?.dispatch_ptrace_traceme(context)
}

/// Runs the frozen exec-credential hooks in declaration order.
/// The first denial aborts the still-unpublished prepared credential.
pub(crate) fn dispatch_exec_credential(
    context: &ExecCredentialSecurityContext<'_>,
) -> AxResult<()> {
    registry_for_dispatch()?.dispatch_exec_credential(context)
}

/// Runs the frozen scheduler hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_scheduler(context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
    registry_for_dispatch()?.dispatch_scheduler(context)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::{AtomicU32, Ordering};
    use std::{sync::Barrier, thread};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_SYS_NICE, CAP_SYS_PTRACE};

    use super::*;
    use crate::task::{
        CapabilityState, Cred, CredentialSlot, Credentials, Kgid, Kuid, creds::CAPABILITY_WORDS,
    };

    static ORDER_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static TRACEME_DIRECTION: AtomicU32 = AtomicU32::new(0);
    static TRACEME_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static EXEC_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static SCHEDULER_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static WHOLE_MODULE_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static MODULE_DROP_TRACE: AtomicU32 = AtomicU32::new(0);
    static RESERVED_MODULE_INIT_TRACE: AtomicU32 = AtomicU32::new(0);

    type TestPtraceAccessHook = for<'a> fn(&PtraceAccessContext<'a>) -> AxResult<()>;
    type TestPtraceTracemeHook = for<'a> fn(&PtraceTracemeContext<'a>) -> AxResult<()>;
    type TestExecCredentialHook = for<'a> fn(&ExecCredentialSecurityContext<'a>) -> AxResult<()>;
    type TestSchedulerHook = for<'a> fn(&SecuritySchedulerContext<'a>) -> AxResult<()>;

    struct TestSecurityModule<const KEY: u64> {
        ptrace_access: Option<TestPtraceAccessHook>,
        ptrace_traceme: Option<TestPtraceTracemeHook>,
        exec_credential: Option<TestExecCredentialHook>,
        scheduler: Option<TestSchedulerHook>,
    }

    impl<const KEY: u64> TestSecurityModule<KEY> {
        const fn empty() -> Self {
            Self {
                ptrace_access: None,
                ptrace_traceme: None,
                exec_credential: None,
                scheduler: None,
            }
        }
    }

    impl<const KEY: u64> SecurityModule for TestSecurityModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Ok(Self::empty())
        }

        fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
            self.ptrace_access.map_or(Ok(()), |hook| hook(context))
        }

        fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
            self.ptrace_traceme.map_or(Ok(()), |hook| hook(context))
        }

        fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
            self.exec_credential.map_or(Ok(()), |hook| hook(context))
        }

        fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
            self.scheduler.map_or(Ok(()), |hook| hook(context))
        }
    }

    struct FailingModule<const KEY: u64>;

    impl<const KEY: u64> SecurityModule for FailingModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Err(RegistryBuildError::ModuleInitFailed)
        }
    }

    struct WholeHookModule;

    impl SecurityModule for WholeHookModule {
        const KEY: ModuleKey = ModuleKey(10);

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Ok(Self)
        }

        fn ptrace_access(&self, _context: &PtraceAccessContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_traceme(&self, _context: &PtraceTracemeContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn exec_credential(&self, _context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
            Ok(())
        }

        fn scheduler(&self, _context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 24, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FailingWholeHookModule;

    impl SecurityModule for FailingWholeHookModule {
        const KEY: ModuleKey = ModuleKey(11);

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Err(RegistryBuildError::ModuleInitFailed)
        }

        fn ptrace_access(&self, _context: &PtraceAccessContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_traceme(&self, _context: &PtraceTracemeContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn exec_credential(&self, _context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
            Ok(())
        }

        fn scheduler(&self, _context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 24, Ordering::SeqCst);
            Ok(())
        }
    }

    struct ReservedKeyModule;

    impl SecurityModule for ReservedKeyModule {
        const KEY: ModuleKey = COMMONCAP_MODULE_KEY;

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            RESERVED_MODULE_INIT_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(Self)
        }
    }

    struct DroppingModule<const KEY: u64>;

    impl<const KEY: u64> SecurityModule for DroppingModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Ok(Self)
        }
    }

    impl<const KEY: u64> Drop for DroppingModule<KEY> {
        fn drop(&mut self) {
            let key = u32::try_from(KEY).expect("test key fits u32");
            MODULE_DROP_TRACE
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |trace| {
                    Some(trace * 10 + key)
                })
                .unwrap();
        }
    }

    fn test_registry_builder() -> SecurityRegistryBuilder<HasCommoncap> {
        SecurityRegistryBuilder::try_new()
            .unwrap()
            .try_register_commoncap()
            .unwrap()
    }

    fn dispatch_all_hook_families(registry: &SecurityRegistry) {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let access = PtraceAccessContext::new(
            &root,
            &root,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let traceme =
            PtraceTracemeContext::new(&root, &root, image_ref.owner_user_ns(), &image_ref);
        let proposal = exec_proposal(&root, crate::task::ExecTraceState::NotSuppressingPrivilege);
        let exec = ExecCredentialSecurityContext::new(&proposal);
        let scheduler = scheduler_context(&root, &root, SchedulerSecurityOperation::SetAffinity);

        registry.dispatch_ptrace_access(&access).unwrap();
        registry.dispatch_ptrace_traceme(&traceme).unwrap();
        registry.dispatch_exec_credential(&exec).unwrap();
        registry.dispatch_scheduler(&scheduler).unwrap();
    }

    fn capability_set(capabilities: &[u32]) -> [u32; CAPABILITY_WORDS] {
        let mut result = [0; CAPABILITY_WORDS];
        for capability in capabilities {
            let (word, mask) = CapabilityState::cap_mask(*capability).unwrap();
            result[word] |= mask;
        }
        result
    }

    fn credential_with_caps(base: &Arc<Cred>, permitted: &[u32], effective: &[u32]) -> Arc<Cred> {
        let slot = CredentialSlot::new(base.clone());
        let mut update = slot.prepare();
        update.builder.caps.permitted = capability_set(permitted);
        update.builder.caps.effective = capability_set(effective);
        update.builder.caps.inheritable = [0; CAPABILITY_WORDS];
        update.builder.caps.ambient = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit()
    }

    fn credential_with_identity_and_caps(
        base: &Arc<Cred>,
        uid: u32,
        permitted: &[u32],
        effective: &[u32],
    ) -> Arc<Cred> {
        let slot = CredentialSlot::new(base.clone());
        let mut update = slot.prepare();
        let gid = Kgid::from_raw(uid).unwrap();
        let uid = Kuid::from_raw(uid).unwrap();
        update.builder.ids = Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        };
        update.builder.caps.permitted = capability_set(permitted);
        update.builder.caps.effective = capability_set(effective);
        update.builder.caps.inheritable = [0; CAPABILITY_WORDS];
        update.builder.caps.ambient = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit()
    }

    fn scheduler_context<'a>(
        actor: &'a Cred,
        target: &'a Cred,
        operation: SchedulerSecurityOperation,
    ) -> SecuritySchedulerContext<'a> {
        SecuritySchedulerContext::new(actor, target, operation)
    }

    fn access_context<'a>(
        actor: &'a Cred,
        target: &'a Cred,
        image: &'a ProcessImageSecurityRef<'a>,
        credential_kind: PtraceCredentialKind,
    ) -> PtraceAccessContext<'a> {
        PtraceAccessContext::new(
            actor,
            target,
            image.owner_user_ns(),
            image,
            PtraceAccessKind::Attach,
            credential_kind,
        )
    }

    fn ordered_first(context: &PtraceAccessContext<'_>) -> AxResult<()> {
        assert_eq!(context.access_kind(), PtraceAccessKind::Read);
        assert_eq!(ORDER_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_second(_: &PtraceAccessContext<'_>) -> AxResult<()> {
        assert_eq!(ORDER_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_first(_: &PtraceAccessContext<'_>) -> AxResult<()> {
        DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn must_not_run(_: &PtraceAccessContext<'_>) -> AxResult<()> {
        DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn record_traceme_direction(context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        let parent = context.parent_actor().ids().euid;
        let child = context.child_target().ids().euid;
        if parent == Kuid::INITIAL_ROOT && child == Kuid::from_raw(1000).unwrap() {
            TRACEME_DIRECTION.store(1, Ordering::SeqCst);
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
        }
    }

    fn deny_traceme_first(_: &PtraceTracemeContext<'_>) -> AxResult<()> {
        TRACEME_DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn traceme_must_not_run(_: &PtraceTracemeContext<'_>) -> AxResult<()> {
        TRACEME_DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn deny_exec_first(_: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        EXEC_DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn exec_must_not_run(_: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        EXEC_DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn deny_scheduler_first(_: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        SCHEDULER_DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn scheduler_must_not_run(_: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        SCHEDULER_DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn exec_proposal(
        credential: &Arc<Cred>,
        trace_state: crate::task::ExecTraceState,
    ) -> thekernel_linux_cred::ExecCredentialProposal<UserNamespace> {
        let input = crate::task::ExecCredentialInput::new(
            0,
            Some(crate::task::ExecFileOwner::new(
                Kuid::INITIAL_ROOT,
                Kgid::INITIAL_ROOT,
            )),
            crate::task::ExecMountPrivilege::Honor,
            trace_state,
            crate::task::ExecImageReadability::Readable,
            None,
        );
        thekernel_linux_cred::derive_exec_credential(credential, input).unwrap()
    }

    #[test]
    fn registry_builder_reports_reservation_failure() {
        assert!(matches!(
            SecurityRegistryBuilder::<NeedsCommoncap>::try_new_with_reservation(usize::MAX),
            Err(RegistryBuildError::NoMemory)
        ));
    }

    #[test]
    fn registry_builder_requires_and_preserves_commoncap_first() {
        let mut builder = test_registry_builder();
        assert_eq!(builder.modules().len(), 1);
        assert_eq!(builder.modules()[0].id, ModuleId(0));
        assert_eq!(builder.modules()[0].key, COMMONCAP_MODULE_KEY);

        assert_eq!(
            builder.try_register::<TestSecurityModule<2>>().unwrap(),
            ModuleId(1)
        );
        builder.try_register::<TestSecurityModule<3>>().unwrap();
        let allocation = builder.modules().as_ptr();
        let capacity = builder
            .modules
            .as_ref()
            .expect("builder is live")
            .capacity();
        let registry = builder.freeze();

        assert_eq!(registry.modules.as_ptr(), allocation);
        assert_eq!(registry.modules.capacity(), capacity);
        assert_eq!(
            registry
                .modules
                .iter()
                .map(|module| module.key)
                .collect::<Vec<_>>(),
            [COMMONCAP_MODULE_KEY, ModuleKey(2), ModuleKey(3)]
        );
    }

    #[test]
    fn registry_builder_enforces_total_capacity() {
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        builder.try_register::<TestSecurityModule<3>>().unwrap();
        builder.try_register::<TestSecurityModule<4>>().unwrap();
        builder.try_register::<TestSecurityModule<5>>().unwrap();
        builder.try_register::<TestSecurityModule<6>>().unwrap();
        builder.try_register::<TestSecurityModule<7>>().unwrap();
        assert_eq!(builder.modules().len(), 7);

        assert_eq!(
            builder.try_register::<TestSecurityModule<8>>().unwrap(),
            ModuleId(7)
        );
        assert_eq!(builder.modules().len(), SECURITY_MODULE_LIMIT);
        assert_eq!(
            builder.try_register::<TestSecurityModule<9>>(),
            Err(RegistryBuildError::Capacity)
        );
        assert_eq!(builder.modules().len(), SECURITY_MODULE_LIMIT);
    }

    #[test]
    fn registry_registration_rejects_duplicate_and_reserved_keys_before_init() {
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        let original_len = builder.modules().len();

        assert_eq!(
            builder.try_register::<TestSecurityModule<2>>(),
            Err(RegistryBuildError::DuplicateModule)
        );
        assert_eq!(builder.modules().len(), original_len);

        RESERVED_MODULE_INIT_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            builder.try_register::<ReservedKeyModule>(),
            Err(RegistryBuildError::ReservedModuleKey)
        );
        assert_eq!(RESERVED_MODULE_INIT_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(builder.modules().len(), original_len);
    }

    #[test]
    fn registry_module_init_failure_leaves_builder_unchanged() {
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        let original = builder
            .modules()
            .iter()
            .map(|module| (module.id, module.key))
            .collect::<Vec<_>>();

        assert_eq!(
            builder.try_register::<FailingModule<3>>(),
            Err(RegistryBuildError::ModuleInitFailed)
        );
        assert_eq!(
            builder
                .modules()
                .iter()
                .map(|module| (module.id, module.key))
                .collect::<Vec<_>>(),
            original
        );
    }

    #[test]
    fn registry_module_allocation_failure_drops_candidate_without_mutation() {
        MODULE_DROP_TRACE.store(0, Ordering::SeqCst);
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        let original = builder
            .modules()
            .iter()
            .map(|module| (module.id, module.key))
            .collect::<Vec<_>>();

        assert_eq!(
            builder.try_register_with_allocator::<DroppingModule<4>, _>(|module| {
                drop(module);
                Err(RegistryBuildError::NoMemory)
            }),
            Err(RegistryBuildError::NoMemory)
        );
        assert_eq!(MODULE_DROP_TRACE.load(Ordering::SeqCst), 4);
        assert_eq!(
            builder
                .modules()
                .iter()
                .map(|module| (module.id, module.key))
                .collect::<Vec<_>>(),
            original
        );
    }

    #[test]
    fn registry_build_rollback_drops_initialized_modules_in_reverse_order() {
        MODULE_DROP_TRACE.store(0, Ordering::SeqCst);
        {
            let mut builder = test_registry_builder();
            builder.try_register::<DroppingModule<2>>().unwrap();
            builder.try_register::<DroppingModule<3>>().unwrap();
            assert_eq!(
                builder.try_register::<FailingModule<4>>(),
                Err(RegistryBuildError::ModuleInitFailed)
            );
        }
        assert_eq!(MODULE_DROP_TRACE.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn commoncap_init_failure_cannot_produce_a_freezable_registry() {
        let builder = SecurityRegistryBuilder::<NeedsCommoncap>::try_new().unwrap();
        assert!(matches!(
            builder.try_register_commoncap_with(|| Err(RegistryBuildError::ModuleInitFailed)),
            Err(RegistryBuildError::ModuleInitFailed)
        ));
    }

    #[test]
    fn frozen_registry_publication_is_one_shot() {
        let publication = SecurityRegistryPublication::new();
        let builds = AtomicU32::new(0);
        assert!(publication.get().is_none());
        assert!(matches!(
            require_published_registry(publication.get()),
            Err(AxError::OperationNotPermitted)
        ));

        let first = publication.try_publish_with(|| {
            builds.fetch_add(1, Ordering::SeqCst);
            Err(RegistryBuildError::ModuleInitFailed)
        });
        assert!(matches!(first, Err(RegistryBuildError::ModuleInitFailed)));
        assert!(publication.get().is_none());

        let first = publication
            .try_publish_with(|| {
                builds.fetch_add(1, Ordering::SeqCst);
                try_build_builtin_registry()
            })
            .unwrap();
        assert!(require_published_registry(publication.get()).is_ok());
        assert!(core::ptr::eq(publication.get().unwrap(), first));
        assert!(matches!(
            publication.try_publish_with(|| {
                builds.fetch_add(1, Ordering::SeqCst);
                try_build_builtin_registry()
            }),
            Err(RegistryBuildError::AlreadyPublished)
        ));
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert!(core::ptr::eq(publication.get().unwrap(), first));
    }

    #[test]
    fn concurrent_registry_publishers_run_exactly_one_builder() {
        let publication = Arc::new(SecurityRegistryPublication::new());
        let builds = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(3));
        let mut publishers = Vec::new();

        for _ in 0..2 {
            let publication = publication.clone();
            let builds = builds.clone();
            let barrier = barrier.clone();
            publishers.push(thread::spawn(move || {
                barrier.wait();
                match publication.try_publish_with(|| {
                    builds.fetch_add(1, Ordering::SeqCst);
                    try_build_builtin_registry()
                }) {
                    Ok(_) => true,
                    Err(RegistryBuildError::AlreadyPublished) => false,
                    Err(error) => panic!("unexpected publication error: {error}"),
                }
            }));
        }

        barrier.wait();
        let winners = publishers
            .into_iter()
            .map(|publisher| publisher.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(publication.get().is_some());
    }

    #[test]
    fn whole_module_registration_is_atomic_across_every_hook_family() {
        let mut builder = test_registry_builder();
        builder.try_register::<WholeHookModule>().unwrap();
        let registry = builder.freeze();

        WHOLE_MODULE_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_all_hook_families(&registry);
        assert_eq!(WHOLE_MODULE_HOOK_TRACE.load(Ordering::SeqCst), 0x0101_0101);

        let mut builder = test_registry_builder();
        assert_eq!(
            builder.try_register::<FailingWholeHookModule>(),
            Err(RegistryBuildError::ModuleInitFailed)
        );
        let registry = builder.freeze();

        WHOLE_MODULE_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_all_hook_families(&registry);
        assert_eq!(WHOLE_MODULE_HOOK_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn security_hook_stack_runs_in_declaration_order() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context = PtraceAccessContext::new(
            &credential,
            &credential,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_access: Some(ordered_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                ptrace_access: Some(ordered_second),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        ORDER_HOOK_TRACE.store(0, Ordering::SeqCst);
        registry.dispatch_ptrace_access(&context).unwrap();
        assert_eq!(ORDER_HOOK_TRACE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn security_hook_stack_short_circuits_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context = access_context(
            &credential,
            &credential,
            &image_ref,
            PtraceCredentialKind::Real,
        );
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_access: Some(deny_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                ptrace_access: Some(must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_ptrace_access(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exec_security_hook_stack_short_circuits_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace).unwrap();
        let proposal = exec_proposal(
            &credential,
            crate::task::ExecTraceState::NotSuppressingPrivilege,
        );
        let context = ExecCredentialSecurityContext::new(&proposal);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                exec_credential: Some(deny_exec_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                exec_credential: Some(exec_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        EXEC_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_exec_credential(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(EXEC_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn traceme_security_hook_stack_short_circuits_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context =
            PtraceTracemeContext::new(&root, &root, image_ref.owner_user_ns(), &image_ref);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_traceme: Some(deny_traceme_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                ptrace_traceme: Some(traceme_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        TRACEME_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_ptrace_traceme(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(TRACEME_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn credential_caller_production_exec_commoncap_accepts_valid_external_proposal() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let unprivileged = credential_with_caps(&root, &[], &[]);
        let proposal = exec_proposal(
            &unprivileged,
            crate::task::ExecTraceState::SuppressingPrivilege,
        );
        let context = ExecCredentialSecurityContext::new(&proposal);

        dispatch_exec_credential(&context).unwrap();
    }

    #[test]
    fn commoncap_selects_effective_caps_for_fs_and_permitted_for_real() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace.clone()).unwrap();
        let actor = credential_with_caps(&root, &[CAP_CHOWN], &[]);
        let target = credential_with_caps(&root, &[CAP_CHOWN], &[]);
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);

        dispatch_ptrace_access(&access_context(
            &actor,
            &target,
            &image_ref,
            PtraceCredentialKind::Real,
        ))
        .unwrap();
        assert_eq!(
            dispatch_ptrace_access(&access_context(
                &actor,
                &target,
                &image_ref,
                PtraceCredentialKind::Fs,
            )),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn traceme_treats_parent_as_actor_and_child_as_target() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace.clone()).unwrap();
        let parent = credential_with_caps(&root, &[], &[]);
        let child_slot = CredentialSlot::new(credential_with_caps(&root, &[CAP_CHOWN], &[]));
        let mut child_update = child_slot.prepare();
        let child_uid = Kuid::from_raw(1000).unwrap();
        let child_gid = Kgid::from_raw(1000).unwrap();
        child_update.builder.ids.ruid = child_uid;
        child_update.builder.ids.euid = child_uid;
        child_update.builder.ids.suid = child_uid;
        child_update.builder.ids.fsuid = child_uid;
        child_update.builder.ids.rgid = child_gid;
        child_update.builder.ids.egid = child_gid;
        child_update.builder.ids.sgid = child_gid;
        child_update.builder.ids.fsgid = child_gid;
        let child = child_update.finish().unwrap().commit();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context =
            PtraceTracemeContext::new(&parent, &child, image_ref.owner_user_ns(), &image_ref);

        // Reversing actor and target would incorrectly allow this relation:
        // the child's CAP_CHOWN set contains the empty parent set.
        assert_eq!(
            dispatch_ptrace_traceme(&context),
            Err(AxError::OperationNotPermitted)
        );

        let allowed_context =
            PtraceTracemeContext::new(&root, &child, image_ref.owner_user_ns(), &image_ref);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_traceme: Some(record_traceme_direction),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        TRACEME_DIRECTION.store(0, Ordering::SeqCst);
        registry.dispatch_ptrace_traceme(&allowed_context).unwrap();
        assert_eq!(TRACEME_DIRECTION.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn commoncap_honors_namespaced_cap_sys_ptrace() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(
                Kuid::from_raw(1000).unwrap(),
                Kgid::from_raw(1000).unwrap(),
                false,
            )
            .unwrap();
        let target_parent = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = Cred::try_with_user_ns(&target_parent, child_namespace.clone()).unwrap();
        let actor = credential_with_caps(&root, &[CAP_SYS_PTRACE], &[CAP_SYS_PTRACE]);
        let unprivileged_actor = credential_with_caps(&root, &[CAP_SYS_PTRACE], &[]);
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&child_namespace, &image);

        dispatch_ptrace_access(&access_context(
            &actor,
            &target,
            &image_ref,
            PtraceCredentialKind::Real,
        ))
        .unwrap();
        assert_eq!(
            dispatch_ptrace_access(&access_context(
                &unprivileged_actor,
                &target,
                &image_ref,
                PtraceCredentialKind::Real,
            )),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn image_security_ref_keeps_mm_owner_distinct_from_credential_namespace() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let target = Cred::try_with_user_ns(&root, child_namespace).unwrap();
        let first_image = Arc::new(());
        let second_image = Arc::new(());
        let first = ProcessImageSecurityRef::new(&root_namespace, &first_image);
        let second = ProcessImageSecurityRef::new(&root_namespace, &second_image);

        assert!(Arc::ptr_eq(first.owner_user_ns(), &root_namespace));
        assert!(!Arc::ptr_eq(first.owner_user_ns(), target.user_ns()));
        assert_ne!(first.identity(), second.identity());
    }

    #[test]
    fn credential_caller_scheduler_child_cannot_administer_ancestor() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(
                Kuid::from_raw(1000).unwrap(),
                Kgid::from_raw(1000).unwrap(),
                false,
            )
            .unwrap();
        let child_parent = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let child_root = Cred::try_with_user_ns(&child_parent, child_namespace).unwrap();
        let actor =
            credential_with_identity_and_caps(&child_root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);

        for operation in [
            SchedulerSecurityOperation::SetAffinity,
            SchedulerSecurityOperation::SetParam { realtime: false },
        ] {
            assert_eq!(
                dispatch_scheduler(&scheduler_context(&actor, &root, operation)),
                Err(AxError::OperationNotPermitted)
            );
        }
    }

    #[test]
    fn credential_caller_scheduler_capable_ancestor_administers_child() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child_root = Cred::try_with_user_ns(&actor, child_namespace).unwrap();
        let target = credential_with_identity_and_caps(&child_root, 1000, &[], &[]);

        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetParam { realtime: true },
        ))
        .unwrap();
    }

    #[test]
    fn credential_caller_scheduler_uid_zero_with_dropped_cap_cannot_enter_rt() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let dropped = credential_with_caps(&root, &[], &[]);

        assert_eq!(
            dispatch_scheduler(&scheduler_context(
                &dropped,
                &root,
                SchedulerSecurityOperation::SetPolicy { realtime: true },
            )),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn credential_caller_scheduler_nonroot_capability_crosses_owner_boundary() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let actor =
            credential_with_identity_and_caps(&root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);
        let target = credential_with_identity_and_caps(&root, 2000, &[], &[]);

        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetNice {
                current_nice: 0,
                requested_nice: -20,
                rlimit_nice: 0,
            },
        ))
        .unwrap();
    }

    #[test]
    fn credential_caller_scheduler_nice_uses_owner_and_frozen_rlimit() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 1000, &[], &[]);

        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetNice {
                current_nice: 0,
                requested_nice: -5,
                rlimit_nice: 25,
            },
        ))
        .unwrap();
        assert_eq!(
            dispatch_scheduler(&scheduler_context(
                &actor,
                &target,
                SchedulerSecurityOperation::SetNice {
                    current_nice: 0,
                    requested_nice: -5,
                    rlimit_nice: 24,
                },
            )),
            Err(AxError::PermissionDenied)
        );
        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetNice {
                current_nice: 0,
                requested_nice: 5,
                rlimit_nice: 0,
            },
        ))
        .unwrap();
    }

    #[test]
    fn credential_caller_scheduler_context_keeps_exact_snapshots() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let old_actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let old_target = credential_with_identity_and_caps(&root, 2000, &[], &[]);
        let actor_slot = CredentialSlot::new(old_actor.clone());
        let target_slot = CredentialSlot::new(old_target.clone());
        let context = scheduler_context(
            &old_actor,
            &old_target,
            SchedulerSecurityOperation::SetAffinity,
        );

        let mut actor_update = actor_slot.prepare();
        actor_update.builder.caps.permitted = capability_set(&[CAP_SYS_NICE]);
        actor_update.builder.caps.effective = capability_set(&[CAP_SYS_NICE]);
        actor_update.finish().unwrap().commit();

        let mut target_update = target_slot.prepare();
        let actor_uid = Kuid::from_raw(1000).unwrap();
        target_update.builder.ids.ruid = actor_uid;
        target_update.builder.ids.euid = actor_uid;
        target_update.finish().unwrap().commit();

        assert_eq!(context.actor().ids().euid, Kuid::from_raw(1000).unwrap());
        assert_eq!(context.target().ids().euid, Kuid::from_raw(2000).unwrap());
        assert!(!context.owner_match());
        assert_eq!(
            dispatch_scheduler(&context),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn credential_caller_scheduler_hooks_stop_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let context = scheduler_context(&root, &root, SchedulerSecurityOperation::SetAffinity);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                scheduler: Some(deny_scheduler_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                scheduler: Some(scheduler_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        SCHEDULER_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_scheduler(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(SCHEDULER_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn authorization_errors_map_to_linux_errno_classes() {
        assert_eq!(
            authorization_error(AuthorizationError::NotPermitted),
            AxError::OperationNotPermitted
        );
        assert_eq!(
            authorization_error(AuthorizationError::AccessDenied),
            AxError::PermissionDenied
        );
    }
}
