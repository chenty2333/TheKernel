use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MappedRunKind {
    Written,
    Hole,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MappedRun {
    pub file_offset: u64,
    pub pblock: u64,
    pub bytes: usize,
    pub kind: MappedRunKind,
    pub seq: u64,
}

impl MappedRun {
    pub fn end_offset(&self) -> u64 {
        self.file_offset + self.bytes as u64
    }
}

/// The allocation state of one filesystem extent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtentState {
    Written,
    Unwritten,
}

/// One allocated extent returned by the low-level ext4 scan. An unwritten
/// allocation remains physical; holes are omitted. This type contains no
/// Linux-specific flags, layouts, or pointers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub state: ExtentState,
}

impl Extent {
    pub const fn new(logical: u64, physical: u64, length: u64, state: ExtentState) -> Self {
        Self {
            logical,
            physical,
            length,
            state,
        }
    }
}

/// Result of one ext4 extent scan. The scanner always walks the complete
/// clipped range, even when only a prefix is retained, so `complete` and the
/// final-extent flag are trustworthy.
#[derive(Debug, Eq, PartialEq)]
pub struct ExtentMap {
    pub extents: Vec<Extent>,
    pub mapped_extents: u32,
    pub complete: bool,
    /// Whether the scanned range reached the current end of the file.
    pub reaches_eof: bool,
    /// First and last mapped extents in the scanned range.  They let an
    /// adapter split a long query into bounded lock-held chunks without
    /// double-counting an extent that crosses a chunk boundary.
    pub first_extent: Option<Extent>,
    pub last_extent: Option<Extent>,
}

impl ExtentMap {
    pub fn new(extents: Vec<Extent>, mapped_extents: u32, complete: bool) -> Self {
        Self {
            extents,
            mapped_extents,
            complete,
            reaches_eof: false,
            first_extent: None,
            last_extent: None,
        }
    }

    pub fn with_bounds(
        extents: Vec<Extent>,
        mapped_extents: u32,
        complete: bool,
        reaches_eof: bool,
        first_extent: Option<Extent>,
        last_extent: Option<Extent>,
    ) -> Self {
        Self {
            extents,
            mapped_extents,
            complete,
            reaches_eof,
            first_extent,
            last_extent,
        }
    }

    pub fn into_parts(self) -> (Vec<Extent>, u32, bool, bool) {
        (
            self.extents,
            self.mapped_extents,
            self.complete,
            self.reaches_eof,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn count_only_result_keeps_exact_count_without_extent_storage() {
        let result = ExtentMap::new(Vec::new(), 3, true);
        assert!(result.extents.is_empty());
        assert_eq!(result.mapped_extents, 3);
        assert!(result.complete);
    }

    #[test]
    fn bounded_result_reports_retained_prefix_and_byte_sizes() {
        let prefix = vec![Extent::new(4096, 8192, 4096, ExtentState::Written)];
        let result = ExtentMap::new(prefix, 1, false);
        assert_eq!(result.extents.len(), 1);
        assert_eq!(result.extents[0].logical, 4096);
        assert_eq!(result.extents[0].physical, 8192);
        assert_eq!(result.extents[0].length, 4096);
        assert_eq!(result.mapped_extents, 1);
        assert!(!result.complete);
    }

    #[test]
    fn typed_result_represents_written_extents_only_and_preserves_holes() {
        let result = ExtentMap::new(
            vec![
                Extent::new(0, 0x1000, 4096, ExtentState::Written),
                Extent::new(8192, 0x3000, 4096, ExtentState::Written),
            ],
            2,
            true,
        );
        assert_eq!(result.extents.len(), 2);
        assert_eq!(result.extents[1].logical - result.extents[0].logical, 8192);
        assert_eq!(result.extents[1].state, ExtentState::Written);
    }

    #[test]
    fn unwritten_extent_is_an_allocated_flagged_run() {
        let extent = Extent::new(0x2000, 0x9000, 0x1000, ExtentState::Unwritten);
        assert_ne!(extent.physical, 0);
        assert_eq!(extent.state, ExtentState::Unwritten);
    }

    #[test]
    fn bounded_result_exposes_scan_edges_without_retaining_a_count_buffer() {
        let first = Extent::new(0, 0x1000, 0x1000, ExtentState::Written);
        let last = Extent::new(0x4000, 0x5000, 0x1000, ExtentState::Unwritten);
        let result = ExtentMap::with_bounds(Vec::new(), 2, true, true, Some(first), Some(last));
        assert!(result.extents.is_empty());
        assert_eq!(result.first_extent, Some(first));
        assert_eq!(result.last_extent, Some(last));
        assert!(result.reaches_eof);
    }
}
