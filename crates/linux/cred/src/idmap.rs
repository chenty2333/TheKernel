//! Typed kernel/user IDs and immutable user-namespace ID maps.
//!
//! This module deliberately contains no syscall or namespace-publication
//! policy. It is the allocation-aware value layer used by `uid_map`,
//! `gid_map`, and later credential operations.

use alloc::{sync::Arc, vec::Vec};

use crate::CredError;

/// Linux reserves the all-ones ID as an invalid internal value.
const INVALID_ID: u32 = u32::MAX;

/// Linux accepts at most 340 extents in a UID or GID map.
pub const ID_MAP_MAX_EXTENTS: usize = 340;

macro_rules! typed_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u32);

        impl $name {
            /// Constructs an ID unless `raw` is Linux's all-ones invalid
            /// sentinel.
            pub const fn from_raw(raw: u32) -> Option<Self> {
                if raw == INVALID_ID {
                    None
                } else {
                    Some(Self(raw))
                }
            }

            /// Returns the underlying Linux ID value.
            pub const fn into_raw(self) -> u32 {
                self.0
            }
        }
    };
}

typed_id!(Kuid, "A UID in the kernel-global ID space.");
typed_id!(Kgid, "A GID in the kernel-global ID space.");
typed_id!(UserUid, "A UID as observed through one user namespace.");
typed_id!(UserGid, "A GID as observed through one user namespace.");

impl Kuid {
    /// Root's kernel-global UID in the initial user namespace.
    pub const INITIAL_ROOT: Self = Self(0);
}

impl Kgid {
    /// Root's kernel-global GID in the initial user namespace.
    pub const INITIAL_ROOT: Self = Self(0);
}

impl UserUid {
    /// Root's namespace-visible UID.
    pub const ROOT: Self = Self(0);
}

impl UserGid {
    /// Root's namespace-visible GID.
    pub const ROOT: Self = Self(0);
}

/// One userspace map row before its lower range is resolved through the
/// parent namespace.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct IdMapInputExtent {
    /// First ID as observed inside the namespace being configured.
    pub first: u32,
    /// First ID as observed in the parent namespace.
    pub lower_first: u32,
    /// Number of IDs in both half-open ranges.
    pub count: u32,
}

impl IdMapInputExtent {
    /// Constructs an unvalidated user-namespace ID-map row.
    pub const fn new(first: u32, lower_first: u32, count: u32) -> Self {
        Self {
            first,
            lower_first,
            count,
        }
    }
}

/// One validated extent. `lower_first` is always in the kernel-global ID
/// space, never in the parent namespace's userspace view.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct IdMapExtent {
    first: u32,
    lower_first: u32,
    count: u32,
}

impl IdMapExtent {
    fn upper_end(self) -> u32 {
        // Construction proves this addition is valid.
        self.first + self.count
    }

    fn lower_end(self) -> u32 {
        // Construction proves this addition is valid.
        self.lower_first + self.count
    }
}

/// Immutable bidirectional user-namespace ID-map indexes.
///
/// The forward index is ordered by namespace-visible ranges and the reverse
/// index by kernel-global ranges. Readers therefore need neither allocation
/// nor locks.
#[derive(Debug)]
pub struct IdMap {
    forward: Vec<IdMapExtent>,
    reverse: Vec<IdMapExtent>,
}

impl IdMap {
    /// Constructs the empty map installed in a newly created child user
    /// namespace before its one allowed map publication.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::NoMemory`] if the map object cannot be allocated.
    pub fn try_empty() -> Result<Arc<Self>, CredError> {
        Arc::try_new(Self {
            forward: Vec::new(),
            reverse: Vec::new(),
        })
        .map_err(|_| CredError::NoMemory)
    }

    /// Constructs the initial namespace's identity map over every valid ID.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::NoMemory`] if an index cannot be allocated.
    pub fn try_identity() -> Result<Arc<Self>, CredError> {
        let mut input = Vec::new();
        input
            .try_reserve_exact(1)
            .map_err(|_| CredError::NoMemory)?;
        input.push(IdMapExtent {
            first: 0,
            lower_first: 0,
            count: INVALID_ID,
        });
        Self::try_from_kernel_extents(input)
    }

