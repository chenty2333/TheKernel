use core::{mem::MaybeUninit, ops::Range};

use bytemuck::{Pod, Zeroable};
use thekernel_linux_usercopy::{
    CopyStructError, MAX_NUL_SEARCH_BYTES, RawSigevent, UserCopyError, UserMemory,
    UserMemoryContext, VmMutPtr, VmPtr, VmResult, copy_struct_from_user, copy_struct_to_user,
    vm_load, vm_load_any_until_nul, vm_load_any_until_nul_bounded, vm_load_until_nul,
    vm_load_until_nul_bounded, vm_read_slice, vm_write_slice,
};

struct TestMemory {
    bytes: Vec<u8>,
    writable_from: usize,
    reads: usize,
}

impl TestMemory {
    fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
            writable_from: 0x1000,
            reads: 0,
        }
    }

    fn range(&self, start: usize, len: usize) -> VmResult<Range<usize>> {
        let end = start.checked_add(len).ok_or(UserCopyError::BadAddress)?;
        if end > self.bytes.len() {
            return Err(UserCopyError::BadAddress);
        }
        Ok(start..end)
    }
}

// SAFETY: TestMemory treats addresses as checked offsets, never dereferences
// them, and initializes every destination byte before returning success.
unsafe impl UserMemory for TestMemory {
    fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
        let range = self.range(start, dst.len())?;
        self.reads += 1;
        for (output, input) in dst.iter_mut().zip(&self.bytes[range]) {
            output.write(*input);
        }
        Ok(())
    }

    fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
        if start < self.writable_from {
            return Err(UserCopyError::AccessDenied);
        }
        let range = self.range(start, src.len())?;
        self.bytes[range].copy_from_slice(src);
        Ok(())
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
struct Pair {
    first: u64,
    second: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum StructAccess {
    Read(usize, usize),
    Write(usize, Vec<u8>),
}

struct StructMemory {
    bytes: Vec<u8>,
    fail_read_at: Option<usize>,
    fail_write_at: Option<usize>,
    accesses: Vec<StructAccess>,
}

impl StructMemory {
    fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.into(),
            fail_read_at: None,
            fail_write_at: None,
            accesses: Vec::new(),
        }
    }

    fn range(&self, start: usize, len: usize) -> VmResult<Range<usize>> {
        let end = start.checked_add(len).ok_or(UserCopyError::BadAddress)?;
        (end <= self.bytes.len())
            .then_some(start..end)
            .ok_or(UserCopyError::BadAddress)
    }

    fn hits(fault: Option<usize>, start: usize, len: usize) -> bool {
        fault.is_some_and(|fault| start <= fault && fault < start.saturating_add(len))
    }
}

// SAFETY: StructMemory treats user addresses as checked offsets into its
// owned vector and initializes every requested byte on successful reads.
unsafe impl UserMemory for StructMemory {
    fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
        self.accesses.push(StructAccess::Read(start, dst.len()));
        if Self::hits(self.fail_read_at, start, dst.len()) {
            return Err(UserCopyError::BadAddress);
        }
        let range = self.range(start, dst.len())?;
        for (destination, source) in dst.iter_mut().zip(&self.bytes[range]) {
            destination.write(*source);
        }
        Ok(())
    }

    fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
        self.accesses.push(StructAccess::Write(start, src.into()));
        if Self::hits(self.fail_write_at, start, src.len()) {
            return Err(UserCopyError::BadAddress);
        }
        let range = self.range(start, src.len())?;
        self.bytes[range].copy_from_slice(src);
        Ok(())
    }
}

