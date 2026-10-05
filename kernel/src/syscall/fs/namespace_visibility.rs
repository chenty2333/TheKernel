//! Namespace relationship visibility, independent of file descriptor creation.
//! Linux 7.2.3 ns_get_owner permits only owners in the caller's user-ns subtree.
use alloc::sync::Arc;

use axerrno::{AxError, AxResult};

use crate::task::UserNamespace;

pub(super) fn visible_owner(
    caller: &Arc<UserNamespace>,
    owner: Option<Arc<UserNamespace>>,
) -> AxResult<Arc<UserNamespace>> {
    let owner = owner.ok_or(AxError::OperationNotPermitted)?;
    let mut cursor = Some(owner.clone());
    while let Some(namespace) = cursor {
        if Arc::ptr_eq(&namespace, caller) {
            return Ok(owner);
        }
        cursor = namespace.parent();
    }
    Err(AxError::OperationNotPermitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Kgid, Kuid};

    #[test]
    fn namespace_owner_visibility_uses_ancestry_not_a_capability_gate() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let sibling = root
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        assert!(Arc::ptr_eq(
            &visible_owner(&root, Some(root.clone())).unwrap(),
            &root
        ));
        assert!(Arc::ptr_eq(
            &visible_owner(&root, Some(child.clone())).unwrap(),
            &child
        ));
        assert!(Arc::ptr_eq(
            &visible_owner(&child, Some(child.clone())).unwrap(),
            &child
        ));
        assert!(matches!(
            visible_owner(&child, Some(root.clone())),
            Err(AxError::OperationNotPermitted)
        ));
        assert!(matches!(
            visible_owner(&child, Some(sibling)),
            Err(AxError::OperationNotPermitted)
        ));
        assert!(matches!(
            visible_owner(&root, None),
            Err(AxError::OperationNotPermitted)
        ));
        assert!(Arc::ptr_eq(
            &visible_owner(&root, child.parent()).unwrap(),
            &root
        ));
        assert!(matches!(
            visible_owner(&child, child.parent()),
            Err(AxError::OperationNotPermitted)
        ));
    }
}
