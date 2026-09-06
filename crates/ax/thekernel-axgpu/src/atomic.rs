use crate::{DisplayLimits, Mode, ResourceDescriptor, ResourceHandle, ScanoutId};

/// A fully specified primary-plane image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLayout {
    pub resource: ResourceHandle,
    pub descriptor: ResourceDescriptor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneState {
    pub frame: FrameLayout,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub destination_x: i32,
    pub destination_y: i32,
    pub destination_width: u32,
    pub destination_height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationState {
    pub scanout: ScanoutId,
    pub enabled: bool,
    pub mode: Option<Mode>,
    pub primary_plane: Option<PlaneState>,
}

impl PresentationState {
    pub const fn disabled(scanout: ScanoutId) -> Self {
        Self {
            scanout,
            enabled: false,
            mode: None,
            primary_plane: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicError {
    WrongScanout,
    ModeOutOfRange,
    MissingMode,
    MissingPlane,
    IncompleteDisable,
    InvalidResource,
    InvalidRectangle,
    UnsupportedScaling,
    AdapterFailure,
}

/// Validated, immutable work which an adapter may submit without holding a
/// state lock.  `previous` makes completion publication conditional: a caller
/// only replaces its visible state after adapter submission succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitPlan {
    pub previous: PresentationState,
    pub next: PresentationState,
}

/// Pure first-stage validation.  It has no device or global state and makes no
/// allocation, so a rejected request has no externally observable effects.
#[derive(Clone, Copy, Debug)]
pub struct AtomicPlanner {
    limits: DisplayLimits,
}

impl AtomicPlanner {
    pub const fn new(limits: DisplayLimits) -> Self {
        Self { limits }
    }
    pub const fn limits(self) -> DisplayLimits {
        self.limits
    }

    pub fn plan(
        &self,
        previous: PresentationState,
        next: PresentationState,
    ) -> Result<CommitPlan, AtomicError> {
        if previous.scanout != self.limits.scanout || next.scanout != self.limits.scanout {
            return Err(AtomicError::WrongScanout);
        }
        if !next.enabled {
            if next.mode.is_some() || next.primary_plane.is_some() {
                return Err(AtomicError::IncompleteDisable);
            }
            return Ok(CommitPlan { previous, next });
        }
        let mode = next.mode.ok_or(AtomicError::MissingMode)?;
        if !self.limits.accepts_mode(mode) {
            return Err(AtomicError::ModeOutOfRange);
        }
        let plane = next.primary_plane.ok_or(AtomicError::MissingPlane)?;
        validate_plane(self.limits, mode, plane)?;
        Ok(CommitPlan { previous, next })
    }
}

fn validate_plane(limits: DisplayLimits, mode: Mode, plane: PlaneState) -> Result<(), AtomicError> {
    let descriptor = plane.frame.descriptor;
    if !descriptor.is_well_formed() || descriptor.stride_bytes > limits.max_stride_bytes {
        return Err(AtomicError::InvalidResource);
    }
    let source_right = plane
        .source_x
        .checked_add(plane.source_width)
        .ok_or(AtomicError::InvalidRectangle)?;
    let source_bottom = plane
        .source_y
        .checked_add(plane.source_height)
        .ok_or(AtomicError::InvalidRectangle)?;
    if plane.source_width == 0
        || plane.source_height == 0
        || source_right > descriptor.width
        || source_bottom > descriptor.height
    {
        return Err(AtomicError::InvalidRectangle);
    }
    if plane.destination_x != 0
        || plane.destination_y != 0
        || plane.destination_width != mode.width
        || plane.destination_height != mode.height
    {
        return Err(AtomicError::UnsupportedScaling);
    }
    if plane.source_width != mode.width || plane.source_height != mode.height {
        return Err(AtomicError::UnsupportedScaling);
    }
    Ok(())
}

/// Device transport boundary.  Implementations submit the complete plan
/// atomically or return an error without presenting any part of `next`.
pub trait DisplayAdapter: Send + Sync {
    fn submit(&self, plan: &CommitPlan) -> Result<(), AtomicError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    fn id(value: u32) -> ScanoutId {
        ScanoutId::new(value).unwrap()
    }
    fn resource(value: u32) -> ResourceHandle {
        ResourceHandle::new(value).unwrap()
    }
    fn planner() -> AtomicPlanner {
        AtomicPlanner::new(DisplayLimits {
            scanout: id(1),
            max_width: 1920,
            max_height: 1080,
            max_stride_bytes: 8192,
        })
    }
    fn state() -> PresentationState {
        PresentationState {
            scanout: id(1),
            enabled: true,
            mode: Some(Mode {
                width: 64,
                height: 64,
                refresh_millihz: 60_000,
            }),
            primary_plane: Some(PlaneState {
                frame: FrameLayout {
                    resource: resource(9),
                    descriptor: ResourceDescriptor {
                        bytes: 16_384,
                        width: 64,
                        height: 64,
                        stride_bytes: 256,
                        bytes_per_pixel: 4,
                    },
                },
                source_x: 0,
                source_y: 0,
                source_width: 64,
                source_height: 64,
                destination_x: 0,
                destination_y: 0,
                destination_width: 64,
                destination_height: 64,
            }),
        }
    }

    #[test]
    fn invalid_plan_has_no_state_transition() {
        let before = PresentationState::disabled(id(1));
        let mut invalid = state();
        invalid.primary_plane.as_mut().unwrap().destination_width = 63;
        assert_eq!(
            planner().plan(before, invalid),
            Err(AtomicError::UnsupportedScaling)
        );
        assert_eq!(before, PresentationState::disabled(id(1)));
    }

    struct FailingAdapter(AtomicUsize);
    impl DisplayAdapter for FailingAdapter {
        fn submit(&self, _: &CommitPlan) -> Result<(), AtomicError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(AtomicError::AdapterFailure)
        }
    }
    #[test]
    fn adapter_failure_leaves_caller_owned_visible_state_unchanged() {
        let before = PresentationState::disabled(id(1));
        let plan = planner().plan(before, state()).unwrap();
        let adapter = FailingAdapter(AtomicUsize::new(0));
        assert_eq!(adapter.submit(&plan), Err(AtomicError::AdapterFailure));
        assert_eq!(before, plan.previous);
        assert_eq!(adapter.0.load(Ordering::Relaxed), 1);
    }
}
