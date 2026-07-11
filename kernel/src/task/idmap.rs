//! Typed kernel/user IDs and immutable user-namespace ID maps.
//!
//! This module deliberately contains no syscall or namespace-publication
//! policy. It is the allocation-aware value layer used by the later
//! `uid_map`, `gid_map`, and credential migration slices.

use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};

/// Linux reserves the all-ones ID as an invalid internal value.
const INVALID_ID: u32 = u32::MAX;

/// Linux accepts at most 340 extents in a UID or GID map.
pub(crate) const ID_MAP_MAX_EXTENTS: usize = 340;

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub(crate) struct $name(u32);

        impl $name {
            pub(crate) const fn from_raw(raw: u32) -> Option<Self> {
                if raw == INVALID_ID {
                    None
                } else {
                    Some(Self(raw))
                }
            }

            pub(crate) const fn into_raw(self) -> u32 {
                self.0
            }
        }
    };
}

typed_id!(Kuid);
typed_id!(Kgid);
typed_id!(UserUid);
typed_id!(UserGid);

/// One userspace map row before its lower range is resolved through the
/// parent namespace.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct IdMapInputExtent {
    /// First ID as observed inside the namespace being configured.
    pub(crate) first: u32,
    /// First ID as observed in the parent namespace.
    pub(crate) lower_first: u32,
    /// Number of IDs in both half-open ranges.
    pub(crate) count: u32,
}

