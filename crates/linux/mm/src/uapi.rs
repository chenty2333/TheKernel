use bytemuck::{Pod, Zeroable};

/// Linux x86_64 `struct iovec` as it appears in userspace memory.
///
/// The base is an integer address so every bit pattern remains valid while
/// this ABI crate stays independent of kernel pointer semantics. The root
/// kernel converts it to a pointer only at the usercopy boundary.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod, Zeroable)]
pub struct IoVec {
    pub iov_base: u64,
    pub iov_len: i64,
}

const _: () = assert!(core::mem::size_of::<IoVec>() == 16);
const _: () = assert!(core::mem::align_of::<IoVec>() == 8);

#[cfg(test)]
mod tests {
    use super::IoVec;

    #[test]
    fn iovec_has_linux_x86_64_layout() {
        assert_eq!(core::mem::size_of::<IoVec>(), 16);
        assert_eq!(core::mem::align_of::<IoVec>(), 8);
    }
}
