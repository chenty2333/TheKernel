# tk-scope-local

Scope-local storage for TheKernel's x86_64 kernel. This is a `no_std` library,
not a userspace thread-local storage replacement. It uses the repository's
pinned nightly toolchain; the inherited Rust version is not a stable-support
promise.

## Integration

The final image must retain the `scope_local` metadata section and provide
`__start_scope_local` / `__stop_scope_local` section bounds, as well as the
per-CPU storage required by `percpu`. TheKernel's HAL linker script provides
these sections. A successful `cargo check` does not establish that a downstream
image supplies this linkage or initializes per-CPU storage correctly.

The packaged `percpu.x` script and Linux-only test linker arguments support
this crate's own host tests. Cargo does not propagate test linker arguments to
downstream applications; they must supply their own final-image linker setup.
