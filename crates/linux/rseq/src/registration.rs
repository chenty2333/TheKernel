use crate::{RSEQ_ABI_ALIGN, RSEQ_ABI_SIZE, RSEQ_AREA_SIZE, RSEQ_AREA_ALIGN, RSEQ_FLAG_UNREGISTER, RSEQ_FLAG_SLICE_EXT_DEFAULT_ON, RseqError};

/// A non-wrapping lifecycle epoch attached to a registration generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RseqEpoch(u64);

impl RseqEpoch {
    /// Builds an epoch for test fixtures or an adapter-owned initial state.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw epoch number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances without wrapping into an older plan identity.
    pub const fn next(self) -> Result<Self, RseqError> {
        match self.0.checked_add(1) {
            Some(raw) => Ok(Self(raw)),
            None => Err(RseqError::EpochExhausted),
        }
    }
}

/// Typed operation selector for the `rseq(2)` flags word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RseqRegistrationOperation {
    /// Register the one area for this thread.
    Register,
    /// Unregister the exact active area.
    Unregister,
}

impl RseqRegistrationOperation {
    /// Accepts the slice-default request even when the adapter provides no slice extension.
    pub const fn from_flags(flags: u32) -> Result<Self, RseqError> {
        match flags {
            0 | RSEQ_FLAG_SLICE_EXT_DEFAULT_ON => Ok(Self::Register),
            RSEQ_FLAG_UNREGISTER => Ok(Self::Unregister),
            _ => Err(RseqError::InvalidRegistrationFlags),
        }
    }

    /// Returns the raw Linux operation flag.
    pub const fn flags(self) -> u32 {
        match self {
            Self::Register => 0,
            Self::Unregister => RSEQ_FLAG_UNREGISTER,
        }
    }

    /// Whether this operation is unregister.
    pub const fn is_unregister(self) -> bool {
        matches!(self, Self::Unregister)
    }
}

/// Registration fields copied from the `rseq(2)` syscall header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RseqRegistrationRequest {
    area_address: u64,
    area_length: u32,
    signature: u32,
}

impl RseqRegistrationRequest {
    /// Preserves raw registration fields without reading user memory.
    pub const fn new(area_address: u64, area_length: u32, signature: u32) -> Self {
        Self {
            area_address,
            area_length,
            signature,
        }
    }

    /// Fallible constructor for adapters that want validation at decode time.
    ///
    /// A null address is deliberately accepted here.  Linux's `access_ok`
    /// check owns the resulting `EFAULT`; rejecting null in this pure stage
    /// would turn a user-memory fault into the wrong `EINVAL`.
    pub fn try_new(area_address: u64, area_length: u32, signature: u32) -> Result<Self, RseqError> {
        let request = Self::new(area_address, area_length, signature);
        request.validate()?.into_request()
    }

    /// Raw area address.
    pub const fn area_address(self) -> u64 {
        self.area_address
    }

    /// Raw area length.
    pub const fn area_length(self) -> u32 {
        self.area_length
    }

    /// Registration signature passed to Linux.
    pub const fn signature(self) -> u32 {
        self.signature
    }

    /// Checks the Linux 7.2.3 legacy and extended size/alignment contract.
    pub const fn validate(self) -> Result<ValidatedRegistrationRequest, RseqError> {
        if self.area_length < RSEQ_AREA_SIZE as u32 {
            return Err(RseqError::InvalidLength);
        }
        let alignment = if self.area_length == RSEQ_AREA_SIZE as u32 {
            RSEQ_AREA_ALIGN
        } else {
            if self.area_length < RSEQ_ABI_SIZE as u32 {
                return Err(RseqError::InvalidLength);
            }
            RSEQ_ABI_ALIGN
        };
        if self.area_address % alignment as u64 != 0 {
            return Err(RseqError::InvalidAlignment);
        }
        Ok(ValidatedRegistrationRequest(self))
    }
}

/// Registration request after pure size/address validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidatedRegistrationRequest(RseqRegistrationRequest);

impl ValidatedRegistrationRequest {
    /// Returns the raw request.
    pub const fn request(self) -> RseqRegistrationRequest {
        self.0
    }

    const fn into_request(self) -> Result<RseqRegistrationRequest, RseqError> {
        Ok(self.0)
    }
}

/// Exact active registration record, tagged with the epoch that published it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RseqRegistration {
    request: RseqRegistrationRequest,
    epoch: RseqEpoch,
}

impl RseqRegistration {
    /// Registered area address.
    pub const fn area_address(self) -> u64 {
        self.request.area_address()
    }

    /// Registered area length.
    pub const fn area_length(self) -> u32 {
        self.request.area_length()
    }

    /// Registered signature.
    pub const fn signature(self) -> u32 {
        self.request.signature()
    }