impl IdMapInputExtent {
    pub(crate) const fn new(first: u32, lower_first: u32, count: u32) -> Self {
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

/// Immutable bidirectional ID-map indexes.
///
/// `forward` is ordered by the namespace-visible range and `reverse` by the
/// kernel-global range. Readers therefore need neither allocation nor locks.
#[derive(Debug)]
pub(crate) struct IdMap {
    forward: Vec<IdMapExtent>,
    reverse: Vec<IdMapExtent>,
}

impl IdMap {
    /// Constructs the empty map installed in a newly created child user
    /// namespace before procfs publishes its one allowed map write.
    pub(crate) fn try_empty() -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            forward: Vec::new(),
            reverse: Vec::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Constructs the initial namespace's identity map over every valid ID.
    pub(crate) fn try_identity() -> AxResult<Arc<Self>> {
        let mut input = Vec::new();
        input.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
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
    pub(crate) fn try_from_parent(
        input: Vec<IdMapInputExtent>,
        parent: &Self,
    ) -> AxResult<Arc<Self>> {
        Self::try_from_parent_slice(&input, parent)
    }

    /// Slice-based constructor for callers which must authorize against the
    /// original rows after semantic validation without cloning a fallible
    /// userspace-sized vector.
    pub(crate) fn try_from_parent_slice(
        input: &[IdMapInputExtent],
        parent: &Self,
    ) -> AxResult<Arc<Self>> {
        validate_id_map_input(input)?;

        let mut resolved = Vec::new();
        resolved
            .try_reserve_exact(input.len())
            .map_err(|_| AxError::NoMemory)?;
        for extent in input.iter().copied() {
            let lower_first = parent
                .map_range_to_kernel(extent.lower_first, extent.count)
                // Linux reports a syntactically and structurally valid range
                // outside the parent map as an authorization failure.
                .ok_or(AxError::OperationNotPermitted)?;
            resolved.push(IdMapExtent {
                first: extent.first,
                lower_first,
                count: extent.count,
            });
        }
        Self::try_from_kernel_extents(resolved)
    }

    fn try_from_kernel_extents(mut forward: Vec<IdMapExtent>) -> AxResult<Arc<Self>> {
        if forward.is_empty() || forward.len() > ID_MAP_MAX_EXTENTS {
            return Err(AxError::InvalidInput);
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
            .map_err(|_| AxError::NoMemory)?;
        reverse.extend_from_slice(&forward);
        reverse.sort_unstable_by_key(|extent| extent.lower_first);
        validate_non_overlapping(
            &reverse,
            |extent| extent.lower_first,
            |extent| extent.lower_end(),
        )?;

        Arc::try_new(Self { forward, reverse }).map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.forward.len()
    }

    pub(crate) fn user_uid_to_kernel(&self, id: UserUid) -> Option<Kuid> {
        self.map_id_to_kernel(id.into_raw())
            .and_then(Kuid::from_raw)
    }

    pub(crate) fn kernel_uid_to_user(&self, id: Kuid) -> Option<UserUid> {
        self.map_id_from_kernel(id.into_raw())
            .and_then(UserUid::from_raw)
    }

    pub(crate) fn user_gid_to_kernel(&self, id: UserGid) -> Option<Kgid> {
        self.map_id_to_kernel(id.into_raw())
            .and_then(Kgid::from_raw)
    }

    pub(crate) fn kernel_gid_to_user(&self, id: Kgid) -> Option<UserGid> {
        self.map_id_from_kernel(id.into_raw())
            .and_then(UserGid::from_raw)
    }

    /// Fallibly snapshots rows as Linux displays them to one reader namespace.
    ///
    /// Stored lower IDs are kernel-global. `uid_m_show()`/`gid_m_show()` map
    /// only the first lower ID through `seq_user_ns()` and preserve the count;
    /// they do not require the entire range to be contiguous in the reader's
    /// map. An unmapped first ID is rendered as the all-ones invalid value.
    pub(crate) fn try_extents_for_lower(&self, lower: &Self) -> AxResult<Vec<IdMapInputExtent>> {
        let mut rows = Vec::new();
        rows.try_reserve_exact(self.forward.len())
            .map_err(|_| AxError::NoMemory)?;
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

/// Validates the user-visible map rows without resolving them through the
/// parent namespace. Procfs performs this phase before `new_idmap_permitted()`
/// so malformed ranges retain Linux's EINVAL-before-EPERM error ordering.
pub(crate) fn validate_id_map_input(input: &[IdMapInputExtent]) -> AxResult<()> {
    if input.is_empty() || input.len() > ID_MAP_MAX_EXTENTS {
        return Err(AxError::InvalidInput);
    }

    let mut ordered = Vec::new();
    ordered
        .try_reserve_exact(input.len())
        .map_err(|_| AxError::NoMemory)?;
    ordered.extend_from_slice(input);
    for extent in &ordered {
        validate_range(extent.first, extent.count)?;
        validate_range(extent.lower_first, extent.count)?;
    }

    ordered.sort_unstable_by_key(|extent| extent.first);
    for pair in ordered.windows(2) {
        let previous_end =
            valid_range_end(pair[0].first, pair[0].count).ok_or(AxError::InvalidInput)?;
        if previous_end > pair[1].first {
            return Err(AxError::InvalidInput);
        }
    }

    ordered.sort_unstable_by_key(|extent| extent.lower_first);
    for pair in ordered.windows(2) {
        let previous_end =
            valid_range_end(pair[0].lower_first, pair[0].count).ok_or(AxError::InvalidInput)?;
        if previous_end > pair[1].lower_first {
            return Err(AxError::InvalidInput);
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
    // whose last member is INVALID_ID - 1. No range may include INVALID_ID.
    (end <= INVALID_ID).then_some(end)
}

fn validate_range(first: u32, count: u32) -> AxResult<u32> {
    valid_range_end(first, count).ok_or(AxError::InvalidInput)
}

fn validate_non_overlapping(
    extents: &[IdMapExtent],
    start: impl Fn(IdMapExtent) -> u32,
    end: impl Fn(IdMapExtent) -> u32,
) -> AxResult<()> {
    for pair in extents.windows(2) {
        if end(pair[0]) > start(pair[1]) {
            return Err(AxError::InvalidInput);
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
                AxError::InvalidInput
            );
        }
    }

    #[test]
    fn overlapping_namespace_ranges_are_rejected() {
        let root = IdMap::try_identity().unwrap();
        let error = IdMap::try_from_parent(vec![input(0, 1_000, 10), input(9, 2_000, 10)], &root)
            .unwrap_err();
        assert_eq!(error, AxError::InvalidInput);
    }

    #[test]
    fn overlapping_kernel_ranges_are_rejected() {
        let root = IdMap::try_identity().unwrap();
        let error = IdMap::try_from_parent(vec![input(0, 1_000, 10), input(100, 1_009, 10)], &root)
            .unwrap_err();
        assert_eq!(error, AxError::InvalidInput);
    }

    #[test]
    fn one_child_row_cannot_cross_discontinuous_parent_extents() {
        let root = IdMap::try_identity().unwrap();
        let parent =
            IdMap::try_from_parent(vec![input(0, 1_000, 5), input(5, 2_000, 5)], &root).unwrap();
        let error = IdMap::try_from_parent(vec![input(0, 3, 4)], &parent).unwrap_err();
        assert_eq!(error, AxError::OperationNotPermitted);
    }

    #[test]
    fn malformed_rows_precede_unmapped_parent_authorization_failure() {
        let empty_parent = IdMap::try_empty().unwrap();
        assert_eq!(
            validate_id_map_input(&[input(0, 0, 0)]),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            IdMap::try_from_parent(vec![input(0, 0, 1)], &empty_parent).unwrap_err(),
            AxError::OperationNotPermitted
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
            AxError::InvalidInput
        );
        rows.pop();
        assert_eq!(
            IdMap::try_from_parent(rows, &root).unwrap().len(),
            ID_MAP_MAX_EXTENTS
        );
    }
}
