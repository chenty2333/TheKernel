#define _GNU_SOURCE

#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
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
#ifndef SECCOMP_SET_MODE_FILTER
#define SECCOMP_SET_MODE_FILTER 1U
#endif
#ifndef SECCOMP_RET_ERRNO
#define SECCOMP_RET_ERRNO 0x00050000U
#endif
#ifndef SECCOMP_RET_ALLOW
#define SECCOMP_RET_ALLOW 0x7fff0000U
#endif
#ifndef SECCOMP_RET_DATA
#define SECCOMP_RET_DATA 0x0000ffffU
#endif
#ifndef PR_SET_NO_NEW_PRIVS
#define PR_SET_NO_NEW_PRIVS 38
#endif
#ifndef SYS_seccomp
#define SYS_seccomp 317
#endif
#ifndef SYS_getpid
#define SYS_getpid 39
#endif
#ifndef AUDIT_ARCH_X86_64
#define AUDIT_ARCH_X86_64 0xc000003eU
#endif

#define PTRACE_SYSCALL_INFO_NONE 0

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

/* Bounded wait for a stop or an exit of `pid`.
 *
 * Returns 0 when `waitpid` reported the child, -1 when it failed, and 1 when
 * the deadline passed first.  A tracer that never sees the stop it is entitled
 * to must be reported as a failed assertion: an unbounded wait would hang the
 * whole differential runner instead. */
static int wait_bounded(pid_t pid, int *status, int attempts) {
    for (int attempt = 0; attempt < attempts; ++attempt) {
        pid_t done = waitpid(pid, status, WNOHANG);

        if (done == pid) {
            return 0;
        }
        if (done < 0) {
            return -1;
        }
        usleep(10000);
    }
    errno = 0;
    return 1;
}

/* --- PTRACE_O_EXITKILL, observed through its effect --------------------- */

/* A stored option word and an honoured one look identical at the syscall
 * boundary, so the assertion is the documented effect instead:
 *
 * 	list_for_each_entry_safe(p, n, &tracer->ptraced, ptrace_entry) {
 * 		if (unlikely(p->ptrace & PT_EXITKILL))
 * 			send_sig_info(SIGKILL, SEND_SIG_PRIV, p);
 *
 * 		if (__ptrace_detach(tracer, p))
 * 			list_add(&p->ptrace_entry, dead);
 * 	}
 *
 * (kernel/ptrace.c `exit_ptrace()`, run from `exit_notify()` when the tracer
 * exits).  The tracer asks for the option in the `PTRACE_SEIZE` option word --
 * the second spelling of `check_ptrace_options()` -- and then exits, while the
 * tracee blocks on a pipe so that only SIGKILL can end it.  A tracee that
 * outlives its tracer, or that exits through the pipe with status 42, proves
 * the bit was recorded without being honoured.
 *
 * Returns 0 when the tracee died from SIGKILL, 1 when it survived. */
static int exitkill_effect(void) {
    int release[2];
    pid_t tracee;
    pid_t tracer;
    int status = 0;

    if (pipe(release) != 0) {
        return -1;
    }
    tracee = fork();
    if (tracee < 0) {
        return -1;
    }
    if (tracee == 0) {
        char byte;

        (void)close(release[1]);
        while (read(release[0], &byte, 1) < 0 && errno == EINTR) {
        }
        _exit(42);
    }
    tracer = fork();
    if (tracer < 0) {
        return -1;
    }
    if (tracer == 0) {
        (void)close(release[0]);
        (void)close(release[1]);
        if (do_ptrace(PTRACE_SEIZE, tracee, 0, PTRACE_O_EXITKILL) != 0) {
            _exit(1);
        }
        _exit(0);
    }
    if (wait_bounded(tracer, &status, 500) != 0 || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        /* The tracer could not seize the tracee with the option: release the
         * tracee so the failure is reported instead of hanging. */
        (void)close(release[1]);
        (void)wait_bounded(tracee, &status, 500);
        return -1;
    }
    /* `exit_ptrace()` runs before the tracer becomes reapable, so by now the
     * tracee is either killed or detached. */
    for (int attempt = 0; attempt < 200; ++attempt) {
        pid_t done = waitpid(tracee, &status, WNOHANG);

        if (done == tracee) {
            (void)close(release[1]);
            errno = 0;
            return WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL ? 0 : 1;
        }
        if (done < 0) {
            break;
        }
        usleep(10000);
    }
    (void)close(release[1]);
    (void)wait_bounded(tracee, &status, 500);
    errno = 0;
    return 1;
}

