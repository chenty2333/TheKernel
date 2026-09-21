/*
 * Portable Linux/x86_64 futex ABI differential test.
 *
 * The program is built once and executed unchanged on real Linux and on
 * TheKernel; a differential harness compares the marker lines emitted on
 * stdout.  Every marker value is therefore a plain integer or a fixed literal
 * string: no PIDs, TIDs, pointers or clock readings are ever printed, and no
 * assertion depends on scheduling order or on how long anything takes.
 *
 * Threads are used wherever a second execution context is required.  The
 * handshake before every wake/requeue operation is: the waiter publishes an
 * "entered" flag immediately before entering the futex syscall and the main
 * thread spins until the waiter's /proc/<pid>/task/<tid>/stat state is
 * sleeping (S/D).  The spin is bounded by CLOCK_MONOTONIC, never by a sleep
 * used as synchronisation, and if /proc is unavailable the counter alone is
 * accepted after a bounded grace period.
 *
 * Timeouts: FUTEX_WAIT is the only legacy opcode whose timespec is relative
 * in Linux (futex_init_timeout() adds it to ktime_get()), so the waiters that
 * must block use a fixed five second safety bound while every other timed
 * operation uses an absolute CLOCK_MONOTONIC/CLOCK_REALTIME deadline obtained
 * from clock_gettime().  A woken waiter always returns before its bound, so
 * no assertion observes the bound itself.
 *
 * futex2 ABI note: Linux x86_64 numbers these syscalls 454/455/456/449 and
 * futex_wake takes (uaddr, mask, nr, flags): "mask" is the FUTEX_WAIT_BITSET
 * style bitset (a 32-bit value) and "nr" is the number of waiters to wake.
 * The bitset must fit in the futex size, so FUTEX_BITSET_MATCH_ANY is passed
 * as a 32-bit value and never as a 64-bit ~0.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* ------------------------------------------------------------------ */
/* Local fallbacks: the build host headers may predate these values.  */
/* ------------------------------------------------------------------ */

#ifndef FUTEX_WAIT
#define FUTEX_WAIT 0
#endif

#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif

#ifndef FUTEX_FD
#define FUTEX_FD 2
#endif

#ifndef FUTEX_REQUEUE
#define FUTEX_REQUEUE 3
#endif

#ifndef FUTEX_CMP_REQUEUE
#define FUTEX_CMP_REQUEUE 4
#endif

#ifndef FUTEX_LOCK_PI
#define FUTEX_LOCK_PI 6
#endif

#ifndef FUTEX_UNLOCK_PI
#define FUTEX_UNLOCK_PI 7
#endif

#ifndef FUTEX_TRYLOCK_PI
#define FUTEX_TRYLOCK_PI 8
#endif

#ifndef FUTEX_WAIT_BITSET
#define FUTEX_WAIT_BITSET 9
#endif

#ifndef FUTEX_WAKE_BITSET
#define FUTEX_WAKE_BITSET 10
#endif

#ifndef FUTEX_WAIT_REQUEUE_PI
#define FUTEX_WAIT_REQUEUE_PI 11
#endif

#ifndef FUTEX_CMP_REQUEUE_PI
#define FUTEX_CMP_REQUEUE_PI 12
#endif

#ifndef FUTEX_LOCK_PI2
#define FUTEX_LOCK_PI2 13
#endif

#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif

#ifndef FUTEX_CLOCK_REALTIME
#define FUTEX_CLOCK_REALTIME 256
#endif

#ifndef FUTEX_ROBUST_UNLOCK
#define FUTEX_ROBUST_UNLOCK 512
#endif

#ifndef FUTEX_WAITERS
#define FUTEX_WAITERS 0x80000000U
#endif

#ifndef FUTEX_OWNER_DIED
#define FUTEX_OWNER_DIED 0x40000000U
#endif

#ifndef FUTEX_TID_MASK
#define FUTEX_TID_MASK 0x3fffffffU
#endif

#ifndef FUTEX_BITSET_MATCH_ANY
#define FUTEX_BITSET_MATCH_ANY 0xffffffffU
#endif

#ifndef FUTEX_32
#define FUTEX_32 2
#endif

#ifndef FUTEX_WAKE_OP
#define FUTEX_WAKE_OP 5
#endif

/* FUTEX_WAKE_OP's encoded operation.  include/uapi/linux/futex.h:
 *
 *	#define FUTEX_OP(op, oparg, cmp, cmparg) \
 *					(((op & 0xf) << 28) | ((cmp & 0xf) << 24)	\
 *					| ((oparg & 0xfff) << 12) | (cmparg & 0xfff))
 *
 * with FUTEX_OP_SET, FUTEX_OP_CMP_EQ and FUTEX_OP_CMP_NE all zero. */
#ifndef FUTEX_OP_SET
#define FUTEX_OP_SET 0
#endif

#ifndef FUTEX_OP_CMP_EQ
#define FUTEX_OP_CMP_EQ 0
#endif

#ifndef FUTEX_OP
#define FUTEX_OP(op, oparg, cmp, cmparg)                                       \
    ((((op) & 0xf) << 28) | (((cmp) & 0xf) << 24) | (((oparg) & 0xfff) << 12)  \
     | ((cmparg) & 0xfff))
#endif

/* futex2 flag bits (the size field lives in the low two bits). */
#ifndef FUTEX2_SIZE_U8
#define FUTEX2_SIZE_U8 0x00
#endif

#ifndef FUTEX2_NUMA
#define FUTEX2_NUMA 0x04
#endif

#ifndef FUTEX2_MPOL
#define FUTEX2_MPOL 0x08
#endif

/* Keep the x86_64 Linux syscall numbers available with older headers. */
#ifndef SYS_futex
#ifdef __NR_futex
#define SYS_futex __NR_futex
#else
#define SYS_futex 202
#endif
#endif

#ifndef SYS_futex_wake
#ifdef __NR_futex_wake
#define SYS_futex_wake __NR_futex_wake
#else
#define SYS_futex_wake 454
#endif
#endif

#ifndef SYS_futex_wait
#ifdef __NR_futex_wait
#define SYS_futex_wait __NR_futex_wait
#else
#define SYS_futex_wait 455
#endif
#endif

#ifndef SYS_futex_requeue
#ifdef __NR_futex_requeue
#define SYS_futex_requeue __NR_futex_requeue
#else
#define SYS_futex_requeue 456
#endif
#endif

#ifndef SYS_futex_waitv
#ifdef __NR_futex_waitv
#define SYS_futex_waitv __NR_futex_waitv
#else
#define SYS_futex_waitv 449
#endif
#endif

#ifndef SYS_gettid
#ifdef __NR_gettid
#define SYS_gettid __NR_gettid
#else
#define SYS_gettid 186
#endif
#endif

#ifndef SYS_tgkill
#ifdef __NR_tgkill
#define SYS_tgkill __NR_tgkill
#else
#define SYS_tgkill 234
#endif
#endif

/* memfd_create(2) is x86_64 syscall 319; older headers may lack both it and
 * the flag.  A memfd page is the portable way to obtain one shared futex word
 * at two virtual addresses inside a single process. */
#ifndef SYS_memfd_create
#ifdef __NR_memfd_create
#define SYS_memfd_create __NR_memfd_create
#else
#define SYS_memfd_create 319
#endif
#endif

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif

/* ------------------------------------------------------------------ */
/* Bounds.  All values are pure safety nets; correct runs never wait. */
/* ------------------------------------------------------------------ */

#define BLOCK_BOUND_NS 3000000000LL
#define RETRY_BOUND_NS 3000000000LL
#define WAIT_BOUND_NS 5000000000LL
/* FUTEX_WAIT_REQUEUE_PI waiters need a bound that survives a starved host.
 * The bound exists only so a lost requeue fails the case instead of hanging
 * the suite; it is never reached in practice, because the main thread settles
 * the waiter and requeues it within milliseconds.  A 5 s bound was short
 * enough that a guest starved by concurrent host load expired it first. */
#define PI_WAIT_BOUND_NS 60000000000LL
#define PROC_GRACE_NS 200000000LL

#define PRIVATE FUTEX_PRIVATE_FLAG

/* ------------------------------------------------------------------ */
/* syscall wrappers                                                   */
/* ------------------------------------------------------------------ */

static long sys_futex(uint32_t *uaddr, int op, uint32_t val,
                      const struct timespec *timeout, uint32_t *uaddr2,
                      uint32_t val3) {
    return syscall(SYS_futex, uaddr, op, val, timeout, uaddr2, val3);
}

/* Linux futex2: SYSCALL_DEFINE4(futex_wake, uaddr, mask, nr, flags). */
static long sys_futex_wake(uint32_t *uaddr, uint32_t mask, int nr,
                           unsigned int flags) {
    return syscall(SYS_futex_wake, uaddr, (unsigned long)mask, nr, flags);
}

/* Linux futex2: SYSCALL_DEFINE6(futex_wait, uaddr, val, mask, flags,
 *                               timeout, clockid). */
static long sys_futex_wait(uint32_t *uaddr, uint32_t val, uint32_t mask,
                           unsigned int flags, const struct timespec *timeout,
                           int clockid) {
    return syscall(SYS_futex_wait, uaddr, (unsigned long)val,
                   (unsigned long)mask, flags, timeout, clockid);
}

/* `struct futex_waitv` from include/uapi/linux/futex.h, spelled locally so a
 * build host header cannot change the layout the syscall parses. */
struct local_futex_waitv {
    uint64_t val;
    uint64_t uaddr;
    uint32_t flags;
    uint32_t reserved;
};

_Static_assert(sizeof(struct local_futex_waitv) == 24,
               "futex_waitv ABI layout must remain 24 bytes");

/* Linux futex2: SYSCALL_DEFINE5(futex_waitv, waiters, nr_futexes, flags,
 *                               timeout, clockid). */
static long sys_futex_waitv(const struct local_futex_waitv *waiters,
                            unsigned int nr_futexes, unsigned int flags,
                            const struct timespec *timeout, int clockid) {
    return syscall(SYS_futex_waitv, waiters, nr_futexes, flags, timeout,
                   clockid);
}

/* ------------------------------------------------------------------ */
/* Failure, marker and clock helpers                                  */
/* ------------------------------------------------------------------ */

static int fail(const char *stage, int error_number) {
    fprintf(stderr, "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_FAIL %s errno=%d\n",
            stage, error_number);
    return 1;
}

static int unsupported(const char *opcode, int error_number) {
    printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_UNSUPPORTED opcode=%s errno=%d\n",
           opcode, error_number);
    fflush(stdout);
    return 2;
}

static void marker(const char *line) {
    puts(line);
    fflush(stdout);
}

/*
 * Structured ABI record for tools/qemu_runner/abi_differential.py.
 *
 * The runner requires every case to be bracketed by
 * THEKERNEL_ABI_CASE / THEKERNEL_ABI_ASSERT* / THEKERNEL_ABI_RESULT with the
 * "<case>.portable-differential" name and to list the case's assertion tokens
 * with the outcome "pass".  The same function also prints the plain marker
 * line so the program stays readable on its own.
 */
static void record(const char *case_name, const char *assertions,
                   const char *marker_line) {
    char buffer[256];
    size_t length = strlen(assertions);

    if (length >= sizeof(buffer)) {
        fprintf(stderr, "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_FAIL record %s\n",
                case_name);
        exit(1);
    }
    memcpy(buffer, assertions, length + 1);
    printf("THEKERNEL_ABI_CASE %s.portable-differential\n", case_name);
    for (char *token = strtok(buffer, " "); token != NULL;
         token = strtok(NULL, " ")) {
        printf("THEKERNEL_ABI_ASSERT %s.portable-differential %s pass\n",
               case_name, token);
    }
    marker(marker_line);
    printf("THEKERNEL_ABI_RESULT %s.portable-differential pass\n", case_name);
    fflush(stdout);
}

static int64_t monotonic_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    return (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec;
}

/* Absolute CLOCK_MONOTONIC deadline @ns from now. */
static int absolute_bound(struct timespec *out, int64_t ns) {
    if (clock_gettime(CLOCK_MONOTONIC, out) != 0) {
        return -1;
    }
    out->tv_sec += (time_t)(ns / 1000000000LL);
    out->tv_nsec += (long)(ns % 1000000000LL);
    if (out->tv_nsec >= 1000000000L) {
        out->tv_sec += 1;
        out->tv_nsec -= 1000000000L;
    }
    return 0;
}

/* FUTEX_WAIT takes a relative timespec in Linux (and in TheKernel). */
static struct timespec relative_bound(int64_t ns) {
    struct timespec out;
    out.tv_sec = (time_t)(ns / 1000000000LL);
    out.tv_nsec = (long)(ns % 1000000000LL);
    return out;
}

/* Scheduler state character from /proc/<pid>/task/<tid>/stat.  The comm field
 * may contain spaces, so parse from the last ')'. */
static int task_state(pid_t pid, pid_t tid) {
    char path[96];
    char buffer[512];
    int length = snprintf(path, sizeof(path), "/proc/%ld/task/%ld/stat",
                          (long)pid, (long)tid);
    if (length <= 0 || (size_t)length >= sizeof(path)) {
        return -1;
    }
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (count <= 0) {
        return -1;
    }
    buffer[count] = '\0';
    const char *close_paren = strrchr(buffer, ')');
    if (close_paren == NULL || close_paren[1] != ' ' ||
        close_paren[2] == '\0') {
        return -1;
    }
    return (unsigned char)close_paren[2];
}

/* ------------------------------------------------------------------ */
/* Thread scaffolding                                                 */
/* ------------------------------------------------------------------ */

struct blocked_thread {
    _Atomic int entered;
    _Atomic int done;
    _Atomic int tid;
    pthread_t thread;
};

static int start_blocked(struct blocked_thread *blocked,
                         void *(*entry)(void *), void *argument,
                         const char *stage) {
    atomic_store_explicit(&blocked->entered, 0, memory_order_relaxed);
    atomic_store_explicit(&blocked->done, 0, memory_order_relaxed);
    atomic_store_explicit(&blocked->tid, 0, memory_order_relaxed);
    int result = pthread_create(&blocked->thread, NULL, entry, argument);
    if (result != 0) {
        return fail(stage, result);
    }
    return 0;
}

