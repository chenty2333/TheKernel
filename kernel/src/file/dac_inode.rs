//! Linux DAC override scope for inode IDs mapped in the actor's namespace.
use linux_raw_sys::general::{CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH};

use crate::task::{Cred, DacCredentialView, Kgid, Kuid};

pub(super) fn capable_wrt_inode_ids(
    actor: &Cred,
    selected: &DacCredentialView,
    uid: u32,
    gid: u32,
    capability: u32,
) -> bool {
    matches!(capability, CAP_DAC_OVERRIDE | CAP_DAC_READ_SEARCH)
        && selected.selected_capability(capability)
        && Kuid::from_raw(uid).is_some_and(|uid| actor.user_ns().kernel_uid_to_user(uid).is_some())
        && Kgid::from_raw(gid).is_some_and(|gid| actor.user_ns().kernel_gid_to_user(gid).is_some())
        && actor.has_effective_capability_in_own_user_ns(capability)
}
