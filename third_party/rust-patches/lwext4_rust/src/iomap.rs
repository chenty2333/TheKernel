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
