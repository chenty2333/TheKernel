use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Shared aggregate byte budget for live seccomp programs and chain nodes.
///
/// Kernel adapters normally create one budget for their seccomp subsystem and
/// pass it to every filter installation. Fork and clone share immutable chain
/// nodes, so they do not reserve the same program a second time. The charge is
/// returned only when the final reference to the node is released.
#[derive(Clone)]
pub struct FilterBudget {
    inner: Arc<FilterBudgetInner>,
}

struct FilterBudgetInner {
    limit_bytes: usize,
    used_bytes: AtomicUsize,
}

impl FilterBudget {
    /// Fallibly allocates a budget with the supplied aggregate byte limit.
    pub fn try_new(limit_bytes: usize) -> Result<Self, FilterBudgetCreateError> {
        let inner = Arc::try_new(FilterBudgetInner {
            limit_bytes,
            used_bytes: AtomicUsize::new(0),
        })
        .map_err(|_| FilterBudgetCreateError::NoMemory)?;
        Ok(Self { inner })
    }

    /// Returns the maximum number of logical bytes that may be live.
    pub fn limit_bytes(&self) -> usize {
        self.inner.limit_bytes
    }

    /// Returns the bytes currently reserved by live immutable nodes.
    pub fn used_bytes(&self) -> usize {
        self.inner.used_bytes.load(Ordering::Acquire)
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn try_reserve(&self, bytes: usize) -> Result<FilterCharge, ChargeError> {
        let mut current = self.inner.used_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return Err(ChargeError::LimitExceeded);
            };
            if next > self.inner.limit_bytes {
                return Err(ChargeError::LimitExceeded);
            }
            match self.inner.used_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(FilterCharge {
                        budget: self.clone(),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

/// Failure while creating a shared aggregate filter budget.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FilterBudgetCreateError {
    /// Allocation for the shared accounting object failed.
    NoMemory,
}

pub(crate) struct FilterCharge {
    budget: FilterBudget,
    bytes: usize,
}

impl FilterCharge {
    pub(crate) fn belongs_to(&self, budget: &FilterBudget) -> bool {
        self.budget.same_identity(budget)
    }
}

impl Drop for FilterCharge {
    fn drop(&mut self) {
        let previous = self
            .budget
            .inner
            .used_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

pub(crate) enum ChargeError {
    LimitExceeded,
}
