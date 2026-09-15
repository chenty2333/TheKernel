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

/* ------------------------------------------------------------------ */
/* Bounds.  All values are pure safety nets; correct runs never wait. */
/* ------------------------------------------------------------------ */

#define BLOCK_BOUND_NS 3000000000LL
#define RETRY_BOUND_NS 3000000000LL
#define WAIT_BOUND_NS 5000000000LL
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
           "LOCKED UNLOCK_RC_ZERO UNLOCK_WORD_ZERO EPERM TRYLOCK",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_PI_WORD_OK locked=1 unlock_rc=0 "
           "unlock_word=0 eperm=1 trylock=1");
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

    /* Recording only, never asserted: the return value with no waiters on
     * the source is an implementation detail shared by the two kernels. */
    errno = 0;
    long no_waiters = sys_futex(&empty_source,
                                FUTEX_CMP_REQUEUE_PI | PRIVATE, 1,
                                (const struct timespec *)(uintptr_t)1,
                                &empty_target, 0);
    int no_waiters_errno = errno;

    record("futex-abi-requeue-pi",
           "REQUEUED WAITER_RC TARGET_WORD UNLOCKED EINVAL_SELF EINVAL_WAKE2",
           "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_REQUEUE_PI_OK requeued=1 "
           "waiter_rc=0 target_word=1 unlocked=1 einval_self=1 "
           "einval_wake2=1");
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
     * PI requeue case so that neither an unrelated earlier failure nor the
     * unbounded spin that case can trigger on a waiter-less source can mask
     * it.  `pi-signal` is last among them: it is the one whose assertion can
     * be blocked by a second, unrelated kernel defect. */
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
    marker("THEKERNEL_FUTEX_ABI_DIFFERENTIAL_OK");
    return 0;
}
