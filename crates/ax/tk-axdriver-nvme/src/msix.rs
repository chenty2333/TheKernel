//! PCI MSI-X capability/table layout; register facts from PCI/NVMe, no PCI policy.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Layout {
    pub bir: u8,
    pub offset: usize,
    pub vectors: usize,
}
impl Layout {
    pub fn decode(control: u16, table: u32) -> Option<Self> {
        let bir = (table & 7) as u8;
        (bir < 6).then_some(Self {
            bir,
            offset: (table & !7) as usize,
            vectors: usize::from(control & 0x7ff) + 1,
        })
    }
    pub fn fits(self, bytes: usize) -> bool {
        self.offset
            .checked_add(self.vectors * 16)
            .is_some_and(|end| end <= bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_complete_table_not_just_entry_zero() {
        let layout = Layout::decode(64, 0x2004).unwrap();
        assert_eq!(layout.bir, 4);
        assert_eq!(layout.vectors, 65);
        assert!(!layout.fits(0x2010));
        assert!(layout.fits(0x2410));
        assert!(Layout::decode(0, 6).is_none());
    }
}
