use core::num::NonZeroU32;

/// An adapter-issued opaque reference to a GPU resource.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ResourceHandle(NonZeroU32);

impl ResourceHandle {
    /// Creates a handle from an adapter namespace value; zero is reserved.
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the value only for adapter-side table lookup.
    pub const fn raw(self) -> u32 {
        self.0.get()
    }
}

/// Layout metadata required for a resource to be selected for presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceDescriptor {
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub bytes_per_pixel: u8,
}

impl ResourceDescriptor {
    /// Checks that the declared linear image fits entirely inside the resource.
    pub fn is_well_formed(self) -> bool {
        let row = u64::from(self.width).checked_mul(u64::from(self.bytes_per_pixel));
        let used = u64::from(self.height).checked_mul(u64::from(self.stride_bytes));
        self.width != 0
            && self.height != 0
            && self.bytes_per_pixel != 0
            && row.is_some_and(|row| row <= u64::from(self.stride_bytes))
            && used.is_some_and(|used| used <= self.bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceError {
    Exhausted,
    InvalidDescriptor,
    UnknownHandle,
    BackendFailure,
}

/// Resource lifetime boundary supplied by an OS or device-specific adapter.
///
/// Implementations must make `release` idempotent with respect to ownership:
/// a resource is released exactly once by the owner that received it.
pub trait ResourceProvider: Send + Sync {
    fn allocate(&self, descriptor: ResourceDescriptor) -> Result<ResourceHandle, ResourceError>;
    fn describe(&self, handle: ResourceHandle) -> Result<ResourceDescriptor, ResourceError>;
    fn release(&self, handle: ResourceHandle) -> Result<(), ResourceError>;
}
