//! Owning, dual-alias W^X executable memory publication.
//!
//! An allocation has a final alias and a direct alias to the same backing.
//! `Writable`, `PublishedExecutable`, and `PublishedReadonly` are linear
//! states. A failed transition becomes `Quarantined`, which deliberately
//! retains uncertain aliases rather than permitting backing-page reuse.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}
impl Permissions {
    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
        execute: false,
    };
    pub const READ_ONLY: Self = Self {
        read: true,
        write: false,
        execute: false,
    };
    pub const READ_EXECUTE: Self = Self {
        read: true,
        write: false,
        execute: true,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Alias {
    Final,
    Direct,
}

/// Backend operations for two aliases of one backing allocation.
///
/// Both `map` calls must address the same backing. A successful synchronization
/// must invalidate stale translations globally; a failed `protect` or `unmap`
/// may have changed any prefix, so this crate quarantines the whole owner.
pub trait ExecBackend {
    type Allocation;
    type Mapping;
    type Error;
    fn allocate(&self, len: usize) -> Result<Self::Allocation, Self::Error>;
    fn map(
        &self,
        allocation: &Self::Allocation,
        alias: Alias,
        permissions: Permissions,
    ) -> Result<Self::Mapping, Self::Error>;
    fn protect(
        &self,
        mapping: &mut Self::Mapping,
        permissions: Permissions,
    ) -> Result<(), Self::Error>;
    fn writable_bytes<'a>(
        &self,
        mapping: &'a mut Self::Mapping,
        len: usize,
    ) -> Result<&'a mut [u8], Self::Error>;
    fn unmap(&self, mapping: Self::Mapping) -> Result<(), Self::Error>;
    fn deallocate(&self, allocation: Self::Allocation);
    fn synchronize_tlb(&self) -> Result<(), Self::Error>;
    fn synchronize_icache(&self) -> Result<(), Self::Error>;
}

#[must_use = "the returned owner must be dropped or retained"]
pub struct LifecycleFailure<T, E> {
    pub error: E,
    pub allocation: T,
}

struct Owned<B: ExecBackend> {
    backend: B,
    allocation: Option<B::Allocation>,
    final_alias: Option<B::Mapping>,
    direct_alias: Option<B::Mapping>,
    len: usize,
}
#[must_use]
pub struct Writable<B: ExecBackend>(Option<Owned<B>>);
#[must_use]
pub struct PublishedExecutable<B: ExecBackend>(Option<Owned<B>>);
#[must_use]
pub struct PublishedReadonly<B: ExecBackend>(Option<Owned<B>>);
#[must_use]
pub struct Quarantined<B: ExecBackend>(Option<Owned<B>>);

pub fn allocate<B: ExecBackend>(backend: B, len: usize) -> Result<Writable<B>, B::Error> {
    let allocation = backend.allocate(len)?;
    let final_alias = match backend.map(&allocation, Alias::Final, Permissions::READ_WRITE) {
        Ok(mapping) => mapping,
        Err(error) => {
            backend.deallocate(allocation);
            return Err(error);
        }
    };
    let direct_alias = match backend.map(&allocation, Alias::Direct, Permissions::READ_WRITE) {
        Ok(mapping) => mapping,
        Err(error) => {
            if backend.unmap(final_alias).is_ok() {
                backend.deallocate(allocation);
            } else {
                core::mem::forget(allocation);
            }
            return Err(error);
        }
    };
    Ok(Writable(Some(Owned {
        backend,
        allocation: Some(allocation),
        final_alias: Some(final_alias),
        direct_alias: Some(direct_alias),
        len,
    })))
}

impl<B: ExecBackend> Owned<B> {
    fn quarantine(self) -> Quarantined<B> {
        Quarantined(Some(self))
    }
    fn unmap_and_free(&mut self) -> Result<(), B::Error> {
        if let Some(mapping) = self.final_alias.take() {
            self.backend.unmap(mapping)?;
        }
        if let Some(mapping) = self.direct_alias.take() {
            self.backend.unmap(mapping)?;
        }
        if let Some(allocation) = self.allocation.take() {
            self.backend.deallocate(allocation);
        }
        Ok(())
    }