    /// Publication epoch of this registration.
    pub const fn epoch(self) -> RseqEpoch {
        self.epoch
    }

    /// Original validated registration request.
    pub const fn request(self) -> RseqRegistrationRequest {
        self.request
    }
}

#[derive(Debug, Eq, PartialEq)]
enum PendingRegistration {
    Register {
        request: RseqRegistrationRequest,
        epoch: RseqEpoch,
    },
    Unregister {
        registration: RseqRegistration,
        epoch: RseqEpoch,
    },
}

/// One registration's lifecycle state.
#[derive(Debug, Eq, PartialEq)]
pub struct RseqRegistrationState {
    registration: Option<RseqRegistration>,
    epoch: RseqEpoch,
    pending: Option<PendingRegistration>,
}

impl RseqRegistrationState {
    /// Creates an empty state at epoch zero.
    pub const fn new() -> Self {
        Self {
            registration: None,
            epoch: RseqEpoch::new(0),
            pending: None,
        }
    }

    /// Creates an empty state at an adapter-supplied epoch.
    pub const fn with_epoch(epoch: RseqEpoch) -> Self {
        Self {
            registration: None,
            epoch,
            pending: None,
        }
    }

    /// Current lifecycle phase.
    pub const fn lifecycle(&self) -> RegistrationLifecycle {
        match self.registration {
            Some(_) => RegistrationLifecycle::Registered,
            None => RegistrationLifecycle::Unregistered,
        }
    }

    /// Current epoch used to fence future plans.
    pub const fn epoch(&self) -> RseqEpoch {
        self.epoch
    }

    /// Current active registration, if any.
    pub const fn registration(&self) -> Option<RseqRegistration> {
        self.registration
    }

    /// Whether one registration is currently active.
    pub const fn is_registered(&self) -> bool {
        self.registration.is_some()
    }

    /// Whether an external registration side effect is in flight.
    pub const fn has_pending_operation(&self) -> bool {
        self.pending.is_some()
    }

    /// Reserves a registration generation before the adapter touches user
    /// memory.  `commit_register` is then infallible for the returned token;
    /// use `cancel_register` when the adapter-side side effect fails.
    pub fn prepare_register(
        &mut self,
        request: RseqRegistrationRequest,
    ) -> Result<RseqRegisterPlan, RseqError> {
        request.validate()?;
        if self.pending.is_some() {
            return Err(RseqError::OperationInProgress);
        }
        if let Some(active) = self.registration {
            if active.area_address() != request.area_address()
                || active.area_length() != request.area_length()
            {
                return Err(RseqError::RegistrationMismatch);
            }
            if active.signature() != request.signature() {
                return Err(RseqError::SignatureMismatch);
            }
            return Err(RseqError::AlreadyRegistered);
        }
        let epoch = self.epoch.next()?;
        self.epoch = epoch;
        self.pending = Some(PendingRegistration::Register { request, epoch });
        Ok(RseqRegisterPlan { request, epoch })
    }

    /// Short operation-neutral spelling for [`Self::prepare_register`].
    pub fn prepare(
        &mut self,
        request: RseqRegistrationRequest,
    ) -> Result<RseqRegisterPlan, RseqError> {
        self.prepare_register(request)
    }

    /// Publishes a reserved registration after successful adapter-side setup.
    ///
    /// This method intentionally cannot fail after the external side effect:
    /// the generation and pending operation were reserved by preparation.
    /// Passing a token not returned by this state is a programming error and
    /// panics rather than exposing a fallible post-side-effect fork.
    pub fn commit_register(&mut self, plan: RseqRegisterPlan) -> RseqRegistration {
        let pending = self.pending.take();
        assert_eq!(
            pending,
            Some(PendingRegistration::Register {
                request: plan.request,
                epoch: plan.epoch,
            }),
            "rseq register finalize token does not belong to this state"
        );
        let registration = RseqRegistration {
            request: plan.request,
            epoch: plan.epoch,
        };
        self.registration = Some(registration);
        registration
    }

    /// Cancels a reserved registration after an adapter-side failure.
    pub fn cancel_register(&mut self, plan: RseqRegisterPlan) {
        let pending = self.pending.take();
        assert_eq!(
            pending,
            Some(PendingRegistration::Register {
                request: plan.request,
                epoch: plan.epoch,
            }),
            "rseq register cancel token does not belong to this state"
        );
    }