#[test]
fn versioned_copyin_zero_extends_and_checks_unknown_tail_first() {
    let mut short = StructMemory::new(&[1, 0, 0, 0]);
    let value = copy_struct_from_user::<_, Pair>(
        &mut UserMemoryContext::new(&mut short),
        core::ptr::null(),
        4,
    )
    .unwrap();
    assert_eq!(
        value,
        Pair {
            first: 1,
            second: 0
        }
    );

    let mut nonzero_tail = StructMemory::new(&[1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(
        copy_struct_from_user::<_, Pair>(
            &mut UserMemoryContext::new(&mut nonzero_tail),
            core::ptr::null(),
            17,
        ),
        Err(CopyStructError::NonZeroTrailing)
    );
    assert_eq!(nonzero_tail.accesses, [StructAccess::Read(16, 1)]);

    let mut faulting_tail = StructMemory::new(&[0; 17]);
    faulting_tail.fail_read_at = Some(16);
    assert_eq!(
        copy_struct_from_user::<_, Pair>(
            &mut UserMemoryContext::new(&mut faulting_tail),
            core::ptr::null(),
            17,
        ),
        Err(CopyStructError::UserCopy(UserCopyError::BadAddress))
    );
    assert_eq!(faulting_tail.accesses, [StructAccess::Read(16, 1)]);
}

#[test]
fn versioned_copyout_clears_tail_before_prefix_and_reports_loss() {
    let source = Pair {
        first: 1,
        second: 2,
    };
    let mut larger = StructMemory::new(&[0xff; 20]);
    assert!(
        !copy_struct_to_user(
            &mut UserMemoryContext::new(&mut larger),
            core::ptr::null_mut(),
            20,
            &source,
        )
        .unwrap()
    );
    assert_eq!(
        larger.accesses,
        [
            StructAccess::Write(16, vec![0; 4]),
            StructAccess::Write(0, bytemuck::bytes_of(&source).into()),
        ]
    );
    assert_eq!(&larger.bytes[16..], &[0; 4]);

    let mut short = StructMemory::new(&[0; 8]);
    assert!(
        copy_struct_to_user(
            &mut UserMemoryContext::new(&mut short),
            core::ptr::null_mut(),
            8,
            &source,
        )
        .unwrap()
    );

    let mut faulting_tail = StructMemory::new(&[0xff; 20]);
    faulting_tail.fail_write_at = Some(16);
    assert_eq!(
        copy_struct_to_user(
            &mut UserMemoryContext::new(&mut faulting_tail),
            core::ptr::null_mut(),
            20,
            &source,
        ),
        Err(UserCopyError::BadAddress)
    );
    assert_eq!(
        faulting_tail.accesses,
        [StructAccess::Write(16, vec![0; 4])]
    );
}

#[test]
fn raw_sigevent_copyin_preserves_integer_storage_through_context() {
    let mut provider = TestMemory::new(0x8000);
    let base = 0x2003;
    let value_offset = 0;
    let signo_offset = value_offset + core::mem::size_of::<usize>();
    let notify_offset = signo_offset + core::mem::size_of::<i32>();
    let union_offset = notify_offset + core::mem::size_of::<i32>();

    provider.bytes[base + value_offset..base + value_offset + core::mem::size_of::<usize>()]
        .copy_from_slice(&usize::MAX.to_ne_bytes());
    provider.bytes[base + signo_offset..base + signo_offset + core::mem::size_of::<i32>()]
        .copy_from_slice(&64i32.to_ne_bytes());
    provider.bytes[base + notify_offset..base + notify_offset + core::mem::size_of::<i32>()]
        .copy_from_slice(&2i32.to_ne_bytes());
    provider.bytes[base + union_offset..base + union_offset + core::mem::size_of::<i32>()]
        .copy_from_slice(&123i32.to_ne_bytes());

    let mut memory = UserMemoryContext::new(&mut provider);
    let event = RawSigevent::read_from_user(&mut memory, base as *const RawSigevent).unwrap();

    assert_eq!(event.value_ptr_address(), usize::MAX);
    assert_eq!(event.signo(), 64);
    assert_eq!(event.notify(), 2);
    assert_eq!(event.thread_id(), 123);
}

#[test]
fn typed_slice_and_pointer_round_trip_use_one_explicit_context() {
    let mut provider = TestMemory::new(0x8000);
    let mut memory = UserMemoryContext::new(&mut provider);
    let values = [
        Pair {
            first: 1,
            second: 2,
        },
        Pair {
            first: 3,
            second: 4,
        },
    ];
    let ptr = 0x2000usize as *mut Pair;

    vm_write_slice(&mut memory, ptr, &values).unwrap();
    let mut output = [MaybeUninit::<Pair>::uninit(); 2];
    vm_read_slice(&mut memory, ptr, &mut output).unwrap();
    // SAFETY: the successful read initialized both values and Pair is Pod.
    let output = unsafe { core::mem::transmute::<[MaybeUninit<Pair>; 2], [Pair; 2]>(output) };
    assert_eq!(output, values);

    assert_eq!(ptr.vm_read(&mut memory), Ok(values[0]));
    ptr.wrapping_add(1)
        .vm_write(
            &mut memory,
            Pair {
                first: 5,
                second: 6,
            },
        )
        .unwrap();
    assert_eq!(
        ptr.wrapping_add(1).vm_read(&mut memory),
        Ok(Pair {
            first: 5,
            second: 6
        })
    );
}

#[test]
fn typed_usercopy_accepts_linux_unaligned_user_addresses() {
    let mut provider = TestMemory::new(0x8000);
    let mut memory = UserMemoryContext::new(&mut provider);
    let value = Pair {
        first: 0x1122_3344_5566_7788,
        second: 0x99aa_bbcc_ddee_ff00,
    };
    let ptr = 0x2001usize as *mut Pair;

    ptr.vm_write(&mut memory, value).unwrap();
    assert_eq!(ptr.vm_read(&mut memory), Ok(value));
}

#[test]
fn independent_contexts_never_share_an_implicit_provider() {
    let mut first = TestMemory::new(0x4000);
    let mut second = TestMemory::new(0x4000);
    let ptr = 0x1000usize as *mut u32;

    {
        let mut memory = UserMemoryContext::new(&mut first);
        ptr.vm_write(&mut memory, 11).unwrap();
    }
    {
        let mut memory = UserMemoryContext::new(&mut second);
        ptr.vm_write(&mut memory, 22).unwrap();
    }

    let mut first_memory = UserMemoryContext::new(&mut first);
    let mut second_memory = UserMemoryContext::new(&mut second);
    assert_eq!(ptr.vm_read(&mut first_memory), Ok(11));
    assert_eq!(ptr.vm_read(&mut second_memory), Ok(22));
}

#[test]
fn provider_errors_and_address_checks_remain_distinct() {
    let mut provider = TestMemory::new(0x4000);
    let mut memory = UserMemoryContext::new(&mut provider);

    // Linux-style zero-length usercopy performs no provider access and does
    // not validate the otherwise unusable address.
    memory.write_bytes(usize::MAX, &[]).unwrap();
    let reads_before = memory.memory_mut().reads;
    memory.read_bytes(0x200, &mut []).unwrap();
    assert_eq!(memory.memory_mut().reads, reads_before);
    assert_eq!(
        memory.write_bytes(0x100, &[1]),
        Err(UserCopyError::AccessDenied)
    );
    assert_eq!(
        memory.read_bytes(usize::MAX, &mut [MaybeUninit::uninit(); 2]),
        Err(UserCopyError::BadAddress)
    );
}

#[test]
fn owned_load_is_fallible_and_preserves_values() {
    let mut provider = TestMemory::new(0x8000);
    let mut memory = UserMemoryContext::new(&mut provider);
    let ptr = 0x3000usize as *mut u8;
    let value = b"a quick brown fox";

    vm_write_slice(&mut memory, ptr, value).unwrap();
    assert_eq!(vm_load(&mut memory, ptr, value.len()).unwrap(), value);
    assert_eq!(
        memory.load::<u64>(0x1000usize as *const u64, usize::MAX),
        Err(UserCopyError::NoMemory)
    );
}

#[test]
fn nul_load_stops_at_zero_and_enforces_the_byte_bound() {
    let base = 0x1000usize;
    let mut provider = TestMemory::new(base + MAX_NUL_SEARCH_BYTES + 64);
    provider.bytes[base..base + 6].copy_from_slice(b"abc\0de");
    {
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            vm_load_until_nul(&mut memory, base as *const u8).unwrap(),
            b"abc"
        );
    }

    provider.bytes[base..base + MAX_NUL_SEARCH_BYTES].fill(1);
    let mut memory = UserMemoryContext::new(&mut provider);
    assert_eq!(
        vm_load_until_nul(&mut memory, base as *const u8),
        Err(UserCopyError::TooLong)
    );
}

#[test]
fn bounded_nul_budget_includes_the_terminator_and_zero_forbids_access() {
    let base = 0x1800usize;
    let mut provider = TestMemory::new(0x4000);
    provider.bytes[base..base + 3].copy_from_slice(b"ab\0");

    let mut memory = UserMemoryContext::new(&mut provider);
    assert_eq!(
        memory.load_until_nul_bounded(base as *const u8, 3).unwrap(),
        b"ab"
    );
    assert_eq!(
        vm_load_until_nul_bounded(&mut memory, base as *const u8, 2),
        Err(UserCopyError::TooLong)
    );
    let reads_before = memory.memory_mut().reads;
    assert_eq!(
        vm_load_until_nul_bounded(&mut memory, base as *const u8, 0),
        Err(UserCopyError::TooLong)
    );
    assert_eq!(memory.memory_mut().reads, reads_before);
}

#[test]
fn bounded_nul_loader_preserves_raw_pointer_unsafe_boundary() {
    let base = 0x2001usize;
    let mut provider = TestMemory::new(0x8000);
    let raw = [0x4000usize, 0usize];
    provider.bytes[base..base + core::mem::size_of_val(&raw)]
        .copy_from_slice(bytemuck::cast_slice(&raw));
    let mut memory = UserMemoryContext::new(&mut provider);

    // SAFETY: each loaded word is a valid exposed pointer representation and
    // the null pointer is the all-zero sentinel.
    let pointers =
        unsafe { vm_load_any_until_nul_bounded(&mut memory, base as *const *const u8, 2) }.unwrap();
    assert_eq!(pointers, [0x4000usize as *const u8]);
    // The budget includes the sentinel, so one slot cannot hold this array.
    assert_eq!(
        // SAFETY: the provider still exposes the same valid pointer
        // representation and the bounded call never dereferences the user
        // address itself.
        unsafe { vm_load_any_until_nul_bounded(&mut memory, base as *const *const u8, 1) },
        Err(UserCopyError::TooLong)
    );
}

#[test]
fn bounded_nul_loader_propagates_provider_faults() {
    let base = 0x1000usize;
    let mut provider = TestMemory::new(base + 1);
    provider.bytes[base] = 1;
    let mut memory = UserMemoryContext::new(&mut provider);

    assert_eq!(
        vm_load_until_nul_bounded(&mut memory, base as *const u8, 2),
        Err(UserCopyError::BadAddress)
    );
}

#[test]
fn bounded_nul_loader_applies_byte_ceiling_without_overflow() {
    let base = 0x1000usize;
    let mut provider = TestMemory::new(base + MAX_NUL_SEARCH_BYTES + 32);
    provider.bytes[base..base + MAX_NUL_SEARCH_BYTES].fill(1);
    let mut memory = UserMemoryContext::new(&mut provider);

    assert_eq!(
        vm_load_until_nul_bounded(&mut memory, base as *const Pair, usize::MAX),
        Err(UserCopyError::TooLong)
    );
}

#[test]
fn unsafe_nul_loader_supports_raw_pointer_arrays_without_pointer_pod() {
    let base = 0x2001usize;
    let mut provider = TestMemory::new(0x8000);
    let raw = [0x4000usize, 0x5000usize, 0usize];
    provider.bytes[base..base + core::mem::size_of_val(&raw)]
        .copy_from_slice(bytemuck::cast_slice(&raw));
    let mut memory = UserMemoryContext::new(&mut provider);

    // SAFETY: every loaded word is converted into a raw pointer, for which
    // these exposed address representations and the null sentinel are valid.
    let pointers = unsafe { vm_load_any_until_nul(&mut memory, base as *const *const u8) }.unwrap();
    assert_eq!(
        pointers,
        [0x4000usize as *const u8, 0x5000usize as *const u8]
    );
}