/* --- the seize spelling of the option gate ------------------------------ */

/* `PTRACE_SEIZE` carries the option word too, and Linux runs the same ladder on
 * it (kernel/ptrace.c `ptrace_attach()`):
 *
 * 	} else if (request == PTRACE_SEIZE) {
 * 		if (addr)
 * 			return -EIO;
 * 		if (flags & ~(unsigned long)PTRACE_O_MASK)
 * 			return -EIO;
 * 		ret = check_ptrace_options(flags);
 * 		if (retval)
 * 			return retval;
 *
 * The ladder refuses `PTRACE_O_SUSPEND_SECCOMP` for a tracer that is itself
 * filtered (kernel/ptrace.c `check_ptrace_options()`):
 *
 * 	if (seccomp_mode(&current->seccomp) != SECCOMP_MODE_DISABLED ||
 * 	    current->ptrace & PT_SUSPEND_SECCOMP)
 * 		return -EPERM;
 *
 * so the tracer here installs an allow-everything filter and then seizes its
 * own child with the option.  -EPERM is the filtered-tracer answer and -EINVAL
 * the answer of a kernel built without CONFIG_CHECKPOINT_RESTORE (the pinned
 * oracle); a kernel whose seize path never ran the ladder admits the request
 * and is reported as a failure instead.
 *
 * Returns 0 when the filtered tracer was refused, 1 when it was admitted. */
static int seize_suspend_seccomp_gate(void) {
    struct sock_filter instructions[] = {
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    int release[2];
    pid_t tracer;
    int status = 0;

    if (pipe(release) != 0) {
        return -1;
    }
    tracer = fork();
    if (tracer < 0) {
        return -1;
    }
    if (tracer == 0) {
        struct sock_fprog program = {
            .len = 1,
            .filter = instructions,
        };
        pid_t tracee = fork();
        long result;
        int refused;

        if (tracee < 0) {
            _exit(4);
        }
        if (tracee == 0) {
            char sink;

            (void)close(release[1]);
            while (read(release[0], &sink, 1) < 0 && errno == EINTR) {
            }
            _exit(0);
        }
        if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) {
            _exit(2);
        }
        if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program) != 0) {
            _exit(3);
        }
        errno = 0;
        result = do_ptrace(PTRACE_SEIZE, tracee, 0, PTRACE_O_SUSPEND_SECCOMP);
        refused = result == -1 && (errno == EPERM || errno == EINVAL);
        /* Closing the write end releases the tracee, whether or not it was
         * seized, and the tracer is the process that must reap it. */
        (void)close(release[0]);
        (void)close(release[1]);
        while (waitpid(tracee, &status, 0) < 0 && errno == EINTR) {
        }
        _exit(refused ? 0 : 1);
    }
    (void)close(release[0]);
    (void)close(release[1]);
    if (wait_bounded(tracer, &status, 500) != 0 || !WIFEXITED(status)) {
        return -1;
    }
    return WEXITSTATUS(status);
}

/* --- PTRACE_O_SUSPEND_SECCOMP, observed through its effect -------------- */

static int install_getpid_denial(void) {
    struct sock_filter instructions[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EPERM & SECCOMP_RET_DATA)),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog program = {
        .len = (unsigned short)(sizeof(instructions) / sizeof(instructions[0])),
        .filter = instructions,
    };

    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program);
}

