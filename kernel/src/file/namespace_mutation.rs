use alloc::string::String;
use core::marker::PhantomData;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    CreateDisposition, DeviceId, Location, NamedCreateOptions, NamespaceGeneration, NodePermission,
    NodeType,
};
use linux_vfs::{MutationBackend, MutationTransaction};

use super::permission::{
    check_create_permissions, check_remove_permissions, check_rename_permissions,
    initial_named_create_owner_mode,
};
use crate::{mounts::NamespaceOperationGuard, task::DacCredentialView};

const GENERATION_SNAPSHOT_RETRIES: usize = 4;

fn try_owned(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

fn lookup_optional(parent: &Location, name: &str) -> AxResult<Option<Location>> {
    match parent.lookup_no_follow(name) {
        Ok(location) => Ok(Some(location)),
        Err(AxError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn stable_lookup(
    parent: &Location,
    name: &str,
) -> AxResult<(Option<Location>, NamespaceGeneration)> {
    for _ in 0..GENERATION_SNAPSHOT_RETRIES {
        let before = parent.namespace_generation()?;
        let current = lookup_optional(parent, name)?;
        let after = parent.namespace_generation()?;
        if before == after {
            return Ok((current, after));
        }
    }
    Err(AxError::ResourceBusy)
}

fn validate_expected_identity(
    current: Option<&Location>,
    expected: Option<&Location>,
) -> AxResult<()> {
    match (current, expected) {
        (None, None) => Ok(()),
        (Some(current), Some(expected)) if current.same_node(expected) => Ok(()),
        (Some(_), None) => Err(AxError::AlreadyExists),
        (None, Some(_)) | (Some(_), Some(_)) => Err(AxError::NotFound),
    }
}

/// Owned name snapshot retained by one prepared namespace mutation.
struct PreparedName {
    parent: Location,
    name: String,
    expected: Option<Location>,
    generation: NamespaceGeneration,
}

impl PreparedName {
    fn reserve(parent: &Location, name: &str, expected: Option<&Location>) -> AxResult<Self> {
        let name = try_owned(name)?;
        let (current, generation) = stable_lookup(parent, &name)?;
        validate_expected_identity(current.as_ref(), expected)?;
        Ok(Self {
            parent: parent.clone(),
            name,
            expected: expected.cloned(),
            generation,
        })
    }

    fn revalidate(&self) -> AxResult<()> {
        if self
            .parent
            .namespace_generation_is_current(self.generation)?
        {
            return Ok(());
        }
        let (current, _) = stable_lookup(&self.parent, &self.name)?;
        validate_expected_identity(current.as_ref(), self.expected.as_ref())
    }
}

trait KernelMutationRequest: Sized {
    type Reservation;
    type Output;

    fn reserve(self) -> AxResult<Self::Reservation>;
    fn revalidate(reservation: &Self::Reservation) -> AxResult<()>;
    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output>;

    fn rollback(_reservation: &mut Self::Reservation) {}
}

struct KernelMutationBackend<M>(PhantomData<fn() -> M>);

impl<M> KernelMutationBackend<M> {
    const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<M: KernelMutationRequest> MutationBackend for KernelMutationBackend<M> {
    type Request = M;
    type Reservation = M::Reservation;
    type Output = M::Output;
    type Error = AxError;

    fn reserve(&self, request: Self::Request) -> Result<Self::Reservation, Self::Error> {
        request.reserve()
    }

    fn revalidate(&self, reservation: &Self::Reservation) -> Result<(), Self::Error> {
        M::revalidate(reservation)
    }

    fn publish(&self, reservation: &mut Self::Reservation) -> Result<Self::Output, Self::Error> {
        M::publish(reservation)
    }

    fn rollback(&self, reservation: &mut Self::Reservation) {
        M::rollback(reservation);
    }
}

fn commit<M: KernelMutationRequest>(request: M) -> AxResult<M::Output> {
    let backend = KernelMutationBackend::<M>::new();
    MutationTransaction::prepare(&backend, request)?.commit()
}

struct CreateRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
    rdev: Option<DeviceId>,
    credentials: &'a DacCredentialView,
}

struct PreparedCreate {
    name: PreparedName,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
    rdev: Option<DeviceId>,
    credentials: DacCredentialView,
}

impl KernelMutationRequest for CreateRequest<'_> {
    type Reservation = PreparedCreate;
    type Output = Location;

    fn reserve(self) -> AxResult<Self::Reservation> {
        check_create_permissions(self.parent, self.credentials)?;
        Ok(PreparedCreate {
            name: PreparedName::reserve(self.parent, self.name, None)?,
            node_type: self.node_type,
            requested_mode: self.requested_mode,
            umask: self.umask,
            rdev: self.rdev,
            credentials: self.credentials.clone(),
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()?;
        check_create_permissions(&reservation.name.parent, &reservation.credentials)
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let parent_metadata = reservation.name.parent.metadata()?;
        let (permission, owner) = initial_named_create_owner_mode(
            &parent_metadata,
            &reservation.credentials,
            reservation.node_type,
            reservation.requested_mode,
            reservation.umask,
        );
        reservation
            .name
            .parent
            .create_named(
                &reservation.name.name,
                &NamedCreateOptions {
                    node_type: reservation.node_type,
                    permission,
                    owner: Some(owner),
                    rdev: reservation.rdev,
                    initial_data: None,
                },
                CreateDisposition::Exclusive,
            )
            .map(|outcome| outcome.entry)
    }
}

pub(crate) fn create_named(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
    rdev: Option<DeviceId>,
    credentials: &DacCredentialView,
) -> AxResult<Location> {
    commit(CreateRequest {
        parent,
        name,
        node_type,
        requested_mode,
        umask,
        rdev,
        credentials,
    })
}

struct SymlinkRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    target: &'a str,
    credentials: &'a DacCredentialView,
}

struct PreparedSymlink {
    name: PreparedName,
    target: String,
    credentials: DacCredentialView,
}

impl KernelMutationRequest for SymlinkRequest<'_> {
    type Reservation = PreparedSymlink;
    type Output = Location;

    fn reserve(self) -> AxResult<Self::Reservation> {
        check_create_permissions(self.parent, self.credentials)?;
        Ok(PreparedSymlink {
            name: PreparedName::reserve(self.parent, self.name, None)?,
            target: try_owned(self.target)?,
            credentials: self.credentials.clone(),
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()?;
        check_create_permissions(&reservation.name.parent, &reservation.credentials)
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let parent_metadata = reservation.name.parent.metadata()?;
        let owner_gid = if parent_metadata.mode.contains(NodePermission::SET_GID) {
            parent_metadata.gid
        } else {
            reservation.credentials.gid().into_raw()
        };
        reservation.name.parent.create_symlink(
            &reservation.name.name,
            &reservation.target,
            NodePermission::from_bits_truncate(0o777),
            Some((reservation.credentials.uid().into_raw(), owner_gid)),
        )
    }
}

pub(crate) fn create_symlink(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    target: &str,
    credentials: &DacCredentialView,
) -> AxResult<Location> {
    commit(SymlinkRequest {
        parent,
        name,
        target,
        credentials,
    })
}

struct LinkRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    source: &'a Location,
    credentials: &'a DacCredentialView,
}

struct PreparedLink {
    name: PreparedName,
    source: Location,
    credentials: DacCredentialView,
}

impl KernelMutationRequest for LinkRequest<'_> {
    type Reservation = PreparedLink;
    type Output = Location;

    fn reserve(self) -> AxResult<Self::Reservation> {
        check_create_permissions(self.parent, self.credentials)?;
        Ok(PreparedLink {
            name: PreparedName::reserve(self.parent, self.name, None)?,
            source: self.source.clone(),
            credentials: self.credentials.clone(),
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()?;
        check_create_permissions(&reservation.name.parent, &reservation.credentials)
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        reservation
            .name
            .parent
            .link(&reservation.name.name, &reservation.source)
    }
}

pub(crate) fn link(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    source: &Location,
    credentials: &DacCredentialView,
) -> AxResult<Location> {
    commit(LinkRequest {
        parent,
        name,
        source,
        credentials,
    })
}

pub(crate) struct UnlinkOutcome {
    pub(crate) is_dir: bool,
    pub(crate) loses_last_link: bool,
}

struct UnlinkRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    target: &'a Location,
    remove_dir: bool,
    credentials: &'a DacCredentialView,
}

struct PreparedUnlink {
    name: PreparedName,
    target: Location,
    remove_dir: bool,
    credentials: DacCredentialView,
}

impl KernelMutationRequest for UnlinkRequest<'_> {
    type Reservation = PreparedUnlink;
    type Output = UnlinkOutcome;

    fn reserve(self) -> AxResult<Self::Reservation> {
        check_remove_permissions(self.parent, self.target, self.credentials)?;
        Ok(PreparedUnlink {
            name: PreparedName::reserve(self.parent, self.name, Some(self.target))?,
            target: self.target.clone(),
            remove_dir: self.remove_dir,
            credentials: self.credentials.clone(),
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()?;
        check_remove_permissions(
            &reservation.name.parent,
            &reservation.target,
            &reservation.credentials,
        )
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let is_dir = reservation.target.is_dir();
        let loses_last_link = is_dir || reservation.target.metadata()?.nlink <= 1;
        reservation.name.parent.unlink_checked(
            &reservation.name.name,
            reservation.remove_dir,
            &reservation.target,
        )?;
        Ok(UnlinkOutcome {
            is_dir,
            loses_last_link,
        })
    }
}

pub(crate) fn unlink(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    target: &Location,
    remove_dir: bool,
    credentials: &DacCredentialView,
) -> AxResult<UnlinkOutcome> {
    commit(UnlinkRequest {
        parent,
        name,
        target,
        remove_dir,
        credentials,
    })
}

pub(crate) struct RenameOutcome {
    pub(crate) replaced: Option<Location>,
    pub(crate) replaced_loses_last_link: bool,
}

struct RenameRequest<'a> {
    old_parent: &'a Location,
    old_name: &'a str,
    source: &'a Location,
    new_parent: &'a Location,
    new_name: &'a str,
    replaced: Option<&'a Location>,
    no_replace: bool,
    credentials: &'a DacCredentialView,
}

struct PreparedRename {
    source_name: PreparedName,
    destination_name: PreparedName,
    source: Location,
    replaced: Option<Location>,
    no_replace: bool,
    credentials: DacCredentialView,
}

fn validate_rename_types(source: &Location, replaced: Option<&Location>) -> AxResult<()> {
    if let Some(replaced) = replaced {
        match (source.is_dir(), replaced.is_dir()) {
            (true, false) => Err(AxError::NotADirectory),
            (false, true) => Err(AxError::IsADirectory),
            _ => Ok(()),
        }
    } else {
        Ok(())
    }
}

impl KernelMutationRequest for RenameRequest<'_> {
    type Reservation = PreparedRename;
    type Output = RenameOutcome;

    fn reserve(self) -> AxResult<Self::Reservation> {
        if self.no_replace && self.replaced.is_some() {
            return Err(AxError::AlreadyExists);
        }
        validate_rename_types(self.source, self.replaced)?;
        check_rename_permissions(
            self.old_parent,
            self.source,
            self.new_parent,
            self.replaced,
            self.credentials,
        )?;
        Ok(PreparedRename {
            source_name: PreparedName::reserve(self.old_parent, self.old_name, Some(self.source))?,
            destination_name: PreparedName::reserve(self.new_parent, self.new_name, self.replaced)?,
            source: self.source.clone(),
            replaced: self.replaced.cloned(),
            no_replace: self.no_replace,
            credentials: self.credentials.clone(),
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.source_name.revalidate()?;
        reservation.destination_name.revalidate()?;
        if reservation.no_replace && reservation.replaced.is_some() {
            return Err(AxError::AlreadyExists);
        }
        validate_rename_types(&reservation.source, reservation.replaced.as_ref())?;
        check_rename_permissions(
            &reservation.source_name.parent,
            &reservation.source,
            &reservation.destination_name.parent,
            reservation.replaced.as_ref(),
            &reservation.credentials,
        )
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let replaced_loses_last_link = match reservation.replaced.as_ref() {
            Some(replaced) if replaced.is_file() => replaced.metadata()?.nlink == 1,
            _ => false,
        };
        reservation.source_name.parent.rename_checked(
            &reservation.source_name.name,
            &reservation.source,
            &reservation.destination_name.parent,
            &reservation.destination_name.name,
            reservation.replaced.as_ref(),
        )?;
        Ok(RenameOutcome {
            replaced: reservation.replaced.clone(),
            replaced_loses_last_link,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rename(
    _operation: &NamespaceOperationGuard,
    old_parent: &Location,
    old_name: &str,
    source: &Location,
    new_parent: &Location,
    new_name: &str,
    replaced: Option<&Location>,
    no_replace: bool,
    credentials: &DacCredentialView,
) -> AxResult<RenameOutcome> {
    commit(RenameRequest {
        old_parent,
        old_name,
        source,
        new_parent,
        new_name,
        replaced,
        no_replace,
        credentials,
    })
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use axfs_ng_vfs::Mountpoint;

    use super::*;
    use crate::pseudofs::tmp::MemoryFs;

    fn credentials() -> DacCredentialView {
        DacCredentialView::try_for_test(0, 0, &[], [0; 2]).unwrap()
    }

    fn memory_root() -> Location {
        let fs = MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&fs);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        mount.root_location()
    }

    fn create_file(parent: &Location, name: &str) -> Location {
        parent
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap()
    }

    fn create_dir(parent: &Location, name: &str) -> Location {
        parent
            .create(
                name,
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap()
    }

    #[derive(Clone, Copy)]
    enum Failure {
        None,
        Revalidate,
        Publish,
    }

    struct ProbeRequest {
        failure: Failure,
        reserved: Arc<AtomicUsize>,
        published: Arc<AtomicUsize>,
        rolled_back: Arc<AtomicUsize>,
    }

    struct ProbeReservation {
        failure: Failure,
        published: Arc<AtomicUsize>,
        rolled_back: Arc<AtomicUsize>,
        rollback_complete: bool,
    }

    impl KernelMutationRequest for ProbeRequest {
        type Reservation = ProbeReservation;
        type Output = ();

        fn reserve(self) -> AxResult<Self::Reservation> {
            self.reserved.fetch_add(1, Ordering::SeqCst);
            Ok(ProbeReservation {
                failure: self.failure,
                published: self.published,
                rolled_back: self.rolled_back,
                rollback_complete: false,
            })
        }

        fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
            if matches!(reservation.failure, Failure::Revalidate) {
                Err(AxError::ResourceBusy)
            } else {
                Ok(())
            }
        }

        fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
            if matches!(reservation.failure, Failure::Publish) {
                return Err(AxError::Io);
            }
            reservation.published.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn rollback(reservation: &mut Self::Reservation) {
            if !reservation.rollback_complete {
                reservation.rolled_back.fetch_add(1, Ordering::SeqCst);
                reservation.rollback_complete = true;
            }
        }
    }

    fn probe(failure: Failure) -> (AxResult<()>, usize, usize, usize) {
        let reserved = Arc::new(AtomicUsize::new(0));
        let published = Arc::new(AtomicUsize::new(0));
        let rolled_back = Arc::new(AtomicUsize::new(0));
        let result = commit(ProbeRequest {
            failure,
            reserved: reserved.clone(),
            published: published.clone(),
            rolled_back: rolled_back.clone(),
        });
        (
            result,
            reserved.load(Ordering::SeqCst),
            published.load(Ordering::SeqCst),
            rolled_back.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn revalidation_failure_rolls_back_once_without_publication() {
        let (result, reserved, published, rolled_back) = probe(Failure::Revalidate);
        assert_eq!(result, Err(AxError::ResourceBusy));
        assert_eq!((reserved, published, rolled_back), (1, 0, 1));
    }

    #[test]
    fn publication_failure_rolls_back_once() {
        let (result, reserved, published, rolled_back) = probe(Failure::Publish);
        assert_eq!(result, Err(AxError::Io));
        assert_eq!((reserved, published, rolled_back), (1, 0, 1));
    }

    #[test]
    fn successful_publication_does_not_run_rollback() {
        let (result, reserved, published, rolled_back) = probe(Failure::None);
        assert_eq!(result, Ok(()));
        assert_eq!((reserved, published, rolled_back), (1, 1, 0));
    }

    #[test]
    fn unrelated_generation_drift_revalidates_the_exact_expected_identity() {
        let root = memory_root();
        let victim = create_file(&root, "victim");
        let prepared = PreparedName::reserve(&root, "victim", Some(&victim)).unwrap();

        create_file(&root, "unrelated");

        let result = prepared.revalidate();
        assert!(result.is_ok(), "revalidation failed: {result:?}");
        assert!(root.lookup_no_follow("victim").unwrap().same_node(&victim));
    }

    #[test]
    fn replacement_snapshot_is_rejected_without_touching_the_new_object() {
        let root = memory_root();
        let old = create_file(&root, "slot");
        let prepared = PreparedName::reserve(&root, "slot", Some(&old)).unwrap();
        let replacement = create_file(&root, "replacement");

        assert!(matches!(
            validate_expected_identity(Some(&replacement), prepared.expected.as_ref()),
            Err(AxError::NotFound)
        ));
        assert!(
            root.lookup_no_follow("replacement")
                .unwrap()
                .same_node(&replacement)
        );
        assert!(root.lookup_no_follow("slot").unwrap().same_node(&old));
    }

    #[test]
    fn rename_revalidates_both_parent_generations_and_exact_identities() {
        let root = memory_root();
        let old_parent = create_dir(&root, "old");
        let new_parent = create_dir(&root, "new");
        let source = create_file(&old_parent, "source");
        let credentials = credentials();
        let permission =
            check_rename_permissions(&old_parent, &source, &new_parent, None, &credentials);
        assert!(
            permission.is_ok(),
            "rename permission failed: {permission:?}"
        );
        let destination = create_file(&new_parent, "destination");
        let mut prepared = RenameRequest {
            old_parent: &old_parent,
            old_name: "source",
            source: &source,
            new_parent: &new_parent,
            new_name: "destination",
            replaced: Some(&destination),
            no_replace: false,
            credentials: &credentials,
        }
        .reserve()
        .unwrap();

        create_file(&old_parent, "old-noise");
        create_file(&new_parent, "new-noise");
        RenameRequest::revalidate(&prepared).unwrap();
        assert!(
            old_parent
                .lookup_no_follow("source")
                .unwrap()
                .same_node(&source)
        );
        assert!(
            new_parent
                .lookup_no_follow("destination")
                .unwrap()
                .same_node(&destination)
        );

        let replacement = create_file(&new_parent, "replacement");
        prepared.destination_name.expected = Some(replacement.clone());
        create_file(&new_parent, "force-destination-revalidation");
        assert!(matches!(
            RenameRequest::revalidate(&prepared),
            Err(AxError::NotFound)
        ));
        assert!(
            new_parent
                .lookup_no_follow("replacement")
                .unwrap()
                .same_node(&replacement)
        );
    }

    #[test]
    fn public_namespace_mutations_require_the_shared_operation_capability() {
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            NodeType,
            NodePermission,
            u32,
            Option<DeviceId>,
            &DacCredentialView,
        ) -> AxResult<Location> = create_named;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &str,
            &DacCredentialView,
        ) -> AxResult<Location> = create_symlink;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &Location,
            &DacCredentialView,
        ) -> AxResult<Location> = link;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &Location,
            bool,
            &DacCredentialView,
        ) -> AxResult<UnlinkOutcome> = unlink;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &Location,
            &Location,
            &str,
            Option<&Location>,
            bool,
            &DacCredentialView,
        ) -> AxResult<RenameOutcome> = rename;
    }
}