    /// Validates map rows and resolves every parent-visible lower range into
    /// the kernel-global ID space.
    ///
    /// A row must fit wholly in one parent extent. This prevents a single
    /// child row from pretending that a discontinuous parent mapping is a
    /// contiguous global range.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::InvalidInput`] for malformed or overlapping rows,
    /// [`CredError::NotPermitted`] for a valid row outside `parent`, or
    /// [`CredError::NoMemory`] if an index cannot be allocated.
    pub fn try_from_parent(
        input: Vec<IdMapInputExtent>,
        parent: &Self,
    ) -> Result<Arc<Self>, CredError> {
        Self::try_from_parent_slice(&input, parent)
    }

    /// Slice-based form of [`IdMap::try_from_parent`].
    ///
    /// This lets a caller authorize against the original rows after semantic
    /// validation without cloning a fallible userspace-sized vector.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::InvalidInput`] for malformed or overlapping rows,
    /// [`CredError::NotPermitted`] for a valid row outside `parent`, or
    /// [`CredError::NoMemory`] if an index cannot be allocated.
    pub fn try_from_parent_slice(
        input: &[IdMapInputExtent],
        parent: &Self,
    ) -> Result<Arc<Self>, CredError> {
        validate_id_map_input(input)?;

        let mut resolved = Vec::new();
        resolved
            .try_reserve_exact(input.len())
            .map_err(|_| CredError::NoMemory)?;
        for extent in input.iter().copied() {
            let lower_first = parent
                .map_range_to_kernel(extent.lower_first, extent.count)
                // A structurally valid range outside the parent map is an
                // authorization failure, not malformed input.
                .ok_or(CredError::NotPermitted)?;
            resolved.push(IdMapExtent {
                first: extent.first,
                lower_first,
                count: extent.count,
            });
        }
        Self::try_from_kernel_extents(resolved)
    }

    fn try_from_kernel_extents(mut forward: Vec<IdMapExtent>) -> Result<Arc<Self>, CredError> {
        if forward.is_empty() || forward.len() > ID_MAP_MAX_EXTENTS {
            return Err(CredError::InvalidInput);
        }
        for extent in &forward {
            validate_range(extent.first, extent.count)?;
            validate_range(extent.lower_first, extent.count)?;
        }

        forward.sort_unstable_by_key(|extent| extent.first);
        validate_non_overlapping(&forward, |extent| extent.first, |extent| extent.upper_end())?;

        let mut reverse = Vec::new();
        reverse
            .try_reserve_exact(forward.len())
            .map_err(|_| CredError::NoMemory)?;
        reverse.extend_from_slice(&forward);
        reverse.sort_unstable_by_key(|extent| extent.lower_first);
        validate_non_overlapping(
            &reverse,
            |extent| extent.lower_first,
            |extent| extent.lower_end(),
        )?;

        Arc::try_new(Self { forward, reverse }).map_err(|_| CredError::NoMemory)
    }

    /// Returns whether this map contains no ID ranges.
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Returns the number of mapped ranges.
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// Maps a namespace-visible UID into the kernel-global ID space.
    pub fn user_uid_to_kernel(&self, id: UserUid) -> Option<Kuid> {
        self.map_id_to_kernel(id.into_raw())
            .and_then(Kuid::from_raw)
    }

    /// Maps a kernel-global UID into this namespace's visible ID space.
    pub fn kernel_uid_to_user(&self, id: Kuid) -> Option<UserUid> {
        self.map_id_from_kernel(id.into_raw())
            .and_then(UserUid::from_raw)
    }

    /// Maps a namespace-visible GID into the kernel-global ID space.
    pub fn user_gid_to_kernel(&self, id: UserGid) -> Option<Kgid> {
        self.map_id_to_kernel(id.into_raw())
            .and_then(Kgid::from_raw)
    }