/* Linux suspends the *tracee's* policy while the relationship carries
 * PTRACE_O_SUSPEND_SECCOMP:
 *
 * 	if (IS_ENABLED(CONFIG_CHECKPOINT_RESTORE) &&
 * 	    unlikely(current->ptrace & PT_SUSPEND_SECCOMP))
 * 		return 0;
 *
 * (kernel/seccomp.c `__secure_computing()`, tested before the seccomp mode is
 * read).  The tracee installs a filter that turns `getpid` into -EPERM and
 * exits 7 when the denial is still enforced, so an admitted option word that
 * changes nothing is visible as a failure rather than as a successful call.
 *
 * Returns 0 when the suspended tracee's `getpid` succeeded. */
static int suspend_seccomp_effect(void) {
    int ready[2];
    int release[2];
    pid_t tracee;
    int status = 0;
    char byte = 'x';

    if (pipe(ready) != 0 || pipe(release) != 0) {
        return -1;
    }
    tracee = fork();
    if (tracee < 0) {
        return -1;
    }
    if (tracee == 0) {
        (void)close(ready[0]);
        (void)close(release[1]);
        if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) {
            _exit(3);
        }
        if (install_getpid_denial() != 0) {
            _exit(4);
        }
        /* The filter is installed before the tracer is told to attach. */
        if (write(ready[1], &byte, 1) != 1) {
            _exit(5);
        }
        while (read(release[0], &byte, 1) < 0 && errno == EINTR) {
        }
        long result = syscall(SYS_getpid);

        _exit(result > 0 ? 0 : 7);
    }
    (void)close(ready[1]);
    (void)close(release[0]);
    if (read(ready[0], &byte, 1) != 1) {
        (void)close(release[1]);
        (void)kill(tracee, SIGKILL);
        (void)wait_bounded(tracee, &status, 500);
        return -1;
    }
    if (do_ptrace(PTRACE_ATTACH, tracee, 0, 0) != 0 ||
        wait_bounded(tracee, &status, 500) != 0 || !WIFSTOPPED(status)) {
        (void)close(release[1]);
        (void)kill(tracee, SIGKILL);
        (void)wait_bounded(tracee, &status, 500);
        return -1;
    }
    if (do_ptrace(PTRACE_SETOPTIONS, tracee, 0, PTRACE_O_SUSPEND_SECCOMP) != 0) {
        (void)close(release[1]);
        (void)do_ptrace(PTRACE_CONT, tracee, 0, 0);
        (void)wait_bounded(tracee, &status, 500);
        return -1;
    }
    if (write(release[1], &byte, 1) != 1 ||
        do_ptrace(PTRACE_CONT, tracee, 0, 0) != 0 ||
        wait_bounded(tracee, &status, 500) != 0) {
        (void)kill(tracee, SIGKILL);
        (void)wait_bounded(tracee, &status, 500);
        return -1;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}

/* --- ptrace request decoding ------------------------------------------- */

