#define _GNU_SOURCE

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/uio.h>
#include <sys/swap.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* sysadmin ABI differential: ptrace request decoding, unshare flag
 * admission, swapon/swapoff validation order and the module syscall
 * front-gates.
 *
 * Every assertion below was taken from the Linux v7.2.3 sources
 * (kernel/ptrace.c, kernel/fork.c, mm/swapfile.c, kernel/module/main.c) and is
 * environment independent: no timing, no PIDs and no addresses are compared.
 * The program must print exactly the same records on TheKernel and on a real
 * Linux 7.2.3 guest. */

#ifndef PTRACE_OLDSETOPTIONS
#define PTRACE_OLDSETOPTIONS 21
#endif
#ifndef PTRACE_GET_SYSCALL_INFO
#define PTRACE_GET_SYSCALL_INFO 0x420e
#endif
#ifndef PTRACE_O_EXITKILL
#define PTRACE_O_EXITKILL (1 << 20)
#endif
#ifndef PTRACE_O_TRACESYSGOOD
#define PTRACE_O_TRACESYSGOOD 1
#endif
#ifndef PTRACE_O_SUSPEND_SECCOMP
#define PTRACE_O_SUSPEND_SECCOMP (1 << 21)
#endif

#define PTRACE_SYSCALL_INFO_NONE 0
#define AUDIT_ARCH_X86_64 0xc000003eU

/* struct ptrace_syscall_info (include/uapi/linux/ptrace.h); 88 bytes. */
struct ptrace_syscall_info {
    uint8_t op;
    uint8_t reserved;
    uint16_t flags;
    uint32_t arch;
    uint64_t instruction_pointer;
    uint64_t stack_pointer;
    union {
        struct {
            uint64_t nr;
            uint64_t args[6];
        } entry;
        struct {
            int64_t rval;
            uint8_t is_error;
        } exit;
        struct {
            uint64_t nr;
            uint64_t args[6];
            uint32_t ret_data;
        } seccomp;
    };
};

static int failures;

static long do_ptrace(long request, long pid, unsigned long addr,
                      unsigned long data) {
    return syscall(SYS_ptrace, request, pid, addr, data);
}

static void report_failure(const char *assertion, long result, int error) {
    fprintf(stderr,
            "THEKERNEL_SYSADMIN_ABI_FAIL assertion=%s result=%ld errno=%d (%s)\n",
            assertion, result, error, strerror(error));
    ++failures;
}

/* Expect `result == -1 && errno == expected`. */
static void expect_errno(const char *kase, const char *assertion, long result,
                         int expected) {
    int error = errno;

    if (result == -1 && error == expected) {
        printf("THEKERNEL_ABI_ASSERT %s %s pass\n", kase, assertion);
        return;
    }
    report_failure(assertion, result, error);
}

/* Expect a result equal to `expected` (which is 0 for a successful request). */
static void expect_value(const char *kase, const char *assertion, long result,
                         long expected) {
    int error = errno;

    if (result == expected) {
        printf("THEKERNEL_ABI_ASSERT %s %s pass\n", kase, assertion);
        return;
    }
    report_failure(assertion, result, error);
}

#define PTRACE_CASE "sysadmin-abi.ptrace-requests.raw-differential"
#define UNSHARE_CASE "sysadmin-abi.unshare-flags.raw-differential"
#define SWAP_CASE "sysadmin-abi.swap-flags.raw-differential"
#define MODULE_CASE "sysadmin-abi.module-image.raw-differential"

/* --- ptrace request decoding ------------------------------------------- */

