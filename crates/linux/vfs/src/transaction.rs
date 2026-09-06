/// Backend contract for one fallible, generation-revalidated VFS mutation.
///
/// Pathwalk and preliminary admission complete before [`reserve`](Self::reserve).
/// After generation revalidation, [`admit`](Self::admit) performs any final
/// policy check which depends on the exact revalidated destination. `publish`
/// must make the name/tree change visible at most once. Rollback is infallible
/// and is invoked for every prepared transaction that does not report
/// successful publication.
///
/// Rollback releases only the private reservation or hidden admission owned by
/// this transaction. It never promises to reverse a namespace change. A
/// filesystem that can report an indeterminate or partially committed metadata
/// mutation must enforce its own fail-closed policy (for example, poison the
/// writable metadata state) before returning that error. A backend whose
/// namespace decision is already known to be committed must complete with the
/// committed outcome rather than convert a secondary cleanup failure into an
/// apparently retryable publication error.
pub trait MutationBackend {
    /// Mutation request after pathname and preliminary admission.
    type Request;
    /// Private reservation containing all fallible resources and generations.
    type Reservation;
    /// Successful publication result.
    type Output;
    /// Backend error mapped to Linux errno by the adapter.
    type Error;

    /// Reserves initial private metadata and accounting without publication.
    fn reserve(&self, request: Self::Request) -> Result<Self::Reservation, Self::Error>;

    /// Revalidates parent/name generations before final admission and
    /// publication.
    fn revalidate(&self, reservation: &Self::Reservation) -> Result<(), Self::Error>;

    /// Performs final policy admission over the revalidated reservation.
    ///
    /// This phase may enrich private prepared state needed by publication, but
    /// must not make any namespace or tree mutation visible. The default is a
    /// policy-neutral no-op for backends whose admission is already complete.
    fn admit(&self, _reservation: &mut Self::Reservation) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Makes the transaction's single, at-most-once publication attempt.
    ///
    /// On error, the *private reservation* remains releasable by
    /// [`rollback`](Self::rollback); rollback does not undo filesystem
    /// namespace metadata. Clean errors therefore precede publication, while
    /// indeterminate lower metadata errors must already have triggered the
    /// backend's fail-closed policy.
    fn publish(&self, reservation: &mut Self::Reservation) -> Result<Self::Output, Self::Error>;

    /// Releases every reservation after abort or failure without allocating.
    ///
    /// Rollback operates in place so the transaction can retain structural
    /// ownership of the reservation without an `Option` or an unreachable
    /// runtime panic. Implementations must leave the value safe to drop after
    /// releasing its external charges and hidden backend state.
    fn rollback(&self, reservation: &mut Self::Reservation);
}

/// RAII owner for a prepared VFS mutation.
#[must_use = "dropping a prepared mutation rolls it back"]
pub struct MutationTransaction<'a, B: MutationBackend> {
    backend: &'a B,
    reservation: B::Reservation,
    completed: bool,
}

impl<'a, B: MutationBackend> MutationTransaction<'a, B> {
    /// Reserves private mutation state without publishing it.
    pub fn prepare(backend: &'a B, request: B::Request) -> Result<Self, B::Error> {
        Ok(Self {
            backend,
            reservation: backend.reserve(request)?,
            completed: false,
        })
    }

    /// Revalidates, performs final admission, and makes the single publication
    /// attempt.
    ///
    /// Any failure leaves the reservation owned by `self`, so `Drop` releases
    /// that private reservation and any hidden admission before the error
    /// reaches the syscall adapter. This cleanup does not imply compensation
    /// of a lower filesystem's partially committed namespace operation.
    pub fn commit(mut self) -> Result<B::Output, B::Error> {
        self.backend.revalidate(&self.reservation)?;
        self.backend.admit(&mut self.reservation)?;
        let output = self.backend.publish(&mut self.reservation)?;
        self.completed = true;
        Ok(output)
    }

    /// Explicitly aborts; rollback runs before this function returns.
    pub fn abort(mut self) {
        self.backend.rollback(&mut self.reservation);
        self.completed = true;
    }
}

impl<B: MutationBackend> Drop for MutationTransaction<'_, B> {
    fn drop(&mut self) {
        if !self.completed {
            self.backend.rollback(&mut self.reservation);
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Error {
        Reserve,
        Revalidate,
        Admit,
        Publish,
    }

    struct Backend {
        fail: Option<Error>,
        reserved: Cell<usize>,
        admitted: Cell<usize>,
        published: Cell<usize>,
        rolled_back: Cell<usize>,
    }

    impl Backend {
        fn new(fail: Option<Error>) -> Self {
            Self {
                fail,
                reserved: Cell::new(0),
                admitted: Cell::new(0),
                published: Cell::new(0),
                rolled_back: Cell::new(0),
            }
        }
    }

    impl MutationBackend for Backend {
        type Request = ();
        type Reservation = u64;
        type Output = u64;
        type Error = Error;

        fn reserve(&self, (): Self::Request) -> Result<Self::Reservation, Self::Error> {
            if self.fail == Some(Error::Reserve) {
                return Err(Error::Reserve);
            }
            self.reserved.set(self.reserved.get() + 1);
            Ok(7)
        }

        fn revalidate(&self, _reservation: &Self::Reservation) -> Result<(), Self::Error> {
            if self.fail == Some(Error::Revalidate) {
                Err(Error::Revalidate)
            } else {
                Ok(())
            }
        }

        fn admit(&self, reservation: &mut Self::Reservation) -> Result<(), Self::Error> {
            if self.fail == Some(Error::Admit) {
                return Err(Error::Admit);
            }
            *reservation += 1;
            self.admitted.set(self.admitted.get() + 1);
            Ok(())
        }

        fn publish(
            &self,
            reservation: &mut Self::Reservation,
        ) -> Result<Self::Output, Self::Error> {
            if self.fail == Some(Error::Publish) {
                return Err(Error::Publish);
            }
            self.published.set(self.published.get() + 1);
            Ok(*reservation)
        }

        fn rollback(&self, _reservation: &mut Self::Reservation) {
            self.rolled_back.set(self.rolled_back.get() + 1);
        }
    }

    #[test]
    fn every_post_reserve_failure_rolls_back_once() {
        for failure in [Error::Revalidate, Error::Admit, Error::Publish] {
            let backend = Backend::new(Some(failure));
            assert_eq!(
                MutationTransaction::prepare(&backend, ()).and_then(MutationTransaction::commit),
                Err(failure)
            );
            assert_eq!(backend.reserved.get(), 1);
            assert_eq!(
                backend.admitted.get(),
                usize::from(failure == Error::Publish)
            );
            assert_eq!(backend.published.get(), 0);
            assert_eq!(backend.rolled_back.get(), 1);
        }
    }

    #[test]
    fn successful_publication_is_not_rolled_back() {
        let backend = Backend::new(None);
        assert_eq!(
            MutationTransaction::prepare(&backend, ()).and_then(MutationTransaction::commit),
            Ok(8)
        );
        assert_eq!(backend.admitted.get(), 1);
        assert_eq!(backend.published.get(), 1);
        assert_eq!(backend.rolled_back.get(), 0);
    }

    #[test]
    fn explicit_or_drop_abort_releases_reservation() {
        let backend = Backend::new(None);
        MutationTransaction::prepare(&backend, ()).unwrap().abort();
        assert_eq!(backend.rolled_back.get(), 1);

        let transaction = MutationTransaction::prepare(&backend, ()).unwrap();
        drop(transaction);
        assert_eq!(backend.rolled_back.get(), 2);
    }
}