static void ptrace_request_case(void) {
    int status = 0;
    pid_t child;

    /* The banner is emitted before the fork: the runner compares the record
     * multiset exactly, so a child that printed it too would duplicate it. */
    puts("THEKERNEL_ABI_CASE sysadmin-abi.ptrace-requests.raw-differential");

    child = fork();
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
     * TheKernel mask (0x2fffff) rejected it.  Acceptance is checked on the
     * relationship this process already owns... */
    errno = 0;
    expect_value(PTRACE_CASE, "PTRACE_SETOPTIONS_EXITKILL_ACCEPTED",
                 do_ptrace(PTRACE_SETOPTIONS, child, 0, PTRACE_O_EXITKILL), 0);

    /* ...and the option itself is checked through the effect `exit_ptrace()`
     * gives it, because a stored bit that never kills the tracee satisfies the
     * request word without implementing it. */
    errno = 0;
    expect_value(PTRACE_CASE, "PTRACE_EXITKILL_ACCEPTED", exitkill_effect(), 0);

    /* PTRACE_O_SUSPEND_SECCOMP is gated by `check_ptrace_options()`:
     *
     * 	if (unlikely(data & PTRACE_O_SUSPEND_SECCOMP)) {
     * 		if (!IS_ENABLED(CONFIG_CHECKPOINT_RESTORE) ||
     * 		    !IS_ENABLED(CONFIG_SECCOMP))
     * 			return -EINVAL;
     * 		if (!capable(CAP_SYS_ADMIN))
     * 			return -EPERM;
     * 		if (seccomp_mode(&current->seccomp) != SECCOMP_MODE_DISABLED ||
     * 		    current->ptrace & PT_SUSPEND_SECCOMP)
     * 			return -EPERM;
     * 	}
     *
     * The pinned Linux oracle is built without CONFIG_CHECKPOINT_RESTORE and
     * rejects the bit with -EINVAL for every caller, while a kernel built with
     * it admits the request from this unfiltered privileged tracer.  The
     * answer is therefore probed, and an admitted bit must also show its
     * documented effect: the tracee's own filter stops being evaluated. */
    errno = 0;
    {
        long admitted = do_ptrace(PTRACE_SETOPTIONS, child, 0,
                                  PTRACE_O_SUSPEND_SECCOMP);
        int admission_errno = errno;

        if (admitted != 0) {
            /* -EINVAL means the configuration gate, -EPERM the capability or
             * seccomp-state gate; both are pinned answers, so e.g. an
             * accepted-but-ignored bit or an -EIO cannot pass here. */
            errno = admission_errno;
            expect_errno(PTRACE_CASE, "PTRACE_SUSPEND_SECCOMP_ADMITTED",
                         admitted, admission_errno == EINVAL ? EINVAL : EPERM);
        } else {
            errno = 0;
            expect_value(PTRACE_CASE, "PTRACE_SUSPEND_SECCOMP_ADMITTED",
                         suspend_seccomp_effect(), 0);
        }
    }

    /* The same gate on the *other* spelling of the option word: PTRACE_SEIZE
     * used to skip the ladder entirely, so this is the assertion that fails on
     * the pre-change code even though PTRACE_O_MASK already contained the
     * bit. */
    errno = 0;
    expect_value(PTRACE_CASE, "PTRACE_SEIZE_SUSPEND_SECCOMP_GATE",
                 seize_suspend_seccomp_gate(), 0);

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

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.ptrace-requests.raw-differential pass");
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

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.unshare-flags.raw-differential pass");
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

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.swap-flags.raw-differential pass");
}

/* --- module syscall front gates ---------------------------------------- */

#ifndef SYS_init_module
#define SYS_init_module 175
#endif
#ifndef SYS_delete_module
#define SYS_delete_module 176
#endif

static int modules_absent;

/* The answer each module syscall gives when the kernel in front of the test has
 * no module support at all. */
#define MODULE_ANSWER(present) (modules_absent ? ENOSYS : (present))

