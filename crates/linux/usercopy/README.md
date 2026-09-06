# thekernel-linux-usercopy

`thekernel-linux-usercopy` provides bounded and fallible access to a
caller-supplied userspace memory implementation. It is `no_std`, contains no
current-task or address-space global, and never dereferences a userspace
pointer itself.

Typed helpers accept unaligned userspace addresses, matching Linux byte-copy
semantics. Rust alignment applies only to kernel-owned typed storage; address
overflow and provider faults remain explicit errors, and a failed provider
read never exposes partially initialized typed values.

The Rust library name is `thekernel_linux_usercopy`. An existing consumer may
temporarily preserve `use starry_vm::...` with a Cargo dependency alias:

```toml
starry-vm = { package = "thekernel-linux-usercopy", version = "0.1", features = ["alloc"] }
```

The API intentionally differs from the historical crate: every operation
receives an explicit `UserMemoryContext`.

```rust,no_run
use core::mem::MaybeUninit;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmResult};

struct AddressSpace;

// SAFETY: a real implementation must validate the user range and initialize
// every destination byte before returning Ok.
unsafe impl UserMemory for AddressSpace {
    fn read(&mut self, _start: usize, _dst: &mut [MaybeUninit<u8>]) -> VmResult {
        unimplemented!()
    }

    fn write(&mut self, _start: usize, _src: &[u8]) -> VmResult {
        unimplemented!()
    }
}

let mut address_space = AddressSpace;
let mut user = UserMemoryContext::new(&mut address_space);
let mut bytes = [MaybeUninit::<u8>::uninit(); 16];
let _ = user.read_slice(0x1000 as *const u8, &mut bytes);
```

Enable the additive `alloc` feature for fallible owned snapshots and bounded
NUL-terminated array loading. The maximum NUL search is 128 KiB.
