//! Main ELF image metadata, not a union of every executable VMA.
//! Range facts: Linux 7.2.3 fs/binfmt_elf.c load_elf_binary.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ElfBounds {
    pub(crate) start_code: usize,
    pub(crate) end_code: usize,
    pub(crate) start_data: usize,
    pub(crate) end_data: usize,
    has_code: bool,
}

impl ElfBounds {
    /// Observe one validated PT_LOAD from the main executable. `end` is
    /// the initialized file extent, not p_memsz or a synthetic mapping end.
    pub(crate) fn observe_load(&mut self, start: usize, end: usize, executable: bool) {
        if executable {
            if !self.has_code || start < self.start_code {
                self.start_code = start;
            }
            self.end_code = self.end_code.max(end);
            self.has_code = true;
        }
        // Linux's start_data records the highest main PT_LOAD start; end_data
        // is the highest initialized end. BSS and brk have separate metadata.
        self.start_data = self.start_data.max(start);
        self.end_data = self.end_data.max(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_load_ranges_exclude_other_images_and_keep_initialized_ends() {
        let mut bounds = ElfBounds::default();
        bounds.observe_load(0x400000, 0x400200, false);
        bounds.observe_load(0x401000, 0x401234, true);
        bounds.observe_load(0x402000, 0x402800, false);
        assert_eq!((bounds.start_code, bounds.end_code), (0x401000, 0x401234));
        assert_eq!((bounds.start_data, bounds.end_data), (0x402000, 0x402800));
        // A trampoline/ldso is not passed to this main-image accumulator.
        assert!(bounds.end_code < 0x60000000);
    }

    #[test]
    fn executable_segments_use_min_start_and_max_initialized_end() {
        let mut bounds = ElfBounds::default();
        bounds.observe_load(0x3000, 0x3100, true);
        bounds.observe_load(0x1000, 0x1900, true);
        assert_eq!((bounds.start_code, bounds.end_code), (0x1000, 0x3100));
    }
}
