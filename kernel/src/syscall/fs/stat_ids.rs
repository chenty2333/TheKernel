//! Caller-namespace ownership projection at the stat-family copyout boundary.
use linux_raw_sys::general::{stat, statx};
use tk_linux_cred::USER_NAMESPACE_OVERFLOW_ID;

use crate::task::{Kgid, Kuid, UserNamespace};

fn visible_ids(namespace: &UserNamespace, uid: u32, gid: u32) -> (u32, u32) {
    (
        Kuid::from_raw(uid).map_or(USER_NAMESPACE_OVERFLOW_ID, |uid| {
            namespace.from_kuid_munged(uid)
        }),
        Kgid::from_raw(gid).map_or(USER_NAMESPACE_OVERFLOW_ID, |gid| {
            namespace.from_kgid_munged(gid)
        }),
    )
}

pub(super) fn project_stat(namespace: &UserNamespace, record: &mut stat) {
    (record.st_uid, record.st_gid) = visible_ids(namespace, record.st_uid, record.st_gid);
}

pub(super) fn project_statx(namespace: &UserNamespace, record: &mut statx) {
    (record.stx_uid, record.stx_gid) = visible_ids(namespace, record.stx_uid, record.stx_gid);
}

#[cfg(test)]
mod tests {
    use tk_linux_cred::IdMapInputExtent;

    use super::*;

    #[test]
    fn wire_records_project_only_owner_fields() {
        let initial = UserNamespace::try_new_root().unwrap();
        assert_eq!(visible_ids(&initial, 1001, 2001), (1001, 2001));
        let child = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        child
            .publish_uid_map(
                child
                    .try_build_uid_map(alloc::vec![IdMapInputExtent::new(0, 1000, 2)])
                    .unwrap(),
            )
            .unwrap();
        child
            .publish_gid_map(
                child
                    .try_build_gid_map(alloc::vec![IdMapInputExtent::new(0, 2000, 2)])
                    .unwrap(),
                false,
            )
            .unwrap();
        // SAFETY: these Linux native ABI records contain only integer fields;
        // every padding byte also remains initialized for the copyout.
        let mut native: stat = unsafe { core::mem::zeroed() };
        let mut extended: statx = unsafe { core::mem::zeroed() };
        native.st_uid = 1001;
        native.st_gid = 2001;
        native.st_ino = 321;
        native.st_mode = 0o40755;
        extended.stx_uid = 1001;
        extended.stx_gid = 2001;
        extended.stx_ino = 321;
        extended.stx_mode = 0o40755;
        project_stat(&child, &mut native);
        project_statx(&child, &mut extended);
        assert_eq!((native.st_uid, native.st_gid), (1, 1));
        assert_eq!((extended.stx_uid, extended.stx_gid), (1, 1));
        assert_eq!((native.st_ino, native.st_mode), (321, 0o40755));
        assert_eq!((extended.stx_ino, extended.stx_mode), (321, 0o40755));
        assert_eq!(
            visible_ids(&child, 0, 2000),
            (USER_NAMESPACE_OVERFLOW_ID, 0)
        );
        assert_eq!(
            visible_ids(&child, 1000, 0),
            (0, USER_NAMESPACE_OVERFLOW_ID)
        );
        assert_eq!(
            visible_ids(&child, u32::MAX, u32::MAX),
            (USER_NAMESPACE_OVERFLOW_ID, USER_NAMESPACE_OVERFLOW_ID)
        );
        assert_eq!(visible_ids(&initial, 1001, 2001), (1001, 2001));
    }
}