static void ptrace_request_case(void) {
    int status = 0;
    pid_t child = fork();

    puts("THEKERNEL_ABI_CASE sysadmin-abi.ptrace-requests.raw-differential");

    if (child < 0) {
        report_failure("PTRACE_FORK", -1, errno);
        return;
    }
    if (child == 0) {
        if (do_ptrace(PTRACE_TRACEME, 0, 0, 0) != 0) {
            _exit(1);
        }
        if (raise(SIGSTOP) != 0) {
            _exit(1);
        }
        _exit(0);
    }

    if (waitpid(child, &status, 0) != child || !WIFSTOPPED(status)) {
        report_failure("PTRACE_WAIT_STOP", -1, errno);
        return;
    }

    /* kernel/ptrace.c `check_ptrace_options()`: `data & ~PTRACE_O_MASK` is
     * -EINVAL.  PTRACE_O_MASK is 0x003000ff, so an all-ones word is invalid. */
    errno = 0;
    expect_errno(PTRACE_CASE, "PTRACE_SETOPTIONS_UNKNOWN_BITS_EINVAL",
                 do_ptrace(PTRACE_SETOPTIONS, child, 0, ~0UL), EINVAL);

    /* PTRACE_OLDSETOPTIONS (21) is the pre-0x4200 spelling of the same
     * request and must be decoded identically. */
    errno = 0;
    expect_value(PTRACE_CASE, "PTRACE_OLDSETOPTIONS_ACCEPTED",
                 do_ptrace(PTRACE_OLDSETOPTIONS, child, 0,
                           PTRACE_O_TRACESYSGOOD),
                 0);

    /* PTRACE_O_EXITKILL is (1 << 20) and is part of PTRACE_O_MASK; the old
     * TheKernel mask (0x2fffff) rejected it. */
    errno = 0;
    expect_value(PTRACE_CASE, "PTRACE_EXITKILL_ACCEPTED",
                 do_ptrace(PTRACE_SETOPTIONS, child, 0, PTRACE_O_EXITKILL), 0);

    /* PTRACE_O_SUSPEND_SECCOMP is admitted for a tracer that holds
     * CAP_SYS_ADMIN and is not itself in seccomp mode (this program is root
     * and unfiltered). */
    errno = 0;
    expect_value(PTRACE_CASE, "PTRACE_SUSPEND_SECCOMP_ADMITTED",
                 do_ptrace(PTRACE_SETOPTIONS, child, 0,
                           PTRACE_O_SUSPEND_SECCOMP),
                 0);

    /* kernel/ptrace.c `ptrace_request()` initialises `int ret = -EIO;` and
     * leaves it untouched for an unrecognised request. */
    errno = 0;
    expect_errno(PTRACE_CASE, "PTRACE_UNKNOWN_REQUEST_EIO", do_ptrace(0x9999, child, 0, 0),
                 EIO);

    /* `ptrace_resume()` rejects `!valid_signal(data)` with -EIO, not -EINVAL. */
    errno = 0;
    expect_errno(PTRACE_CASE, "PTRACE_RESUME_BAD_SIGNAL_EIO",
                 do_ptrace(PTRACE_CONT, child, 0, 0x1000), EIO);

    /* `ptrace_regset()` answers an unknown record type with -EINVAL because
     * `find_regset()` returns NULL. */
    {
        struct iovec iov;
        uint8_t buffer[8];

        iov.iov_base = buffer;
        iov.iov_len = sizeof(buffer);
        errno = 0;
        expect_errno(PTRACE_CASE, "PTRACE_GETREGSET_UNKNOWN_TYPE_EINVAL",
                     do_ptrace(PTRACE_GETREGSET, child, 0xdeadbeefUL,
                               (unsigned long)&iov),
                     EINVAL);
    }

    /* `ptrace_get_syscall_info()` reports the *active* size, not the number of
     * bytes copied, and answers PTRACE_SYSCALL_INFO_NONE for a stop that is not
     * a syscall stop.  The header is 24 bytes and the architecture word is
     * AUDIT_ARCH_X86_64. */
    {
        struct ptrace_syscall_info info;

        memset(&info, 0xff, sizeof(info));
        errno = 0;
        long result = do_ptrace(PTRACE_GET_SYSCALL_INFO, child, sizeof(info),
                                (unsigned long)&info);
        expect_value(PTRACE_CASE, "PTRACE_GET_SYSCALL_INFO_SIZE", result, 24);
        expect_value(PTRACE_CASE, "PTRACE_GET_SYSCALL_INFO_OP", info.op,
                     PTRACE_SYSCALL_INFO_NONE);
        expect_value(PTRACE_CASE, "PTRACE_GET_SYSCALL_INFO_ARCH", info.arch,
                     AUDIT_ARCH_X86_64);
        expect_value(PTRACE_CASE, "PTRACE_GET_SYSCALL_INFO_RESERVED", info.reserved, 0);
        expect_value(PTRACE_CASE, "PTRACE_GET_SYSCALL_INFO_FLAGS", info.flags, 0);

        /* A short buffer still reports the full available size and must not
         * touch the bytes beyond it. */
        memset(&info, 0xff, sizeof(info));
        errno = 0;
        result = do_ptrace(PTRACE_GET_SYSCALL_INFO, child, 4,
                           (unsigned long)&info);
        expect_value(PTRACE_CASE, "PTRACE_GET_SYSCALL_INFO_SHORT_BUFFER_SIZE", result, 24);
        expect_value(PTRACE_CASE, "PTRACE_GET_SYSCALL_INFO_SHORT_BUFFER_OP", info.op,
                     PTRACE_SYSCALL_INFO_NONE);
    }

    /* Detach and let the tracee run to completion. */
    errno = 0;
    expect_value(PTRACE_CASE, "PTRACE_DETACH_ACCEPTED", do_ptrace(PTRACE_DETACH, child, 0, 0),
                 0);
    if (waitpid(child, &status, 0) == child && WIFEXITED(status)) {
        expect_value(PTRACE_CASE, "PTRACE_TRACEE_EXIT_STATUS", WEXITSTATUS(status), 0);
    } else {
        report_failure("PTRACE_TRACEE_EXIT_STATUS", -1, errno);
    }

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.ptrace-requests pass");
}

