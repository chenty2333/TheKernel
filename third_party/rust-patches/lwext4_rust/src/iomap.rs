use alloc::vec::Vec;

/// Upper bound on extents retained by one typed FIEMAP query.
pub const MAX_FIEMAP_EXTENTS: usize = 4096;

/// Maximum byte offset supported by the x86_64 Linux file ABI.  ext4's
/// `s_maxbytes` is bounded by `MAX_LFS_FILESIZE` on this target.
pub const FIEMAP_MAX_BYTES: u64 = i64::MAX as u64;

/// FIEMAP's stable `LAST` bit, kept as a typed lower-layer flag rather than a
/// dependency on Linux userspace headers.
pub const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;

/// Physical blocks allocated for delayed/unwritten data are still extents;
/// callers must not report them as holes.
pub const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0000_0800;

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

/// One allocated extent returned by the low-level ext4 FIEMAP scan.  An
/// unwritten allocation remains physical and is marked in `flags`; holes are
/// omitted.  This type contains no Linux userspace representation or pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FiemapExtent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub flags: u32,
}

impl FiemapExtent {
    pub const fn new(logical: u64, physical: u64, length: u64, flags: u32) -> Self {
        Self {
            logical,
            physical,
            length,
            flags,
        }
    }
}

/// Result of one ext4 FIEMAP scan.  The scanner always walks the complete
/// clipped range, even when only a prefix is retained, so `complete` and the
/// final-extent flag are trustworthy.
#[derive(Debug, Eq, PartialEq)]
pub struct FiemapResult {
    pub extents: Vec<FiemapExtent>,
    pub mapped_extents: u32,
    pub complete: bool,
    /// First and last mapped extents in the scanned range.  They let an
    /// adapter split a long query into bounded lock-held chunks without
    /// double-counting an extent that crosses a chunk boundary.
    pub first_extent: Option<FiemapExtent>,
    pub last_extent: Option<FiemapExtent>,
}

impl FiemapResult {
    pub fn new(extents: Vec<FiemapExtent>, mapped_extents: u32, complete: bool) -> Self {
        Self {
            extents,
            mapped_extents,
            complete,
            first_extent: None,
            last_extent: None,
        }
    }

    pub fn with_bounds(
        extents: Vec<FiemapExtent>,
        mapped_extents: u32,
        complete: bool,
        first_extent: Option<FiemapExtent>,
        last_extent: Option<FiemapExtent>,
    ) -> Self {
        Self {
            extents,
            mapped_extents,
            complete,
            first_extent,
            last_extent,
        }
    }

    pub fn into_parts(self) -> (Vec<FiemapExtent>, u32, bool) {
        (self.extents, self.mapped_extents, self.complete)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn count_only_result_keeps_exact_count_without_extent_storage() {
        let result = FiemapResult::new(Vec::new(), 3, true);
        assert!(result.extents.is_empty());
        assert_eq!(result.mapped_extents, 3);
        assert!(result.complete);
    }

    #[test]
    fn bounded_result_reports_retained_prefix_and_byte_sizes() {
        let prefix = vec![FiemapExtent::new(4096, 8192, 4096, 0)];
        let result = FiemapResult::new(prefix, 1, false);
        assert_eq!(result.extents.len(), 1);
        assert_eq!(result.extents[0].logical, 4096);
        assert_eq!(result.extents[0].physical, 8192);
        assert_eq!(result.extents[0].length, 4096);
        assert_eq!(result.mapped_extents, 1);
        assert!(!result.complete);
    }

    #[test]
    fn typed_result_represents_written_extents_only_and_preserves_holes() {
        let result = FiemapResult::new(
            vec![
                FiemapExtent::new(0, 0x1000, 4096, 0),
                FiemapExtent::new(8192, 0x3000, 4096, FIEMAP_EXTENT_LAST),
            ],
            2,
            true,
        );
        assert_eq!(result.extents.len(), 2);
        assert_eq!(result.extents[1].logical - result.extents[0].logical, 8192);
        assert_eq!(result.extents[1].flags, FIEMAP_EXTENT_LAST);
    }

    #[test]
    fn unwritten_extent_is_an_allocated_flagged_run() {
        let extent = FiemapExtent::new(0x2000, 0x9000, 0x1000, FIEMAP_EXTENT_UNWRITTEN);
        assert_ne!(extent.physical, 0);
        assert_eq!(FIEMAP_EXTENT_UNWRITTEN, 0x0000_0800);
        assert_eq!(extent.flags, FIEMAP_EXTENT_UNWRITTEN);
    }

    #[test]
    fn bounded_result_exposes_scan_edges_without_retaining_a_count_buffer() {
        let first = FiemapExtent::new(0, 0x1000, 0x1000, 0);
        let last = FiemapExtent::new(0x4000, 0x5000, 0x1000, FIEMAP_EXTENT_UNWRITTEN);
        let result = FiemapResult::with_bounds(Vec::new(), 2, true, Some(first), Some(last));
        assert!(result.extents.is_empty());
        assert_eq!(result.first_extent, Some(first));
        assert_eq!(result.last_extent, Some(last));
    }
}