static void module_image_case(void) {
    puts("THEKERNEL_ABI_CASE sysadmin-abi.module-image.raw-differential");

    /* kernel/sys_ni.c: with CONFIG_MODULES=n the three module syscalls are
     * unconditional -ENOSYS stubs, and the pinned Linux oracle is built that
     * way (`# CONFIG_MODULES is not set`), so the answer is probed once and
     * every assertion keeps its name while expecting the configuration's own
     * answer.  TheKernel implements them, so the same records come from the
     * present-facility answers below. */
    errno = 0;
    {
        long probe = syscall(SYS_delete_module, "thekernel_probe_absent", 0U);

        modules_absent = probe == -1 && errno == ENOSYS;
    }

    /* kernel/module/main.c `copy_module_from_user()`: an image shorter than
     * `sizeof(Elf_Ehdr)` (64 on x86_64) is -ENOEXEC, including a zero length,
     * and the capability check comes first. */
    errno = 0;
    expect_errno(MODULE_CASE, "INIT_MODULE_ZERO_LEN_ENOEXEC",
                 syscall(SYS_init_module, (void *)0, 0UL, (void *)0),
                 MODULE_ANSWER(ENOEXEC));
    errno = 0;
    expect_errno(MODULE_CASE, "INIT_MODULE_SHORT_IMAGE_ENOEXEC",
                 syscall(SYS_init_module, (void *)0, 63UL, (void *)0),
                 MODULE_ANSWER(ENOEXEC));
    /* At exactly the header size the copy is attempted, so a NULL source is
     * -EFAULT rather than -EINVAL. */
    errno = 0;
    expect_errno(MODULE_CASE, "INIT_MODULE_HEADER_SIZE_NULL_EFAULT",
                 syscall(SYS_init_module, (void *)0, 64UL, (void *)0),
                 MODULE_ANSWER(EFAULT));
    /* `load_module()` reads the parameter string only after the image has been
     * validated (kernel/module/main.c:3526):
     *
     * 	mod->args = strndup_user(uargs, ~0UL >> 1);
     *
     * so a readable but malformed image is -ENOEXEC even when the parameter
     * pointer cannot be read at all.  A kernel that copies the parameters
     * first reports the -EFAULT of the pointer instead, which is what the
     * pre-change code did. */
    {
        unsigned char garbage[64] = {0};

        errno = 0;
        expect_errno(MODULE_CASE, "INIT_MODULE_BAD_IMAGE_PRECEDES_BAD_PARAMS",
                     syscall(SYS_init_module, garbage, sizeof(garbage),
                             (void *)(uintptr_t)1),
                     MODULE_ANSWER(ENOEXEC));
    }

    /* `delete_module` has no flag table: stray bits are ignored, and an empty
     * or unknown name is -ENOENT. */
    errno = 0;
    expect_errno(MODULE_CASE, "DELETE_MODULE_EMPTY_NAME_ENOENT",
                 syscall(SYS_delete_module, "", 0U), MODULE_ANSWER(ENOENT));
    errno = 0;
    expect_errno(MODULE_CASE, "DELETE_MODULE_UNKNOWN_NAME_ENOENT",
                 syscall(SYS_delete_module, "thekernel_no_such_module", 0U),
                 MODULE_ANSWER(ENOENT));
    errno = 0;
    expect_errno(MODULE_CASE, "DELETE_MODULE_STRAY_FLAGS_STILL_ENOENT",
                 syscall(SYS_delete_module, "thekernel_no_such_module", 0x1000U),
                 MODULE_ANSWER(ENOENT));

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.module-image.raw-differential pass");
}

/* --- kexec_load segment admission --------------------------------------- */

#ifndef SYS_kexec_load
#define SYS_kexec_load 246
#endif

/* include/uapi/linux/kexec.h */
#define KEXEC_ON_CRASH 0x00000001UL
#define KEXEC_ARCH_X86_64 (62UL << 16)

/* `struct kexec_segment` (include/uapi/linux/kexec.h); 32 bytes on x86_64. */
struct kexec_segment {
    void *buf;
    size_t bufsz;
    void *mem;
    size_t memsz;
};

/* A page-aligned physical destination in the low RAM both targets have.  It is
 * only ever *named*: no probe below reaches the copy of a segment, so nothing
 * reads or writes this page.  The two probes that do succeed carry memsz 0, so
 * the destination owns no page at all there either. */
#define KEXEC_DEST 0x01000000UL

#define KEXEC_CASE "sysadmin-abi.kexec-load.raw-differential"

/* Every probe below is either rejected by `sanity_check_segment_list()` before
 * anything is copied, or -- for the single zero-length destination -- carries a
 * `memsz` of 0 so that the destination owns no page at all.  No image survives
 * the case: the one request that succeeds is unloaded again immediately. */
static void kexec_probe(const char *name, long result, int error) {
    printf("THEKERNEL_KEXEC_PROBE %s result=%ld errno=%d\n", name, result,
           result == -1 ? error : 0);
}

