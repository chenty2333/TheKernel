use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicUsize, Ordering};

use axerrno::{AxError, AxResult, LinuxError};

use crate::task::Kuid;

pub(super) const KEY_MAXKEYS_DEFAULT: usize = 200;
pub(super) const KEY_MAXBYTES_DEFAULT: usize = 20_000;
const KEY_ROOT_MAXKEYS_DEFAULT: usize = 1_000_000;
const KEY_ROOT_MAXBYTES_DEFAULT: usize = 25_000_000;

// These are private implementation pressure bounds, not Linux ABI quota.
const MANAGER_MAX_LIVE_OBJECTS: usize = 1 << 20;
const MANAGER_MAX_RESIDENT_BYTES: usize = 256 << 20;
pub(super) const MANAGER_MAX_LINK_BYTES: usize = 16 << 20;

static KEY_MAXKEYS: AtomicUsize = AtomicUsize::new(KEY_MAXKEYS_DEFAULT);
static KEY_MAXBYTES: AtomicUsize = AtomicUsize::new(KEY_MAXBYTES_DEFAULT);
static KEY_ROOT_MAXKEYS: AtomicUsize = AtomicUsize::new(KEY_ROOT_MAXKEYS_DEFAULT);
static KEY_ROOT_MAXBYTES: AtomicUsize = AtomicUsize::new(KEY_ROOT_MAXBYTES_DEFAULT);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct AbiQuotaCharge {
    pub(super) keys: usize,
    pub(super) bytes: usize,
}