    /// Maps a kernel-global GID into this namespace's visible ID space.
    pub fn kernel_gid_to_user(&self, id: Kgid) -> Option<UserGid> {
        self.map_id_from_kernel(id.into_raw())
            .and_then(UserGid::from_raw)
    }

    /// Fallibly snapshots rows as Linux displays them to one reader namespace.
    ///
    /// Stored lower IDs are kernel-global. Linux maps only the first lower ID
    /// through the reader's namespace and preserves the count; it does not
    /// require the entire range to be contiguous in the reader's map. An
    /// unmapped first ID is rendered as the all-ones invalid value.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::NoMemory`] if the row snapshot cannot be allocated.
    pub fn try_extents_for_lower(&self, lower: &Self) -> Result<Vec<IdMapInputExtent>, CredError> {
        let mut rows = Vec::new();
        rows.try_reserve_exact(self.forward.len())
            .map_err(|_| CredError::NoMemory)?;
        for extent in &self.forward {
            let lower_first = lower
                .map_id_from_kernel(extent.lower_first)
                .unwrap_or(INVALID_ID);
            rows.push(IdMapInputExtent {
                first: extent.first,
                lower_first,
                count: extent.count,
            });
        }
        Ok(rows)
    }

    fn map_id_to_kernel(&self, id: u32) -> Option<u32> {
        let extent = find_extent(
            &self.forward,
            id,
            |extent| extent.first,
            |extent| extent.upper_end(),
        )?;
        extent.lower_first.checked_add(id - extent.first)
    }

    fn map_id_from_kernel(&self, id: u32) -> Option<u32> {
        let extent = find_extent(
            &self.reverse,
            id,
            |extent| extent.lower_first,
            |extent| extent.lower_end(),
        )?;
        extent.first.checked_add(id - extent.lower_first)
    }

    fn map_range_to_kernel(&self, first: u32, count: u32) -> Option<u32> {
        let end = valid_range_end(first, count)?;
        let extent = find_extent(
            &self.forward,
            first,
            |extent| extent.first,
            |extent| extent.upper_end(),
        )?;
        if end > extent.upper_end() {
            return None;
        }
        extent.lower_first.checked_add(first - extent.first)
    }
}

/// Validates user-visible map rows without resolving them through a parent.
///
/// Callers can perform this phase before authorization so malformed ranges
/// retain Linux's `EINVAL`-before-`EPERM` error ordering.
///
/// # Errors
///
/// Returns [`CredError::InvalidInput`] for malformed or overlapping rows, or
/// [`CredError::NoMemory`] if validation storage cannot be allocated.
pub fn validate_id_map_input(input: &[IdMapInputExtent]) -> Result<(), CredError> {
    if input.is_empty() || input.len() > ID_MAP_MAX_EXTENTS {
        return Err(CredError::InvalidInput);
    }

    let mut ordered = Vec::new();
    ordered
        .try_reserve_exact(input.len())
        .map_err(|_| CredError::NoMemory)?;
    ordered.extend_from_slice(input);
    for extent in &ordered {
        validate_range(extent.first, extent.count)?;
        validate_range(extent.lower_first, extent.count)?;
    }

    ordered.sort_unstable_by_key(|extent| extent.first);
    for pair in ordered.windows(2) {
        let previous_end =
            valid_range_end(pair[0].first, pair[0].count).ok_or(CredError::InvalidInput)?;
        if previous_end > pair[1].first {
            return Err(CredError::InvalidInput);
        }
    }

    ordered.sort_unstable_by_key(|extent| extent.lower_first);
    for pair in ordered.windows(2) {
        let previous_end =
            valid_range_end(pair[0].lower_first, pair[0].count).ok_or(CredError::InvalidInput)?;
        if previous_end > pair[1].lower_first {
            return Err(CredError::InvalidInput);
        }
    }
    Ok(())
}

