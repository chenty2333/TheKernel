//! Build script for the CPU-abstraction crate.
//!
//! Its only job is dependency tracking.  The assembly sources below are pulled
//! in with `global_asm!(include_str!("..."))`, and Cargo's automatic
//! dependency scanner does not look inside macro invocations: it sees the
//! macro, not the file the macro reads.  Without an explicit
//! `rerun-if-changed`, editing one of these files changes nothing in the
//! build graph, so the crate is not rebuilt and the old machine code is
//! linked into the kernel.
//!
//! That failure is silent and badly misleading.  The edit is in the source,
//! the build succeeds, and the running kernel behaves exactly as before --
//! which reads as "my change had no effect" rather than "my change was never
//! compiled", and sends you looking for a logic error in code that is not
//! even present.

fn main() {
    for source in ["src/x86_64/user_copy.S", "src/x86_64/trap.S"] {
        println!("cargo:rerun-if-changed={source}");
    }
}