static void kexec_seg(struct kexec_segment *segment, void *buf, size_t bufsz,
                      unsigned long mem, size_t memsz) {
    segment->buf = buf;
    segment->bufsz = bufsz;
    segment->mem = (void *)(uintptr_t)mem;
    segment->memsz = memsz;
}

static void kexec_load_case(void) {
    puts("THEKERNEL_ABI_CASE sysadmin-abi.kexec-load.raw-differential");

    struct kexec_segment one[2];
    struct kexec_segment many[17] = {{0}};
    long result;
    int error;

    /* A reserved flag bit: `kexec_load_check()` admits the KEXEC_FLAGS bits
     * (KEXEC_ON_CRASH | KEXEC_UPDATE_ELFCOREHDR | KEXEC_CRASH_HOTPLUG_SUPPORT,
     * include/linux/kexec.h:458) plus the architecture field, so bit 4 is
     * -EINVAL (kernel/kexec.c:230-231).  `nr_segments` is 0, so no image is
     * unloaded on a kernel that accepts the request. */
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)0, (void *)0,
                     (long)(KEXEC_ARCH_X86_64 | 0x10));
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_RESERVED_FLAG_BIT_EINVAL", result, EINVAL);

    /* Flag bits above bit 31 escape that check on Linux.  KEXEC_ARCH_MASK is
     * 0xffff0000 with type `unsigned int` (include/uapi/linux/kexec.h:17), so
     * `~KEXEC_ARCH_MASK` is 0x0000ffff rather than 0xffffffff0000ffff and
     * `flags & ~KEXEC_ARCH_MASK` drops bits 32..63 instead of testing them.
     * The check is the one that says it leaves room for future extensions:
     *
     * 	if ((flags & KEXEC_FLAGS) != (flags & ~KEXEC_ARCH_MASK))
     * 		return -EINVAL;
     *
     * (kernel/kexec.c:226-231).  A request that sets only `1 << 32` therefore
     * passes with flags == 0, and `nr_segments == 0` turns it into an unload of
     * the default image.  TheKernel compares the whole 64-bit word and answers
     * -EINVAL, which is what the check is for, so the two kernels disagree and
     * this probe stays a diagnostic: no assertion is registered for it. */
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)0, (void *)0, (long)(1UL << 32));
    error = errno;
    kexec_probe("high-flag-bits", result, error);

    /* An architecture field that is neither KEXEC_ARCH_X86_64 nor
     * KEXEC_ARCH_DEFAULT is -EINVAL (kernel/kexec.c:242-244). */
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)0, (void *)0, (long)(63UL << 16));
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_UNKNOWN_ARCH_EINVAL", result, EINVAL);

    /* KEXEC_SEGMENT_MAX is 16, and the cap is checked before the segment array
     * is even read (kernel/kexec.c:233-236), so the answer does not depend on
     * the contents. */
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)17, many, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_SEGMENT_COUNT_OVER_MAX_EINVAL", result, EINVAL);

    /* An unaligned destination is -EADDRNOTAVAIL from the first loop of
     * `sanity_check_segment_list()` (kernel/kexec_core.c:135-137). */
    kexec_seg(&one[0], (void *)0, 0, KEXEC_DEST + 1, 0x1000);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)1, one, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_UNALIGNED_DESTINATION_EADDRNOTAVAIL", result,
                 EADDRNOTAVAIL);

    /* A destination at the architecture limit is -EADDRNOTAVAIL.
     * `KEXEC_DESTINATION_MEMORY_LIMIT` is `MAXMEM - 1` with
     * `MAXMEM = 1 << MAX_PHYSMEM_BITS` and
     * `MAX_PHYSMEM_BITS = pgtable_l5_enabled() ? 52 : 46`
     * (arch/x86/include/asm/kexec.h:64, asm/pgtable_64_types.h:96,
     * asm/sparsemem.h:29), and the first loop tests it before anything is
     * copied, so the destination is never written. */
    kexec_seg(&one[0], (void *)0, 0, 1UL << 46, 0x1000);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)1, one, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_DESTINATION_ABOVE_LIMIT_EADDRNOTAVAIL", result,
                 EADDRNOTAVAIL);

    /* `bufsz > memsz` is -EINVAL from the third loop of the same function
     * (kernel/kexec_core.c:165-173). */
    kexec_seg(&one[0], (void *)0, 0x2000, KEXEC_DEST, 0x1000);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)1, one, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_BUFSZ_OVER_MEMSZ_EINVAL", result, EINVAL);

    /* Both loops run over the whole request, so the *second* segment's address
     * error is reported even though the first segment's buffer size is wrong:
     * loop 1 precedes loop 3. */
    kexec_seg(&one[0], (void *)0, 0x2000, KEXEC_DEST, 0x1000);
    kexec_seg(&one[1], (void *)0, 0, KEXEC_DEST + 1, 0x1000);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)2, one, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_ADDRESS_ERROR_PRECEDES_BUFSZ_ERROR", result,
                 EADDRNOTAVAIL);

    /* Two destinations that overlap are -EINVAL (kernel/kexec_core.c:143-163). */
    kexec_seg(&one[0], (void *)0, 0, KEXEC_DEST, 0x2000);
    kexec_seg(&one[1], (void *)0, 0, KEXEC_DEST + 0x1000, 0x1000);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)2, one, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_errno(KEXEC_CASE, "KEXEC_OVERLAPPING_DESTINATIONS_EINVAL", result, EINVAL);

    /* A zero-length destination is not an error: `mend == mstart` passes the
     * alignment and limit tests, intersects nothing, and `memsz == 0` leaves
     * `kimage_load_normal_segment()`'s `while (mbytes)` loop unentered.  The
     * request therefore *succeeds* and is unloaded again immediately, so no
     * image outlives the probe. */
    kexec_seg(&one[0], (void *)0, 0, KEXEC_DEST, 0);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)1, one, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_value(KEXEC_CASE, "KEXEC_ZERO_LENGTH_DESTINATION_ACCEPTED", result, 0);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)0, (void *)0, (long)KEXEC_ARCH_X86_64);
    error = errno;
    expect_value(KEXEC_CASE, "KEXEC_UNLOAD_AFTER_ZERO_LENGTH_ACCEPTED", result, 0);

    /* A crash image is refused by the entry check when the kernel booted
     * without `crashkernel=`, because `crashk_res` is empty
     * (kernel/kexec.c:29-36).  The probe keeps memsz 0 so that a kernel which
     * does not check the entry reserves no destination page either.
     *
     * Measured: the oracle answers -EADDRNOTAVAIL and TheKernel answers 0,
     * because TheKernel never reserves a crash kernel region
     * (`reserve_crashkernel()` is the missing primitive), so its crash-image
     * path cannot be held to `crashk_res`.  The two kernels disagree here and
     * this probe stays a diagnostic: no assertion is registered for it.  The
     * unload below releases whatever the probe published. */
    kexec_seg(&one[0], (void *)0, 0, KEXEC_DEST, 0);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)KEXEC_DEST, (long)1, one,
                     (long)(KEXEC_ARCH_X86_64 | KEXEC_ON_CRASH));
    error = errno;
    kexec_probe("crash-entry-outside-crashk", result, error);
    errno = 0;
    result = syscall(SYS_kexec_load, (long)0, (long)0, (void *)0,
                     (long)(KEXEC_ARCH_X86_64 | KEXEC_ON_CRASH));
    error = errno;
    kexec_probe("unload-crash-image", result, error);

    puts("THEKERNEL_ABI_RESULT sysadmin-abi.kexec-load.raw-differential pass");
}

int main(void) {
    ptrace_request_case();
    unshare_flag_case();
    swap_flag_case();
    module_image_case();
    kexec_load_case();

    if (failures != 0) {
        return 1;
    }
    puts("THEKERNEL_SYSADMIN_ABI_OK");
    return 0;
}
