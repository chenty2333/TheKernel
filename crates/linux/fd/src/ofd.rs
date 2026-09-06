use crate::OfdId;

/// Marker for an open file description whose sequential offset remains owned
/// by the consumer's file object.
///
/// This mode is an explicit migration seam for kernels whose VFS already has
/// a single authoritative shared cursor. The consumer must retain the same
/// sleepable OFD lock across cursor inspection, the underlying sequential I/O,
/// and cursor publication. This crate stores no cursor in this mode and the
/// [`OpenFileDescriptionState::offset`],
/// [`OpenFileDescriptionState::commit_seek`], and
/// [`OpenFileDescriptionState::commit_io`] APIs are deliberately unavailable.
///
/// A future VFS integration can move the cursor into this crate by changing
/// the offset authority to the default `u64` mode; consumers must not mirror
/// the external cursor in a second field in the meantime.
///
/// ```
/// use thekernel_linux_fd::{ExternalOffset, OfdId, OpenFileDescriptionState};
///
/// let mut state: OpenFileDescriptionState<u16, ExternalOffset> =
///     OpenFileDescriptionState::new_external(OfdId::new(1).unwrap(), 2, 7);
/// state.commit_status_flags(4);
/// assert_eq!(state.status_flags(), 4);
/// assert_eq!(state.async_owner(), &7);
/// ```
///
/// ```compile_fail,E0599
/// use thekernel_linux_fd::{ExternalOffset, OfdId, OpenFileDescriptionState};
///
/// let state: OpenFileDescriptionState<(), ExternalOffset> =
///     OpenFileDescriptionState::new_external(OfdId::new(1).unwrap(), 0, ());
/// let _ = state.offset();
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ExternalOffset;

/// Shared open-file-description state which must be protected by the
/// consumer's sleepable OFD lock while an offset-dependent I/O is in flight.
///
/// Keeping this type separate from [`DescriptorEntry`](crate::DescriptorEntry)
/// makes `dup`/fork sharing and descriptor-local close-on-exec state explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFileDescriptionState<A = (), O = u64> {
    id: OfdId,
    status_flags: u32,
    offset: O,
    async_owner: A,
}

impl<A> OpenFileDescriptionState<A, u64> {
    /// Creates unpublished OFD state with a crate-owned shared offset.
    pub const fn new(id: OfdId, status_flags: u32, offset: u64, async_owner: A) -> Self {
        Self {
            id,
            status_flags,
            offset,
            async_owner,
        }
    }

    /// Returns the current shared file offset.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Moves the offset for an explicit seek after adapter validation.
    pub fn commit_seek(&mut self, offset: u64) {
        self.offset = offset;
    }

    /// Commits a short or complete sequential I/O result.
    ///
    /// `start` must be the offset retained while the consumer held its
    /// sleepable OFD lock across the underlying operation. A mismatch is an
    /// adapter serialization bug and is reported rather than silently
    /// overwriting a newer offset.
    pub fn commit_io(&mut self, start: u64, transferred: usize) -> Result<u64, OfdOffsetError> {
        if self.offset != start {
            return Err(OfdOffsetError::StaleStart {
                expected: self.offset,
                supplied: start,
            });
        }
        let transferred = u64::try_from(transferred).map_err(|_| OfdOffsetError::Overflow)?;
        let next = start
            .checked_add(transferred)
            .ok_or(OfdOffsetError::Overflow)?;
        self.offset = next;
        Ok(next)
    }
}

impl<A> OpenFileDescriptionState<A, ExternalOffset> {
    /// Creates unpublished OFD state whose cursor remains externally owned.
    ///
    /// The returned state is authoritative for the OFD identity, status flags,
    /// and async owner only. The consumer remains authoritative for the shared
    /// sequential cursor and must serialize it with the same OFD lock.
    pub const fn new_external(id: OfdId, status_flags: u32, async_owner: A) -> Self {
        Self {
            id,
            status_flags,
            offset: ExternalOffset,
            async_owner,
        }
    }
}

impl<A, O> OpenFileDescriptionState<A, O> {
    /// Returns the stable OFD identity.
    pub const fn id(&self) -> OfdId {
        self.id
    }

    /// Returns file status flags shared by all duplicated descriptors.
    pub const fn status_flags(&self) -> u32 {
        self.status_flags
    }

    /// Replaces status flags after the file adapter has accepted every change.
    pub fn commit_status_flags(&mut self, status_flags: u32) {
        self.status_flags = status_flags;
    }

    /// Returns the async-owner state retained by the OFD.
    pub const fn async_owner(&self) -> &A {
        &self.async_owner
    }

    /// Mutates async-owner state while the consumer holds its OFD state lock.
    pub fn async_owner_mut(&mut self) -> &mut A {
        &mut self.async_owner
    }
}

/// Invalid shared-offset publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OfdOffsetError {
    /// The resulting offset would overflow `u64`.
    Overflow,
    /// The adapter did not retain serialization across an offset-based I/O.
    StaleStart {
        /// Offset currently stored by the OFD.
        expected: u64,
        /// Start offset supplied by the completing operation.
        supplied: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn short_io_advances_only_by_transferred_bytes() {
        let id = OfdId::new(1).unwrap();
        let mut state = OpenFileDescriptionState::new(id, 0, 10, ());
        assert_eq!(state.commit_io(10, 3), Ok(13));
        assert_eq!(state.offset(), 13);
    }

    #[test]
    fn stale_or_overflowing_completion_does_not_mutate_offset() {
        let id = OfdId::new(1).unwrap();
        let mut state = OpenFileDescriptionState::new(id, 0, u64::MAX, ());
        assert_eq!(
            state.commit_io(7, 1),
            Err(OfdOffsetError::StaleStart {
                expected: u64::MAX,
                supplied: 7,
            })
        );
        assert_eq!(state.commit_io(u64::MAX, 1), Err(OfdOffsetError::Overflow));
        assert_eq!(state.offset(), u64::MAX);
    }

    #[test]
    fn external_offset_mode_stores_no_shadow_cursor() {
        assert_eq!(size_of::<ExternalOffset>(), 0);

        let id = OfdId::new(7).unwrap();
        let mut state = OpenFileDescriptionState::new_external(id, 3, 11_u16);
        assert_eq!(state.id(), id);
        assert_eq!(state.status_flags(), 3);
        assert_eq!(state.async_owner(), &11);

        state.commit_status_flags(5);
        *state.async_owner_mut() = 13;
        assert_eq!(state.status_flags(), 5);
        assert_eq!(state.async_owner(), &13);
        assert!(
            size_of::<OpenFileDescriptionState<u16, ExternalOffset>>()
                <= size_of::<OpenFileDescriptionState<u16>>()
        );
    }
}
