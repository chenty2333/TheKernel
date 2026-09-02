//! Thin BPF boundary imports.
//!
//! Linux command values and `bpf_attr` layouts live in `thekernel-linux-bpf`.
//! The portable instruction encoding and verifier limits live in `axbpf`.

pub use axbpf::Instruction as BpfInsn;
pub use thekernel_linux_bpf::*;

// These map kinds are present in the Linux UAPI used by the kernel, but the
// small shared BPF-layout crate intentionally does not expose the complete
// map-kind enum.  Keep the kernel-side values explicit so matches remain
// constants rather than accidental variable bindings.
pub const BPF_MAP_TYPE_DEVMAP: u32 = 14;
pub const BPF_MAP_TYPE_CPUMAP: u32 = 16;
pub const BPF_MAP_TYPE_XSKMAP: u32 = 17;