    /// Prepares an exact unregister operation.  The registration remains
    /// published until `commit_unregister` is called after adapter success.
    pub fn prepare_unregister(
        &mut self,
        request: RseqRegistrationRequest,
    ) -> Result<RseqUnregisterPlan, RseqError> {
        request.validate()?;
        if self.pending.is_some() {
            return Err(RseqError::OperationInProgress);
        }
        let active = match self.registration {
            Some(active) => active,
            None => return Err(RseqError::NotRegistered),
        };
        if active.area_address() != request.area_address()
            || active.area_length() != request.area_length()
        {
            return Err(RseqError::RegistrationMismatch);
        }
        if active.signature() != request.signature() {
            return Err(RseqError::SignatureMismatch);
        }
        let epoch = self.epoch.next()?;
        self.epoch = epoch;
        self.pending = Some(PendingRegistration::Unregister {
            registration: active,
            epoch,
        });
        Ok(RseqUnregisterPlan {
            epoch,
            registration: active,
        })
    }

    /// Commits an unchanged unregister plan after adapter-side success.
    /// Finalization is infallible for a valid reservation token.
    pub fn commit_unregister(&mut self, plan: RseqUnregisterPlan) -> RseqRegistration {
        let pending = self.pending.take();
        assert_eq!(
            pending,
            Some(PendingRegistration::Unregister {
                registration: plan.registration,
                epoch: plan.epoch,
            }),
            "rseq unregister finalize token does not belong to this state"
        );
        assert_eq!(
            self.registration,
            Some(plan.registration),
            "rseq unregister finalize token does not identify the active registration"
        );
        self.registration = None;
        plan.registration
    }

    /// Cancels an unregister reservation after an adapter-side failure.
    pub fn cancel_unregister(&mut self, plan: RseqUnregisterPlan) {
        let pending = self.pending.take();
        assert_eq!(
            pending,
            Some(PendingRegistration::Unregister {
                registration: plan.registration,
                epoch: plan.epoch,
            }),
            "rseq unregister cancel token does not belong to this state"
        );
    }

    /// Convenience unregister spelling over a reserved plan.
    pub fn unregister(&mut self, plan: RseqUnregisterPlan) -> RseqRegistration {
        self.commit_unregister(plan)
    }

    /// Creates the child state for a private-VM fork while preserving the
    /// registration and event mask.  A fresh epoch is reserved in the parent
    /// before the external fork side effect, fencing all later parent plans.
    pub(crate) fn fork_private(&mut self) -> Result<Self, RseqError> {
        if self.pending.is_some() {
            return Err(RseqError::OperationInProgress);
        }
        let epoch = self.epoch.next()?;
        self.epoch = epoch;
        let registration = self.registration.map(|registration| RseqRegistration {
            request: registration.request,
            epoch,
        });
        Ok(Self {
            registration,
            epoch,
            pending: None,
        })
    }

    /// Creates the child state for a `CLONE_VM` thread.  Registration and
    /// events are cleared, while a fresh epoch is reserved in the parent
    /// rather than resetting to zero.
    pub(crate) fn fork_clone_vm(&mut self) -> Result<Self, RseqError> {
        if self.pending.is_some() {
            return Err(RseqError::OperationInProgress);
        }
        let epoch = self.epoch.next()?;
        self.epoch = epoch;
        Ok(Self {
            registration: None,
            epoch,
            pending: None,
        })
    }

    /// Clears registration at a pre-reserved epoch after successful exec.
    pub(crate) fn reset_after_exec(&mut self, epoch: RseqEpoch) -> Option<RseqRegistration> {
        let old = self.registration.take();
        self.epoch = epoch;
        self.pending = None;
        old
    }
}

impl Default for RseqRegistrationState {
    fn default() -> Self {
        Self::new()
    }
}

/// Current lifecycle phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationLifecycle {
    /// No area is currently registered.
    Unregistered,
    /// One area is active.
    Registered,
}

/// Prepare token for registration.  The token is intentionally not `Copy`:
/// one external side effect has exactly one finalize/cancel path.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "finalize or cancel after the adapter-side registration attempt"]
pub struct RseqRegisterPlan {
    epoch: RseqEpoch,
    request: RseqRegistrationRequest,
}

impl RseqRegisterPlan {
    /// Reserved generation epoch.
    pub const fn epoch(&self) -> RseqEpoch {
        self.epoch
    }

    /// Request frozen by this plan.
    pub const fn request(&self) -> RseqRegistrationRequest {
        self.request
    }
}

/// Prepare token for exact unregister.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "finalize or cancel after the adapter-side unregister attempt"]
pub struct RseqUnregisterPlan {
    epoch: RseqEpoch,
    registration: RseqRegistration,
}

impl RseqUnregisterPlan {
    /// Reserved generation epoch.
    pub const fn epoch(&self) -> RseqEpoch {
        self.epoch
    }

    /// Exact active record frozen by this plan.
    pub const fn registration(&self) -> RseqRegistration {
        self.registration
    }
}

/// Concise aliases used by adapters that do not prefix every plan type.
pub type RegisterPlan = RseqRegisterPlan;
pub type UnregisterPlan = RseqUnregisterPlan;