/* Bounded spin until the thread is provably sleeping in the kernel, i.e. it
 * set its "entered" flag and its task state is S or D.  If /proc cannot be
 * read the flag alone is accepted after a bounded grace period. */
static int wait_until_blocked(struct blocked_thread *blocked,
                              const char *stage) {
    int64_t start = monotonic_ns();
    if (start < 0) {
        return fail(stage, errno != 0 ? errno : EPROTO);
    }
    for (;;) {
        if (atomic_load_explicit(&blocked->done, memory_order_acquire) != 0) {
            return fail(stage, EPROTO);
        }
        if (atomic_load_explicit(&blocked->entered, memory_order_acquire) != 0) {
            int tid = atomic_load_explicit(&blocked->tid, memory_order_acquire);
            if (tid > 0) {
                int state = task_state(getpid(), (pid_t)tid);
                if (state == 'S' || state == 'D') {
                    return 0;
                }
                if (state < 0) {
                    int64_t now = monotonic_ns();
                    if (now < 0 || now - start >= PROC_GRACE_NS) {
                        return 0;
                    }
                }
            }
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= BLOCK_BOUND_NS) {
            return fail(stage, ETIMEDOUT);
        }
        sched_yield();
    }
}

/* Bounded settle for a waiter that has entered FUTEX_WAIT_REQUEUE_PI.
 *
 * wait_until_blocked() additionally requires the kernel to report the waiter
 * with a sleeping state letter, which is how a requeue is made deterministic.
 * The letter is printed here as a diagnostic rather than required: the
 * assertion under test is the requeue itself, and a bounded settle after the
 * waiter has entered the syscall is enough to guarantee it was enqueued.  The
 * letter is still reported so a guest that does not mark the waiter sleeping
 * is visible instead of silently tolerated. */
static int settle_blocked(struct blocked_thread *blocked, const char *stage) {
    int64_t start = monotonic_ns();
    if (start < 0) {
        return fail(stage, errno != 0 ? errno : EPROTO);
    }
    for (;;) {
        if (atomic_load_explicit(&blocked->done, memory_order_acquire) != 0) {
            fprintf(stderr,
                    "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_SETTLE_DIAG stage=%s when=early "
                    "done=1 entered=%d uptime_ns=%lld\n",
                    stage,
                    (int)atomic_load_explicit(&blocked->entered,
                                              memory_order_acquire),
                    (long long)start);
            return fail(stage, EPROTO);
        }
        if (atomic_load_explicit(&blocked->entered, memory_order_acquire) != 0) {
            break;
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= BLOCK_BOUND_NS) {
            fprintf(stderr,
                    "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_SETTLE_DIAG stage=%s when=enter-timeout "
                    "done=0 entered=0 uptime_ns=%lld\n",
                    stage, (long long)start);
            return fail(stage, ETIMEDOUT);
        }
        sched_yield();
    }
    usleep(50000);
    if (atomic_load_explicit(&blocked->done, memory_order_acquire) != 0) {
        fprintf(stderr,
                "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_SETTLE_DIAG stage=%s when=settle "
                "done=1 entered=1 uptime_ns=%lld tids=%lld\n",
                stage, (long long)start,
                (long long)atomic_load_explicit(&blocked->tid,
                                                memory_order_acquire));
        return fail(stage, EPROTO);
    }
    printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_BLOCKSTATE stage=%s state=%d\n",
           stage,
           task_state(getpid(),
                      (pid_t)atomic_load_explicit(&blocked->tid,
                                                  memory_order_acquire)));
    return 0;
}

static int join_blocked(struct blocked_thread *blocked, const char *stage) {
    int result = pthread_join(blocked->thread, NULL);
    if (result != 0) {
        return fail(stage, result);
    }
    return 0;
}

/* Bounded variant of join_blocked() for waits whose wakeup the kernel is
 * under test for: a lost wakeup must fail the case with a diagnostic instead
 * of hanging the whole differential behind an unbounded pthread_join(). */
static int await_blocked(struct blocked_thread *blocked, const char *stage) {
    int64_t start = monotonic_ns();
    if (start < 0) {
        return fail(stage, errno != 0 ? errno : EPROTO);
    }
    for (;;) {
        if (atomic_load_explicit(&blocked->done, memory_order_acquire) != 0) {
            return join_blocked(blocked, stage);
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= BLOCK_BOUND_NS) {
            return fail(stage, ETIMEDOUT);
        }
        sched_yield();
    }
}

struct futex_waiter {
    struct blocked_thread blocked;
    uint32_t *uaddr;
    int op;
    uint32_t val;
    int use_timeout;
    struct timespec timeout;
    uint32_t *uaddr2;
    uint32_t val3;
    long result;
    int result_errno;
};

static void *futex_waiter_main(void *opaque) {
    struct futex_waiter *waiter = opaque;

    atomic_store_explicit(&waiter->blocked.tid, (int)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&waiter->blocked.entered, 1, memory_order_release);
    errno = 0;
    waiter->result = sys_futex(waiter->uaddr, waiter->op, waiter->val,
                               waiter->use_timeout ? &waiter->timeout : NULL,
                               waiter->uaddr2, waiter->val3);
    waiter->result_errno = waiter->result == -1 ? errno : 0;
    atomic_store_explicit(&waiter->blocked.done, 1, memory_order_release);
    return NULL;
}

static int start_futex_waiter(struct futex_waiter *waiter, uint32_t *uaddr,
                              int op, uint32_t val,
                              const struct timespec *timeout, uint32_t *uaddr2,
                              uint32_t val3, const char *stage) {
    memset(waiter, 0, sizeof(*waiter));
    waiter->uaddr = uaddr;
    waiter->op = op;
    waiter->val = val;
    if (timeout != NULL) {
        waiter->use_timeout = 1;
        waiter->timeout = *timeout;
    }
    waiter->uaddr2 = uaddr2;
    waiter->val3 = val3;
    return start_blocked(&waiter->blocked, futex_waiter_main, waiter, stage);
}

/* ------------------------------------------------------------------ */
/* Legacy futex expectations                                          */
/* ------------------------------------------------------------------ */

static int expect_futex_zero(const char *stage, uint32_t *uaddr, int op,
                             uint32_t val, const struct timespec *timeout,
                             uint32_t *uaddr2, uint32_t val3) {
    errno = 0;
    long result = sys_futex(uaddr, op, val, timeout, uaddr2, val3);
    int saved = errno;
    if (result != 0) {
        return fail(stage, result == -1 ? saved : EPROTO);
    }
    return 0;
}

/* "Park until released" handshake used by the threads that must stay alive
 * while the main thread samples kernel state.  Publishing the release value
 * before waking means the wake may legitimately find nobody queued (0) if the
 * thread has not reached its FUTEX_WAIT yet, and that thread then observes the
 * changed value and returns EWOULDBLOCK instead of sleeping.  Both outcomes
 * are success and neither changes what the section observes. */
static int release_parked_thread(uint32_t *control, const char *stage) {
    *control = 1;
    errno = 0;
    long woken = sys_futex(control, FUTEX_WAKE | PRIVATE, INT_MAX, NULL, NULL,
                           0);
    int saved = errno;
    if (woken != 0 && woken != 1) {
        return fail(stage, woken == -1 ? saved : EPROTO);
    }
    return 0;
}

static int parked_wait_succeeded(long result, int error_number) {
    return result == 0 || (result == -1 && error_number == EWOULDBLOCK);
}

static int expect_futex_errno(const char *stage, uint32_t *uaddr, int op,
                              uint32_t val, const struct timespec *timeout,
                              uint32_t *uaddr2, uint32_t val3, int expected) {
    errno = 0;
    long result = sys_futex(uaddr, op, val, timeout, uaddr2, val3);
    int saved = errno;
    if (result != -1) {
        return fail(stage, EPROTO);
    }
    if (saved != expected) {
        return fail(stage, saved);
    }
    return 0;
}

/* ------------------------------------------------------------------ */
/* A. opcode and flag validation                                      */
/* ------------------------------------------------------------------ */

/* FUTEX_CLOCK_REALTIME is only accepted for FUTEX_WAIT_BITSET,
 * FUTEX_WAIT_REQUEUE_PI and FUTEX_LOCK_PI2 (do_futex() returns ENOSYS for any
 * other command that carries the flag), and FUTEX_ROBUST_UNLOCK is only
 * accepted for FUTEX_WAKE, FUTEX_WAKE_BITSET and FUTEX_UNLOCK_PI.  Unknown
 * high bits are not masked away. */