impl AbiQuotaCharge {
    pub(super) const ZERO: Self = Self { keys: 0, bytes: 0 };
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ResidentCharge {
    pub(super) objects: usize,
    pub(super) bytes: usize,
    pub(super) link_bytes: usize,
}

impl ResidentCharge {
    pub(super) const ZERO: Self = Self {
        objects: 0,
        bytes: 0,
        link_bytes: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QuotaAdmission {
    /// Charge the owner and enforce its current ABI limits.
    Enforced,
    /// Charge the owner, but permit this allocation to exceed its ABI limits.
    AllowOverrun,
    /// Do not include the object in the userspace-visible owner quota ledger.
    Exempt,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct OwnerUsage {
    pub(super) keys: usize,
    pub(super) bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OwnerGcScratch {
    epoch: u64,
    retire: AbiQuotaCharge,
    after: OwnerUsage,
    next: Option<Kuid>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct OwnerLedgerEntry {
    usage: OwnerUsage,
    gc: OwnerGcScratch,
}

impl OwnerLedgerEntry {
    const fn new(usage: OwnerUsage) -> Self {
        Self {
            usage,
            gc: OwnerGcScratch {
                epoch: 0,
                retire: AbiQuotaCharge::ZERO,
                after: OwnerUsage { keys: 0, bytes: 0 },
                next: None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OwnerLedgerUpdate {
    uid: Kuid,
    after: OwnerUsage,
}

#[derive(Default)]
pub(super) struct OwnerLedger {
    pub(super) usage: BTreeMap<Kuid, OwnerLedgerEntry>,
}

impl OwnerLedger {
    pub(super) fn usage(&self, uid: Kuid) -> OwnerUsage {
        self.usage
            .get(&uid)
            .map(|entry| entry.usage)
            .unwrap_or_default()
    }

    pub(super) fn plan_replace(
        &self,
        uid: Kuid,
        admission: QuotaAdmission,
        old: AbiQuotaCharge,
        new: AbiQuotaCharge,
    ) -> AxResult<Option<OwnerLedgerUpdate>> {
        if admission == QuotaAdmission::Exempt {
            return Ok(None);
        }

        let current = self.usage(uid);
        let base_keys = current
            .keys
            .checked_sub(old.keys)
            .ok_or(AxError::BadState)?;
        let base_bytes = current
            .bytes
            .checked_sub(old.bytes)
            .ok_or(AxError::BadState)?;
        let after = OwnerUsage {
            keys: base_keys
                .checked_add(new.keys)
                .ok_or(AxError::from(LinuxError::EDQUOT))?,
            bytes: base_bytes
                .checked_add(new.bytes)
                .ok_or(AxError::from(LinuxError::EDQUOT))?,
        };
        let grows_keys = new.keys > old.keys;
        let grows_bytes = new.bytes > old.bytes;
        if admission == QuotaAdmission::Enforced
            && (grows_keys && after.keys > user_maxkeys(uid)
                || grows_bytes && after.bytes > user_maxbytes(uid))
        {
            return Err(LinuxError::EDQUOT.into());
        }
        Ok(Some(OwnerLedgerUpdate { uid, after }))
    }

    pub(super) fn plan_transfer(
        &self,
        old_uid: Kuid,
        new_uid: Kuid,
        admission: QuotaAdmission,
        charge: AbiQuotaCharge,
    ) -> AxResult<[Option<OwnerLedgerUpdate>; 2]> {
        if admission == QuotaAdmission::Exempt || old_uid == new_uid {
            return Ok([None, None]);
        }

        let old_usage = self.usage(old_uid);
        let new_usage = self.usage(new_uid);
        let old_after = OwnerUsage {
            keys: old_usage
                .keys
                .checked_sub(charge.keys)
                .ok_or(AxError::BadState)?,
            bytes: old_usage
                .bytes
                .checked_sub(charge.bytes)
                .ok_or(AxError::BadState)?,
        };
        let new_after = OwnerUsage {
            keys: new_usage
                .keys
                .checked_add(charge.keys)
                .ok_or(AxError::from(LinuxError::EDQUOT))?,
            bytes: new_usage
                .bytes
                .checked_add(charge.bytes)
                .ok_or(AxError::from(LinuxError::EDQUOT))?,
        };
        if admission == QuotaAdmission::Enforced
            && (new_after.keys > user_maxkeys(new_uid) || new_after.bytes > user_maxbytes(new_uid))
        {
            return Err(LinuxError::EDQUOT.into());
        }
        Ok([
            Some(OwnerLedgerUpdate {
                uid: old_uid,
                after: old_after,
            }),
            Some(OwnerLedgerUpdate {
                uid: new_uid,
                after: new_after,
            }),
        ])
    }

    pub(super) fn apply(&mut self, update: Option<OwnerLedgerUpdate>) {
        let Some(update) = update else {
            return;
        };
        if update.after == OwnerUsage::default() {
            self.usage.remove(&update.uid);
        } else if let Some(entry) = self.usage.get_mut(&update.uid) {
            debug_assert_eq!(entry.gc, OwnerGcScratch::default());
            entry.usage = update.after;
        } else {
            self.usage
                .insert(update.uid, OwnerLedgerEntry::new(update.after));
        }
    }

    /// Adds one object's charge to an allocation-free GC owner plan.
    ///
    /// The caller holds the key-manager mutex for the complete prepare/commit
    /// transaction, so an epoch can own scratch without another lock.
    pub(super) fn plan_gc_retire(
        &mut self,
        uid: Kuid,
        charge: AbiQuotaCharge,
        epoch: u64,
        owner_head: &mut Option<Kuid>,
    ) -> AxResult<bool> {
        let entry = self.usage.get_mut(&uid).ok_or(AxError::BadState)?;
        let newly_touched = if entry.gc.epoch == 0 {
            if entry.gc != OwnerGcScratch::default() {
                return Err(AxError::BadState);
            }
            true
        } else if entry.gc.epoch == epoch {
            false
        } else {
            return Err(AxError::BadState);
        };
        let retire = AbiQuotaCharge {
            keys: entry
                .gc
                .retire
                .keys
                .checked_add(charge.keys)
                .ok_or(AxError::BadState)?,
            bytes: entry
                .gc
                .retire
                .bytes
                .checked_add(charge.bytes)
                .ok_or(AxError::BadState)?,
        };
        let after = OwnerUsage {
            keys: entry
                .usage
                .keys
                .checked_sub(retire.keys)
                .ok_or(AxError::BadState)?,
            bytes: entry
                .usage
                .bytes
                .checked_sub(retire.bytes)
                .ok_or(AxError::BadState)?,
        };
        if newly_touched {
            entry.gc.epoch = epoch;
            entry.gc.next = *owner_head;
            *owner_head = Some(uid);
        }
        entry.gc.retire = retire;
        entry.gc.after = after;
        Ok(newly_touched)
    }

    pub(super) fn abort_gc(&mut self, epoch: u64, mut head: Option<Kuid>, count: usize) {
        for _ in 0..count {
            let uid = head.expect("prepared owner chain ended early");
            let entry = self
                .usage
                .get_mut(&uid)
                .expect("prepared owner disappeared before abort");
            assert_eq!(entry.gc.epoch, epoch, "foreign owner GC scratch");
            head = entry.gc.next;
            entry.gc = OwnerGcScratch::default();
        }
        assert!(head.is_none(), "prepared owner chain exceeded its count");
    }

    pub(super) fn commit_gc(&mut self, epoch: u64, mut head: Option<Kuid>, count: usize) {
        for _ in 0..count {
            let uid = head.expect("prepared owner chain ended early");
            let (next, after) = {
                let entry = self
                    .usage
                    .get_mut(&uid)
                    .expect("prepared owner disappeared before commit");
                assert_eq!(entry.gc.epoch, epoch, "foreign owner GC scratch");
                (entry.gc.next, entry.gc.after)
            };
            if after == OwnerUsage::default() {
                self.usage
                    .remove(&uid)
                    .expect("prepared owner disappeared during commit");
            } else {
                let entry = self
                    .usage
                    .get_mut(&uid)
                    .expect("prepared owner disappeared during commit");
                entry.usage = after;
                entry.gc = OwnerGcScratch::default();
            }
            head = next;
        }
        assert!(head.is_none(), "prepared owner chain exceeded its count");
    }

    #[cfg(test)]
    pub(super) fn set_usage_for_test(&mut self, uid: Kuid, usage: OwnerUsage) {
        self.usage.insert(uid, OwnerLedgerEntry::new(usage));
    }

    #[cfg(test)]
    pub(super) fn gc_scratch_is_idle(&self) -> bool {
        self.usage
            .values()
            .all(|entry| entry.gc == OwnerGcScratch::default())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ManagerBudgetLimits {
    pub(super) objects: usize,
    pub(super) bytes: usize,
    pub(super) link_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ManagerBudgetUsage {
    pub(super) objects: usize,
    pub(super) bytes: usize,
    pub(super) link_bytes: usize,
}

pub(super) struct ManagerBudget {
    pub(super) limits: ManagerBudgetLimits,
    pub(super) used: ManagerBudgetUsage,
}

impl ManagerBudget {
    pub(super) const fn new(limits: ManagerBudgetLimits) -> Self {
        Self {
            limits,
            used: ManagerBudgetUsage {
                objects: 0,
                bytes: 0,
                link_bytes: 0,
            },
        }
    }

    pub(super) const fn kernel_default() -> Self {
        Self::new(ManagerBudgetLimits {
            objects: MANAGER_MAX_LIVE_OBJECTS,
            bytes: MANAGER_MAX_RESIDENT_BYTES,
            link_bytes: MANAGER_MAX_LINK_BYTES,
        })
    }

    pub(super) fn plan_replace(
        &self,
        old: ResidentCharge,
        new: ResidentCharge,
    ) -> AxResult<ManagerBudgetUsage> {
        let base_objects = self
            .used
            .objects
            .checked_sub(old.objects)
            .ok_or(AxError::BadState)?;
        let base_bytes = self
            .used
            .bytes
            .checked_sub(old.bytes)
            .ok_or(AxError::BadState)?;
        let base_link_bytes = self
            .used
            .link_bytes
            .checked_sub(old.link_bytes)
            .ok_or(AxError::BadState)?;
        let after = ManagerBudgetUsage {
            objects: base_objects
                .checked_add(new.objects)
                .ok_or(AxError::NoMemory)?,
            bytes: base_bytes.checked_add(new.bytes).ok_or(AxError::NoMemory)?,
            link_bytes: base_link_bytes
                .checked_add(new.link_bytes)
                .ok_or(AxError::NoMemory)?,
        };
        if after.objects > self.limits.objects
            || after.bytes > self.limits.bytes
            || after.link_bytes > self.limits.link_bytes
        {
            return Err(AxError::NoMemory);
        }
        Ok(after)
    }

    pub(super) fn apply(&mut self, after: ManagerBudgetUsage) {
        self.used = after;
    }

    pub(super) fn check_transient(&self, additional: ResidentCharge) -> AxResult<()> {
        let peak = ManagerBudgetUsage {
            objects: self
                .used
                .objects
                .checked_add(additional.objects)
                .ok_or(AxError::NoMemory)?,
            bytes: self
                .used
                .bytes
                .checked_add(additional.bytes)
                .ok_or(AxError::NoMemory)?,
            link_bytes: self
                .used
                .link_bytes
                .checked_add(additional.link_bytes)
                .ok_or(AxError::NoMemory)?,
        };
        if peak.objects > self.limits.objects
            || peak.bytes > self.limits.bytes
            || peak.link_bytes > self.limits.link_bytes
        {
            return Err(AxError::NoMemory);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct AccountingPlan {
    pub(super) owner: Option<OwnerLedgerUpdate>,
    pub(super) budget: ManagerBudgetUsage,
}

pub(super) fn user_maxkeys(uid: Kuid) -> usize {
    if uid == Kuid::INITIAL_ROOT {
        KEY_ROOT_MAXKEYS.load(Ordering::Relaxed)
    } else {
        KEY_MAXKEYS.load(Ordering::Relaxed)
    }
}

pub(super) fn user_maxbytes(uid: Kuid) -> usize {
    if uid == Kuid::INITIAL_ROOT {
        KEY_ROOT_MAXBYTES.load(Ordering::Relaxed)
    } else {
        KEY_MAXBYTES.load(Ordering::Relaxed)
    }
}

pub(crate) fn key_maxkeys() -> usize {
    KEY_MAXKEYS.load(Ordering::Relaxed)
}

pub(super) fn validate_key_quota_limit(value: usize) -> AxResult<usize> {
    if !(1..=i32::MAX as usize).contains(&value) {
        return Err(AxError::InvalidInput);
    }
    Ok(value)
}

pub(crate) fn set_key_maxkeys(value: usize) -> AxResult<()> {
    KEY_MAXKEYS.store(validate_key_quota_limit(value)?, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn key_maxbytes() -> usize {
    KEY_MAXBYTES.load(Ordering::Relaxed)
}

pub(crate) fn set_key_maxbytes(value: usize) -> AxResult<()> {
    KEY_MAXBYTES.store(validate_key_quota_limit(value)?, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn key_root_maxkeys() -> usize {
    KEY_ROOT_MAXKEYS.load(Ordering::Relaxed)
}

pub(crate) fn set_key_root_maxkeys(value: usize) -> AxResult<()> {
    KEY_ROOT_MAXKEYS.store(validate_key_quota_limit(value)?, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn key_root_maxbytes() -> usize {
    KEY_ROOT_MAXBYTES.load(Ordering::Relaxed)
}

pub(crate) fn set_key_root_maxbytes(value: usize) -> AxResult<()> {
    KEY_ROOT_MAXBYTES.store(validate_key_quota_limit(value)?, Ordering::Relaxed);
    Ok(())
}
