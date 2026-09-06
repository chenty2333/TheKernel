//! Initialize the user stack for the application
//!
//! The structure of the user stack is described in the following figure:
//! position            content                     size (bytes) + comment
//!   ------------------------------------------------------------------------
//! stack pointer ->  [ argc = number of args ]     8
//!                   [ argv[0] (pointer) ]         8   (program name)
//!                   [ argv[1] (pointer) ]         8
//!                   [ argv[..] (pointer) ]        8 * x
//!                   [ argv[n - 1] (pointer) ]     8
//!                   [ argv[n] (pointer) ]         8   (= NULL)
//!                   [ envp[0] (pointer) ]         8
//!                   [ envp[1] (pointer) ]         8
//!                   [ envp[..] (pointer) ]        8
//!                   [ envp[term] (pointer) ]      8   (= NULL)
//!                   [ auxv[0] (Elf32_auxv_t) ]    16
//!                   [ auxv[1] (Elf32_auxv_t) ]    16
//!                   [ auxv[..] (Elf32_auxv_t) ]   16
//!                   [ auxv[term] (Elf32_auxv_t) ] 16  (= AT_NULL vector)
//!                   [ padding ]                   0 - 16
//!                   [ argument ASCIIZ strings ]   >= 0
//!                   [ environment ASCIIZ str. ]   >= 0
//!
//! (0xbffffff8)      [ end marker ]                8   (= NULL)
//!
//! (0xc0000000)      < bottom of stack >           0   (virtual)
//!
//! More details can be found in the link: <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>

use alloc::{collections::VecDeque, vec::Vec};

use zerocopy::IntoBytes;

use crate::auxv::{AuxEntry, AuxType};

/// Generate initial stack frame for user stack
///
/// # Arguments
///
/// * `args` - Arguments of the application
/// * `envs` - Environment variables of the application
/// * `auxv` - Auxiliary vectors of the application
/// * `execfn` - Executable path used for AT_EXECFN
/// * `sp`   - Highest address of the stack
///
/// # Return
///
/// * [`Vec<u8>`] - Initial stack frame of the application
///
/// # Notes
///
/// The detailed format is described in <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>
pub fn app_stack_region(
    args: &[Vec<u8>],
    envs: &[Vec<u8>],
    auxv: &[AuxEntry],
    execfn: &[u8],
    sp: usize,
) -> Vec<u8> {
    let empty_argv = [Vec::new()];
    let args = if args.is_empty() {
        &empty_argv[..]
    } else {
        args
    };
    let mut data = VecDeque::new();
    let mut push = |src: &[u8]| -> usize {
        data.extend(src.iter().cloned());
        data.rotate_right(src.len());
        sp - data.len()
    };

    // define a random string with 16 bytes
    let random_str_pos = push(b"0123456789abcdef");
    push(b"\0");
    let execfn_pos = push(execfn);
    // Push arguments and environment variables
    let envs_slice: Vec<_> = envs
        .iter()
        .map(|env| {
            push(b"\0");
            push(env)
        })
        .collect();
    let argv_slice: Vec<_> = args
        .iter()
        .map(|arg| {
            push(b"\0");
            push(arg)
        })
        .collect();
    let padding_null = [0u8; 16];
    let null_word = [0u8; 8];
    let sp = push(&null_word);

    push(&padding_null[..sp % 16]);

    // Align stack to 16 bytes by padding if needed.
    // We will push following 8-byte items into stack:
    // - auxv (each entry is 2 * usize, so item count = auxv.len() * 2)
    // - envp (len + 1 for NULL terminator)
    // - argv (len + 1 for NULL terminator)
    // - argc (1 item)
    // Total items = auxv.len() * 2 + (envs.len() + 1) + (args.len() + 1) + 1
    //             = auxv.len() * 2 + envs.len() + args.len() + 3
    // If odd, the stack top will not be aligned to 16 bytes unless we add 8-byte
    // padding
    if (envs.len() + args.len() + 3) & 1 != 0 {
        push(&null_word);
    }

    // Push auxiliary vectors
    let mut has_random = false;
    let mut has_execfn = false;
    for entry in auxv.iter() {
        if entry.get_type() == AuxType::RANDOM {
            has_random = true;
        }
        if entry.get_type() == AuxType::EXECFN {
            has_execfn = true;
        }
        if has_random && has_execfn {
            break;
        }
    }
    push(AuxEntry::new(AuxType::NULL, 0).as_bytes());
    if !has_execfn {
        push(AuxEntry::new(AuxType::EXECFN, execfn_pos).as_bytes());
    }
    if !has_random {
        push(AuxEntry::new(AuxType::RANDOM, random_str_pos).as_bytes());
    }
    push(auxv.as_bytes());

    // Push the argv and envp pointers
    push(&null_word);
    push(envs_slice.as_bytes());
    push(&null_word);
    push(argv_slice.as_bytes());
    // Push argc
    let sp = push(args.len().as_bytes());

    assert!(sp % 16 == 0);

    let mut result = Vec::with_capacity(data.len());
    let (first, second) = data.as_slices();
    result.extend_from_slice(first);
    result.extend_from_slice(second);
    result
}

#[cfg(test)]
mod tests {
    use super::app_stack_region;

    #[test]
    fn app_stack_region_normalizes_empty_argv() {
        let stack = app_stack_region(&[], &[], &[], b"/bin/true", 0x10000);
        assert!(!stack.is_empty());
        let argc = usize::from_ne_bytes(stack[..core::mem::size_of::<usize>()].try_into().unwrap());
        let argv0 = usize::from_ne_bytes(
            stack[core::mem::size_of::<usize>()..2 * core::mem::size_of::<usize>()]
                .try_into()
                .unwrap(),
        );
        assert_eq!(argc, 1);
        assert_ne!(argv0, 0);
        assert!(stack.windows("/bin/true".len()).any(|w| w == b"/bin/true"));
    }

    #[test]
    fn app_stack_region_aligns_all_large_padding_remainders() {
        let args = vec![b"arg".to_vec()];
        let envs = vec![b"ENV=value".to_vec()];
        let execfn = b"/bin/test";
        let string_bytes = 16 + 1 + execfn.len() + args[0].len() + 1 + envs[0].len() + 1;

        for remainder in 9..16 {
            // `string_bytes + 8` is the stack consumption before the
            // variable alignment fill. Select the caller's top so that fill
            // must cover every former out-of-bounds remainder.
            let top = 0x10000 + string_bytes + 8 + remainder;
            let stack = app_stack_region(&args, &envs, &[], execfn, top);
            assert_eq!((top - stack.len()) % 16, 0, "remainder {remainder}");
            let argc =
                usize::from_ne_bytes(stack[..core::mem::size_of::<usize>()].try_into().unwrap());
            assert_eq!(argc, args.len());
            assert!(stack.windows(execfn.len()).any(|bytes| bytes == execfn));
        }
    }
}