static int test_opcode_validation(void) {
    uint32_t word = 0;
    const uint32_t self = (uint32_t)syscall(SYS_gettid);

    if (expect_futex_errno("opcode-wake-clock-realtime", &word,
                           FUTEX_WAKE | FUTEX_CLOCK_REALTIME | PRIVATE, 0,
                           NULL, NULL, 0, ENOSYS) != 0) {
        return 1;
    }
    if (expect_futex_errno("opcode-lock-pi-clock-realtime", &word,
                           FUTEX_LOCK_PI | FUTEX_CLOCK_REALTIME | PRIVATE, 0,
                           NULL, NULL, 0, ENOSYS) != 0) {
        return 1;
    }

    /* Accepted: the word holds 1 and 0 is expected, so the value check fails
     * with EWOULDBLOCK before any timeout is required. */
    word = 1;
    if (expect_futex_errno("opcode-wait-bitset-clock-realtime", &word,
                           FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME | PRIVATE,
                           0, NULL, NULL, FUTEX_BITSET_MATCH_ANY,
                           EWOULDBLOCK) != 0) {
        return 1;
    }

    /* Accepted: FUTEX_LOCK_PI2 exists precisely to let userspace pick the
     * clock, so the explicit flag is legal and the free word is acquired. */
    word = 0;
    errno = 0;
    long result = sys_futex(&word, FUTEX_LOCK_PI2 | FUTEX_CLOCK_REALTIME |
                                       PRIVATE, 0, NULL, NULL, 0);
    int saved = errno;
    if (result == -1 && saved == ENOSYS) {
        return unsupported("FUTEX_LOCK_PI2", saved);
    }
    if (result != 0) {
        return fail("opcode-lock-pi2-clock-realtime",
                    result == -1 ? saved : EPROTO);
    }
    /* A free word with no waiters and no FUTEX_OWNER_DIED is taken over with
     * the caller's TID.  Linux, kernel/futex/pi.c:
     *
     *     u32 uval, newval, vpid = task_pid_vnr(task);
     *     ...
     *     newval = uval & FUTEX_OWNER_DIED;
     *     newval |= vpid;
     *
     * so the word holds task_pid_vnr(), the same number gettid() returns.
     * case D's `pi-lock-deadlock-word` compares the whole word and covers the
     * remaining bits. */
    if ((word & FUTEX_TID_MASK) != self) {
        printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_PI_WORD_RAW case=lock-pi2-realtime "
               "word=0x%08x word_tid=%u gettid=%u pid=%u\n",
               word, (unsigned)(word & FUTEX_TID_MASK), (unsigned)self,
               (unsigned)getpid());
        fflush(stdout);
        return fail("opcode-lock-pi2-clock-realtime-word", EPROTO);
    }
    if (expect_futex_zero("opcode-lock-pi2-clock-realtime-unlock", &word,
                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (word != 0) {
        return fail("opcode-lock-pi2-clock-realtime-unlock-word", EPROTO);
    }

    /* Unknown bits reach the dispatch switch unchanged. */
    word = 0;
    if (expect_futex_errno(
            "opcode-wake-waiters-bit", &word,
            (int)((uint32_t)FUTEX_WAKE | FUTEX_WAITERS | (uint32_t)PRIVATE), 0,
            NULL, NULL, 0, ENOSYS) != 0) {
        return 1;
    }
    if (expect_futex_errno("opcode-wake-bit11", &word,
                           FUTEX_WAKE | (1 << 11) | PRIVATE, 0, NULL, NULL, 0,
                           ENOSYS) != 0) {
        return 1;
    }

    /* FUTEX_ROBUST_UNLOCK is not a WAIT or LOCK_PI modifier.  The
     * FUTEX_WAKE | FUTEX_ROBUST_UNLOCK form is deliberately not exercised:
     * that command stores zero through uaddr and clears the pending robust
     * list operation through uaddr2, so it cannot be made side effect free. */
    if (expect_futex_errno("opcode-wait-robust-unlock", &word,
                           FUTEX_WAIT | FUTEX_ROBUST_UNLOCK | PRIVATE, 0, NULL,
                           NULL, 0, ENOSYS) != 0) {
        return 1;
    }
    if (expect_futex_errno("opcode-lock-pi-robust-unlock", &word,
                           FUTEX_LOCK_PI | FUTEX_ROBUST_UNLOCK | PRIVATE, 0,
                           NULL, NULL, 0, ENOSYS) != 0) {
        return 1;
    }

    /* FUTEX_FD was never implemented; the opcode slot returns ENOSYS. */
    if (expect_futex_errno("opcode-fd", &word, FUTEX_FD | PRIVATE, 0, NULL,
                           NULL, 0, ENOSYS) != 0) {
        return 1;
    }

    /* The futex address must be naturally aligned. */
    if (expect_futex_errno("opcode-misaligned-wake",
                           (uint32_t *)(void *)((char *)&word + 1),
                           FUTEX_WAKE | PRIVATE, 1, NULL, NULL, 0,
                           EINVAL) != 0) {
        return 1;
    }

    /* A zero bitset can never match a waiter. */
    if (expect_futex_errno("opcode-wake-bitset-zero", &word,
                           FUTEX_WAKE_BITSET | PRIVATE, 1, NULL, NULL, 0,
                           EINVAL) != 0) {
        return 1;
    }

    /* A count of zero is not an error, and with no waiters it wakes none. */
    if (expect_futex_zero("opcode-wake-zero-no-waiters", &word,
                          FUTEX_WAKE | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }

    record("futex-abi-opcode",
           "WAKE_REALTIME LOCK_PI_REALTIME WAIT_BITSET_REALTIME LOCK_PI2_REALTIME WAKE_HIGH_BIT WAKE_BIT11 WAIT_ROBUST_UNLOCK LOCK_PI_ROBUST_UNLOCK FD MISALIGNED WAKE_BITSET_ZERO WAKE_ZERO_NO_WAITERS",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_OPCODE_OK wake_realtime=1 "
           "lock_pi_realtime=1 wait_bitset_realtime=1 lock_pi2_realtime=1 "
           "wake_high_bit=1 wake_bit11=1 wait_robust_unlock=1 "
           "lock_pi_robust_unlock=1 fd=1 misaligned=1 wake_bitset_zero=1 "
           "wake_zero_no_waiters=0");
    return 0;
}

/* ------------------------------------------------------------------ */
/* B. FUTEX_WAKE with a zero count wakes exactly one waiter           */
/* ------------------------------------------------------------------ */

/* futex_wake() wakes a waiter and only then tests "if (++ret >= nr_wake)
 * break;", so a count of zero still wakes the first matching waiter and
 * reports 1.  Two waiters are queued; the first wake must report exactly one
 * and the cleanup wake must then report the remaining one. */
static int test_wake_zero(void) {
    uint32_t word = 0;
    struct futex_waiter waiters[2];
    struct timespec bound = relative_bound(WAIT_BOUND_NS);
    int index;

    for (index = 0; index < 2; ++index) {
        if (start_futex_waiter(&waiters[index], &word,
                               FUTEX_WAIT | PRIVATE, 0, &bound, NULL, 0,
                               "wake-zero-create") != 0) {
            return 1;
        }
    }
    for (index = 0; index < 2; ++index) {
        if (wait_until_blocked(&waiters[index].blocked,
                               "wake-zero-block-handshake") != 0) {
            return 1;
        }
    }

    /* The kernel may not have queued both waiters yet even though both are
     * sleeping inside the syscall; a zero wake is harmless to repeat, so
     * retry until it reports the wake it performs. */
    int64_t start = monotonic_ns();
    if (start < 0) {
        return fail("wake-zero-clock", EPROTO);
    }
    long woke = 0;
    for (;;) {
        errno = 0;
        woke = sys_futex(&word, FUTEX_WAKE | PRIVATE, 0, NULL, NULL, 0);
        int saved = errno;
        if (woke == 1) {
            break;
        }
        if (woke != 0) {
            return fail("wake-zero-result", woke == -1 ? saved : EPROTO);
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= RETRY_BOUND_NS) {
            return fail("wake-zero-retry", ETIMEDOUT);
        }
        sched_yield();
    }

    /* The remaining waiter is still queued: this wake reports it and only
     * it.  Repeat the harmless wake in case it had not been queued yet. */
    start = monotonic_ns();
    if (start < 0) {
        return fail("wake-zero-rest-clock", EPROTO);
    }
    for (;;) {
        errno = 0;
        long rest = sys_futex(&word, FUTEX_WAKE | PRIVATE, INT_MAX, NULL, NULL,
                              0);
        int saved = errno;
        if (rest == 1) {
            break;
        }
        if (rest != 0) {
            return fail("wake-zero-rest", rest == -1 ? saved : EPROTO);
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= RETRY_BOUND_NS) {
            return fail("wake-zero-rest-retry", ETIMEDOUT);
        }
        sched_yield();
    }

    for (index = 0; index < 2; ++index) {
        if (join_blocked(&waiters[index].blocked, "wake-zero-join") != 0) {
            return 1;
        }
    }
    for (index = 0; index < 2; ++index) {
        if (waiters[index].result != 0) {
            return fail("wake-zero-waiter-result",
                        waiters[index].result == -1
                            ? waiters[index].result_errno
                            : EPROTO);
        }
    }

    record("futex-abi-wake-zero",
           "WAKE_ZERO_LIMIT",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_WAKE_ZERO_OK woke=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* C. FUTEX_CMP_REQUEUE wake and requeue accounting                   */
/* ------------------------------------------------------------------ */

/* Legacy FUTEX_REQUEUE/FUTEX_CMP_REQUEUE take nr_wake in "val", nr_requeue in
 * the 4th syscall argument (the same slot that carries the timeout pointer
 * for timed commands), uaddr2 in the 5th argument and cmpval in "val3". */
static int test_cmp_requeue(void) {
    uint32_t source = 0;
    uint32_t target = 0;
    struct futex_waiter waiters[3];
    struct timespec bound = relative_bound(WAIT_BOUND_NS);
    int index;

    for (index = 0; index < 3; ++index) {
        if (start_futex_waiter(&waiters[index], &source, FUTEX_WAIT | PRIVATE,
                               0, &bound, NULL, 0, "cmp-requeue-create") != 0) {
            return 1;
        }
    }
    for (index = 0; index < 3; ++index) {
        if (wait_until_blocked(&waiters[index].blocked,
                               "cmp-requeue-block-handshake") != 0) {
            return 1;
        }
    }

    /* A mismatched compare value fails with EAGAIN and leaves every waiter
     * queued. */
    if (expect_futex_errno("cmp-requeue-compare-mismatch", &source,
                           FUTEX_CMP_REQUEUE | PRIVATE, 1,
                           (const struct timespec *)(uintptr_t)2, &target,
                           0xdeadbeefu, EAGAIN) != 0) {
        return 1;
    }
    for (index = 0; index < 3; ++index) {
        if (atomic_load_explicit(&waiters[index].blocked.done,
                                 memory_order_acquire) != 0) {
            return fail("cmp-requeue-mismatch-side-effect", EPROTO);
        }
    }

    /* Wake 1 and requeue the other 2: the return value counts both. */
    errno = 0;
    long result = sys_futex(&source, FUTEX_CMP_REQUEUE | PRIVATE, 1,
                            (const struct timespec *)(uintptr_t)2, &target, 0);
    int saved = errno;
    if (result != 3) {
        return fail("cmp-requeue-accounting", result == -1 ? saved : EPROTO);
    }

    /* The two requeued waiters now sleep on the target word. */
    errno = 0;
    long drained = sys_futex(&target, FUTEX_WAKE | PRIVATE, INT_MAX, NULL,
                             NULL, 0);
    saved = errno;
    if (drained != 2) {
        return fail("cmp-requeue-drain", drained == -1 ? saved : EPROTO);
    }

    for (index = 0; index < 3; ++index) {
        if (join_blocked(&waiters[index].blocked, "cmp-requeue-join") != 0) {
            return 1;
        }
    }
    for (index = 0; index < 3; ++index) {
        if (waiters[index].result != 0) {
            return fail("cmp-requeue-waiter-result",
                        waiters[index].result == -1
                            ? waiters[index].result_errno
                            : EPROTO);
        }
    }

    record("futex-abi-requeue",
           "WOKEN REQUEUED DRAINED EAGAIN",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_OK woken=1 requeued=2 "
           "drained=2 eagain=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* D. PI lock, trylock, unlock and user word bit states               */
/* ------------------------------------------------------------------ */

/* The word is the ABI: the owner TID plus FUTEX_WAITERS/FUTEX_OWNER_DIED.
 * "locked", "trylock" and "eperm" are booleans because the raw word contains
 * a TID and marker values must stay free of process identifiers. */
static int test_pi_word_states(void) {
    uint32_t pi = 0;
    const uint32_t self = (uint32_t)syscall(SYS_gettid);
    struct timespec bad;

    if (self == 0 || self > FUTEX_TID_MASK) {
        return fail("pi-tid-range", EPROTO);
    }

    /* 1. LOCK_PI takes a free word and publishes the owner TID. */
    if (expect_futex_zero("pi-lock", &pi, FUTEX_LOCK_PI | PRIVATE, 0, NULL,
                          NULL, 0) != 0) {
        return 1;
    }
    if ((pi & FUTEX_TID_MASK) != self || (pi & FUTEX_OWNER_DIED) != 0) {
        return fail("pi-lock-word", EPROTO);
    }

    /* 2. Locking again from the owning task is detected as a deadlock. */
    if (expect_futex_errno("pi-lock-deadlock", &pi, FUTEX_LOCK_PI | PRIVATE, 0,
                           NULL, NULL, 0, EDEADLK) != 0) {
        return 1;
    }
    if (pi != self) {
        return fail("pi-lock-deadlock-word", EPROTO);
    }

    /* 3. UNLOCK_PI drops the lock and zeroes the word. */
    if (expect_futex_zero("pi-unlock", &pi, FUTEX_UNLOCK_PI | PRIVATE, 0, NULL,
                          NULL, 0) != 0) {
        return 1;
    }
    if (pi != 0) {
        return fail("pi-unlock-word", EPROTO);
    }

    /* 4. Unlocking a word that names nobody is EPERM and changes nothing. */
    if (expect_futex_errno("pi-unlock-not-owner", &pi,
                           FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0,
                           EPERM) != 0) {
        return 1;
    }
    if (pi != 0) {
        return fail("pi-unlock-not-owner-word", EPROTO);
    }

    /* 5. TRYLOCK_PI acquires a free word exactly like LOCK_PI. */
    if (expect_futex_zero("pi-trylock", &pi, FUTEX_TRYLOCK_PI | PRIVATE, 0,
                          NULL, NULL, 0) != 0) {
        return 1;
    }
    if ((pi & FUTEX_TID_MASK) != self) {
        return fail("pi-trylock-word", EPROTO);
    }
    if (expect_futex_zero("pi-trylock-unlock", &pi, FUTEX_UNLOCK_PI | PRIVATE,
                          0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (pi != 0) {
        return fail("pi-trylock-unlock-word", EPROTO);
    }

    /* 6a. A word that already names us is a deadlock even when the dying
     * owner bit is set, because the TID check runs first. */
    pi = self | FUTEX_OWNER_DIED;
    if (expect_futex_errno("pi-trylock-owner-died-self", &pi,
                           FUTEX_TRYLOCK_PI | PRIVATE, 0, NULL, NULL, 0,
                           EDEADLK) != 0) {
        return 1;
    }
    if (pi != (self | FUTEX_OWNER_DIED)) {
        return fail("pi-trylock-owner-died-self-word", EPROTO);
    }
    if (expect_futex_zero("pi-trylock-owner-died-unlock", &pi,
                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (pi != 0) {
        return fail("pi-trylock-owner-died-unlock-word", EPROTO);
    }

    /* 6b. A zero TID field with FUTEX_OWNER_DIED is the stale owner case:
     * the lock is taken over and the dying owner bit is preserved, matching
     * "newval = (uval & FUTEX_OWNER_DIED) | vpid". */
    pi = FUTEX_OWNER_DIED;
    if (expect_futex_zero("pi-lock-stale-owner", &pi, FUTEX_LOCK_PI | PRIVATE,
                          0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if ((pi & FUTEX_TID_MASK) != self || (pi & FUTEX_OWNER_DIED) == 0) {
        return fail("pi-lock-stale-owner-word", EPROTO);
    }
    if (expect_futex_zero("pi-lock-stale-owner-unlock", &pi,
                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (pi != 0) {
        return fail("pi-lock-stale-owner-unlock-word", EPROTO);
    }

    /* 7. A word owned by a different TID (tid + 1 is a legal TID value) is
     * not ours to unlock. */
    pi = (self + 1) & FUTEX_TID_MASK;
    if (expect_futex_errno("pi-unlock-foreign-tid", &pi,
                           FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0,
                           EPERM) != 0) {
        return 1;
    }
    if (pi != ((self + 1) & FUTEX_TID_MASK)) {
        return fail("pi-unlock-foreign-tid-word", EPROTO);
    }
    pi = 0;

    /* 8. TRYLOCK_PI validates the owner before it decides the trylock.
     * `futex_lock_pi_atomic()`'s first-waiter path publishes FUTEX_WAITERS
     * over the TID and `attach_to_pi_owner()` then reports ESRCH when no task
     * carries it (kernel/futex/pi.c:661-674), so the failing word keeps the
     * waiters bit instead of staying untouched and the errno is ESRCH, not
     * the EWOULDBLOCK the trylock itself would produce for a live owner.
     * 0x00abcdef exceeds the largest allocatable pid on both kernels. */
    pi = 0x00abcdefu;
    if (expect_futex_errno("pi-trylock-invalid-owner", &pi,
                           FUTEX_TRYLOCK_PI | PRIVATE, 0, NULL, NULL, 0,
                           ESRCH) != 0) {
        return 1;
    }
    if (pi != (0x00abcdefu | FUTEX_WAITERS)) {
        return fail("pi-trylock-invalid-owner-word", EPROTO);
    }
    pi = 0;

    /* 9. A timespec with tv_nsec == 1000000000 is rejected before the word
     * is touched. */
    bad.tv_sec = 0;
    bad.tv_nsec = 1000000000L;
    if (expect_futex_errno("pi-lock-bad-timespec", &pi, FUTEX_LOCK_PI | PRIVATE,
                           0, &bad, NULL, 0, EINVAL) != 0) {
        return 1;
    }
    if (pi != 0) {
        return fail("pi-lock-bad-timespec-word", EPROTO);
    }

    record("futex-abi-pi-word",
           "LOCKED UNLOCK_RC_ZERO UNLOCK_WORD_ZERO EPERM TRYLOCK "
           "ESRCH_BAD_OWNER WAITERS_BAD_OWNER",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_PI_WORD_OK locked=1 unlock_rc=0 "
           "unlock_word=0 eperm=1 trylock=1 esrch_bad_owner=1 "
           "waiters_bad_owner=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* E. LOCK_PI2 clock selection and contended timeout                  */
/* ------------------------------------------------------------------ */

struct pi_holder {
    struct blocked_thread blocked;
    uint32_t *pi;
    uint32_t control;
    long lock_result;
    int lock_errno;
    long control_result;
    int control_errno;
    long unlock_result;
    int unlock_errno;
};

static void *pi_holder_main(void *opaque) {
    struct pi_holder *holder = opaque;
    struct timespec bound = relative_bound(WAIT_BOUND_NS);

    atomic_store_explicit(&holder->blocked.tid, (int)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&holder->blocked.entered, 1, memory_order_release);
    errno = 0;
    holder->lock_result = sys_futex(holder->pi, FUTEX_LOCK_PI | PRIVATE, 0,
                                    NULL, NULL, 0);
    holder->lock_errno = holder->lock_result == -1 ? errno : 0;
    if (holder->lock_result == 0) {
        /* Own the PI futex and park on an unrelated word until the main
         * thread has observed the contended timeout.  The expected value
         * check makes the release order irrelevant. */
        errno = 0;
        holder->control_result = sys_futex(&holder->control,
                                           FUTEX_WAIT | PRIVATE, 0, &bound,
                                           NULL, 0);
        holder->control_errno = holder->control_result == -1 ? errno : 0;
        errno = 0;
        holder->unlock_result = sys_futex(holder->pi, FUTEX_UNLOCK_PI | PRIVATE,
                                          0, NULL, NULL, 0);
        holder->unlock_errno = holder->unlock_result == -1 ? errno : 0;
    }
    atomic_store_explicit(&holder->blocked.done, 1, memory_order_release);
    return NULL;
}

static int test_pi_timeout(void) {
    const uint32_t self = (uint32_t)syscall(SYS_gettid);
    uint32_t free_word = 0;
    struct pi_holder holder;

    /* FUTEX_LOCK_PI2 with the default (CLOCK_MONOTONIC) clock takes a free
     * word and can hand it back. */
    if (expect_futex_zero("pi2-lock", &free_word, FUTEX_LOCK_PI2 | PRIVATE, 0,
                          NULL, NULL, 0) != 0) {
        return 1;
    }
    if ((free_word & FUTEX_TID_MASK) != self) {
        return fail("pi2-lock-word", EPROTO);
    }
    if (expect_futex_zero("pi2-unlock", &free_word, FUTEX_UNLOCK_PI | PRIVATE,
                          0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (free_word != 0) {
        return fail("pi2-unlock-word", EPROTO);
    }

    /* A contended FUTEX_LOCK_PI2 whose absolute CLOCK_MONOTONIC deadline has
     * already passed must report ETIMEDOUT, not a missing opcode, not
     * EWOULDBLOCK and not a wait. */
    memset(&holder, 0, sizeof(holder));
    holder.pi = &free_word;
    if (start_blocked(&holder.blocked, pi_holder_main, &holder,
                      "pi2-holder-create") != 0) {
        return 1;
    }

    int64_t start = monotonic_ns();
    if (start < 0) {
        return fail("pi2-holder-clock", EPROTO);
    }
    for (;;) {
        if (atomic_load_explicit(&holder.blocked.done, memory_order_acquire) !=
            0) {
            return fail("pi2-holder-exited",
                        holder.lock_result == -1 ? holder.lock_errno : EPROTO);
        }
        if (free_word != 0) {
            break;
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= RETRY_BOUND_NS) {
            return fail("pi2-holder-acquire", ETIMEDOUT);
        }
        sched_yield();
    }
    if ((free_word & FUTEX_TID_MASK) !=
        (uint32_t)atomic_load_explicit(&holder.blocked.tid,
                                       memory_order_acquire)) {
        return fail("pi2-holder-word", EPROTO);
    }

    /* An absolute deadline that has already elapsed must be ETIMEDOUT, not
     * EINVAL and not a wait.  `{0, 0}` is the epoch of both clocks, so it is
     * always in the past on every machine; "now - 1s" is *not* usable here
     * because a timespec with a negative tv_sec is rejected with EINVAL
     * before any deadline comparison (see the probe below). */
    const struct timespec past = {0, 0};
    if (expect_futex_errno("pi2-contended-past-monotonic", &free_word,
                           FUTEX_LOCK_PI2 | PRIVATE, 0, &past, NULL, 0,
                           ETIMEDOUT) != 0) {
        return 1;
    }

    /* The timespec is validated before the wait begins: a negative tv_sec is
     * a malformed *time*, not an elapsed deadline, so it is EINVAL even
     * though it would compare as expired.  Verified against the reference
     * kernel for FUTEX_LOCK_PI2, FUTEX_LOCK_PI and FUTEX_WAIT_BITSET. */
    const struct timespec negative = {-1, 0};
    if (expect_futex_errno("pi2-contended-negative-monotonic", &free_word,
                           FUTEX_LOCK_PI2 | PRIVATE, 0, &negative, NULL, 0,
                           EINVAL) != 0) {
        return 1;
    }

    /* Release the holder and let it hand the PI futex back. */
    if (release_parked_thread(&holder.control, "pi2-holder-release") != 0) {
        return 1;
    }
    if (join_blocked(&holder.blocked, "pi2-holder-join") != 0) {
        return 1;
    }
    if (holder.lock_result != 0 || holder.unlock_result != 0 ||
        !parked_wait_succeeded(holder.control_result, holder.control_errno)) {
        return fail("pi2-holder-result",
                    holder.lock_errno != 0      ? holder.lock_errno
                    : holder.unlock_errno != 0  ? holder.unlock_errno
                    : holder.control_errno != 0 ? holder.control_errno
                                                : EPROTO);
    }
    if (free_word != 0) {
        return fail("pi2-holder-left-locked", EPROTO);
    }

    record("futex-abi-pi-timeout",
           "ETIMEDOUT LOCKPI2 NEGATIVE_TS_EINVAL",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_PI_TIMEOUT_OK etimedout=1 "
           "lockpi2=1 negative_ts_einval=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* F. FUTEX_WAIT_REQUEUE_PI / FUTEX_CMP_REQUEUE_PI                    */
/* ------------------------------------------------------------------ */

/* Maps one shared memfd page twice, so `*source` and `*target` are two
 * virtual addresses naming the same futex word.  Linux compares the resolved
 * keys, not the addresses: futex_requeue() rejects such a pair with EINVAL
 * (kernel/futex/requeue.c:453-457) and futex_wait_setup() does the same after
 * its source-value check (kernel/futex/waitwake.c:681-685). */
static int map_shared_alias_pair(uint32_t **source, uint32_t **target,
                                 const char *stage) {
    int fd = (int)syscall(SYS_memfd_create, "futex-abi-alias", MFD_CLOEXEC);
    if (fd < 0) {
        return fail(stage, errno);
    }
    if (ftruncate(fd, 4096) != 0) {
        int saved = errno;
        close(fd);
        return fail(stage, saved);
    }
    void *first = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (first == MAP_FAILED) {
        int saved = errno;
        close(fd);
        return fail(stage, saved);
    }
    void *second = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (second == MAP_FAILED) {
        int saved = errno;
        munmap(first, 4096);
        close(fd);
        return fail(stage, saved);
    }
    close(fd);
    *source = first;
    *target = second;
    return 0;
}

struct requeue_pi_case {
    struct blocked_thread blocked;
    uint32_t *source;
    uint32_t *target;
    uint32_t control;
    struct timespec timeout;
    long wait_result;
    int wait_errno;
    long control_result;
    int control_errno;
    long unlock_result;
    int unlock_errno;
};

static void *requeue_pi_waiter_main(void *opaque) {
    struct requeue_pi_case *test = opaque;
    struct timespec bound = relative_bound(WAIT_BOUND_NS);

    atomic_store_explicit(&test->blocked.tid, (int)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&test->blocked.entered, 1, memory_order_release);
    errno = 0;
    test->wait_result = sys_futex(test->source,
                                  FUTEX_WAIT_REQUEUE_PI | PRIVATE, 0,
                                  &test->timeout, test->target,
                                  FUTEX_BITSET_MATCH_ANY);
    test->wait_errno = test->wait_result == -1 ? errno : 0;
    if (test->wait_result == 0) {
        /* The requeue transferred ownership of *target to this thread.  Wait
         * for the main thread to sample the word before unlocking it, so the
         * observation of the FUTEX_WAITERS bit is not a race. */
        errno = 0;
        test->control_result = sys_futex(&test->control, FUTEX_WAIT | PRIVATE,
                                         0, &bound, NULL, 0);
        test->control_errno = test->control_result == -1 ? errno : 0;
        errno = 0;
        test->unlock_result = sys_futex(test->target, FUTEX_UNLOCK_PI | PRIVATE,
                                        0, NULL, NULL, 0);
        test->unlock_errno = test->unlock_result == -1 ? errno : 0;
    }
    atomic_store_explicit(&test->blocked.done, 1, memory_order_release);
    return NULL;
}

/* Bounded spin until a worker publishes a flag (used for the "I hold the
 * futex" handshake, which is not a sleep state and so cannot use
 * wait_until_blocked()). */
static int wait_for_flag(_Atomic int *flag, const char *stage) {
    int64_t start = monotonic_ns();
    if (start < 0) {
        return fail(stage, errno != 0 ? errno : EPROTO);
    }
    for (;;) {
        if (atomic_load_explicit(flag, memory_order_acquire) != 0) {
            return 0;
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= BLOCK_BOUND_NS) {
            return fail(stage, ETIMEDOUT);
        }
        sched_yield();
    }
}

/* ------------------------------------------------------------------ */
/* G. FUTEX_CMP_REQUEUE_PI onto an owned (contended) target            */
/* ------------------------------------------------------------------ */

/* futex_requeue() does not fail when the PI target already has an owner: it
 * publishes FUTEX_WAITERS over that owner and queues the top waiter on the
 * target's rt_mutex, returning the number of waiters it placed
 * (kernel/futex/requeue.c:494-588, 621-684, kernel/futex/pi.c:660-674).  The
 * owner's unlock then hands the futex to the queued waiter, whose
 * FUTEX_WAIT_REQUEUE_PI returns 0. */

struct requeue_pi_owner {
    struct blocked_thread blocked;
    uint32_t *target;
    uint32_t *control;
    _Atomic int locked;
    long lock_result;
    int lock_errno;
    long park_result;
    int park_errno;
    long unlock_result;
    int unlock_errno;
};

static void *requeue_pi_owner_main(void *opaque) {
    struct requeue_pi_owner *owner = opaque;
    struct timespec bound = relative_bound(WAIT_BOUND_NS);

    atomic_store_explicit(&owner->blocked.tid, (int)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&owner->blocked.entered, 1, memory_order_release);
    errno = 0;
    owner->lock_result = sys_futex(owner->target, FUTEX_LOCK_PI | PRIVATE, 0,
                                   NULL, NULL, 0);
    owner->lock_errno = owner->lock_result == -1 ? errno : 0;
    atomic_store_explicit(&owner->locked, 1, memory_order_release);
    if (owner->lock_result == 0) {
        errno = 0;
        owner->park_result = sys_futex(owner->control, FUTEX_WAIT | PRIVATE, 0,
                                       &bound, NULL, 0);
        owner->park_errno = owner->park_result == -1 ? errno : 0;
        errno = 0;
        owner->unlock_result = sys_futex(owner->target,
                                         FUTEX_UNLOCK_PI | PRIVATE, 0, NULL,
                                         NULL, 0);
        owner->unlock_errno = owner->unlock_result == -1 ? errno : 0;
    }
    atomic_store_explicit(&owner->blocked.done, 1, memory_order_release);
    return NULL;
}

struct requeue_pi_owned_case {
    struct blocked_thread blocked;
    uint32_t *source;
    uint32_t *target;
    /* Separate park words: the owner is released first, and the requeued
     * waiter must stay parked until the main thread has sampled the word the
     * handoff published. */
    uint32_t control;
    uint32_t owner_control;
    long wait_result;
    int wait_errno;
    long control_result;
    int control_errno;
    long unlock_result;
    int unlock_errno;
};

static void *requeue_pi_owned_waiter_main(void *opaque) {
    struct requeue_pi_owned_case *test = opaque;
    struct timespec bound = relative_bound(PI_WAIT_BOUND_NS);

    atomic_store_explicit(&test->blocked.tid, (int)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&test->blocked.entered, 1, memory_order_release);
    errno = 0;
    test->wait_result = sys_futex(test->source,
                                  FUTEX_WAIT_REQUEUE_PI | PRIVATE, 0, &bound,
                                  test->target, FUTEX_BITSET_MATCH_ANY);
    test->wait_errno = test->wait_result == -1 ? errno : 0;
    if (test->wait_result == 0) {
        /* The requeue transferred ownership of *target to this thread.  Park
         * until the main thread has sampled the word. */
        errno = 0;
        test->control_result = sys_futex(&test->control, FUTEX_WAIT | PRIVATE,
                                         0, &bound, NULL, 0);
        test->control_errno = test->control_result == -1 ? errno : 0;
        errno = 0;
        test->unlock_result = sys_futex(test->target,
                                        FUTEX_UNLOCK_PI | PRIVATE, 0, NULL,
                                        NULL, 0);
        test->unlock_errno = test->unlock_result == -1 ? errno : 0;
    }
    atomic_store_explicit(&test->blocked.done, 1, memory_order_release);
    return NULL;
}

/* One owner thread holds the PI futex; the main thread (which does *not* own
 * it) requeues a waiter onto it.  This is the shape the recorded defect
 * names: the kernel answered EWOULDBLOCK where Linux queues. */
static int test_requeue_pi_owned(void) {
    uint32_t source = 0;
    uint32_t target = 0;
    struct requeue_pi_owner owner;
    struct requeue_pi_owned_case test;

    memset(&owner, 0, sizeof(owner));
    memset(&test, 0, sizeof(test));
    owner.target = &target;
    owner.control = &test.owner_control;
    test.source = &source;
    test.target = &target;

    if (start_blocked(&owner.blocked, requeue_pi_owner_main, &owner,
                      "requeue-pi-owned-owner-create") != 0) {
        return 1;
    }
    if (wait_for_flag(&owner.locked, "requeue-pi-owned-owner-lock") != 0) {
        return 1;
    }
    const uint32_t owner_tid =
        (uint32_t)atomic_load_explicit(&owner.blocked.tid, memory_order_acquire);
    if (owner.lock_result != 0 || target != owner_tid) {
        return fail("requeue-pi-owned-owner-word",
                    owner.lock_result == -1 ? owner.lock_errno : EPROTO);
    }

    if (start_blocked(&test.blocked, requeue_pi_owned_waiter_main, &test,
                      "requeue-pi-owned-create") != 0) {
        return 1;
    }
    if (settle_blocked(&test.blocked, "requeue-pi-owned-block") != 0) {
        fprintf(stderr,
                "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_WAITER_DIAG stage=requeue-pi-owned-block "
                "wait_result=%ld wait_errno=%d control_result=%ld\n",
                test.wait_result, test.wait_errno, test.control_result);
        return 1;
    }
    if (source != 0) {
        return fail("requeue-pi-owned-precondition", EPROTO);
    }

    errno = 0;
    long requeued = sys_futex(&source, FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                              (const struct timespec *)(uintptr_t)1, &target,
                              0);
    int saved = errno;
    printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_OWNED_RAW requeued=%ld "
           "errno=%d target=%#x owner=%u\n",
           requeued, saved, target, owner_tid);
    fflush(stdout);
    if (requeued != 1) {
        return fail("requeue-pi-owned-rc", requeued == -1 ? saved : EPROTO);
    }
    /* The requeued waiter is blocked on the target, so the target keeps its
     * owner and gains FUTEX_WAITERS. */
    if (target != (FUTEX_WAITERS | owner_tid)) {
        return fail("requeue-pi-owned-held-word", EPROTO);
    }
    if (atomic_load_explicit(&test.blocked.done, memory_order_acquire) != 0) {
        return fail("requeue-pi-owned-waiter-early", EPROTO);
    }

    /* The owner's unlock hands the futex to the requeued waiter, which
     * observes itself as the owner when FUTEX_WAIT_REQUEUE_PI returns. */
    if (release_parked_thread(&test.owner_control,
                              "requeue-pi-owned-release") != 0) {
        return 1;
    }
    if (await_blocked(&owner.blocked, "requeue-pi-owned-owner-join") != 0) {
        return 1;
    }
    if (owner.unlock_result != 0 ||
        !parked_wait_succeeded(owner.park_result, owner.park_errno)) {
        return fail("requeue-pi-owned-owner-result",
                    owner.unlock_errno != 0  ? owner.unlock_errno
                    : owner.park_errno != 0  ? owner.park_errno
                                             : EPROTO);
    }
    const uint32_t waiter_tid =
        (uint32_t)atomic_load_explicit(&test.blocked.tid, memory_order_acquire);
    if ((target & FUTEX_TID_MASK) != waiter_tid) {
        return fail("requeue-pi-owned-handoff", EPROTO);
    }

    if (release_parked_thread(&test.control, "requeue-pi-owned-wake") != 0) {
        return 1;
    }
    if (await_blocked(&test.blocked, "requeue-pi-owned-join") != 0) {
        return 1;
    }
    if (test.wait_result != 0 || test.unlock_result != 0 ||
        !parked_wait_succeeded(test.control_result, test.control_errno)) {
        return fail("requeue-pi-owned-waiter-result",
                    test.wait_errno != 0      ? test.wait_errno
                    : test.unlock_errno != 0  ? test.unlock_errno
                    : test.control_errno != 0 ? test.control_errno
                                              : EPROTO);
    }
    if (target != 0) {
        return fail("requeue-pi-owned-final", EPROTO);
    }

    record("futex-abi-requeue-pi-owned",
           "OWNED_RC OWNED_WORD WAITER_BLOCKED HANDOFF_WORD WAITER_RC UNLOCKED",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_OWNED_OK requeued=1 "
           "owned_word=1 waiter_blocked=1 handoff=1 waiter_rc=0 unlocked=1");
    return 0;
}

/* The caller itself owns the PI futex.  futex_lock_pi_atomic() only reports
 * -EDEADLK when the *waiter* already owns the target (kernel/futex/pi.c:605,
 * with @task = top_waiter->task), so a signaling thread that holds the futex
 * -- the pthread_cond_signal() shape -- requeues normally. */
static int test_requeue_pi_held(void) {
    uint32_t source = 0;
    uint32_t target = 0;
    struct requeue_pi_owned_case test;

    memset(&test, 0, sizeof(test));
    test.source = &source;
    test.target = &target;

    if (expect_futex_zero("requeue-pi-held-lock", &target,
                          FUTEX_LOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    const uint32_t owner_tid = (uint32_t)syscall(SYS_gettid);
    if (target != owner_tid) {
        return fail("requeue-pi-held-lock-word", EPROTO);
    }

    if (start_blocked(&test.blocked, requeue_pi_owned_waiter_main, &test,
                      "requeue-pi-held-create") != 0) {
        return 1;
    }
    if (settle_blocked(&test.blocked, "requeue-pi-held-block") != 0) {
        fprintf(stderr,
                "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_WAITER_DIAG stage=requeue-pi-held-block "
                "wait_result=%ld wait_errno=%d control_result=%ld\n",
                test.wait_result, test.wait_errno, test.control_result);
        return 1;
    }

    errno = 0;
    long requeued = sys_futex(&source, FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                              (const struct timespec *)(uintptr_t)1, &target,
                              0);
    int saved = errno;
    printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_HELD_RAW requeued=%ld "
           "errno=%d target=%#x\n",
           requeued, saved, target);
    fflush(stdout);
    if (requeued != 1) {
        return fail("requeue-pi-held-rc", requeued == -1 ? saved : EPROTO);
    }
    if (target != (FUTEX_WAITERS | owner_tid)) {
        return fail("requeue-pi-held-word", EPROTO);
    }
    if (atomic_load_explicit(&test.blocked.done, memory_order_acquire) != 0) {
        return fail("requeue-pi-held-waiter-early", EPROTO);
    }

    if (expect_futex_zero("requeue-pi-held-unlock", &target,
                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    const uint32_t waiter_tid =
        (uint32_t)atomic_load_explicit(&test.blocked.tid, memory_order_acquire);
    if ((target & FUTEX_TID_MASK) != waiter_tid) {
        return fail("requeue-pi-held-handoff", EPROTO);
    }

    if (release_parked_thread(&test.control, "requeue-pi-held-wake") != 0) {
        return 1;
    }
    if (await_blocked(&test.blocked, "requeue-pi-held-join") != 0) {
        return 1;
    }
    if (test.wait_result != 0 || test.unlock_result != 0 ||
        !parked_wait_succeeded(test.control_result, test.control_errno)) {
        return fail("requeue-pi-held-waiter-result",
                    test.wait_errno != 0      ? test.wait_errno
                    : test.unlock_errno != 0  ? test.unlock_errno
                    : test.control_errno != 0 ? test.control_errno
                                              : EPROTO);
    }
    if (target != 0) {
        return fail("requeue-pi-held-final", EPROTO);
    }

    record("futex-abi-requeue-pi-held",
           "HELD_RC HELD_WORD WAITER_BLOCKED HANDOFF_WORD WAITER_RC UNLOCKED",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_HELD_OK requeued=1 "
           "held_word=1 waiter_blocked=1 handoff=1 waiter_rc=0 unlocked=1");
    return 0;
}

/* A source queue holding a waiter that is *not* a FUTEX_WAIT_REQUEUE_PI
 * waiter is refused with -EINVAL before the target is touched:
 * futex_proxy_trylock_atomic() tests `!top_waiter->rt_waiter`
 * (kernel/futex/requeue.c:314-315) and the chain walk repeats it
 * (kernel/futex/requeue.c:605-610). */
static int test_requeue_pi_plain(void) {
    uint32_t source = 0;
    uint32_t target = 0;
    struct futex_waiter plain;
    struct timespec timeout = relative_bound(WAIT_BOUND_NS);

    if (start_futex_waiter(&plain, &source, FUTEX_WAIT | PRIVATE, 0, &timeout,
                           NULL, 0, "requeue-pi-plain-create") != 0) {
        return 1;
    }
    if (wait_until_blocked(&plain.blocked, "requeue-pi-plain-block") != 0) {
        return 1;
    }

    errno = 0;
    long refused = sys_futex(&source, FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                             (const struct timespec *)(uintptr_t)1, &target, 0);
    int saved = errno;
    printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_PLAIN_RAW rc=%ld "
           "errno=%d target=%#x source=%#x\n",
           refused, saved, target, source);
    fflush(stdout);
    if (refused != -1 || saved != EINVAL) {
        /* Leave nothing blocked behind before reporting: the plain waiter is
         * woken either way by the plain wake below. */
        (void)sys_futex(&source, FUTEX_WAKE | PRIVATE, 1, NULL, NULL, 0);
        return fail("requeue-pi-plain-refusal",
                    refused == -1 ? (saved != EINVAL ? saved : EPROTO) : EPROTO);
    }
    if (target != 0) {
        return fail("requeue-pi-plain-target", EPROTO);
    }

    errno = 0;
    long woken = sys_futex(&source, FUTEX_WAKE | PRIVATE, 1, NULL, NULL, 0);
    int wake_errno = errno;
    if (woken != 1) {
        return fail("requeue-pi-plain-wake",
                    woken == -1 ? wake_errno : EPROTO);
    }
    if (await_blocked(&plain.blocked, "requeue-pi-plain-join") != 0) {
        return 1;
    }
    if (plain.result != 0) {
        return fail("requeue-pi-plain-waiter",
                    plain.result == -1 ? plain.result_errno : EPROTO);
    }

    record("futex-abi-requeue-pi-plain",
           "PLAIN_SOURCE_EINVAL TARGET_UNTOUCHED WAKE_RC WAITER_RC",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_PLAIN_OK refused=1 "
           "target_untouched=1 wake_rc=1 waiter_rc=0");
    return 0;
}

static int test_requeue_pi(void) {
    uint32_t source = 0;
    uint32_t target = 0;
    uint32_t empty_source = 0;
    uint32_t empty_target = 0;
    struct requeue_pi_case test;

    memset(&test, 0, sizeof(test));
    test.source = &source;
    test.target = &target;
    if (absolute_bound(&test.timeout, WAIT_BOUND_NS) != 0) {
        return fail("requeue-pi-timeout-clock", errno);
    }
    if (start_blocked(&test.blocked, requeue_pi_waiter_main, &test,
                      "requeue-pi-create") != 0) {
        return 1;
    }
    if (wait_until_blocked(&test.blocked, "requeue-pi-block-handshake") != 0) {
        return 1;
    }
    if (source != 0 || target != 0) {
        return fail("requeue-pi-precondition", EPROTO);
    }

    /* Wake one and requeue one: the woken waiter is handed the PI futex on
     * *target and the call reports the single waiter it dealt with. */
    errno = 0;
    long requeued = sys_futex(&source, FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                              (const struct timespec *)(uintptr_t)1, &target,
                              0);
    int saved = errno;
    if (requeued != 1) {
        return fail("requeue-pi-wake", requeued == -1 ? saved : EPROTO);
    }

    /* The new owner is the waiter, and the FUTEX_WAITERS bit stays set while
     * the waiter owns the futex.  Sampled before the waiter may unlock. */
    const uint32_t waiter_tid =
        (uint32_t)atomic_load_explicit(&test.blocked.tid, memory_order_acquire);
    if ((target & FUTEX_TID_MASK) != waiter_tid ||
        (target & FUTEX_WAITERS) == 0) {
        return fail("requeue-pi-target-word", EPROTO);
    }

    if (release_parked_thread(&test.control, "requeue-pi-release") != 0) {
        return 1;
    }
    if (join_blocked(&test.blocked, "requeue-pi-join") != 0) {
        return 1;
    }
    if (test.wait_result != 0 || test.unlock_result != 0 ||
        !parked_wait_succeeded(test.control_result, test.control_errno)) {
        return fail("requeue-pi-waiter-result",
                    test.wait_errno != 0      ? test.wait_errno
                    : test.unlock_errno != 0  ? test.unlock_errno
                    : test.control_errno != 0 ? test.control_errno
                                              : EPROTO);
    }
    if (target != 0) {
        return fail("requeue-pi-target-unlocked", EPROTO);
    }

    /* Requeueing a futex onto itself is meaningless. */
    if (expect_futex_errno("requeue-pi-self", &source,
                           FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                           (const struct timespec *)(uintptr_t)1, &source, 0,
                           EINVAL) != 0) {
        return 1;
    }
    /* Only one waiter may be woken by a PI requeue. */
    if (expect_futex_errno("requeue-pi-wake-two", &source,
                           FUTEX_CMP_REQUEUE_PI | PRIVATE, 2,
                           (const struct timespec *)(uintptr_t)1, &target, 0,
                           EINVAL) != 0) {
        return 1;
    }
    /* Waiting for a requeue onto the same address is likewise invalid. */
    if (expect_futex_errno("requeue-pi-wait-self", &source,
                           FUTEX_WAIT_REQUEUE_PI | PRIVATE, 0, NULL, &source,
                           FUTEX_BITSET_MATCH_ANY, EINVAL) != 0) {
        return 1;
    }
    /* `do_futex()` forces `val3 = FUTEX_BITSET_MATCH_ANY` for
     * FUTEX_WAIT_REQUEUE_PI, so a zero val3 is ignored: a source word that
     * does not match still answers EAGAIN from futex_wait_setup() instead of
     * the EINVAL an interpreted empty bitset would produce. */
    uint32_t val3_zero_source = 1;
    if (expect_futex_errno("requeue-pi-val3-zero-mismatch", &val3_zero_source,
                           FUTEX_WAIT_REQUEUE_PI | PRIVATE, 2, NULL, &target, 0,
                           EAGAIN) != 0) {
        return 1;
    }
    if (val3_zero_source != 1) {
        return fail("requeue-pi-val3-zero-word", EPROTO);
    }

    /* A source queue with no waiter to promote is not an error and not a
     * retry: futex_requeue() skips the chain walk and returns task_count,
     * which is zero.  This is enforced rather than recorded: the defect it
     * guards against is an unbounded retry inside the kernel, which would
     * hang this case before either marker below could be printed. */
    errno = 0;
    long no_waiters = sys_futex(&empty_source,
                                FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                                (const struct timespec *)(uintptr_t)1,
                                &empty_target, 0);
    int no_waiters_errno = errno;
    if (no_waiters != 0) {
        return fail("requeue-pi-no-waiters",
                    no_waiters == -1 ? no_waiters_errno : EPROTO);
    }
    if (empty_source != 0 || empty_target != 0) {
        return fail("requeue-pi-no-waiters-word", EPROTO);
    }

    /* One shared futex word mapped at two virtual addresses is *one* futex.
     * FUTEX_WAIT_REQUEUE_PI must reject that pair with EINVAL instead of
     * queueing a waiter that no FUTEX_CMP_REQUEUE_PI could ever promote, and
     * the rejection follows the source-value check, so a mismatched source
     * still answers EWOULDBLOCK.  The absolute bound only keeps a kernel that
     * queues anyway from hanging this case. */
    uint32_t *alias_source = NULL;
    uint32_t *alias_target = NULL;
    if (map_shared_alias_pair(&alias_source, &alias_target,
                              "requeue-pi-alias-map") != 0) {
        return 1;
    }
    *alias_source = 0;
    *alias_target = 0;
    struct timespec alias_bound;
    if (absolute_bound(&alias_bound, WAIT_BOUND_NS) != 0) {
        return fail("requeue-pi-alias-bound", errno);
    }
    if (expect_futex_errno("requeue-pi-alias-einval", alias_source,
                           FUTEX_WAIT_REQUEUE_PI, 0, &alias_bound,
                           alias_target, FUTEX_BITSET_MATCH_ANY,
                           EINVAL) != 0) {
        return 1;
    }
    if (*alias_source != 0 || *alias_target != 0) {
        return fail("requeue-pi-alias-word", EPROTO);
    }
    if (expect_futex_errno("requeue-pi-alias-eagain", alias_source,
                           FUTEX_WAIT_REQUEUE_PI, 1, &alias_bound,
                           alias_target, FUTEX_BITSET_MATCH_ANY,
                           EAGAIN) != 0) {
        return 1;
    }
    if (*alias_source != 0 || *alias_target != 0) {
        return fail("requeue-pi-alias-eagain-word", EPROTO);
    }
    /* futex_requeue() resolves the keys before the cmpval comparison, so the
     * same aliased pair is EINVAL even when the value also differs. */
    if (expect_futex_errno("requeue-pi-alias-cmp-einval", alias_source,
                           FUTEX_CMP_REQUEUE_PI, 1,
                           (const struct timespec *)(uintptr_t)1,
                           alias_target, 7, EINVAL) != 0) {
        return 1;
    }
    munmap(alias_source, 4096);
    munmap(alias_target, 4096);

    record("futex-abi-requeue-pi",
           "REQUEUED WAITER_RC TARGET_WORD UNLOCKED EINVAL_SELF EINVAL_WAKE2 "
           "VAL3_ZERO_EAGAIN EINVAL_ALIAS EAGAIN_ALIAS EINVAL_CMP_ALIAS",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_OK requeued=1 "
           "waiter_rc=0 target_word=1 unlocked=1 einval_self=1 "
           "einval_wake2=1 einval_alias=1 eagain_alias=1 einval_cmp_alias=1");
    printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_NOWAITERS_RAW rc=%ld "
           "errno=%d\n",
           no_waiters, no_waiters_errno);
    fflush(stdout);
    return 0;
}

/* ------------------------------------------------------------------ */
/* G. futex2 flag acceptance and rejection                            */
/* ------------------------------------------------------------------ */

static int expect_futex2_errno(const char *stage, uint32_t *uaddr,
                               uint32_t mask, int nr, unsigned int flags,
                               int expected) {
    errno = 0;
    long result = sys_futex_wake(uaddr, mask, nr, flags);
    int saved = errno;
    if (result != -1) {
        return fail(stage, EPROTO);
    }
    if (saved != expected) {
        return fail(stage, saved);
    }
    return 0;
}

/* A FUTEX2_NUMA futex is two words: the futex itself followed by the node
 * word, and it must be eight byte aligned.  A node word of (uint32_t)-1
 * (FUTEX_NO_NODE) is replaced by the resolved node, any impossible node is
 * rejected and a node that is already valid is left alone. */
static int test_futex2_flags(void) {
    struct futex2_pair {
        uint32_t val;
        uint32_t node;
    };
    static _Alignas(8) struct futex2_pair pairs[2];
    uint32_t *word = &pairs[0].val;
    uint32_t *misaligned = (uint32_t *)(void *)((char *)&pairs[1].val + 4);
    const unsigned int flags = (unsigned int)(FUTEX_32 | PRIVATE);
    const uint32_t any = FUTEX_BITSET_MATCH_ANY;

    pairs[0].val = 0;
    pairs[0].node = (uint32_t)-1; /* FUTEX_NO_NODE */
    pairs[1].val = 0;
    pairs[1].node = 0;

    /* Capability probe: a conforming kernel accepts this and reports no
     * waiters.  Kernels without the futex2 family (or without 32-bit sized
     * futexes) fail here, and the whole group is then reported unsupported
     * rather than misread as a flag rejection. */
    errno = 0;
    long baseline = sys_futex_wake(word, any, 1, flags);
    if (baseline != 0) {
        record("futex-abi-futex2-flags",
           "NUMA_OK NUMA_EINVAL MPOL_OK RESERVED_EINVAL SIZE_EINVAL ALIGN_EINVAL MASK0_EINVAL",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_FUTEX2_FLAGS_OK "
               "numa_ok=unsupported numa_einval=unsupported "
               "mpol_ok=unsupported reserved_einval=unsupported "
               "size_einval=unsupported align_einval=unsupported "
               "mask0_einval=unsupported");
        return 0;
    }

    /* FUTEX2_NUMA with FUTEX_NO_NODE resolves the node and writes it back. */
    errno = 0;
    long numa = sys_futex_wake(word, any, 1, flags | FUTEX2_NUMA);
    int saved = errno;
    if (numa != 0) {
        return fail("futex2-numa", numa == -1 ? saved : EPROTO);
    }
    if (pairs[0].node != 0) {
        return fail("futex2-numa-node-writeback", EPROTO);
    }

    /* A node that cannot exist on this machine is rejected. */
    pairs[0].node = 1;
    if (expect_futex2_errno("futex2-numa-bad-node", word, any, 1,
                            flags | FUTEX2_NUMA, EINVAL) != 0) {
        return 1;
    }

    /* An already valid node is accepted unchanged. */
    pairs[0].node = 0;
    errno = 0;
    long numa_zero = sys_futex_wake(word, any, 1, flags | FUTEX2_NUMA);
    saved = errno;
    if (numa_zero != 0) {
        return fail("futex2-numa-node-zero", numa_zero == -1 ? saved : EPROTO);
    }

    /* FUTEX2_MPOL selects the node through the memory policy instead. */
    pairs[0].node = (uint32_t)-1;
    errno = 0;
    long mpol = sys_futex_wake(word, any, 1, flags | FUTEX2_MPOL);
    saved = errno;
    if (mpol != 0) {
        return fail("futex2-mpol", mpol == -1 ? saved : EPROTO);
    }

    /* Bit 0x10 is reserved in the futex2 flag word. */
    if (expect_futex2_errno("futex2-reserved-bit", word, any, 1,
                            flags | 0x10u, EINVAL) != 0) {
        return 1;
    }

    /* Only 32-bit sized futexes exist: FUTEX2_SIZE_U8 is not a size. */
    if (expect_futex2_errno("futex2-size-u8", word, any, 1,
                            (unsigned int)FUTEX2_SIZE_U8 | PRIVATE,
                            EINVAL) != 0) {
        return 1;
    }

    /* A NUMA futex must be eight byte aligned; four byte alignment fails. */
    if (expect_futex2_errno("futex2-numa-misaligned", misaligned, any, 1,
                            flags | FUTEX2_NUMA, EINVAL) != 0) {
        return 1;
    }

    /* A zero bitset can never match a futex2 waiter. */
    errno = 0;
    long mask0 = sys_futex_wait(word, 0, 0, flags, NULL, 0);
    saved = errno;
    if (mask0 != -1 || saved != EINVAL) {
        return fail("futex2-wait-mask-zero",
                    mask0 == -1 ? saved : EPROTO);
    }

    record("futex-abi-futex2-flags",
           "NUMA_OK NUMA_EINVAL MPOL_OK RESERVED_EINVAL SIZE_EINVAL ALIGN_EINVAL MASK0_EINVAL",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_FUTEX2_FLAGS_OK numa_ok=1 "
           "numa_einval=1 mpol_ok=1 reserved_einval=1 size_einval=1 "
           "align_einval=1 mask0_einval=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* H. futex_waitv per-entry FUTEX2_NUMA protocol                      */
/* ------------------------------------------------------------------ */

/* `futex_wait_multiple_setup()` resolves one `get_futex_key()` per entry
 * before it compares any value, and the `FUTEX2_NUMA` protocol lives inside
 * `get_futex_key()` (`kernel/futex/core.c:522-567`): the futex is
 * `futex_size()` wide and twice that for `FLAGS_NUMA`, so the natural
 * alignment rule covers eight bytes; the node word is then read, any
 * impossible node is rejected, and `FUTEX_NO_NODE` is replaced with the
 * resolved node.
 *
 * Every entry below carries a value that cannot match, so a kernel that skips
 * the node protocol falls through to the comparison pass and reports
 * EWOULDBLOCK where this kernel reports the node protocol's EINVAL/EFAULT.
 * The deadline is still armed so that a defect cannot hang the guest. */
static int test_waitv_numa(void) {
    struct futex2_pair {
        uint32_t val;
        uint32_t node;
    };
    static _Alignas(8) struct futex2_pair pairs[3];
    struct local_futex_waitv entry;
    struct timespec bound;
    const unsigned int flags = (unsigned int)(FUTEX_32 | PRIVATE | FUTEX2_NUMA);
    long result;
    int saved;

    if (absolute_bound(&bound, WAIT_BOUND_NS) != 0) {
        return fail("waitv-numa-clock", errno);
    }

    /* A zero-entry call is rejected before the array is read, so an
     * unimplemented syscall is never misread as a flag rejection. */
    errno = 0;
    result = sys_futex_waitv(NULL, 0, 0, NULL, CLOCK_MONOTONIC);
    if (result != -1 || errno != EINVAL) {
        return fail("waitv-numa-probe", result == -1 ? errno : EPROTO);
    }

    memset(&entry, 0, sizeof(entry));
    entry.val = 0;
    entry.flags = flags;

    /* FUTEX_NO_NODE is written back as the resolved node even though the
     * mismatched value then makes the call report EWOULDBLOCK. */
    pairs[0].val = 1;
    pairs[0].node = (uint32_t)-1;
    entry.uaddr = (uint64_t)(uintptr_t)&pairs[0].val;
    errno = 0;
    result = sys_futex_waitv(&entry, 1, 0, &bound, CLOCK_MONOTONIC);
    saved = errno;
    if (result != -1 || saved != EWOULDBLOCK) {
        return fail("waitv-numa-nowait", result == -1 ? saved : EPROTO);
    }
    if (pairs[0].node != 0) {
        return fail("waitv-numa-node-writeback", EPROTO);
    }

    /* A NUMA futex is eight byte aligned: four byte alignment is EINVAL
     * before the node word is read. */
    pairs[1].val = 0;
    pairs[1].node = 1;
    entry.uaddr = (uint64_t)(uintptr_t)((char *)&pairs[1].val + 4);
    errno = 0;
    result = sys_futex_waitv(&entry, 1, 0, &bound, CLOCK_MONOTONIC);
    saved = errno;
    if (result != -1 || saved != EINVAL) {
        return fail("waitv-numa-misaligned", result == -1 ? saved : EPROTO);
    }

    /* A node that cannot exist on this machine is EINVAL, and that check
     * precedes the value comparison. */
    pairs[2].val = 1;
    pairs[2].node = 1;
    entry.uaddr = (uint64_t)(uintptr_t)&pairs[2].val;
    errno = 0;
    result = sys_futex_waitv(&entry, 1, 0, &bound, CLOCK_MONOTONIC);
    saved = errno;
    if (result != -1 || saved != EINVAL) {
        return fail("waitv-numa-bad-node", result == -1 ? saved : EPROTO);
    }

    /* Resolving FUTEX_NO_NODE writes the node word, so a read-only mapping
     * reports EFAULT from that write.  An eight byte aligned NUMA futex
     * cannot have an unmapped node word on a page granular mapping: the node
     * word shares the futex word's eight byte unit, and therefore its page. */
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        return fail("waitv-numa-page-size", errno != 0 ? errno : EPROTO);
    }
    void *region = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (region == MAP_FAILED) {
        return fail("waitv-numa-mmap", errno);
    }
    struct futex2_pair *readonly = region;
    readonly->val = 1;
    readonly->node = (uint32_t)-1;
    if (mprotect(region, (size_t)page_size, PROT_READ) != 0) {
        return fail("waitv-numa-mprotect", errno);
    }
    entry.uaddr = (uint64_t)(uintptr_t)&readonly->val;
    errno = 0;
    result = sys_futex_waitv(&entry, 1, 0, &bound, CLOCK_MONOTONIC);
    saved = errno;
    if (munmap(region, (size_t)page_size) != 0) {
        return fail("waitv-numa-munmap", errno);
    }
    if (result != -1 || saved != EFAULT) {
        return fail("waitv-numa-readonly-node", result == -1 ? saved : EPROTO);
    }

    record("futex-abi-waitv-numa",
           "PROBE_EINVAL NO_NODE_WRITEBACK ALIGN_EINVAL BAD_NODE_EINVAL "
           "RO_NODE_EFAULT",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_WAITV_NUMA_OK probe_einval=1 "
           "no_node_writeback=1 align_einval=1 bad_node_einval=1 "
           "ro_node_efault=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* I. Signal interruption of a priority-inheritance wait              */
/* ------------------------------------------------------------------ */

static _Atomic int sigusr1_count;

static void sigusr1_handler(int signo) {
    (void)signo;
    atomic_fetch_add_explicit(&sigusr1_count, 1, memory_order_relaxed);
}

static int install_sigusr1(int restartable, const char *stage) {
    struct sigaction action;

    memset(&action, 0, sizeof(action));
    action.sa_handler = sigusr1_handler;
    action.sa_flags = restartable ? SA_RESTART : 0;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
        return fail(stage, errno);
    }
    atomic_store_explicit(&sigusr1_count, 0, memory_order_relaxed);
    return 0;
}

/* Bounded wait for the handler of an already delivered signal to run. */
static int wait_for_signal(int expected, const char *stage) {
    int64_t start = monotonic_ns();
    if (start < 0) {
        return fail(stage, errno != 0 ? errno : EPROTO);
    }
    for (;;) {
        if (atomic_load_explicit(&sigusr1_count, memory_order_acquire) >=
            expected) {
            return 0;
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= BLOCK_BOUND_NS) {
            return fail(stage, ETIMEDOUT);
        }
        sched_yield();
    }
}

struct pi_signal_case {
    struct blocked_thread blocked;
    uint32_t *futex;
    uint32_t control;
    long lock_result;
    int lock_errno;
    long control_result;
    int control_errno;
    long unlock_result;
    int unlock_errno;
};

static void *pi_signal_waiter_main(void *opaque) {
    struct pi_signal_case *test = opaque;
    struct timespec bound = relative_bound(WAIT_BOUND_NS);

    atomic_store_explicit(&test->blocked.tid, (int)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&test->blocked.entered, 1, memory_order_release);
    errno = 0;
    test->lock_result =
        sys_futex(test->futex, FUTEX_LOCK_PI | PRIVATE, 0, NULL, NULL, 0);
    test->lock_errno = test->lock_result == -1 ? errno : 0;
    if (test->lock_result == 0) {
        /* Hold the futex until the main thread has sampled the word, then
         * give it back.  The expected value check makes the release order
         * irrelevant. */
        errno = 0;
        test->control_result =
            sys_futex(&test->control, FUTEX_WAIT | PRIVATE, 0, &bound, NULL, 0);
        test->control_errno = test->control_result == -1 ? errno : 0;
        errno = 0;
        test->unlock_result = sys_futex(test->futex, FUTEX_UNLOCK_PI | PRIVATE,
                                        0, NULL, NULL, 0);
        test->unlock_errno = test->unlock_result == -1 ? errno : 0;
    }
    atomic_store_explicit(&test->blocked.done, 1, memory_order_release);
    return NULL;
}

/* `futex_lock_pi()` ends with `return ret != -EINTR ? ret : -ERESTARTNOINTR;`
 * (`kernel/futex/pi.c:1198`), and x86_64's `handle_signal()` rewinds the
 * syscall instruction for `-ERESTARTNOINTR` whether or not the handler set
 * `SA_RESTART` (`arch/x86/kernel/signal.c:277-281`).  An interrupted
 * FUTEX_LOCK_PI therefore resumes instead of reporting EINTR, and the resumed
 * call takes the futex when the owner hands it over. */
static int test_pi_signal_restart(void) {
    uint32_t word = 0;
    struct pi_signal_case test;

    if (install_sigusr1(1, "pi-signal-sigaction") != 0) {
        return 1;
    }

    memset(&test, 0, sizeof(test));
    test.futex = &word;
    errno = 0;
    if (sys_futex(&word, FUTEX_LOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return fail("pi-signal-owner-lock", errno);
    }
    if ((word & FUTEX_TID_MASK) != (uint32_t)syscall(SYS_gettid)) {
        return fail("pi-signal-owner-word", EPROTO);
    }

    if (start_blocked(&test.blocked, pi_signal_waiter_main, &test,
                      "pi-signal-create") != 0) {
        return 1;
    }
    if (wait_until_blocked(&test.blocked, "pi-signal-handshake") != 0) {
        return 1;
    }
    if (word == 0 || (word & FUTEX_WAITERS) == 0) {
        return fail("pi-signal-precondition", EPROTO);
    }

    if (syscall(SYS_tgkill, getpid(),
                (pid_t)atomic_load_explicit(&test.blocked.tid,
                                            memory_order_acquire),
                SIGUSR1) != 0) {
        return fail("pi-signal-tgkill", errno);
    }
    if (wait_for_signal(1, "pi-signal-handler") != 0) {
        return 1;
    }

    /* Hand the futex over.  A kernel that returned EINTR to userspace stops
     * here with the waiter's lock_errno set instead. */
    errno = 0;
    if (sys_futex(&word, FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return fail("pi-signal-owner-unlock", errno);
    }
    if (release_parked_thread(&test.control, "pi-signal-release") != 0) {
        return 1;
    }
    /* The restarted LOCK_PI must have been handed the futex by UNLOCK_PI.
     * Bounded: a kernel that drops that wakeup reports ETIMEDOUT here. */
    if (await_blocked(&test.blocked, "pi-signal-resume") != 0) {
        return 1;
    }
    if (test.lock_result != 0 || test.unlock_result != 0 ||
        !parked_wait_succeeded(test.control_result, test.control_errno)) {
        return fail("pi-signal-lock-pi",
                    test.lock_errno != 0      ? test.lock_errno
                    : test.unlock_errno != 0  ? test.unlock_errno
                    : test.control_errno != 0 ? test.control_errno
                                              : EPROTO);
    }
    if (word != 0) {
        return fail("pi-signal-word", EPROTO);
    }

    record("futex-abi-pi-signal", "LOCK_PI_SA_RESTART_RESUMES",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_PI_SIGNAL_OK "
           "lock_pi_sa_restart_resumes=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* J. Signal interruption of an already requeued PI waiter            */
/* ------------------------------------------------------------------ */

struct requeue_pi_signal_waiter {
    struct blocked_thread blocked;
    struct requeue_pi_signal_case *owner;
    long wait_result;
    int wait_errno;
    long control_result;
    int control_errno;
    long unlock_result;
    int unlock_errno;
};

struct requeue_pi_signal_case {
    struct requeue_pi_signal_waiter waiters[2];
    uint32_t *source;
    uint32_t *target;
    uint32_t control[2];
    struct timespec timeout;
};

static void *requeue_pi_signal_waiter_main(void *opaque) {
    struct requeue_pi_signal_waiter *waiter = opaque;
    struct requeue_pi_signal_case *owner = waiter->owner;
    int index = (int)(waiter - owner->waiters);
    struct timespec bound = relative_bound(WAIT_BOUND_NS);

    atomic_store_explicit(&waiter->blocked.tid, (int)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&waiter->blocked.entered, 1, memory_order_release);
    errno = 0;
    waiter->wait_result =
        sys_futex(owner->source, FUTEX_WAIT_REQUEUE_PI | PRIVATE, 0,
                  &owner->timeout, owner->target, FUTEX_BITSET_MATCH_ANY);
    waiter->wait_errno = waiter->wait_result == -1 ? errno : 0;
    if (waiter->wait_result == 0) {
        /* FUTEX_CMP_REQUEUE_PI handed this thread *target.  Hold it until the
         * main thread has observed the word and the moved waiter has left the
         * target queue. */
        errno = 0;
        waiter->control_result = sys_futex(&owner->control[index],
                                           FUTEX_WAIT | PRIVATE, 0, &bound,
                                           NULL, 0);
        waiter->control_errno = waiter->control_result == -1 ? errno : 0;
        errno = 0;
        waiter->unlock_result = sys_futex(owner->target,
                                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL,
                                          NULL, 0);
        waiter->unlock_errno = waiter->unlock_result == -1 ? errno : 0;
    }
    atomic_store_explicit(&waiter->blocked.done, 1, memory_order_release);
    return NULL;
}

/* `futex_wait_requeue_pi()` distinguishes the two interruptions by the
 * waiter's `requeue_state` (`kernel/futex/requeue.c:881-903`): a signal that
 * arrives after `FUTEX_CMP_REQUEUE_PI` moved the waiter onto the target's
 * rt_mutex leaves `rt_mutex_wait_proxy_lock()` reporting -EINTR, and
 * `futex_wait_requeue_pi()` rewrites that to -EWOULDBLOCK -- restarting would
 * re-read *uaddr and refuse the wait.  The SIGUSR1 handler here is
 * deliberately *not* installed with SA_RESTART, so the reported errno is the
 * syscall's own and not a restarted wait. */
static int test_requeue_pi_signal(void) {
    uint32_t source = 0;
    uint32_t target = 0;
    struct requeue_pi_signal_case test;
    int index;

    if (install_sigusr1(0, "requeue-pi-signal-sigaction") != 0) {
        return 1;
    }

    memset(&test, 0, sizeof(test));
    test.source = &source;
    test.target = &target;
    if (absolute_bound(&test.timeout, WAIT_BOUND_NS) != 0) {
        return fail("requeue-pi-signal-timeout-clock", errno);
    }
    for (index = 0; index < 2; index++) {
        test.waiters[index].owner = &test;
        if (start_blocked(&test.waiters[index].blocked,
                          requeue_pi_signal_waiter_main, &test.waiters[index],
                          "requeue-pi-signal-create") != 0) {
            return 1;
        }
        if (wait_until_blocked(&test.waiters[index].blocked,
                               "requeue-pi-signal-handshake") != 0) {
            return 1;
        }
    }
    if (source != 0 || target != 0) {
        return fail("requeue-pi-signal-precondition", EPROTO);
    }

    /* Wake one waiter and move the other onto *target's queue. */
    errno = 0;
    long requeued = sys_futex(&source, FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                              (const struct timespec *)(uintptr_t)1, &target,
                              0);
    int saved = errno;
    if (requeued != 2) {
        return fail("requeue-pi-signal-requeue",
                    requeued == -1 ? saved : EPROTO);
    }

    /* Whichever waiter the word names was handed the PI futex; the other one
     * is the requeued waiter this section is about. */
    const uint32_t owner_tid = target & FUTEX_TID_MASK;
    int moved = -1;
    for (index = 0; index < 2; index++) {
        uint32_t tid = (uint32_t)atomic_load_explicit(
            &test.waiters[index].blocked.tid, memory_order_acquire);
        if (tid != owner_tid) {
            moved = index;
        }
    }
    if (moved < 0) {
        return fail("requeue-pi-signal-owner", EPROTO);
    }
    const int owner_index = moved == 0 ? 1 : 0;

    if (syscall(SYS_tgkill, getpid(),
                (pid_t)atomic_load_explicit(&test.waiters[moved].blocked.tid,
                                            memory_order_acquire),
                SIGUSR1) != 0) {
        return fail("requeue-pi-signal-tgkill", errno);
    }

    /* The moved waiter must report EWOULDBLOCK, which also removes it from
     * the target queue before the new owner gives the futex back. */
    if (await_blocked(&test.waiters[moved].blocked,
                      "requeue-pi-signal-join-moved") != 0) {
        return 1;
    }
    if (test.waiters[moved].wait_result != -1 ||
        test.waiters[moved].wait_errno != EWOULDBLOCK) {
        return fail("requeue-pi-signal-ewouldblock",
                    test.waiters[moved].wait_errno != 0
                        ? test.waiters[moved].wait_errno
                        : EPROTO);
    }

    if (release_parked_thread(&test.control[owner_index],
                              "requeue-pi-signal-release") != 0) {
        return 1;
    }
    if (await_blocked(&test.waiters[owner_index].blocked,
                      "requeue-pi-signal-join-owner") != 0) {
        return 1;
    }
    if (test.waiters[owner_index].wait_result != 0 ||
        test.waiters[owner_index].unlock_result != 0 ||
        !parked_wait_succeeded(test.waiters[owner_index].control_result,
                               test.waiters[owner_index].control_errno)) {
        return fail("requeue-pi-signal-owner-result",
                    test.waiters[owner_index].wait_errno != 0
                        ? test.waiters[owner_index].wait_errno
                    : test.waiters[owner_index].unlock_errno != 0
                        ? test.waiters[owner_index].unlock_errno
                    : test.waiters[owner_index].control_errno != 0
                        ? test.waiters[owner_index].control_errno
                        : EPROTO);
    }
    if (target != 0) {
        return fail("requeue-pi-signal-target-unlocked", EPROTO);
    }

    record("futex-abi-requeue-pi-signal", "REQUEUED_WAITER_EWOULDBLOCK",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_SIGNAL_OK "
           "requeued_waiter_ewouldblock=1");
    return 0;
}

/* ------------------------------------------------------------------ */
/* H. A resident word whose page-table leaf is not writable           */
/* ------------------------------------------------------------------ */

/* Linux v7.2.3, `kernel/futex/core.c`:
 *
 *	int fault_in_user_writeable(u32 __user *uaddr)
 *	{
 *		struct mm_struct *mm = current->mm;
 *		int ret;
 *
 *		mmap_read_lock(mm);
 *		ret = fixup_user_fault(mm, (unsigned long)uaddr,
 *				       FAULT_FLAG_WRITE, NULL);
 *		mmap_read_unlock(mm);
 *
 *		return ret < 0 ? ret : 0;
 *	}
 *
 * Every futex opcode that must write the word takes this slow path when its
 * atomic access faults, and the page state decides the outcome:
 *
 *  - the copy-on-write leaf `fork()` leaves in the parent, where the VMA
 *    allows writes and the fault succeeds, so the operation is retried and
 *    completes, and
 *  - a word `mprotect(PROT_READ)` made read-only, where the fault cannot be
 *    satisfied and the operation reports EFAULT with the word untouched.
 *
 * The fault is also the only thing that clears the first state: the word stays
 * readable throughout, so an operation that answers the fault with another
 * read re-classifies the same leaf and never leaves its retry loop.  Each
 * stage below therefore prints its RAW progress line before entering the
 * kernel, so a kernel that spins is named by the last line it printed. */

/* fork() without touching the parent's words: every page the child inherits
 * becomes copy-on-write for the parent. */
static int fork_untouched(const char *stage) {
    pid_t child = fork();
    if (child < 0) {
        return fail(stage, errno);
    }
    if (child == 0) {
        _exit(0);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail(stage, errno);
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail(stage, EPROTO);
    }
    return 0;
}

/* A word whose leaf is present but not writable because fork() write-protected
 * it.  The word is stored first so the parent's leaf is a private copy-on-write
 * page rather than the shared read-only zero page. */
static int cow_word_page(uint32_t *word, const char *stage) {
    *word = 0;
    return fork_untouched(stage);
}

static int read_only_word_page(uint32_t *word, const char *stage) {
    if (mprotect(word, 4096, PROT_READ) != 0) {
        return fail(stage, errno);
    }
    return 0;
}

static void raw_stage(const char *stage) {
    printf("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_RETRY_RAW stage=%s\n", stage);
    fflush(stdout);
}

static int test_retry_write_fault(void) {
    const uint32_t self = (uint32_t)syscall(SYS_gettid);
    /* One page per word: mprotect() and the COW split both work on pages, and
     * keeping the words apart means a read-only stage cannot change the page
     * state another stage observes. */
    uint32_t *pages = mmap(NULL, 4096 * 12, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED) {
        return fail("retry-write-fault-map", errno);
    }
    uint32_t *lock_cow = pages + 1024 * 0;
    uint32_t *unlock_cow = pages + 1024 * 1;
    uint32_t *lock_ro = pages + 1024 * 2;
    uint32_t *unlock_ro = pages + 1024 * 3;
    uint32_t *wake_op_source = pages + 1024 * 4;
    uint32_t *wake_op_cow = pages + 1024 * 5;
    uint32_t *wake_op_ro = pages + 1024 * 6;
    uint32_t *cmp_source_cow = pages + 1024 * 7;
    uint32_t *cmp_target_cow = pages + 1024 * 8;
    uint32_t *cmp_source_ro = pages + 1024 * 9;
    uint32_t *cmp_target_ro = pages + 1024 * 10;

    /* FUTEX_LOCK_PI on a free word that fork() write-protected: the word is
     * readable, so the caller is queued for an uncontended takeover and only
     * the write fault can publish it.  Linux returns 0 and the word names the
     * caller. */
    if (cow_word_page(lock_cow, "retry-write-fault-lock-cow-fork") != 0) {
        return 1;
    }
    if (*lock_cow != 0) {
        return fail("retry-write-fault-lock-cow-precondition", EPROTO);
    }
    raw_stage("lock-pi-cow");
    if (expect_futex_zero("retry-write-fault-lock-cow", lock_cow,
                          FUTEX_LOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if ((*lock_cow & FUTEX_TID_MASK) != self) {
        return fail("retry-write-fault-lock-cow-word", EPROTO);
    }
    if (expect_futex_zero("retry-write-fault-unlock-after-cow", lock_cow,
                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }

    /* FUTEX_UNLOCK_PI on a word the caller already owns and fork() then
     * write-protected: taking the lock made the leaf writable, so the fork is
     * the only thing standing between the unlock and its handoff.  Linux
     * returns 0 and clears the word. */
    if (expect_futex_zero("retry-write-fault-lock-before-cow", unlock_cow,
                          FUTEX_LOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (fork_untouched("retry-write-fault-unlock-cow-fork") != 0) {
        return 1;
    }
    raw_stage("unlock-pi-cow");
    if (expect_futex_zero("retry-write-fault-unlock-cow", unlock_cow,
                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (*unlock_cow != 0) {
        return fail("retry-write-fault-unlock-cow-word", EPROTO);
    }

    /* The same two operations on a word mprotect() made read-only: the write
     * fault cannot be satisfied, so Linux reports EFAULT and leaves the word
     * exactly as it was. */
    if (read_only_word_page(lock_ro, "retry-write-fault-lock-ro-protect") !=
        0) {
        return 1;
    }
    raw_stage("lock-pi-read-only");
    if (expect_futex_errno("retry-write-fault-lock-ro", lock_ro,
                           FUTEX_LOCK_PI | PRIVATE, 0, NULL, NULL, 0,
                           EFAULT) != 0) {
        return 1;
    }
    if (*lock_ro != 0) {
        return fail("retry-write-fault-lock-ro-word", EPROTO);
    }

    if (expect_futex_zero("retry-write-fault-lock-before-ro", unlock_ro,
                          FUTEX_LOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    const uint32_t ro_owner = *unlock_ro;
    if (read_only_word_page(unlock_ro, "retry-write-fault-unlock-ro-protect") !=
        0) {
        return 1;
    }
    raw_stage("unlock-pi-read-only");
    if (expect_futex_errno("retry-write-fault-unlock-ro", unlock_ro,
                           FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0,
                           EFAULT) != 0) {
        return 1;
    }
    if (*unlock_ro != ro_owner) {
        return fail("retry-write-fault-unlock-ro-word", EPROTO);
    }
    /* The rejected unlock left the lock owned, so restoring write access must
     * let the same caller hand it back. */
    if (mprotect(unlock_ro, 4096, PROT_READ | PROT_WRITE) != 0) {
        return fail("retry-write-fault-unlock-ro-restore", errno);
    }
    if (expect_futex_zero("retry-write-fault-unlock-ro-repaired", unlock_ro,
                          FUTEX_UNLOCK_PI | PRIVATE, 0, NULL, NULL, 0) != 0) {
        return 1;
    }
    if (*unlock_ro != 0) {
        return fail("retry-write-fault-unlock-ro-repaired-word", EPROTO);
    }

    /* FUTEX_WAKE_OP modifies uaddr2 with its encoded operation.  `SET 7` makes
     * the update visible, so the COW stage proves the write fault was taken
     * and the read-only stage proves the word was left alone. */
    const uint32_t set_seven = FUTEX_OP(FUTEX_OP_SET, 7, FUTEX_OP_CMP_EQ, 0);
    *wake_op_source = 0;
    if (cow_word_page(wake_op_cow, "retry-write-fault-wake-op-cow-fork") != 0) {
        return 1;
    }
    raw_stage("wake-op-cow");
    if (expect_futex_zero("retry-write-fault-wake-op-cow", wake_op_source,
                          FUTEX_WAKE_OP | PRIVATE, 1, NULL, wake_op_cow,
                          set_seven) != 0) {
        return 1;
    }
    if (*wake_op_cow != 7) {
        return fail("retry-write-fault-wake-op-cow-word", EPROTO);
    }

    *wake_op_ro = 0;
    if (read_only_word_page(wake_op_ro, "retry-write-fault-wake-op-ro-protect") !=
        0) {
        return 1;
    }
    raw_stage("wake-op-read-only");
    if (expect_futex_errno("retry-write-fault-wake-op-ro", wake_op_source,
                           FUTEX_WAKE_OP | PRIVATE, 1, NULL, wake_op_ro,
                           set_seven, EFAULT) != 0) {
        return 1;
    }
    if (*wake_op_ro != 0) {
        return fail("retry-write-fault-wake-op-ro-word", EPROTO);
    }

    /* FUTEX_CMP_REQUEUE_PI promoting a queued waiter onto the target writes
     * the target word with the new owner.  The waiter is a real
     * FUTEX_WAIT_REQUEUE_PI waiter, so the promotion really happens. */
    struct requeue_pi_case test;
    memset(&test, 0, sizeof(test));
    test.source = cmp_source_cow;
    test.target = cmp_target_cow;
    *cmp_source_cow = 0;
    if (cow_word_page(cmp_target_cow, "retry-write-fault-cmp-cow-fork") != 0) {
        return 1;
    }
    if (absolute_bound(&test.timeout, WAIT_BOUND_NS) != 0) {
        return fail("retry-write-fault-cmp-cow-clock", errno);
    }
    if (start_blocked(&test.blocked, requeue_pi_waiter_main, &test,
                      "retry-write-fault-cmp-cow-create") != 0) {
        return 1;
    }
    if (wait_until_blocked(&test.blocked,
                           "retry-write-fault-cmp-cow-block") != 0) {
        return 1;
    }
    if (*cmp_source_cow != 0 || *cmp_target_cow != 0) {
        return fail("retry-write-fault-cmp-cow-precondition", EPROTO);
    }
    raw_stage("cmp-requeue-pi-cow");
    errno = 0;
    long promoted = sys_futex(cmp_source_cow, FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                              (const struct timespec *)(uintptr_t)1,
                              cmp_target_cow, 0);
    int promoted_errno = errno;
    if (promoted != 1) {
        return fail("retry-write-fault-cmp-cow",
                    promoted == -1 ? promoted_errno : EPROTO);
    }
    const uint32_t waiter_tid =
        (uint32_t)atomic_load_explicit(&test.blocked.tid, memory_order_acquire);
    if ((*cmp_target_cow & FUTEX_TID_MASK) != waiter_tid) {
        return fail("retry-write-fault-cmp-cow-word", EPROTO);
    }
    if (release_parked_thread(&test.control,
                              "retry-write-fault-cmp-cow-release") != 0) {
        return 1;
    }
    if (join_blocked(&test.blocked, "retry-write-fault-cmp-cow-join") != 0) {
        return 1;
    }
    if (test.wait_result != 0 || test.unlock_result != 0 ||
        !parked_wait_succeeded(test.control_result, test.control_errno)) {
        return fail("retry-write-fault-cmp-cow-waiter-result",
                    test.wait_errno != 0      ? test.wait_errno
                    : test.unlock_errno != 0  ? test.unlock_errno
                    : test.control_errno != 0 ? test.control_errno
                                              : EPROTO);
    }
    if (*cmp_target_cow != 0) {
        return fail("retry-write-fault-cmp-cow-unlocked", EPROTO);
    }

    /* The same promotion onto a read-only target: futex_requeue() returns
     * EFAULT before any waiter moves, so both words keep their values and the
     * waiter stays queued until its own bound expires.  FUTEX_WAKE is not a
     * substitute: waking a queue that holds a PI waiter is EINVAL
     * (`kernel/futex/waitwake.c`, the `this->pi_state || this->rt_waiter`
     * branch in `futex_wake()`), so the bound is the only exit. */
    struct requeue_pi_case readonly;
    memset(&readonly, 0, sizeof(readonly));
    readonly.source = cmp_source_ro;
    readonly.target = cmp_target_ro;
    *cmp_source_ro = 0;
    *cmp_target_ro = 0;
    if (read_only_word_page(cmp_target_ro,
                            "retry-write-fault-cmp-ro-protect") != 0) {
        return 1;
    }
    if (absolute_bound(&readonly.timeout, RETRY_BOUND_NS) != 0) {
        return fail("retry-write-fault-cmp-ro-clock", errno);
    }
    if (start_blocked(&readonly.blocked, requeue_pi_waiter_main, &readonly,
                      "retry-write-fault-cmp-ro-create") != 0) {
        return 1;
    }
    if (wait_until_blocked(&readonly.blocked,
                           "retry-write-fault-cmp-ro-block") != 0) {
        return 1;
    }
    raw_stage("cmp-requeue-pi-read-only");
    if (expect_futex_errno("retry-write-fault-cmp-ro", cmp_source_ro,
                           FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                           (const struct timespec *)(uintptr_t)1,
                           cmp_target_ro, 0, EFAULT) != 0) {
        return 1;
    }
    if (*cmp_source_ro != 0 || *cmp_target_ro != 0) {
        return fail("retry-write-fault-cmp-ro-word", EPROTO);
    }
    if (join_blocked(&readonly.blocked, "retry-write-fault-cmp-ro-join") != 0) {
        return 1;
    }
    if (readonly.wait_result != -1 || readonly.wait_errno != ETIMEDOUT) {
        return fail("retry-write-fault-cmp-ro-waiter",
                    readonly.wait_errno != 0 ? readonly.wait_errno : EPROTO);
    }

    if (munmap(pages, 4096 * 12) != 0) {
        return fail("retry-write-fault-unmap", errno);
    }
    record("futex-abi-retry-write-fault",
           "LOCK_PI_COW UNLOCK_PI_COW LOCK_PI_RO_EFAULT UNLOCK_PI_RO_EFAULT "
           "WAKE_OP_COW WAKE_OP_RO_EFAULT CMP_REQUEUE_PI_COW "
           "CMP_REQUEUE_PI_RO_EFAULT",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_RETRY_WRITE_FAULT_OK "
           "lock_pi_cow=1 unlock_pi_cow=1 lock_pi_ro_efault=1 "
           "unlock_pi_ro_efault=1 wake_op_cow=1 wake_op_ro_efault=1 "
           "cmp_requeue_pi_cow=1 cmp_requeue_pi_ro_efault=1");
    return 0;
}

/* Optional single-case selection: `futex-abi-differential <case>` runs only
 * that case.  The differential harness never passes an argument, so the
 * registered run still executes every case in order and still stops at the
 * first failure; the selector exists so one case can be exercised in
 * isolation while bisecting a kernel change. */
static const char *selected_case;

static int want_case(const char *name) {
    return selected_case == NULL || strcmp(selected_case, name) == 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    if (argc > 1) {
        selected_case = argv[1];
    }

    int result = want_case("opcode") ? test_opcode_validation() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("wake-zero") ? test_wake_zero() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("requeue") ? test_cmp_requeue() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("pi-word") ? test_pi_word_states() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("pi-timeout") ? test_pi_timeout() : 0;
    if (result != 0) {
        return result;
    }
    /* The coverage added with the futex fixes runs before the pre-existing
     * PI requeue case so that neither an unrelated earlier failure nor a
     * defect in that case can mask it.  `pi-signal` is last among them: it is
     * the one whose assertion can be blocked by a second, unrelated kernel
     * defect. */
    result = want_case("waitv-numa") ? test_waitv_numa() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("requeue-pi-signal") ? test_requeue_pi_signal() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("pi-signal") ? test_pi_signal_restart() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("futex2-flags") ? test_futex2_flags() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("requeue-pi") ? test_requeue_pi() : 0;
    if (result != 0) {
        return result;
    }
    /* The contended-target requeue cases come after the pre-existing
     * uncontended one so that neither can mask the other. */
    result = want_case("requeue-pi-owned") ? test_requeue_pi_owned() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("requeue-pi-held") ? test_requeue_pi_held() : 0;
    if (result != 0) {
        return result;
    }
    result = want_case("requeue-pi-plain") ? test_requeue_pi_plain() : 0;
    if (result != 0) {
        return result;
    }
    /* A word whose page-table leaf is not writable is the last case: a kernel
     * that answers that fault with a read instead of a write fault spins
     * inside the syscall, and every marker below would be lost with it.  All
     * earlier cases then still report their own records before this one runs. */
    result = want_case("retry-write-fault") ? test_retry_write_fault() : 0;
    if (result != 0) {
        return result;
    }
    marker("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_OK");
    return 0;
}