fn valid_range_end(first: u32, count: u32) -> Option<u32> {
    if count == 0 || first == INVALID_ID {
        return None;
    }
    let end = first.checked_add(count)?;
    // A half-open end equal to INVALID_ID is valid and represents a range
    // whose last member is INVALID_ID - 1. `checked_add` already rejects a
    // range whose exclusive end would exceed the raw ID domain.
    Some(end)
}

fn validate_range(first: u32, count: u32) -> Result<u32, CredError> {
    valid_range_end(first, count).ok_or(CredError::InvalidInput)
}

fn validate_non_overlapping(
    extents: &[IdMapExtent],
    start: impl Fn(IdMapExtent) -> u32,
    end: impl Fn(IdMapExtent) -> u32,
) -> Result<(), CredError> {
    for pair in extents.windows(2) {
        if end(pair[0]) > start(pair[1]) {
            return Err(CredError::InvalidInput);
        }
    }
    Ok(())
}

fn find_extent(
    extents: &[IdMapExtent],
    id: u32,
    start: impl Fn(IdMapExtent) -> u32,
    end: impl Fn(IdMapExtent) -> u32,
) -> Option<IdMapExtent> {
    let mut left = 0;
    let mut right = extents.len();
    while left < right {
        let middle = left + (right - left) / 2;
        if start(extents[middle]) <= id {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    let candidate = *extents.get(left.checked_sub(1)?)?;
    (id < end(candidate)).then_some(candidate)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{vec, vec::Vec};

    use super::*;

    fn input(first: u32, lower_first: u32, count: u32) -> IdMapInputExtent {
        IdMapInputExtent::new(first, lower_first, count)
    }

    #[test]
    fn typed_ids_reject_the_internal_invalid_sentinel() {
        assert_eq!(Kuid::from_raw(INVALID_ID), None);
        assert_eq!(Kgid::from_raw(INVALID_ID), None);
        assert_eq!(UserUid::from_raw(INVALID_ID), None);
        assert_eq!(UserGid::from_raw(INVALID_ID), None);
        assert_eq!(
            Kuid::from_raw(INVALID_ID - 1).unwrap().into_raw(),
            INVALID_ID - 1
        );
    }

    #[test]
    fn identity_map_covers_every_valid_id_in_both_directions() {
        let map = IdMap::try_identity().unwrap();
        for raw in [0, 1, 65_534, INVALID_ID - 1] {
            let user = UserUid::from_raw(raw).unwrap();
            let kernel = map.user_uid_to_kernel(user).unwrap();
            assert_eq!(kernel.into_raw(), raw);
            assert_eq!(map.kernel_uid_to_user(kernel).unwrap().into_raw(), raw);
        }
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn empty_map_maps_nothing() {
        let map = IdMap::try_empty().unwrap();
        assert!(map.is_empty());
        assert_eq!(map.user_uid_to_kernel(UserUid::from_raw(0).unwrap()), None);
        assert_eq!(map.kernel_gid_to_user(Kgid::from_raw(0).unwrap()), None);
    }

    #[test]
    fn child_map_resolves_parent_ids_and_round_trips() {
        let root = IdMap::try_identity().unwrap();
        let parent = IdMap::try_from_parent(vec![input(100, 10_000, 50)], &root).unwrap();
        let child = IdMap::try_from_parent(vec![input(0, 110, 10)], &parent).unwrap();

        let mapped = child
            .user_uid_to_kernel(UserUid::from_raw(4).unwrap())
            .unwrap();
        assert_eq!(mapped.into_raw(), 10_014);
        assert_eq!(child.kernel_uid_to_user(mapped).unwrap().into_raw(), 4);
        assert_eq!(
            child.user_uid_to_kernel(UserUid::from_raw(10).unwrap()),
            None
        );
        assert_eq!(
            child.try_extents_for_lower(&parent).unwrap(),
            vec![input(0, 110, 10)]
        );
    }

    #[test]
    fn display_maps_only_the_first_lower_id_through_the_viewer() {
        let root = IdMap::try_identity().unwrap();
        let target = IdMap::try_from_parent(vec![input(0, 1_000, 10)], &root).unwrap();
        let partial_viewer = IdMap::try_from_parent(vec![input(77, 1_000, 1)], &root).unwrap();
        let unmapped_viewer = IdMap::try_empty().unwrap();

        assert_eq!(
            target.try_extents_for_lower(&partial_viewer).unwrap(),
            vec![input(0, 77, 10)]
        );
        assert_eq!(
            target.try_extents_for_lower(&unmapped_viewer).unwrap(),
            vec![input(0, INVALID_ID, 10)]
        );
    }

    #[test]
    fn unsorted_rows_build_sorted_forward_and_reverse_indexes() {
        let root = IdMap::try_identity().unwrap();
        let map =
            IdMap::try_from_parent(vec![input(50, 2_000, 10), input(0, 9_000, 5)], &root).unwrap();

        assert_eq!(
            map.user_gid_to_kernel(UserGid::from_raw(52).unwrap())
                .unwrap()
                .into_raw(),
            2_002
        );
        assert_eq!(
            map.kernel_gid_to_user(Kgid::from_raw(9_003).unwrap())
                .unwrap()
                .into_raw(),
            3
        );
    }

    #[test]
    fn zero_length_invalid_id_and_overflow_are_rejected() {
        let root = IdMap::try_identity().unwrap();
        for row in [
            input(0, 0, 0),
            input(INVALID_ID, 0, 1),
            input(0, INVALID_ID, 1),
            input(INVALID_ID - 1, 0, 2),
            input(0, INVALID_ID - 1, 2),
        ] {
            assert_eq!(
                IdMap::try_from_parent(vec![row], &root).unwrap_err(),
                CredError::InvalidInput
            );
        }
    }

    #[test]
    fn overlapping_namespace_ranges_are_rejected() {
        let root = IdMap::try_identity().unwrap();
        let error = IdMap::try_from_parent(vec![input(0, 1_000, 10), input(9, 2_000, 10)], &root)
            .unwrap_err();
        assert_eq!(error, CredError::InvalidInput);
    }

    #[test]
    fn overlapping_kernel_ranges_are_rejected() {
        let root = IdMap::try_identity().unwrap();
        let error = IdMap::try_from_parent(vec![input(0, 1_000, 10), input(100, 1_009, 10)], &root)
            .unwrap_err();
        assert_eq!(error, CredError::InvalidInput);
    }

    #[test]
    fn one_child_row_cannot_cross_discontinuous_parent_extents() {
        let root = IdMap::try_identity().unwrap();
        let parent =
            IdMap::try_from_parent(vec![input(0, 1_000, 5), input(5, 2_000, 5)], &root).unwrap();
        let error = IdMap::try_from_parent(vec![input(0, 3, 4)], &parent).unwrap_err();
        assert_eq!(error, CredError::NotPermitted);
    }

    #[test]
    fn malformed_rows_precede_unmapped_parent_authorization_failure() {
        let empty_parent = IdMap::try_empty().unwrap();
        assert_eq!(
            validate_id_map_input(&[input(0, 0, 0)]),
            Err(CredError::InvalidInput)
        );
        assert_eq!(
            IdMap::try_from_parent(vec![input(0, 0, 1)], &empty_parent).unwrap_err(),
            CredError::NotPermitted
        );
    }

    #[test]
    fn extent_limit_is_exact() {
        let root = IdMap::try_identity().unwrap();
        let mut rows = Vec::new();
        rows.try_reserve_exact(ID_MAP_MAX_EXTENTS + 1).unwrap();
        for index in 0..=ID_MAP_MAX_EXTENTS {
            let id = (index as u32) * 2;
            rows.push(input(id, id, 1));
        }
        assert_eq!(
            IdMap::try_from_parent(rows.clone(), &root).unwrap_err(),
            CredError::InvalidInput
        );
        rows.pop();
        assert_eq!(
            IdMap::try_from_parent(rows, &root).unwrap().len(),
            ID_MAP_MAX_EXTENTS
        );
    }
}