/* --- unshare flag admission -------------------------------------------- */

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif
#ifndef CLONE_CLEAR_SIGHAND
#define CLONE_CLEAR_SIGHAND (1ULL << 32)
#endif

static void unshare_flag_case(void) {
    puts("THEKERNEL_ABI_CASE sysadmin-abi.unshare-flags.raw-differential");

    /* kernel/fork.c `check_unshare_flags()` accepts CLONE_THREAD,
     * CLONE_SIGHAND and CLONE_VM, and nothing in `ksys_unshare()` consumes
     * them, so a single-threaded caller gets a successful no-op. */
    errno = 0;
    expect_value(UNSHARE_CASE, "UNSHARE_ZERO_NOOP", syscall(SYS_unshare, (long)0), 0);
    errno = 0;
    expect_value(UNSHARE_CASE, "UNSHARE_THREAD_NOOP", syscall(SYS_unshare, (long)(CLONE_THREAD)), 0);
    errno = 0;
    expect_value(UNSHARE_CASE, "UNSHARE_SIGHAND_NOOP", syscall(SYS_unshare, (long)(CLONE_SIGHAND)), 0);
    errno = 0;
    expect_value(UNSHARE_CASE, "UNSHARE_VM_NOOP", syscall(SYS_unshare, (long)(CLONE_VM)), 0);
    errno = 0;
    expect_value(UNSHARE_CASE, "UNSHARE_THREAD_FS_NOOP",
                 syscall(SYS_unshare, (long)(CLONE_THREAD | CLONE_FS)), 0);
    /* CLONE_FILES is a real replacement and must still succeed. */
    errno = 0;
    expect_value(UNSHARE_CASE, "UNSHARE_FILES_OK", syscall(SYS_unshare, (long)(CLONE_FILES)), 0);

    /* Bits outside the 32-bit mask are unknown flags. */
    errno = 0;
    expect_errno(UNSHARE_CASE, "UNSHARE_PIDFD_EINVAL", syscall(SYS_unshare, (long)(CLONE_PIDFD)),
                 EINVAL);
    errno = 0;
    expect_errno(UNSHARE_CASE, "UNSHARE_SETTLS_EINVAL", syscall(SYS_unshare, (long)(CLONE_SETTLS)),
                 EINVAL);
    errno = 0;
    expect_errno(UNSHARE_CASE, "UNSHARE_IO_EINVAL", syscall(SYS_unshare, (long)(CLONE_IO)), EINVAL);
    /* The parameter is `unsigned long`; the mask is 32 bits wide, so the
     * 64-bit-only clone3 flags are rejected. */
    errno = 0;
    expect_errno(UNSHARE_CASE, "UNSHARE_CLEAR_SIGHAND_EINVAL",
                 syscall(SYS_unshare, (long)(CLONE_CLEAR_SIGHAND)), EINVAL);

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.unshare-flags pass");
}