    // Call only after execute permission has been revoked and both TLB and
    // instruction-cache grace periods have completed. The allocator may
    // scrub or immediately reuse backing through its persistent direct map.
    fn restore_direct_write(&mut self) -> Result<(), B::Error> {
        self.backend.protect(
            self.direct_alias.as_mut().expect("armed published"),
            Permissions::READ_WRITE,
        )?;
        self.backend.synchronize_tlb()
    }
}
impl<B: ExecBackend> Writable<B> {
    pub fn len(&self) -> usize {
        self.0.as_ref().expect("armed writable").len
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn final_mapping(&self) -> &B::Mapping {
        self.0
            .as_ref()
            .expect("armed writable")
            .final_alias
            .as_ref()
            .expect("armed writable")
    }
    pub fn bytes_mut(&mut self) -> Result<&mut [u8], B::Error> {
        let owned = self.0.as_mut().expect("armed writable");
        owned.backend.writable_bytes(
            owned.direct_alias.as_mut().expect("armed writable"),
            owned.len,
        )
    }
    fn publish_into(
        mut self,
        permissions: Permissions,
        executable: bool,
    ) -> Result<Owned<B>, LifecycleFailure<Quarantined<B>, B::Error>> {
        let mut owned = self.0.take().expect("armed writable");
        if let Err(error) = owned.backend.protect(
            owned.direct_alias.as_mut().expect("armed writable"),
            Permissions::READ_ONLY,
        ) {
            return Err(LifecycleFailure {
                error,
                allocation: owned.quarantine(),
            });
        }
        if let Err(error) = owned.backend.synchronize_tlb() {
            return Err(LifecycleFailure {
                error,
                allocation: owned.quarantine(),
            });
        }
        if let Err(error) = owned.backend.protect(
            owned.final_alias.as_mut().expect("armed writable"),
            permissions,
        ) {
            return Err(LifecycleFailure {
                error,
                allocation: owned.quarantine(),
            });
        }
        if let Err(error) = owned.backend.synchronize_tlb() {
            return Err(LifecycleFailure {
                error,
                allocation: owned.quarantine(),
            });
        }
        if executable {
            if let Err(error) = owned.backend.synchronize_icache() {
                return Err(LifecycleFailure {
                    error,
                    allocation: owned.quarantine(),
                });
            }
        }
        Ok(owned)
    }
    pub fn publish(
        self,
    ) -> Result<PublishedExecutable<B>, LifecycleFailure<Quarantined<B>, B::Error>> {
        self.publish_into(Permissions::READ_EXECUTE, true)
            .map(|owned| PublishedExecutable(Some(owned)))
    }
    pub fn publish_readonly(
        self,
    ) -> Result<PublishedReadonly<B>, LifecycleFailure<Quarantined<B>, B::Error>> {
        self.publish_into(Permissions::READ_ONLY, false)
            .map(|owned| PublishedReadonly(Some(owned)))
    }
    pub fn abort(mut self) -> Result<(), LifecycleFailure<Quarantined<B>, B::Error>> {
        let mut owned = self.0.take().expect("armed writable");
        match owned.unmap_and_free() {
            Ok(()) => Ok(()),
            Err(error) => Err(LifecycleFailure {
                error,
                allocation: owned.quarantine(),
            }),
        }
    }
}
impl<B: ExecBackend> Drop for Writable<B> {
    fn drop(&mut self) {
        if let Some(mut owned) = self.0.take() {
            if owned.unmap_and_free().is_err() {
                if let Some(allocation) = owned.allocation.take() {
                    core::mem::forget(allocation);
                }
            }
        }
    }
}
macro_rules! published {
    ($type:ident) => {
        impl<B: ExecBackend> $type<B> {
            pub fn len(&self) -> usize {
                self.0.as_ref().expect("armed published").len
            }
            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }
            pub fn final_mapping(&self) -> &B::Mapping {
                self.0
                    .as_ref()
                    .expect("armed published")
                    .final_alias
                    .as_ref()
                    .expect("armed published")
            }
            pub fn retire(mut self) -> Result<(), LifecycleFailure<Quarantined<B>, B::Error>> {
                let mut owned = self.0.take().expect("armed published");
                if let Err(error) = owned.backend.protect(
                    owned.final_alias.as_mut().expect("armed published"),
                    Permissions::READ_ONLY,
                ) {
                    return Err(LifecycleFailure {
                        error,
                        allocation: owned.quarantine(),
                    });
                }
                if let Err(error) = owned.backend.synchronize_tlb() {
                    return Err(LifecycleFailure {
                        error,
                        allocation: owned.quarantine(),
                    });
                }
                if let Err(error) = owned.backend.synchronize_icache() {
                    return Err(LifecycleFailure {
                        error,
                        allocation: owned.quarantine(),
                    });
                }
                match owned
                    .restore_direct_write()
                    .and_then(|()| owned.unmap_and_free())
                {
                    Ok(()) => Ok(()),
                    Err(error) => Err(LifecycleFailure {
                        error,
                        allocation: owned.quarantine(),
                    }),
                }
            }
        }
        impl<B: ExecBackend> Drop for $type<B> {
            fn drop(&mut self) {
                let Some(mut owned) = self.0.take() else {
                    return;
                };
                let Some(final_alias) = owned.final_alias.as_mut() else {
                    return;
                };
                if owned
                    .backend
                    .protect(final_alias, Permissions::READ_ONLY)
                    .is_err()
                    || owned.backend.synchronize_tlb().is_err()
                    || owned.backend.synchronize_icache().is_err()
                    || owned.restore_direct_write().is_err()
                    || owned.unmap_and_free().is_err()
                {
                    if let Some(allocation) = owned.allocation.take() {
                        core::mem::forget(allocation);
                    }
                }
            }
        }
    };
}
published!(PublishedExecutable);
published!(PublishedReadonly);
impl<B: ExecBackend> Drop for Quarantined<B> {
    fn drop(&mut self) {
        if let Some(mut owned) = self.0.take() {
            if let Some(allocation) = owned.allocation.take() {
                core::mem::forget(allocation);
            }
            if let Some(final_alias) = owned.final_alias.take() {
                core::mem::forget(final_alias);
            }
            if let Some(direct_alias) = owned.direct_alias.take() {
                core::mem::forget(direct_alias);
            }
        }
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Fail {
        Protect,
        Tlb,
        Icache,
        Unmap,
    }
    struct Mock {
        fail: Cell<Option<Fail>>,
        aliases: Cell<usize>,
        freed: Cell<usize>,
        final_p: Cell<Permissions>,
        direct_p: Cell<Permissions>,
        events: RefCell<std::vec::Vec<&'static str>>,
    }
    impl Mock {
        fn new(fail: Option<Fail>) -> Self {
            Self {
                fail: Cell::new(fail),
                aliases: Cell::new(0),
                freed: Cell::new(0),
                final_p: Cell::new(Permissions::READ_WRITE),
                direct_p: Cell::new(Permissions::READ_WRITE),
                events: RefCell::new(std::vec::Vec::new()),
            }
        }
        fn fail(&self, f: Fail) -> Result<(), Fail> {
            if self.fail.get() == Some(f) {
                Err(f)
            } else {
                Ok(())
            }
        }
    }
    impl ExecBackend for &Mock {
        type Allocation = usize;
        type Mapping = Alias;
        type Error = Fail;
        fn allocate(&self, len: usize) -> Result<usize, Fail> {
            Ok(len)
        }
        fn map(&self, _: &usize, a: Alias, _: Permissions) -> Result<Alias, Fail> {
            self.aliases.set(self.aliases.get() + 1);
            Ok(a)
        }
        fn protect(&self, m: &mut Alias, p: Permissions) -> Result<(), Fail> {
            self.fail(Fail::Protect)?;
            match m {
                Alias::Final => {
                    self.events.borrow_mut().push("protect-final");
                    self.final_p.set(p);
                }
                Alias::Direct => {
                    self.events.borrow_mut().push("protect-direct");
                    assert!(!(p.write && self.final_p.get().execute));
                    self.direct_p.set(p);
                }
            };
            Ok(())
        }
        fn writable_bytes<'a>(&self, _: &'a mut Alias, _: usize) -> Result<&'a mut [u8], Fail> {
            Err(Fail::Protect)
        }
        fn unmap(&self, _: Alias) -> Result<(), Fail> {
            self.events.borrow_mut().push("unmap");
            self.fail(Fail::Unmap)?;
            self.aliases.set(self.aliases.get() - 1);
            Ok(())
        }
        fn deallocate(&self, _: usize) {
            self.events.borrow_mut().push("free");
            assert_eq!(self.direct_p.get(), Permissions::READ_WRITE);
            assert!(!self.final_p.get().execute);
            self.freed.set(self.freed.get() + 1)
        }
        fn synchronize_tlb(&self) -> Result<(), Fail> {
            self.events.borrow_mut().push("tlb");
            self.fail(Fail::Tlb)
        }
        fn synchronize_icache(&self) -> Result<(), Fail> {
            self.events.borrow_mut().push("icache");
            self.fail(Fail::Icache)
        }
    }
    #[test]
    fn dual_aliases_publish_and_retire() {
        let b = Mock::new(None);
        let w = match allocate(&b, 2) {
            Ok(w) => w,
            Err(_) => panic!(),
        };
        let p = match w.publish() {
            Ok(p) => p,
            Err(_) => panic!(),
        };
        assert_eq!(b.aliases.get(), 2);
        assert_eq!(b.direct_p.get(), Permissions::READ_ONLY);
        assert_eq!(b.final_p.get(), Permissions::READ_EXECUTE);
        b.events.borrow_mut().clear();
        assert!(p.retire().is_ok());
        assert_eq!(
            b.events.borrow().as_slice(),
            [
                "protect-final",
                "tlb",
                "icache",
                "protect-direct",
                "tlb",
                "unmap",
                "unmap",
                "free"
            ]
        );
        assert_eq!(b.aliases.get(), 0);
        assert_eq!(b.freed.get(), 1);
    }
    #[test]
    fn published_drop_restores_allocator_write_access() {
        let b = Mock::new(None);
        let w = allocate(&b, 2).ok().unwrap();
        drop(w.publish().ok().unwrap());
        assert_eq!(b.direct_p.get(), Permissions::READ_WRITE);
        assert_eq!(b.freed.get(), 1);

        let w = allocate(&b, 2).ok().unwrap();
        drop(w.publish_readonly().ok().unwrap());
        assert_eq!(b.direct_p.get(), Permissions::READ_WRITE);
        assert_eq!(b.freed.get(), 2);
    }
    #[test]
    fn each_publish_failure_quarantines() {
        for f in [Fail::Protect, Fail::Tlb, Fail::Icache] {
            let b = Mock::new(Some(f));
            let w = match allocate(&b, 1) {
                Ok(w) => w,
                Err(_) => panic!(),
            };
            let e = match w.publish() {
                Err(e) => e,
                Ok(_) => panic!(),
            };
            drop(e.allocation);
            assert_eq!(b.freed.get(), 0);
        }
    }
    #[test]
    fn retire_and_unmap_failures_do_not_reuse() {
        for f in [Fail::Protect, Fail::Tlb, Fail::Icache, Fail::Unmap] {
            let b = Mock::new(None);
            let w = match allocate(&b, 1) {
                Ok(w) => w,
                Err(_) => panic!(),
            };
            let p = match w.publish() {
                Ok(p) => p,
                Err(_) => panic!(),
            };
            b.fail.set(Some(f));
            let e = match p.retire() {
                Err(e) => e,
                Ok(_) => panic!(),
            };
            drop(e.allocation);
            assert_eq!(b.freed.get(), 0);
        }
    }
}
