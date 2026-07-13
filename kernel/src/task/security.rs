//! Typed, allocation-free security-hook dispatch.
//!
//! The hook registries in this module are deliberately static: extending a
//! stack is an explicit source change, its maximum size is checked while the
//! registry is built, and dispatch cannot allocate or silently skip a hook.

use alloc::sync::Arc;
use core::marker::PhantomData;

use axerrno::{AxError, AxResult};
use thekernel_linux_cred::{
    AuthorizationError, commoncap_ptrace_access as external_commoncap_ptrace_access,
    commoncap_ptrace_traceme as external_commoncap_ptrace_traceme,
    commoncap_scheduler as external_commoncap_scheduler,
};
pub(crate) use thekernel_linux_cred::{
    PtraceAccessKind, PtraceCredentialKind, SchedulerSecurityOperation,
};

use super::{ExecCredentialSecurityContext, UserNamespace, exec_cred::authorize_commoncap_exec};

const SECURITY_HOOK_LIMIT: usize = 8;

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

type PtraceAccessHook = for<'a> fn(&PtraceAccessContext<'a>) -> AxResult<()>;
type PtraceTracemeHook = for<'a> fn(&PtraceTracemeContext<'a>) -> AxResult<()>;
type ExecCredentialHook = for<'a> fn(&ExecCredentialSecurityContext<'a>) -> AxResult<()>;
type SchedulerHook = for<'a> fn(&SecuritySchedulerContext<'a>) -> AxResult<()>;

/// Compile-time-sized hook registry. There is no runtime registration path.
struct StaticHookRegistry<H, const N: usize> {
    hooks: [H; N],
}

impl<H, const N: usize> StaticHookRegistry<H, N> {
    const fn new(hooks: [H; N]) -> Self {
        assert!(N <= SECURITY_HOOK_LIMIT);
        Self { hooks }
    }
}

impl<const N: usize> StaticHookRegistry<PtraceAccessHook, N> {
    fn dispatch(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        for hook in &self.hooks {
            hook(context)?;
        }
        Ok(())
    }
}

impl<const N: usize> StaticHookRegistry<PtraceTracemeHook, N> {
    fn dispatch(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        for hook in &self.hooks {
            hook(context)?;
        }
        Ok(())
    }
}

impl<const N: usize> StaticHookRegistry<ExecCredentialHook, N> {
    fn dispatch(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        for hook in &self.hooks {
            hook(context)?;
        }
        Ok(())
    }
}

impl<const N: usize> StaticHookRegistry<SchedulerHook, N> {
    fn dispatch(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        for hook in &self.hooks {
            hook(context)?;
        }
        Ok(())
    }
}

fn commoncap_ptrace_access(context: &PtraceAccessContext<'_>) -> AxResult<()> {
    external_commoncap_ptrace_access(context).map_err(authorization_error)
}

fn commoncap_ptrace_traceme(context: &PtraceTracemeContext<'_>) -> AxResult<()> {
    external_commoncap_ptrace_traceme(context).map_err(authorization_error)
}

/// Validates the invariants that must still hold after commoncap's exec
/// credential algebra has produced its proposed value. Keeping this in the
/// production registry prevents an allow-by-default closure at the exec call
/// site from becoming the effective policy.
fn commoncap_exec_credential(context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
    authorize_commoncap_exec(context)
}

fn commoncap_scheduler(context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
    external_commoncap_scheduler(context).map_err(authorization_error)
}

const PTRACE_ACCESS_HOOKS: StaticHookRegistry<PtraceAccessHook, 1> =
    StaticHookRegistry::new([commoncap_ptrace_access]);
const PTRACE_TRACEME_HOOKS: StaticHookRegistry<PtraceTracemeHook, 1> =
    StaticHookRegistry::new([commoncap_ptrace_traceme]);
const EXEC_CREDENTIAL_HOOKS: StaticHookRegistry<ExecCredentialHook, 1> =
    StaticHookRegistry::new([commoncap_exec_credential]);
const SCHEDULER_HOOKS: StaticHookRegistry<SchedulerHook, 1> =
    StaticHookRegistry::new([commoncap_scheduler]);

/// Runs the statically registered ptrace access hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_access(context: &PtraceAccessContext<'_>) -> AxResult<()> {
    PTRACE_ACCESS_HOOKS.dispatch(context)
}

/// Runs the statically registered traceme hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_traceme(context: &PtraceTracemeContext<'_>) -> AxResult<()> {
    PTRACE_TRACEME_HOOKS.dispatch(context)
}

/// Runs the statically registered exec-credential hooks in declaration order.
/// The first denial aborts the still-unpublished prepared credential.
pub(crate) fn dispatch_exec_credential(
    context: &ExecCredentialSecurityContext<'_>,
) -> AxResult<()> {
    EXEC_CREDENTIAL_HOOKS.dispatch(context)
}

/// Runs the statically registered scheduler hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_scheduler(context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
    SCHEDULER_HOOKS.dispatch(context)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::{AtomicU32, Ordering};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_SYS_NICE, CAP_SYS_PTRACE};

    use super::*;
    use crate::task::{
        CapabilityState, Cred, CredentialSlot, Credentials, Kgid, Kuid, creds::CAPABILITY_WORDS,
    };

    static ORDER_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static TRACEME_DIRECTION: AtomicU32 = AtomicU32::new(0);
    static EXEC_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static SCHEDULER_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);

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
        let hooks = StaticHookRegistry::new([
            ordered_first as PtraceAccessHook,
            ordered_second as PtraceAccessHook,
        ]);

        ORDER_HOOK_TRACE.store(0, Ordering::SeqCst);
        hooks.dispatch(&context).unwrap();
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
        let hooks = StaticHookRegistry::new([
            deny_first as PtraceAccessHook,
            must_not_run as PtraceAccessHook,
        ]);

        DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(hooks.dispatch(&context), Err(AxError::PermissionDenied));
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
        let hooks = StaticHookRegistry::new([
            deny_exec_first as ExecCredentialHook,
            exec_must_not_run as ExecCredentialHook,
        ]);

        EXEC_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(hooks.dispatch(&context), Err(AxError::PermissionDenied));
        assert_eq!(EXEC_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn production_exec_commoncap_accepts_valid_external_proposal() {
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

        let direction_hook =
            StaticHookRegistry::new([record_traceme_direction as PtraceTracemeHook]);
        TRACEME_DIRECTION.store(0, Ordering::SeqCst);
        direction_hook.dispatch(&context).unwrap();
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
        let hooks = StaticHookRegistry::new([
            deny_scheduler_first as SchedulerHook,
            scheduler_must_not_run as SchedulerHook,
        ]);

        SCHEDULER_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(hooks.dispatch(&context), Err(AxError::PermissionDenied));
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