/* --- swapon / swapoff validation order --------------------------------- */

static void swap_flag_case(void) {
    puts("THEKERNEL_ABI_CASE sysadmin-abi.swap-flags.raw-differential");

    /* mm/swapfile.c `SYSCALL_FINE2(swapon, ...)` tests `swap_flags &
     * ~SWAP_FLAGS_VALID` *before* `capable(CAP_SYS_ADMIN)`, and both before the
     * path is opened.  SWAP_FLAGS_VALID is 0x7ffff, so 0x80000 is invalid: the
     * caller must see -EINVAL and never a path error. */
    errno = 0;
    expect_errno(SWAP_CASE, "SWAPON_BAD_FLAGS_EINVAL", swapon("", 0x80000), EINVAL);
    errno = 0;
    expect_errno(SWAP_CASE, "SWAPON_BAD_FLAGS_ABSENT_PATH_EINVAL",
                 swapon("/definitely-absent-swapfile", 0x80000), EINVAL);

    /* With a valid flag word the empty path reaches getname(), which answers
     * -ENOENT for a zero-length name on both syscalls. */
    errno = 0;
    expect_errno(SWAP_CASE, "SWAPON_EMPTY_PATH_ENOENT", swapon("", 0), ENOENT);
    errno = 0;
    expect_errno(SWAP_CASE, "SWAPOFF_EMPTY_PATH_ENOENT", swapoff(""), ENOENT);

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.swap-flags pass");
}

/* --- module syscall front gates ---------------------------------------- */

#ifndef SYS_init_module
#define SYS_init_module 175
#endif
#ifndef SYS_delete_module
#define SYS_delete_module 176
#endif

static void module_image_case(void) {
    puts("THEKERNEL_ABI_CASE sysadmin-abi.module-image.raw-differential");

    /* kernel/module/main.c `copy_module_from_user()`: an image shorter than
     * `sizeof(Elf_Ehdr)` (64 on x86_64) is -ENOEXEC, including a zero length,
     * and the capability check comes first. */
    errno = 0;
    expect_errno(MODULE_CASE, "INIT_MODULE_ZERO_LEN_ENOEXEC",
                 syscall(SYS_init_module, (void *)0, 0UL, (void *)0), ENOEXEC);
    errno = 0;
    expect_errno(MODULE_CASE, "INIT_MODULE_SHORT_IMAGE_ENOEXEC",
                 syscall(SYS_init_module, (void *)0, 63UL, (void *)0), ENOEXEC);
    /* At exactly the header size the copy is attempted, so a NULL source is
     * -EFAULT rather than -EINVAL. */
    errno = 0;
    expect_errno(MODULE_CASE, "INIT_MODULE_HEADER_SIZE_NULL_EFAULT",
                 syscall(SYS_init_module, (void *)0, 64UL, (void *)0), EFAULT);

    /* `delete_module` has no flag table: stray bits are ignored, and an empty
     * or unknown name is -ENOENT. */
    errno = 0;
    expect_errno(MODULE_CASE, "DELETE_MODULE_EMPTY_NAME_ENOENT",
                 syscall(SYS_delete_module, "", 0U), ENOENT);
    errno = 0;
    expect_errno(MODULE_CASE, "DELETE_MODULE_UNKNOWN_NAME_ENOENT",
                 syscall(SYS_delete_module, "thekernel_no_such_module", 0U),
                 ENOENT);
    errno = 0;
    expect_errno(MODULE_CASE, "DELETE_MODULE_STRAY_FLAGS_STILL_ENOENT",
                 syscall(SYS_delete_module, "thekernel_no_such_module", 0x1000U),
                 ENOENT);

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.module-image pass");
}

int main(void) {
    ptrace_request_case();
    unshare_flag_case();
    swap_flag_case();
    module_image_case();

    if (failures != 0) {
        return 1;
    }
    puts("THEKERNEL_SYSADMIN_ABI_OK");
    return 0;
}
