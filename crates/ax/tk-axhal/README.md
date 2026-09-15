# axhal

ArceOS hardware abstraction layer for TheKernel's x86_64 platform: unified
APIs for CPU, platform, paging, IRQ, and related hardware operations.

Depends on axconfig, axplat, and axcpu. TheKernel maintains the x86_64 path;
the host-test dummy backend remains available for Cargo tests.

## Linking

For non-dummy platforms, the build script generates `linker_<platform>.lds`
inside Cargo's `OUT_DIR`. Set `AX_LINKER_SCRIPT_OUTPUT` to an explicit output
path when integrating a final kernel image; TheKernel's build tool does this.
The final image must pass the generated script to its linker. `DWARF=y`
retains the script's DWARF layout sections. These are kernel integration
requirements, not ordinary userspace library linkage.

## License

GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0
