//! Build script for the x86-64 platform crate.
//!
//! Besides reacting to the generated configuration, this declares the assembly
//! sources as build inputs.  They are pulled in with
//! `global_asm!(include_str!("..."))`, and Cargo's automatic dependency scanner
//! does not look inside macro invocations: it sees the macro, not the file the
//! macro reads.  Without an explicit `rerun-if-changed`, editing one of these
//! files changes nothing in the build graph, so the crate is not rebuilt and
//! the old machine code is linked into the kernel.
//!
//! This matters most for `multiboot.S`, which carries the Multiboot2 header the
//! bootloader reads.  A stale copy of it means the kernel boots with a header
//! nobody wrote: the build succeeds, the image is produced, and the bootloader
//! behaves according to tags that are no longer in the source.

fn main() {
    println!("cargo:rerun-if-env-changed=AX_CONFIG_PATH");
    if let Ok(config_path) = std::env::var("AX_CONFIG_PATH") {
        println!("cargo:rerun-if-changed={config_path}");
    }
    for source in [
        "src/multiboot.S",
        "src/ap_start.S",
        "src/kexec_transition.S",
    ] {
        println!("cargo:rerun-if-changed={source}");
    }
}
