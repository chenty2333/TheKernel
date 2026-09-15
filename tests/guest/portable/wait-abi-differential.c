/*
 * wait4(2)/waitid(2) argument contract plus the scheduler and mempolicy
 * argument contracts that accompany them.
 *
 * Every assertion here is a value both guests must agree on. Cases whose
 * answer depends on host topology (which NUMA nodes exist, how many CPUs) are
 * deliberately excluded, and the pointer-probing cases rely on Linux's
 * documented "argument validation precedes the copy" ordering rather than on
 * any particular success value.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef P_PIDFD
#define P_PIDFD 3
#endif
#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif
#ifndef PTRACE_DETACH
#define PTRACE_DETACH 17
#endif
#ifndef SYS_ptrace
#define SYS_ptrace 101
#endif
#ifndef SCHED_DEADLINE
#define SCHED_DEADLINE 6
#endif
#ifndef SCHED_FLAG_RECLAIM
#define SCHED_FLAG_RECLAIM 0x02
#endif
#ifndef SCHED_FLAG_DL_OVERRUN
#define SCHED_FLAG_DL_OVERRUN 0x04
#endif
#ifndef SCHED_FLAG_RESET_ON_FORK
#define SCHED_FLAG_RESET_ON_FORK 0x01
#endif
#ifndef MPOL_DEFAULT
#define MPOL_DEFAULT 0
#define MPOL_PREFERRED 1
#define MPOL_BIND 2
#define MPOL_INTERLEAVE 3
/* include/uapi/linux/mempolicy.h: MPOL_LOCAL is 4 and
 * MPOL_WEIGHTED_INTERLEAVE is 6; MPOL_PREFERRED_MANY is the 5 that sits
 * between them. */
#define MPOL_LOCAL 4
#define MPOL_WEIGHTED_INTERLEAVE 6
#define MPOL_PREFERRED_MANY 5
#define MPOL_F_NUMA_BALANCING (1 << 13)
#define MPOL_F_RELATIVE_NODES (1 << 14)
#define MPOL_F_STATIC_NODES (1 << 15)
#define MPOL_F_NODE (1 << 0)
#define MPOL_F_ADDR (1 << 1)
#define MPOL_F_MEMS_ALLOWED (1 << 2)
#define MPOL_MF_STRICT (1 << 0)
#define MPOL_MF_MOVE (1 << 1)
#define MPOL_MF_MOVE_ALL (1 << 2)
#endif
/* The page size every guest in this suite is built for. */
#ifndef PAGE
#define PAGE 4096UL
#endif
#ifndef __WNOTHREAD
#define __WNOTHREAD 0x20000000
#endif
#ifndef __WALL
#define __WALL 0x40000000
#endif
#ifndef __WCLONE
#define __WCLONE 0x80000000
#endif
/* linux/ioprio.h */
#ifndef IOPRIO_CLASS_SHIFT
#define IOPRIO_CLASS_SHIFT 13
#endif
#ifndef IOPRIO_WHO_PROCESS
#define IOPRIO_WHO_PROCESS 1
#define IOPRIO_WHO_PGRP 2
#define IOPRIO_WHO_USER 3
#endif
#ifndef IOPRIO_CLASS_NONE
#define IOPRIO_CLASS_NONE 0
#define IOPRIO_CLASS_RT 1
#define IOPRIO_CLASS_BE 2
#define IOPRIO_CLASS_IDLE 3
#endif
#define IOPRIO_VALUE(class, level) (((class) << IOPRIO_CLASS_SHIFT) | (level))

struct sched_attr_local {
    uint32_t size;
    uint32_t sched_policy;
    uint64_t sched_flags;
    int32_t sched_nice;
    uint32_t sched_priority;
    uint64_t sched_runtime;
    uint64_t sched_deadline;
    uint64_t sched_period;
    uint32_t sched_util_min;
    uint32_t sched_util_max;
};

static int failed;
static int case_failed;
static const char *active;

static void begin(const char *name)
{
    active = name;
    case_failed = 0;
    printf("THEKERNEL_ABI_CASE %s\n", active);
}

static void done(void)
{
    if (!case_failed) printf("THEKERNEL_ABI_RESULT %s pass\n", active);
}

static void check(const char *name, int good)
{
    if (!good) {
        fprintf(stderr, "THEKERNEL_WAIT_ABI_FAIL %s errno=%d\n", name, errno);
        failed = case_failed = 1;
    } else {
        printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
    }
}

#define EXPECT_ERR(name, call, error) do { \
    errno = 0; \
    long rc_ = (long)(call); \
    check(name, rc_ == -1 && errno == (error)); \
} while (0)

#define EXPECT_OK(name, call) do { \
    errno = 0; \
    long rc_ = (long)(call); \
    check(name, rc_ != -1); \
} while (0)

static pid_t fork_child(void)
{
    pid_t pid = fork();
    if (pid == 0) {
        /* Hold still until the parent is done probing argument validation. */
        for (;;) pause();
    }
    if (pid < 0) {
        fprintf(stderr, "THEKERNEL_WAIT_ABI_FAIL fork errno=%d\n", errno);
        exit(1);
    }
    return pid;
}

static void collect(pid_t pid)
{
    kill(pid, SIGKILL);
    while (waitpid(pid, NULL, 0) < 0 && errno == EINTR) { }
}

/* ------------------------------------------------------------------ *
 * wait4(2) __WNOTHREAD: only this exact task's own relation counts.
 *
 * __do_wait() walks `current->children` and `current->ptraced`, and breaks
 * out of the thread-group loop as soon as the flag is set
 * (kernel/exit.c:1725-1735); do_wait_pid() admits a named target only through
 * is_effectively_child(), which requires `current == target->real_parent` for
 * the thread-group arm and `current == target->parent` for the ptrace arm
 * (kernel/exit.c:1657-1664, :1672-1697).  A child forked by a sibling thread
 * is therefore invisible to this thread, and stays invisible after it has
 * exited, because release_task() has not run yet.
 * ------------------------------------------------------------------ */
static atomic_int nothread_stage;
static pid_t nothread_live_child;
static pid_t nothread_zombie_child;
static long nothread_own_rc;
static int nothread_own_errno;
static pid_t nothread_zombie_reaper;
static int nothread_zombie_status;

static void nothread_wait_for(int value)
{
    while (atomic_load_explicit(&nothread_stage, memory_order_acquire) != value) {
        sched_yield();
    }
}

static void *nothread_forker(void *unused)
{
    (void)unused;
    pid_t child = fork();
    if (child == 0) {
        for (;;) pause();
    }
    if (child < 0) _exit(9);
    nothread_live_child = child;
    atomic_store_explicit(&nothread_stage, 1, memory_order_release);

    nothread_wait_for(2);
    /* The thread that forked it is its `real_parent`, so its own
     * __WNOTHREAD wait still finds the live child. */
    errno = 0;
    nothread_own_rc = syscall(SYS_wait4, child, NULL, WNOHANG | __WNOTHREAD, NULL);
    nothread_own_errno = errno;
    kill(child, SIGKILL);
    while (waitpid(child, NULL, 0) < 0 && errno == EINTR) { }

    child = fork();
    if (child == 0) _exit(7);
    if (child < 0) _exit(9);
    {
        siginfo_t info;
        memset(&info, 0, sizeof(info));
        /* WNOWAIT observes the exit without reaping, so the sibling probe
         * below runs against a real zombie. */
        if (syscall(SYS_waitid, P_PID, child, &info, WEXITED | WNOWAIT, NULL) != 0) {
            _exit(9);
        }
        if (info.si_pid != child) _exit(9);
    }
    nothread_zombie_child = child;
    atomic_store_explicit(&nothread_stage, 4, memory_order_release);
    nothread_wait_for(5);
    return NULL;
}

int main(void)
{
    /* ---------------- wait4(2) argument contract ---------------- */
    pid_t child = fork_child();

    begin("wait4.raw-differential");
    EXPECT_ERR("wait4-int-min-esrch", syscall(SYS_wait4, INT32_MIN, NULL, 0, NULL), ESRCH);
    /* Unknown option bits are rejected before the child list is even walked,
     * so this is EINVAL and not ECHILD even though a matching child exists. */
    EXPECT_ERR("wait4-unknown-option", syscall(SYS_wait4, child, NULL, 0x10, NULL), EINVAL);
    EXPECT_ERR("wait4-wnowait-unknown", syscall(SYS_wait4, child, NULL, 0x01000000, NULL), EINVAL);
    /* WNOHANG with a live child and no pending event is 0, including when the
     * thread-scoping and clone-type bits are combined with it. */
    {
        errno = 0;
        long rc = syscall(SYS_wait4, child, NULL, WNOHANG | __WNOTHREAD | __WALL, NULL);
        check("wait4-wnohang-flags-zero", rc == 0 && errno == 0);
    }
    /* __WCLONE asks for children whose exit signal is not SIGCHLD; this child
     * reports SIGCHLD, so the only matching child is filtered out and
     * do_wait() reports that no eliglble child exists. */
    EXPECT_ERR("wait4-wclone-excludes-sigchld",
               syscall(SYS_wait4, child, NULL, (int)(__WCLONE | WNOHANG), NULL), ECHILD);
    /* A stopped child is only reported with WUNTRACED. */
    kill(child, SIGSTOP);
    {
        int status = -1;
        pid_t got;
        do {
            got = waitpid(child, &status, WUNTRACED);
        } while (got < 0 && errno == EINTR);
        check("wait4-wuntraced-reports-stop",
              got == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
        /* The stop report was consumed by the call above. */
        errno = 0;
        got = waitpid(child, &status, WUNTRACED | WNOHANG);
        check("wait4-stop-consumed-once", got == 0 && errno == 0);
    }
    kill(child, SIGCONT);
    {
        int status = -1;
        pid_t got;
        do {
            got = waitpid(child, &status, WCONTINUED);
        } while (got < 0 && errno == EINTR);
        check("wait4-wcontinued-reports-continue",
              got == child && WIFCONTINUED(status));
        errno = 0;
        got = waitpid(child, &status, WCONTINUED | WNOHANG);
        check("wait4-continue-consumed-once", got == 0 && errno == 0);
    }
    /* WNOHANG with no matching event, while the child still exists. */
    {
        int status = 0;
        errno = 0;
        pid_t got = waitpid(child, &status, WNOHANG);
        check("wait4-wnohang-zero", got == 0 && errno == 0);
    }
    /* Reaping consumes the child: a second wait on the same PID is ECHILD.
     * `collect()` sends SIGKILL and reaps without inspecting the status. */
    collect(child);
    EXPECT_ERR("wait4-reaped-child-echild", syscall(SYS_wait4, child, NULL, 0, NULL), ECHILD);

    /* ---------------- wait4(2) __WNOTHREAD ---------------- */
    pthread_t forker;
    atomic_store(&nothread_stage, 0);
    {
        int created = pthread_create(&forker, NULL, nothread_forker, NULL);
        check("wait4-wnothread-thread-create", created == 0);
        nothread_wait_for(1);
        pid_t sibling_child = nothread_live_child;
        /* A sibling thread's live child is not this thread's child, so the
         * walk finds no candidate at all: ECHILD, not 0. */
        EXPECT_ERR("wait4-wnothread-sibling-echild",
                   syscall(SYS_wait4, sibling_child, NULL, WNOHANG | __WNOTHREAD, NULL),
                   ECHILD);
        /* Without the bit it is still this thread group's child, so the same
         * live child with no pending event reports success with zero. */
        errno = 0;
        {
            long rc = syscall(SYS_wait4, -1, NULL, WNOHANG, NULL);
            check("wait4-wnothread-any-live-zero", rc == 0 && errno == 0);
        }
        atomic_store_explicit(&nothread_stage, 2, memory_order_release);
        nothread_wait_for(4);
        check("wait4-wnothread-own-live-zero",
              nothread_own_rc == 0 && nothread_own_errno == 0);
        /* An exited-but-unreaped child of a sibling thread is just as
         * invisible under the flag: `p->real_parent` is still that thread. */
        EXPECT_ERR("wait4-wnothread-zombie-sibling-echild",
                   syscall(SYS_wait4, nothread_zombie_child, NULL,
                           WNOHANG | __WNOTHREAD, NULL),
                   ECHILD);
        /* The group as a whole still owns it, and reaps it as a normal
         * SIGCHLD child. */
        {
            int status = 0;
            pid_t got = waitpid(nothread_zombie_child, &status, 0);
            nothread_zombie_reaper = got;
            nothread_zombie_status = status;
        }
        check("wait4-wnothread-zombie-group-reap",
              nothread_zombie_reaper == nothread_zombie_child &&
              WIFEXITED(nothread_zombie_status) &&
              WEXITSTATUS(nothread_zombie_status) == 7);
        atomic_store_explicit(&nothread_stage, 5, memory_order_release);
        check("wait4-wnothread-thread-join", pthread_join(forker, NULL) == 0);
    }
    done();

    /* ---------------- waitid(2) argument contract ---------------- */
    child = fork_child();

    begin("waitid.raw-differential");
    /* kernel_waitid_prepare(): option mask first, then "at least one event
     * bit", then the which/upid pair. */
    EXPECT_ERR("waitid-unknown-option", syscall(SYS_waitid, P_PID, child, NULL, 0x2000, NULL), EINVAL);
    EXPECT_ERR("waitid-no-event-bit", syscall(SYS_waitid, P_PID, child, NULL, WNOHANG, NULL), EINVAL);
    EXPECT_ERR("waitid-nowait-alone", syscall(SYS_waitid, P_PID, child, NULL, WNOWAIT, NULL), EINVAL);
    EXPECT_ERR("waitid-p-pid-zero", syscall(SYS_waitid, P_PID, 0, NULL, WEXITED, NULL), EINVAL);
    EXPECT_ERR("waitid-p-pid-negative", syscall(SYS_waitid, P_PID, -1, NULL, WEXITED, NULL), EINVAL);
    EXPECT_ERR("waitid-p-pgid-negative", syscall(SYS_waitid, P_PGID, -1, NULL, WEXITED, NULL), EINVAL);
    EXPECT_ERR("waitid-p-pidfd-negative", syscall(SYS_waitid, P_PIDFD, -1, NULL, WEXITED, NULL), EINVAL);
    EXPECT_ERR("waitid-bad-idtype", syscall(SYS_waitid, 9, child, NULL, WEXITED, NULL), EINVAL);
    /* P_ALL ignores upid entirely, including a negative one. */
    {
        siginfo_t info;
        memset(&info, 0, sizeof(info));
        errno = 0;
        long rc = syscall(SYS_waitid, P_ALL, -1, &info, WEXITED | WNOHANG | WNOWAIT, NULL);
        check("waitid-p-all-ignores-upid", rc == 0);
    }
    /* A failing wait still stores the six siginfo fields: `do_wait()` fills a
     * `struct waitid_info` that `{.status = 0}` initializes and
     * SYSCALL_DEFINE5(waitid) stores it before returning the error, so a
     * caller's stale buffer cannot survive. Exactly those six fields are
     * stored -- the ABI padding after `si_code` and the rest of the union stay
     * the caller's bytes, which is what the tail probe below proves. */
    {
        siginfo_t info;
        unsigned char *raw = (unsigned char *)&info;
        int untouched = 1;
        size_t i;
        memset(&info, 0xaa, sizeof(info));
        errno = 0;
        long rc = syscall(SYS_waitid, P_PID, INT32_MAX, &info, WEXITED | WNOHANG, NULL);
        for (i = 12; i < 16; i++) untouched = untouched && raw[i] == 0xaa;
        for (i = 28; i < sizeof(info); i++) untouched = untouched && raw[i] == 0xaa;
        check("waitid-echild-zeroes-info",
              rc == -1 && errno == ECHILD && info.si_signo == 0 && info.si_errno == 0
                  && info.si_code == 0 && info.si_pid == 0 && info.si_uid == 0
                  && info.si_status == 0 && untouched);
    }
    /* The resource usage is copied *before* the siginfo_t, and only for a
     * reported event, so a faulting `rusage` returns EFAULT with the caller's
     * siginfo_t untouched rather than half-updated. WNOWAIT keeps the stop
     * pending for the checks below. */
    kill(child, SIGSTOP);
    {
        siginfo_t info;
        unsigned char *raw = (unsigned char *)&info;
        int intact = 1;
        size_t i;
        memset(&info, 0xaa, sizeof(info));
        errno = 0;
        long rc = syscall(SYS_waitid, P_PID, child, &info, WSTOPPED | WNOWAIT, (void *)1);
        for (i = 0; i < sizeof(info); i++) intact = intact && raw[i] == 0xaa;
        check("waitid-rusage-fault-before-info", rc == -1 && errno == EFAULT && intact);
    }
    /* WNOHANG with a live child and no pending event reports success with a
     * zeroed siginfo rather than an error. */
    {
        siginfo_t info;
        memset(&info, 0xaa, sizeof(info));
        errno = 0;
        long rc = syscall(SYS_waitid, P_PID, child, &info, WEXITED | WNOHANG, NULL);
        check("waitid-wnohang-zeroes-info", rc == 0 && info.si_pid == 0);
    }
    /* waitid with WNOWAIT on a stopped child observes the stop without
     * consuming it, so a second WNOWAIT call reports the same stop again. */
    {
        siginfo_t first, second;
        memset(&first, 0, sizeof(first));
        memset(&second, 0, sizeof(second));
        long a, b;
        do {
            a = syscall(SYS_waitid, P_PID, child, &first, WSTOPPED | WNOWAIT, NULL);
        } while (a < 0 && errno == EINTR);
        do {
            b = syscall(SYS_waitid, P_PID, child, &second, WSTOPPED | WNOWAIT, NULL);
        } while (b < 0 && errno == EINTR);
        check("waitid-nowait-preserves-stop",
              a == 0 && b == 0 && first.si_pid == child && second.si_pid == child
                  && first.si_code == CLD_STOPPED && second.si_code == CLD_STOPPED
                  && first.si_status == SIGSTOP && second.si_status == SIGSTOP);
        /* A consuming call then reports it exactly once. */
        memset(&first, 0, sizeof(first));
        do {
            a = syscall(SYS_waitid, P_PID, child, &first, WSTOPPED, NULL);
        } while (a < 0 && errno == EINTR);
        check("waitid-consume-after-nowait",
              a == 0 && first.si_pid == child && first.si_code == CLD_STOPPED);
        memset(&second, 0, sizeof(second));
        errno = 0;
        b = syscall(SYS_waitid, P_PID, child, &second, WSTOPPED | WNOHANG, NULL);
        check("waitid-consumed-once", b == 0 && second.si_pid == 0);
    }
    kill(child, SIGCONT);
    /* An exited child is reported with CLD_EXITED and its real status. */
    kill(child, SIGKILL);
    {
        siginfo_t info;
        memset(&info, 0, sizeof(info));
        long rc;
        do {
            rc = syscall(SYS_waitid, P_PID, child, &info, WEXITED, NULL);
        } while (rc < 0 && errno == EINTR);
        check("waitid-exited-status",
              rc == 0 && info.si_pid == child
                  && (info.si_code == CLD_KILLED || info.si_code == CLD_DUMPED)
                  && info.si_status == SIGKILL);
    }
    EXPECT_ERR("waitid-reaped-child-echild",
               syscall(SYS_waitid, P_PID, child, NULL, WEXITED | WNOHANG, NULL), ECHILD);
    done();

    /* ---------------- sched_rr_get_interval(2) ---------------- */
    begin("sched_rr_get_interval.raw-differential");
    EXPECT_ERR("rr-negative-pid", syscall(SYS_sched_rr_get_interval, -1, (void *)1), EINVAL);
    {
        struct timespec ts;
        memset(&ts, 0xaa, sizeof(ts));
        errno = 0;
        long rc = syscall(SYS_sched_rr_get_interval, 0, &ts);
        /* SCHED_OTHER runs on the fair class, whose interval is "infinity" when
         * the runqueue is otherwise idle, so only the call's success and the
         * non-negative value are portable. */
        check("rr-self-succeeds", rc == 0 && ts.tv_sec >= 0 && ts.tv_nsec >= 0
                                      && ts.tv_nsec < 1000000000L);
    }
    {
        /* SCHED_FIFO's slice is zero ("infinity") on every configuration. */
        struct sched_param param;
        struct timespec ts;
        memset(&param, 0, sizeof(param));
        param.sched_priority = 1;
        /* `get_rr_interval_rt()` returns 0 for SCHED_FIFO because its slice is
         * "infinity", so this is the same answer under every configuration.
         * Without CAP_SYS_NICE the policy change fails and the case is skipped
         * rather than failing, since privilege is not what is under test. */
        if (syscall(SYS_sched_setscheduler, 0, SCHED_FIFO, &param) == 0) {
            memset(&ts, 0xaa, sizeof(ts));
            errno = 0;
            long rc = syscall(SYS_sched_rr_get_interval, 0, &ts);
            check("rr-fifo-is-infinity", rc == 0 && ts.tv_sec == 0 && ts.tv_nsec == 0);
            param.sched_priority = 0;
            syscall(SYS_sched_setscheduler, 0, SCHED_OTHER, &param);
        }
        /* A NULL interval pointer is EFAULT: the task resolves and the copyout
         * fails, so this needs no privilege either way. */
        EXPECT_ERR("rr-null-interval", syscall(SYS_sched_rr_get_interval, 0, NULL), EFAULT);
    }
    done();

    /* ---------------- sched_setattr/getattr(2) ---------------- */
    begin("sched_attr.raw-differential");
    {
        struct sched_attr_local attr;
        memset(&attr, 0, sizeof(attr));
        attr.size = sizeof(attr);
        attr.sched_policy = SCHED_OTHER;
        /* SYSCALL_DEFINE3(sched_setattr): a size below SCHED_ATTR_SIZE_VER0 is
         * E2BIG, a zero size means "VER0", and a size above a page is EINVAL. */
        attr.size = 0;
        EXPECT_OK("setattr-size-zero-means-ver0", syscall(SYS_sched_setattr, 0, &attr, 0));
        attr.size = 47;
        EXPECT_ERR("setattr-size-47-e2big", syscall(SYS_sched_setattr, 0, &attr, 0), E2BIG);
        /* Above a page is E2BIG too: sched_copy_attr() funnels both the
         * below-VER0 and the copy_struct_from_user() overflow case into
         * err_size, which also writes the kernel's size back to the caller. */
        attr.size = 4097;
        EXPECT_ERR("setattr-size-4097-e2big", syscall(SYS_sched_setattr, 0, &attr, 0), E2BIG);
        /* err_size writes the kernel's own sizeof(struct sched_attr) back, so
         * the caller can retry with the right length. Both guests are x86_64
         * and both track the current Linux layout. */
        check("setattr-err-size-writes-back", attr.size == 56);
        attr.size = sizeof(attr);
        EXPECT_ERR("setattr-negative-pid", syscall(SYS_sched_setattr, -1, &attr, 0), EINVAL);
        EXPECT_ERR("setattr-nonzero-flags", syscall(SYS_sched_setattr, 0, &attr, 1), EINVAL);
        EXPECT_ERR("setattr-null-attr", syscall(SYS_sched_setattr, 0, NULL, 0), EINVAL);
        /* `SCHED_FLAG_RECLAIM` is inside SCHED_FLAG_ALL, so what admits or
         * rejects it is the deadline implementation, not the flag table. On a
         * SCHED_OTHER target the flag is simply stored by the fair class. */
        attr.sched_flags = SCHED_FLAG_RECLAIM;
        EXPECT_OK("setattr-reclaim-on-other", syscall(SYS_sched_setattr, 0, &attr, 0));
        attr.sched_flags = SCHED_FLAG_DL_OVERRUN;
        EXPECT_OK("setattr-overrun-on-other", syscall(SYS_sched_setattr, 0, &attr, 0));
        attr.sched_flags = 0;
        EXPECT_OK("setattr-restore", syscall(SYS_sched_setattr, 0, &attr, 0));
        /* A bit outside SCHED_FLAG_ALL | SCHED_FLAG_SUGOV is EINVAL. */
        attr.sched_flags = 0x80;
        EXPECT_ERR("setattr-unknown-flag", syscall(SYS_sched_setattr, 0, &attr, 0), EINVAL);
    }
    {
        struct sched_attr_local attr;
        memset(&attr, 0, sizeof(attr));
        /* SYSCALL_DEFINE4(sched_getattr): below VER0 or *above* a page is
         * EINVAL, there is no "zero means VER0" spelling, and a page exactly is
         * still accepted (the check is `usize > PAGE_SIZE`). */
        EXPECT_ERR("getattr-size-47-einval", syscall(SYS_sched_getattr, 0, &attr, 47, 0), EINVAL);
        EXPECT_ERR("getattr-size-0-einval", syscall(SYS_sched_getattr, 0, &attr, 0, 0), EINVAL);
        EXPECT_ERR("getattr-size-4097-einval", syscall(SYS_sched_getattr, 0, &attr, 4097, 0), EINVAL);
        {
            /* A page-sized request is accepted, and the reply states the
             * kernel's own size rather than the requested one. The buffer must
             * therefore be a whole page: the kernel fills `min(usize, 56)`
             * bytes and the first field is the reported size. */
            static unsigned char page[4096];
            uint32_t reported;
            int tail_cleared = 1;
            size_t i;
            memset(page, 0, sizeof(page));
            errno = 0;
            long rc = syscall(SYS_sched_getattr, 0, page, sizeof(page), 0);
            memcpy(&reported, page, sizeof(reported));
            check("getattr-size-page-accepted", rc == 0 && reported == 56);
            /* copy_struct_to_user() clears the unknown part of a larger
             * userspace structure, so every byte of the requested extent is
             * written and none of the caller's old contents survive. */
            memset(page, 0xaa, sizeof(page));
            errno = 0;
            rc = syscall(SYS_sched_getattr, 0, page, sizeof(page), 0);
            memcpy(&reported, page, sizeof(reported));
            for (i = 56; i < sizeof(page); i++) tail_cleared = tail_cleared && page[i] == 0;
            check("getattr-page-clears-tail", rc == 0 && reported == 56 && tail_cleared);
        }
        EXPECT_ERR("getattr-size-4097-einval", syscall(SYS_sched_getattr, 0, &attr, 4097, 0), EINVAL);
        EXPECT_ERR("getattr-null-attr", syscall(SYS_sched_getattr, 0, NULL, 48, 0), EINVAL);
        EXPECT_ERR("getattr-negative-pid", syscall(SYS_sched_getattr, -1, &attr, 48, 0), EINVAL);
        /* The flags argument is EINVAL unless the target is a deadline task and
         * the value is exactly SCHED_GETATTR_FLAG_DL_DYNAMIC. */
        EXPECT_ERR("getattr-unknown-flags", syscall(SYS_sched_getattr, 0, &attr, 48, 2), EINVAL);
        EXPECT_ERR("getattr-dl-dynamic-on-other",
                   syscall(SYS_sched_getattr, 0, &attr, 48, 1), EINVAL);
        /* The flag check needs the resolved target, so it runs *after* the
         * lookup: an unknown flag with a pid that names nothing is ESRCH, not
         * EINVAL. */
        EXPECT_ERR("getattr-unknown-flag-bad-pid-esrch",
                   syscall(SYS_sched_getattr, INT32_MAX, &attr, 48, 2), ESRCH);
        /* A plain request round-trips the caller's own non-deadline identity. */
        memset(&attr, 0xaa, sizeof(attr));
        errno = 0;
        long rc = syscall(SYS_sched_getattr, 0, &attr, 48, 0);
        check("getattr-ver0-fields",
              rc == 0 && attr.size == 48 && attr.sched_policy == SCHED_OTHER
                  && (attr.sched_flags & SCHED_FLAG_RESET_ON_FORK) == 0);
    }

    done();

    /* ---------------- mbind(2)/set_mempolicy(2)/get_mempolicy(2) ---------- */
    begin("mempolicy.raw-differential");
    /* An unaligned start is EINVAL, not rounded down, and is checked after the
     * bind-flag mask but before anything is faulted in. */
    EXPECT_ERR("mbind-unaligned-start",
               syscall(SYS_mbind, 0x1001, 0x1000, MPOL_DEFAULT, NULL, 0, 0), EINVAL);
    /* An unknown bind flag is EINVAL even for a zero-length range, because
     * the flag mask is checked before the early "nothing to do" return. */
    EXPECT_ERR("mbind-unknown-flag",
               syscall(SYS_mbind, 0, 0, MPOL_DEFAULT, NULL, 0, 0x8), EINVAL);
    /* sanitize_mpol_flags() runs before get_nodes(), so an out-of-range mode
     * is EINVAL even with a garbage mask pointer. */
    EXPECT_ERR("mbind-bad-mode-before-mask",
               syscall(SYS_mbind, 0, 0, 0xffff, (void *)1, 64, 0), EINVAL);
    EXPECT_ERR("set_mempolicy-bad-mode",
               syscall(SYS_set_mempolicy, 0xffff, NULL, 0), EINVAL);
    /* sanitize_mpol_flags() splits MPOL_MODE_FLAGS out of the mode argument.
     * MPOL_F_NUMA_BALANCING is only legal for MPOL_BIND and
     * MPOL_PREFERRED_MANY; STATIC|RELATIVE together is never legal. Both are
     * rejected before the (unreadable) mask is touched. */
    EXPECT_ERR("set_mempolicy-balancing-default",
               syscall(SYS_set_mempolicy, MPOL_DEFAULT | MPOL_F_NUMA_BALANCING, (void *)1, 64),
               EINVAL);
    EXPECT_ERR("set_mempolicy-balancing-preferred",
               syscall(SYS_set_mempolicy, MPOL_PREFERRED | MPOL_F_NUMA_BALANCING, (void *)1, 64),
               EINVAL);
    EXPECT_ERR("set_mempolicy-balancing-interleave",
               syscall(SYS_set_mempolicy, MPOL_INTERLEAVE | MPOL_F_NUMA_BALANCING, (void *)1, 64),
               EINVAL);
    EXPECT_ERR("set_mempolicy-static-and-relative",
               syscall(SYS_set_mempolicy, MPOL_BIND | MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES,
                       (void *)1, 64),
               EINVAL);
    /* The same bits are legal shaping for MPOL_BIND, so the mask is read. */
    EXPECT_ERR("set_mempolicy-balancing-bind-reads-mask",
               syscall(SYS_set_mempolicy, MPOL_BIND | MPOL_F_NUMA_BALANCING, (void *)1, 64),
               EFAULT);
    EXPECT_ERR("set_mempolicy-balancing-preferred-many-reads-mask",
               syscall(SYS_set_mempolicy, MPOL_PREFERRED_MANY | MPOL_F_NUMA_BALANCING, (void *)1, 64),
               EFAULT);
    /* MPOL_F_STATIC_NODES/RELATIVE_NODES are *not* mode bits: putting them in
     * an mbind mode is accepted, and they only affect mask interpretation. */
    EXPECT_ERR("set_mempolicy-static-bind-reads-mask",
               syscall(SYS_set_mempolicy, MPOL_BIND | MPOL_F_STATIC_NODES, (void *)1, 64), EFAULT);
    EXPECT_ERR("set_mempolicy-relative-bind-reads-mask",
               syscall(SYS_set_mempolicy, MPOL_BIND | MPOL_F_RELATIVE_NODES, (void *)1, 64), EFAULT);
    /* `maxnode` is a bit count compared against PAGE_SIZE * BITS_PER_BYTE
     * *after* the decrement, so 32769 is the largest admitted window. 32770 is
     * EINVAL before the mask is read at all; 32769 is admitted and then faults
     * on the mask, which proves the bound is on `maxnode - 1`. */
    EXPECT_ERR("set_mempolicy-maxnode-too-long",
               syscall(SYS_set_mempolicy, MPOL_DEFAULT, (void *)1, 32770), EINVAL);
    EXPECT_ERR("set_mempolicy-maxnode-at-bound-faults",
               syscall(SYS_set_mempolicy, MPOL_DEFAULT, (void *)1, 32769), EFAULT);
    /* Above MAX_NUMNODES the mask is checked one word at a time *from the end*,
     * and the word holding the top of the window is read whole: `get_nodes()`
     * calls `get_bitmap(&t, &nmask[(maxnode - 1) / BITS_PER_LONG], bits)` and
     * the `t &= ~((1UL << (MAX_NUMNODES % BITS_PER_LONG)) - 1)` clamp in that
     * arm is a no-op whenever MAX_NUMNODES is a multiple of BITS_PER_LONG
     * (`mm/mempolicy.c:1668-1692`).  `maxnode` counts bits and 102 leaves 101,
     * so word 1 is read whole and bit 104 is above MAX_NUMNODES
     * (CONFIG_NODES_SHIFT=6 -> 64) even though it is also above the caller's
     * own window.  Node 0 alone is a legal MPOL_BIND mask, so the EINVAL here
     * can only come from the bit the caller did not count. */
    {
        unsigned long high_bit_outside_window[2] = {1, 1UL << 40};
        EXPECT_ERR("set_mempolicy-mask-bit-above-max-numnodes",
                   syscall(SYS_set_mempolicy, MPOL_BIND, high_bit_outside_window, 102),
                   EINVAL);
    }
    /* "From the end" is observable, not just a description: the in-loop
     * `if (t) return -EINVAL;` runs before the next lower word is read
     * (`mm/mempolicy.c:1673-1688`), so a set bit above MAX_NUMNODES in the
     * high word outranks an unreadable low word -- the low word is never
     * touched.  `maxnode == 66` leaves 65 bits, so with MAX_NUMNODES == 64
     * exactly two words are in the window, and this window straddles the
     * boundary: word 1 is the mapped page's first word and names node 64, word
     * 0 is the last word of the unmapped page before it.  Reading word 0 first
     * would be EFAULT; the answer is EINVAL. */
    {
        char *pair = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (pair != MAP_FAILED) {
            if (munmap(pair, PAGE) == 0) {
                unsigned long *window = (unsigned long *)(pair + PAGE - sizeof(unsigned long));
                *(unsigned long *)(pair + PAGE) = 1;
                EXPECT_ERR("set_mempolicy-checks-high-word-first",
                           syscall(SYS_set_mempolicy, MPOL_BIND, window, 66), EINVAL);
            }
            munmap(pair + PAGE, PAGE);
        }
    }
    /* An unreadable mask is EFAULT for every mode that reads it -- get_nodes()
     * cannot know the mode and runs first. */
    EXPECT_ERR("set_mempolicy-mask-ptr-fault",
               syscall(SYS_set_mempolicy, MPOL_DEFAULT, (void *)1, 64), EFAULT);
    EXPECT_ERR("set_mempolicy-bind-mask-ptr-fault",
               syscall(SYS_set_mempolicy, MPOL_BIND, (void *)1, 64), EFAULT);
    EXPECT_ERR("set_mempolicy-prefered-mask-ptr-fault",
               syscall(SYS_set_mempolicy, MPOL_PREFERRED, (void *)1, 64), EFAULT);
    /* An all-zero mask is the *empty* nodemask, whether it was supplied or
     * not, so MPOL_DEFAULT accepts it; a non-empty one is EINVAL. Modes other
     * than MPOL_DEFAULT require a non-empty mask. */
    {
        unsigned long empty_mask = 0;
        unsigned long one_node = 1;
        EXPECT_OK("set_mempolicy-default-accepts-empty-mask",
                  syscall(SYS_set_mempolicy, MPOL_DEFAULT, &empty_mask, 64));
        EXPECT_OK("set_mempolicy-default-accepts-absent-mask",
                  syscall(SYS_set_mempolicy, MPOL_DEFAULT, NULL, 64));
        EXPECT_ERR("set_mempolicy-default-rejects-nonempty-mask",
                   syscall(SYS_set_mempolicy, MPOL_DEFAULT, &one_node, 64), EINVAL);
        EXPECT_ERR("set_mempolicy-bind-rejects-empty-mask",
                   syscall(SYS_set_mempolicy, MPOL_BIND, &empty_mask, 64), EINVAL);
        EXPECT_ERR("set_mempolicy-bind-rejects-absent-mask",
                   syscall(SYS_set_mempolicy, MPOL_BIND, NULL, 64), EINVAL);
        /* MPOL_PREFERRED with an empty mask falls back to MPOL_LOCAL. */
        EXPECT_OK("set_mempolicy-preferred-empty-is-local",
                  syscall(SYS_set_mempolicy, MPOL_PREFERRED, &empty_mask, 64));
        /* Restore the default so the remaining probes start from a clean state. */
        EXPECT_OK("set_mempolicy-restore-default",
                  syscall(SYS_set_mempolicy, MPOL_DEFAULT, NULL, 0));
    }
    /* MPOL_PREFERRED with a non-empty mask that names no *allowed* node is
     * EINVAL, not the MPOL_LOCAL rewrite: `mpol_new()` only rewrites an empty
     * user mask, and the surviving preferred policy is then built by
     * `mpol_new_preferred()` from the mask intersected with the allowed set.
     * Node 1 is disallowed on a one-node configuration, so this mask has a
     * non-empty user value and an empty intersection. */
    {
        unsigned long disallowed = 2;
        EXPECT_ERR("set_mempolicy-preferred-disallowed-mask",
                   syscall(SYS_set_mempolicy, MPOL_PREFERRED, &disallowed, 64), EINVAL);
    }
    /* `do_mbind()` returns for an empty range before `mpol_new()` looks at the
     * mask's contents, so a zero-length bind with an empty-intersection mask
     * succeeds on the flag mask and range alone. */
    {
        unsigned long disallowed = 2;
        EXPECT_OK("mbind-zero-length-skips-mask-validation",
                  syscall(SYS_mbind, 0, 0, MPOL_BIND, &disallowed, 64, 0));
    }
    /* A range that spans a hole is not an error by itself.  `do_mbind()` sets
     * `MPOL_MF_DISCONTIG_OK` whenever `mpol_new()` returned NULL, which is
     * MPOL_DEFAULT alone (`mm/mempolicy.c:1519-1528`, `mm/mempolicy.c:446-450`),
     * and `queue_pages_test_walk()` then skips both of its hole reports -- the
     * head hole (`qp->start < vma->vm_start`) and the middle/tail one
     * (`vma->vm_end < qp->end && (!next || vma->vm_end < next->vm_start)`) --
     * at `mm/mempolicy.c:920-932`.  The one report the bit does not suppress is
     * `queue_pages_range()`'s "the walk entered no VMA at all"
     * (`mm/mempolicy.c:998-1000`), so a wholly unmapped range stays EFAULT for
     * every mode.  MPOL_MF_STRICT is cleared for MPOL_DEFAULT
     * (`mm/mempolicy.c:1508-1509`), so it changes none of this. */
    {
        char *triple = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (triple != MAP_FAILED) {
            unsigned long one_node = 1;
            if (munmap(triple + PAGE, PAGE) == 0) {
                /* [p, p+P) and [p+2P, p+3P) are mapped; [p+P, p+2P) is the hole. */
                EXPECT_OK("mbind-default-spans-middle-hole",
                          syscall(SYS_mbind, (unsigned long)triple, 3 * PAGE, MPOL_DEFAULT,
                                  NULL, 0, 0));
                EXPECT_OK("mbind-default-strict-spans-middle-hole",
                          syscall(SYS_mbind, (unsigned long)triple, 3 * PAGE, MPOL_DEFAULT,
                                  NULL, 0, MPOL_MF_STRICT));
                EXPECT_ERR("mbind-local-rejects-middle-hole",
                           syscall(SYS_mbind, (unsigned long)triple, 3 * PAGE, MPOL_LOCAL,
                                   NULL, 0, 0),
                           EFAULT);
                EXPECT_ERR("mbind-bind-rejects-middle-hole",
                           syscall(SYS_mbind, (unsigned long)triple, 3 * PAGE, MPOL_BIND,
                                   &one_node, 64, 0),
                           EFAULT);
                /* The hole on its own is a range no VMA covers. */
                EXPECT_ERR("mbind-default-whole-range-hole",
                           syscall(SYS_mbind, (unsigned long)triple + PAGE, PAGE, MPOL_DEFAULT,
                                   NULL, 0, 0),
                           EFAULT);
            }
            munmap(triple, 3 * PAGE);
        }
    }
    /* The same rule at the head of the range: the first page of this pair is
     * unmapped, so `qp->start < vma->vm_start` on the first VMA the walk
     * enters. */
    {
        char *pair = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (pair != MAP_FAILED) {
            if (munmap(pair, PAGE) == 0) {
                EXPECT_OK("mbind-default-spans-head-hole",
                          syscall(SYS_mbind, (unsigned long)pair, 2 * PAGE, MPOL_DEFAULT,
                                  NULL, 0, 0));
            }
            munmap(pair + PAGE, PAGE);
        }
    }
    /* The MPOL_MF_MOVE_ALL capability check is likewise inside `do_mbind()` and
     * precedes `mpol_new()`, so an unprivileged caller gets EPERM for the same
     * arguments that only the mask check would reject. */
    {
        pid_t probe = fork();
        if (probe == 0) {
            unsigned long disallowed = 2;
            if (setresuid(1000, 1000, 1000) != 0) _exit(3);
            errno = 0;
            _exit(syscall(SYS_mbind, 0, 0x1000, MPOL_BIND, &disallowed, 64,
                          MPOL_MF_MOVE_ALL) == -1 && errno == EPERM ? 0 : 1);
        }
        int status = -1;
        int reaped;
        do {
            reaped = waitpid(probe, &status, 0);
        } while (reaped < 0 && errno == EINTR);
        check("mbind-move-all-unprivileged-eperm",
              reaped == probe && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    }
    /* MPOL_F_MEMS_ALLOWED short-circuits before the address check, so a
     * non-zero addr with that flag is not EINVAL. */
    {
        unsigned long mask[2] = {0, 0};
        errno = 0;
        long rc = syscall(SYS_get_mempolicy, NULL, mask, 64, 0x1000, MPOL_F_MEMS_ALLOWED);
        check("get_mempolicy-mems-allowed-skips-addr", rc == 0 && (mask[0] & 1UL) != 0);
    }
    /* Without MPOL_F_ADDR, a non-zero addr is EINVAL. */
    EXPECT_ERR("get_mempolicy-addr-without-flag",
               syscall(SYS_get_mempolicy, NULL, NULL, 0, 0x1000, 0), EINVAL);
    /* Unknown get_mempolicy flags are rejected first. */
    EXPECT_ERR("get_mempolicy-unknown-flags",
               syscall(SYS_get_mempolicy, NULL, NULL, 0, 0, (void *)0x100), EINVAL);
    /* MPOL_F_NODE without MPOL_F_ADDR is only valid for the task's own
     * interleave policy, and default policy is not an interleave policy. */
    EXPECT_ERR("get_mempolicy-f-node-without-addr",
               syscall(SYS_get_mempolicy, NULL, NULL, 0, 0, MPOL_F_NODE), EINVAL);
    /* A supplied nodemask with maxnode below nr_node_ids is EINVAL before
     * do_get_mempolicy() runs. `nr_node_ids` is at least one on every
     * configuration, so `maxnode == 0` is always below it. */
    {
        unsigned long mask = 0;
        EXPECT_ERR("get_mempolicy-tiny-maxnode",
                   syscall(SYS_get_mempolicy, NULL, &mask, 0, 0, 0), EINVAL);
    }
    /* That wrapper check is *outside* do_get_mempolicy(), so it also applies to
     * `MPOL_F_MEMS_ALLOWED`, which only skips the address check. */
    {
        unsigned long mask[2] = {0, 0};
        int policy = -1;
        EXPECT_ERR("get_mempolicy-mems-allowed-tiny-maxnode",
                   syscall(SYS_get_mempolicy, &policy, mask, 0, 0, MPOL_F_MEMS_ALLOWED),
                   EINVAL);
    }
    /* `MPOL_F_MEMS_ALLOWED` reports the allowed set and stores 0 -- not the
     * mode -- in *policy, and an unreadable *policy is still EFAULT. */
    {
        unsigned long mask[2] = {0, 0};
        int policy = 0x7eadbeef;
        errno = 0;
        long rc = syscall(SYS_get_mempolicy, &policy, mask, 64, 0, MPOL_F_MEMS_ALLOWED);
        check("get_mempolicy-mems-allowed-zero-policy",
              rc == 0 && policy == 0 && (mask[0] & 1UL) != 0);
        EXPECT_ERR("get_mempolicy-mems-allowed-policy-fault",
                   syscall(SYS_get_mempolicy, (void *)1, mask, 64, 0, MPOL_F_MEMS_ALLOWED),
                   EFAULT);
    }
    /* `MPOL_F_NODE` replaces *policy with a node id but still stores the
     * policy's nodemask; the default policy has an empty one, and only the
     * `nr_node_ids` bits of the caller's buffer are written. */
    {
        static unsigned long probe_word;
        unsigned long mask[2] = {~0UL, ~0UL};
        int policy = -1;
        errno = 0;
        long rc = syscall(SYS_get_mempolicy, &policy, mask, 64,
                          (unsigned long)&probe_word, MPOL_F_NODE | MPOL_F_ADDR);
        check("get_mempolicy-f-node-addr-writes-mask",
              rc == 0 && policy == 0 && mask[0] == 0);
    }
    /* `lookup_node()` resolves the address with
     * `get_user_pages_fast(addr & PAGE_MASK, 1, 0, &p)` followed by
     * `page_to_nid()` (`mm/mempolicy.c:1133-1144`), and GUP *faults the page in*
     * on a read fault rather than only looking it up: a mapped page that was
     * never touched reports its node, it is not EFAULT.  A page the caller may
     * not read fails in GUP itself, so PROT_NONE is EFAULT. */
    {
        void *fresh = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        void *guarded = mmap(NULL, PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (fresh != MAP_FAILED && guarded != MAP_FAILED) {
            int policy = -1;
            errno = 0;
            long rc = syscall(SYS_get_mempolicy, &policy, NULL, 0, (unsigned long)fresh,
                              MPOL_F_NODE | MPOL_F_ADDR);
            check("get_mempolicy-f-node-addr-unpopulated", rc == 0 && policy == 0);
            EXPECT_ERR("get_mempolicy-f-node-addr-prot-none",
                       syscall(SYS_get_mempolicy, &policy, NULL, 0, (unsigned long)guarded,
                               MPOL_F_NODE | MPOL_F_ADDR),
                       EFAULT);
            /* The fault-in belongs to the MPOL_F_NODE arm alone: with
             * MPOL_F_ADDR and no MPOL_F_NODE the policy comes from the VMA and
             * `lookup_node()` is never called (`mm/mempolicy.c:1182`,
             * `:1189-1203`), so the same PROT_NONE page still reports the
             * policy its VMA carries. */
            {
                unsigned long one_node = 1;
                int vma_policy = -1;
                (void)syscall(SYS_mbind, (unsigned long)guarded, PAGE, MPOL_BIND, &one_node,
                              64, 0);
                errno = 0;
                rc = syscall(SYS_get_mempolicy, &vma_policy, NULL, 0, (unsigned long)guarded,
                             MPOL_F_ADDR);
                check("get_mempolicy-f-addr-prot-none-reports-vma-policy",
                      rc == 0 && vma_policy == MPOL_BIND);
            }
        }
        if (fresh != MAP_FAILED) munmap(fresh, PAGE);
        if (guarded != MAP_FAILED) munmap(guarded, PAGE);
    }
    /* A hole fails before `lookup_node()`: `do_get_mempolicy()` resolves the
     * address with `vma_lookup()` and returns EFAULT when no VMA covers it
     * (`mm/mempolicy.c:1176-1181`). */
    {
        char *pair = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (pair != MAP_FAILED) {
            int policy = -1;
            if (munmap(pair, PAGE) == 0) {
                EXPECT_ERR("get_mempolicy-f-node-addr-hole",
                           syscall(SYS_get_mempolicy, &policy, NULL, 0, (unsigned long)pair,
                                   MPOL_F_NODE | MPOL_F_ADDR),
                           EFAULT);
            }
            munmap(pair + PAGE, PAGE);
        }
    }
    /* Without MPOL_F_ADDR, MPOL_F_NODE asks for the task's own next interleave
     * node -- `next_node_in(current->il_prev, pol->nodes)` for MPOL_INTERLEAVE
     * and `current->il_prev` for a weighted policy that still holds weight --
     * and every other mode is EINVAL (`mm/mempolicy.c:1204-1217`).  One node is
     * the only answer this configuration can give.  Each check reads the mode
     * back too, so a setup that did not take effect fails its record instead of
     * silently re-testing the policy before it. */
    {
        unsigned long one_node = 1;
        int mode = -1;
        int node = -1;
        long mode_rc;
        long node_rc;
        (void)syscall(SYS_set_mempolicy, MPOL_INTERLEAVE, &one_node, 64);
        mode_rc = syscall(SYS_get_mempolicy, &mode, NULL, 0, 0, 0);
        errno = 0;
        node_rc = syscall(SYS_get_mempolicy, &node, NULL, 0, 0, MPOL_F_NODE);
        check("get_mempolicy-f-node-interleave-task-policy",
              mode_rc == 0 && mode == MPOL_INTERLEAVE && node_rc == 0 && node == 0);
        mode = -1;
        node = -1;
        (void)syscall(SYS_set_mempolicy, MPOL_WEIGHTED_INTERLEAVE, &one_node, 64);
        mode_rc = syscall(SYS_get_mempolicy, &mode, NULL, 0, 0, 0);
        errno = 0;
        node_rc = syscall(SYS_get_mempolicy, &node, NULL, 0, 0, MPOL_F_NODE);
        check("get_mempolicy-f-node-weighted-interleave-task-policy",
              mode_rc == 0 && mode == MPOL_WEIGHTED_INTERLEAVE && node_rc == 0 && node == 0);
        mode = -1;
        (void)syscall(SYS_set_mempolicy, MPOL_PREFERRED_MANY, &one_node, 64);
        mode_rc = syscall(SYS_get_mempolicy, &mode, NULL, 0, 0, 0);
        errno = 0;
        node_rc = syscall(SYS_get_mempolicy, &node, NULL, 0, 0, MPOL_F_NODE);
        check("get_mempolicy-f-node-preferred-many-einval",
              mode_rc == 0 && mode == MPOL_PREFERRED_MANY && node_rc == -1 && errno == EINVAL);
        (void)syscall(SYS_set_mempolicy, MPOL_DEFAULT, NULL, 0);
    }
    /* `copy_nodes_to_user()` writes `ALIGN(maxnode-1, 64)/8` bytes -- the whole
     * window the caller named, not just the node bits -- so the words past
     * `nr_node_ids` are cleared, and a window wider than a page is EINVAL
     * (`mm/mempolicy.c:1694-1711`) with `*policy` already stored, because
     * `kernel_get_mempolicy()` writes the policy before the mask
     * (`mm/mempolicy.c:1228-1240`).  The policy these two read is set up by the
     * call below rather than by an assertion of its own; a failed setup shows
     * up as a failed check here. */
    {
        unsigned long one_node = 1;
        unsigned long window[3] = {~0UL, ~0UL, ~0UL};
        int policy = -1;
        (void)syscall(SYS_set_mempolicy, MPOL_BIND, &one_node, 64);
        errno = 0;
        long rc = syscall(SYS_get_mempolicy, &policy, window, 128, 0, 0);
        check("get_mempolicy-clears-window-past-node-mask",
              rc == 0 && policy == MPOL_BIND && window[0] == 1 && window[1] == 0
                  && window[2] == ~0UL);
        policy = -1;
        errno = 0;
        rc = syscall(SYS_get_mempolicy, &policy, window, 32770, 0, 0);
        check("get_mempolicy-window-too-long-keeps-policy",
              rc == -1 && errno == EINVAL && policy == MPOL_BIND);
    }
    /* The shaping flags stored with the policy come back in *policy, and
     * `mpol_store_user_nodemask()` makes the query report the caller's own mask
     * rather than the intersection: nodes 0 and 1 were requested, only node 0
     * is allowed, and the reported mode keeps MPOL_F_STATIC_NODES. */
    {
        unsigned long user_mask = 3;
        unsigned long reported[2] = {0, 0};
        int policy = -1;
        EXPECT_OK("set_mempolicy-static-bind-keeps-user-mask",
                  syscall(SYS_set_mempolicy, MPOL_BIND | MPOL_F_STATIC_NODES, &user_mask, 64));
        errno = 0;
        long rc = syscall(SYS_get_mempolicy, &policy, reported, 64, 0, 0);
        check("get_mempolicy-reports-mode-and-user-mask",
              rc == 0 && policy == (MPOL_BIND | MPOL_F_STATIC_NODES)
                  && reported[0] == user_mask);
        EXPECT_OK("set_mempolicy-restore-default-after-static",
                  syscall(SYS_set_mempolicy, MPOL_DEFAULT, NULL, 0));
    }
    /* `mpol_new()` returns NULL for MPOL_DEFAULT rather than a policy with
     * default fields (`mm/mempolicy.c:446-450`), so `do_set_mempolicy()` stores
     * no policy at all and `do_get_mempolicy()` reports `&default_policy`, whose
     * flags are zero (`mm/mempolicy.c:1186-1187`, `mm/mempolicy.c:1219-1225`).
     * The shaping flags are therefore dropped: *policy comes back as plain
     * MPOL_DEFAULT, not MPOL_DEFAULT | MPOL_F_STATIC_NODES. */
    {
        int policy = -1;
        EXPECT_OK("set_mempolicy-default-drops-mode-flags",
                  syscall(SYS_set_mempolicy, MPOL_DEFAULT | MPOL_F_STATIC_NODES, NULL, 0));
        errno = 0;
        long rc = syscall(SYS_get_mempolicy, &policy, NULL, 0, 0, 0);
        check("get_mempolicy-default-keeps-no-mode-flags",
              rc == 0 && policy == MPOL_DEFAULT);
    }
    done();

    /* ---------------- sched_getscheduler(2)/sched_getparam(2) ------------- */
    begin("sched_query.raw-differential");
    {
        struct sched_param param;
        /* Both wrappers reject their argument shape before the lookup: a
         * negative pid is EINVAL, and so is a null `param`, while a resolvable
         * pid that names no process is ESRCH. */
        EXPECT_ERR("sched-getscheduler-negative-pid",
                   syscall(SYS_sched_getscheduler, -1), EINVAL);
        EXPECT_ERR("sched-getscheduler-unknown-pid",
                   syscall(SYS_sched_getscheduler, INT32_MAX), ESRCH);
        EXPECT_ERR("sched-getparam-negative-pid",
                   syscall(SYS_sched_getparam, -1, &param), EINVAL);
        EXPECT_ERR("sched-getparam-null-param",
                   syscall(SYS_sched_getparam, 0, NULL), EINVAL);
        EXPECT_ERR("sched-getparam-unknown-pid",
                   syscall(SYS_sched_getparam, INT32_MAX, &param), ESRCH);
        errno = 0;
        long rc = syscall(SYS_sched_getscheduler, 0);
        check("sched-getscheduler-self", rc == SCHED_OTHER);
        memset(&param, 0xaa, sizeof(param));
        errno = 0;
        rc = syscall(SYS_sched_getparam, 0, &param);
        check("sched-getparam-self", rc == 0 && param.sched_priority == 0);
    }
    /* A pid that resolves to an unreaped zombie is still
     * `find_process_by_pid()`-reachable and answers from the scheduler state it
     * last held, which `release_task()` is what finally removes. */
    {
        int ready[2];
        if (pipe(ready) != 0) {
            check("sched-query-pipe", 0);
        } else {
            pid_t zombie = fork();
            if (zombie == 0) {
                struct sched_param param = {.sched_priority = 0};
                char byte = 'B';
                close(ready[0]);
                if (syscall(SYS_sched_setscheduler, 0, SCHED_BATCH, &param) != 0) _exit(1);
                if (write(ready[1], &byte, 1) != 1) _exit(1);
                _exit(0);
            }
            close(ready[1]);
            if (zombie < 0) {
                close(ready[0]);
                check("sched-query-fork", 0);
            } else {
                char byte = 0;
                siginfo_t info;
                struct sched_param param;
                long waited, policy, rc;
                ssize_t ack = read(ready[0], &byte, 1);
                close(ready[0]);
                check("sched-query-child-batch", ack == 1 && byte == 'B');
                memset(&info, 0, sizeof(info));
                errno = 0;
                waited = syscall(SYS_waitid, P_PID, zombie, &info, WEXITED | WNOWAIT, NULL);
                check("sched-query-zombie-waitid", waited == 0 && info.si_pid == zombie);
                errno = 0;
                policy = syscall(SYS_sched_getscheduler, zombie);
                check("sched-getscheduler-zombie-keeps-policy", policy == SCHED_BATCH);
                memset(&param, 0xaa, sizeof(param));
                errno = 0;
                rc = syscall(SYS_sched_getparam, zombie, &param);
                check("sched-getparam-zombie-reports-zero",
                      rc == 0 && param.sched_priority == 0);
                collect(zombie);
                EXPECT_ERR("sched-getscheduler-after-reap",
                           syscall(SYS_sched_getscheduler, zombie), ESRCH);
                EXPECT_ERR("sched-getparam-after-reap",
                           syscall(SYS_sched_getparam, zombie, &param), ESRCH);
            }
        }
    }
    /* ---------------- exiting but unreaped task policy ------------------- *
     * `sched_getscheduler(2)` reads `p->policy` and `p->sched_reset_on_fork`
     * straight out of the still-hashed `task_struct`: `find_process_by_pid()`
     * finds the task and the two fields are copied back
     * (`kernel/sched/syscalls.c:995-1015`), and only `release_task()`
     * unhashes it at reap time.  A child that has started exiting therefore
     * keeps answering with the policy it last installed.
     *
     * `waitid(WEXITED|WNOWAIT)` anchors the window: it returns as soon as the
     * exit is published and deliberately leaves the child unreaped, so every
     * sample below is taken while the child is a zombie whose task can still be
     * addressed.  The sampler then polls in a tight loop, which keeps the weak
     * task-table entry upgraded across the interval in which the scheduler
     * entity is already gone but the task object is not. */
    {
        int ready[2];
        if (pipe(ready) != 0) {
            check("sched-getscheduler-exiting-keeps-policy", 0);
        } else {
            pid_t target = fork();
            if (target == 0) {
                struct sched_param param = {.sched_priority = 0};
                char byte = 'R';
                close(ready[0]);
                if (syscall(SYS_sched_setscheduler, 0, SCHED_BATCH | SCHED_RESET_ON_FORK,
                            &param) != 0)
                    _exit(1);
                if (write(ready[1], &byte, 1) != 1) _exit(1);
                _exit(0);
            }
            close(ready[1]);
            if (target < 0) {
                close(ready[0]);
                check("sched-getscheduler-exiting-keeps-policy", 0);
            } else {
                char byte = 0;
                siginfo_t info;
                struct timespec start, now;
                long samples = 0, refused = 0, wrong = 0, first_errno = 0;
                int exited = 0;
                int ack = (int)read(ready[0], &byte, 1);
                close(ready[0]);
                memset(&info, 0, sizeof(info));
                errno = 0;
                exited = syscall(SYS_waitid, P_PID, target, &info, WEXITED | WNOWAIT, NULL) == 0
                             && info.si_pid == target;
                clock_gettime(CLOCK_MONOTONIC, &start);
                for (;;) {
                    long rc;
                    errno = 0;
                    rc = syscall(SYS_sched_getscheduler, target);
                    samples++;
                    if (rc == -1) {
                        if (!refused) first_errno = errno;
                        refused++;
                    } else if (rc != (SCHED_BATCH | SCHED_RESET_ON_FORK)) {
                        wrong++;
                    }
                    clock_gettime(CLOCK_MONOTONIC, &now);
                    if ((now.tv_sec - start.tv_sec) * 1000000000L
                            + (now.tv_nsec - start.tv_nsec) > 60000000L)
                        break;
                }
                printf("THEKERNEL_SCHED_PROBE exiting_policy ack=%d exited=%d samples=%ld"
                       " refused=%ld first_errno=%ld wrong=%ld\n",
                       ack, exited, samples, refused, first_errno, wrong);
                check("sched-getscheduler-exiting-keeps-policy",
                      ack == 1 && exited == 1 && samples > 0 && refused == 0 && wrong == 0);
                collect(target);
            }
        }
    }
    done();

    /* ---------------- setpriority(2) argument contract ---------------- */
    begin("setpriority.raw-differential");
    /* `set_one_prio()` authorizes a nicer value with `can_nice(p, niceval)`,
     * which reads `task_rlimit(p, RLIMIT_NICE)` from the *target*. A caller
     * holding a generous limit of its own still sees EACCES for a target that
     * lowered its soft limit, and CAP_SYS_NICE overrides the limit. Both
     * children below must therefore be scheduled by a process that is not the
     * one being limited. */
    {
        int ready[2];
        if (pipe(ready) != 0) {
            check("setpriority-pipe", 0);
        } else {
            pid_t limited = fork();
            if (limited == 0) {
                struct rlimit none = {0, 0};
                char byte = 'R';
                close(ready[0]);
                if (prlimit(0, RLIMIT_NICE, &none, NULL) != 0) _exit(1);
                if (setresuid(1000, 1000, 1000) != 0) _exit(1);
                if (write(ready[1], &byte, 1) != 1) _exit(1);
                for (;;) pause();
            }
            close(ready[1]);
            if (limited < 0) {
                close(ready[0]);
                check("setpriority-fork", 0);
            } else {
                char byte = 0;
                ssize_t got = read(ready[0], &byte, 1);
                close(ready[0]);
                check("setpriority-limited-child-ready", got == 1 && byte == 'R');
                pid_t caller = fork();
                if (caller == 0) {
                    /* Raised while still privileged, so the limit is legal and
                     * survives the uid change below. */
                    struct rlimit generous = {40, 40};
                    if (prlimit(0, RLIMIT_NICE, &generous, NULL) != 0) _exit(2);
                    if (setresuid(1000, 1000, 1000) != 0) _exit(2);
                    errno = 0;
                    _exit(setpriority(PRIO_PROCESS, limited, -5) == -1 && errno == EACCES
                              ? 0
                              : 1);
                }
                int status = -1;
                int reaped;
                do {
                    reaped = waitpid(caller, &status, 0);
                } while (reaped < 0 && errno == EINTR);
                check("setpriority-target-rlimit-nice-eacces",
                      reaped == caller && WIFEXITED(status) && WEXITSTATUS(status) == 0);
                EXPECT_OK("setpriority-cap-sys-nice-lowers",
                          setpriority(PRIO_PROCESS, limited, -5));
                collect(limited);
            }
        }
    }
    /* The same `task_rlimit(p, RLIMIT_NICE)` read must still reach a target
     * that has already exited: `p->signal` outlives `do_exit()` until
     * `release_task()`, so a caller with a narrow limit of its own succeeds
     * against a zombie that kept a generous one, and is refused for a value
     * the zombie's own limit excludes. */
    {
        pid_t zombie = fork();
        if (zombie == 0) {
            struct rlimit thirty = {30, 30};
            if (prlimit(0, RLIMIT_NICE, &thirty, NULL) != 0) _exit(1);
            if (setresuid(1000, 1000, 1000) != 0) _exit(1);
            _exit(0);
        }
        if (zombie < 0) {
            check("setpriority-zombie-fork", 0);
        } else {
            siginfo_t info;
            int status = -1;
            int reaped;
            memset(&info, 0, sizeof(info));
            errno = 0;
            long waited = syscall(SYS_waitid, P_PID, zombie, &info, WEXITED | WNOWAIT, NULL);
            check("setpriority-zombie-exited", waited == 0 && info.si_pid == zombie);
            /* A caller that lowered its own limit to zero and dropped
             * CAP_SYS_NICE: only the target's retained limit can authorize the
             * reduction, so 20 - (-5) = 25 <= 30 succeeds. */
            pid_t caller = fork();
            if (caller == 0) {
                struct rlimit none = {0, 0};
                if (prlimit(0, RLIMIT_NICE, &none, NULL) != 0) _exit(2);
                if (setresuid(1000, 1000, 1000) != 0) _exit(2);
                errno = 0;
                _exit(setpriority(PRIO_PROCESS, zombie, -5) == 0 ? 0 : 1);
            }
            do {
                reaped = waitpid(caller, &status, 0);
            } while (reaped < 0 && errno == EINTR);
            check("setpriority-zombie-target-rlimit-nice",
                  reaped == caller && WIFEXITED(status) && WEXITSTATUS(status) == 0);
            /* The reduction landed on the retained value.  libc's
             * `getpriority(2)` undoes the kernel's `nice_to_rlimit()` mapping
             * (`include/linux/sched/prio.h:33`, `20 - nice`), so the observable
             * result is the nice value itself, exactly as in `setpriority(2)`. */
            errno = 0;
            long prio = getpriority(PRIO_PROCESS, zombie);
            check("getpriority-zombie-lowered-nice", prio == -5);
            /* The zombie's own limit is also its ceiling: 20 - (-15) = 35. */
            pid_t narrow = fork();
            if (narrow == 0) {
                struct rlimit none = {0, 0};
                if (prlimit(0, RLIMIT_NICE, &none, NULL) != 0) _exit(2);
                if (setresuid(1000, 1000, 1000) != 0) _exit(2);
                errno = 0;
                _exit(setpriority(PRIO_PROCESS, zombie, -15) == -1 && errno == EACCES
                          ? 0
                          : 1);
            }
            do {
                reaped = waitpid(narrow, &status, 0);
            } while (reaped < 0 && errno == EINTR);
            check("setpriority-zombie-above-target-rlimit-eacces",
                  reaped == narrow && WIFEXITED(status) && WEXITSTATUS(status) == 0);
            collect(zombie);
        }
    }
    done();

    /* ---------------- ioprio_set(2)/ioprio_get(2) contract ---------------- */
    begin("ioprio.raw-differential");
    /* `ioprio_check_cap()` runs before the which/who pair is resolved, so an
     * unusable class/level word is EINVAL (or EPERM for realtime) even for a
     * pid that names nothing. */
    EXPECT_ERR("ioprio-set-none-with-level",
               syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, INT32_MAX,
                       IOPRIO_VALUE(IOPRIO_CLASS_NONE, 1)),
               EINVAL);
    EXPECT_ERR("ioprio-set-invalid-class",
               syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, INT32_MAX,
                       IOPRIO_VALUE(7, 0)),
               EINVAL);
    EXPECT_ERR("ioprio-set-bad-which",
               syscall(SYS_ioprio_set, 9, 0, IOPRIO_VALUE(IOPRIO_CLASS_BE, 3)), EINVAL);
    EXPECT_ERR("ioprio-get-bad-which", syscall(SYS_ioprio_get, 9, 0), EINVAL);
    /* WHO_PROCESS reports the stored word verbatim -- an explicit
     * IOPRIO_CLASS_NONE reads back as 0 -- while WHO_PGRP derives the
     * class/level from the task's nice value. */
    {
        long got;
        EXPECT_OK("ioprio-set-self",
                  syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0,
                          IOPRIO_VALUE(IOPRIO_CLASS_BE, 4)));
        errno = 0;
        got = syscall(SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0);
        check("ioprio-get-self-roundtrip", got == IOPRIO_VALUE(IOPRIO_CLASS_BE, 4));
        EXPECT_OK("ioprio-set-self-none",
                  syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_NONE));
        errno = 0;
        got = syscall(SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0);
        check("ioprio-get-self-reports-stored-none", got == 0);
    }
    /* `IOPRIO_WHO_PGRP` resolves its `who` argument as a process-group id and
     * reports the numerically lowest (`ioprio_best()`) value over every member.
     * A member that never called ioprio_set has no io_context, so its
     * contribution is the class/level `task_nice_ioclass()` and
     * `task_nice_ioprio()` derive from its nice value. The group is a child's
     * own and the second member is forked *by* that child, so membership is
     * inherited and exactly known. */
    {
        int ready[2], gate[2];
        if (pipe(ready) != 0 || pipe(gate) != 0) {
            check("ioprio-group-pipe", 0);
        } else {
            pid_t leader = fork();
            if (leader == 0) {
                char byte;

                close(ready[0]);
                close(gate[1]);
                if (setpgid(0, 0) != 0) _exit(1);
                /* The group's only member has no io_context yet. */
                if (write(ready[1], "L", 1) != 1) _exit(1);
                if (read(gate[0], &byte, 1) != 1) _exit(1);
                pid_t member = fork();
                if (member == 0) {
                    if (syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0,
                                IOPRIO_VALUE(IOPRIO_CLASS_BE, 2)) != 0) _exit(1);
                    if (write(ready[1], "M", 1) != 1) _exit(1);
                    if (read(gate[0], &byte, 1) != 1) _exit(0);
                    _exit(0);
                }
                if (read(gate[0], &byte, 1) != 1) _exit(0);
                _exit(0);
            }
            close(ready[1]);
            close(gate[0]);
            if (leader < 0) {
                close(ready[0]);
                close(gate[1]);
                check("ioprio-group-fork", 0);
            } else {
                char byte = 0;
                long got;
                ssize_t ack = read(ready[0], &byte, 1);

                /* nice 0 for a task with no io_context: `task_nice_ioprio()` is
                 * (0 + 20) / 5 = 4 in `task_nice_ioclass()`'s class BE. */
                errno = 0;
                got = syscall(SYS_ioprio_get, IOPRIO_WHO_PGRP, leader);
                check("ioprio-get-pgrp-derives-from-nice",
                      ack == 1 && byte == 'L' && got == IOPRIO_VALUE(IOPRIO_CLASS_BE, 4));
                if (ack == 1 && byte == 'L' && write(gate[1], "F", 1) == 1) {
                    /* The member's BE|2 is numerically lower than the derived
                     * BE|4 of the leader, and `ioprio_best()` is `min()`. */
                    byte = 0;
                    ack = read(ready[0], &byte, 1);
                    errno = 0;
                    got = syscall(SYS_ioprio_get, IOPRIO_WHO_PGRP, leader);
                    check("ioprio-get-pgrp-best-of-group",
                          ack == 1 && byte == 'M'
                              && got == IOPRIO_VALUE(IOPRIO_CLASS_BE, 2));
                } else {
                    check("ioprio-get-pgrp-best-of-group", 0);
                }
                /* Both members exit when the gate closes. */
                close(ready[0]);
                close(gate[1]);
                collect(leader);
            }
        }
    }
    /* An exiting task keeps answering from the terminal state Linux freezes:
     * `exit_io_context()` drops `io_context`, so a zombie reports
     * IOPRIO_DEFAULT and `set_task_ioprio()` silently does nothing for it
     * (blk-ioc.c's `PF_EXITING` early return), and only the reap makes the pid
     * unreachable. */
    {
        pid_t zombie = fork();
        if (zombie == 0) {
            if (syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0,
                        IOPRIO_VALUE(IOPRIO_CLASS_BE, 3)) != 0) _exit(1);
            _exit(0);
        }
        if (zombie < 0) {
            check("ioprio-zombie-fork", 0);
        } else {
            siginfo_t info;
            long got;
            memset(&info, 0, sizeof(info));
            errno = 0;
            long waited = syscall(SYS_waitid, P_PID, zombie, &info, WEXITED | WNOWAIT, NULL);
            errno = 0;
            got = syscall(SYS_ioprio_get, IOPRIO_WHO_PROCESS, zombie);
            check("ioprio-zombie-reports-default", waited == 0 && got == 0);
            EXPECT_OK("ioprio-zombie-set-is-a-noop",
                      syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, zombie,
                              IOPRIO_VALUE(IOPRIO_CLASS_BE, 2)));
            errno = 0;
            got = syscall(SYS_ioprio_get, IOPRIO_WHO_PROCESS, zombie);
            check("ioprio-zombie-keeps-default", got == 0);
            collect(zombie);
            EXPECT_ERR("ioprio-get-after-reap",
                       syscall(SYS_ioprio_get, IOPRIO_WHO_PROCESS, zombie), ESRCH);
            EXPECT_ERR("ioprio-set-after-reap",
                       syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, zombie,
                               IOPRIO_VALUE(IOPRIO_CLASS_BE, 2)),
                       ESRCH);
        }
    }
    done();

    /* ---------------- a tracee's stop reaches its real parent ---------------
     * `wait_consider_task()` forces `ptrace = 1` for a child traced from the
     * waiter's own thread group (`if (!ptrace_reparented(p)) ptrace = 1;`,
     * kernel/exit.c:1522-1523), and `wait_task_stopped()` then reports that
     * stop "regardless of options" (kernel/exit.c:1374-1375). A `PTRACE_TRACEME`
     * child stopped by `raise(SIGSTOP)` must therefore be visible to a bare
     * `wait4(2)`/`waitid(2)` that names no stop option and no session. */
    begin("ptrace_stop.raw-differential");
    {
        pid_t forked[2];
        pid_t child;
        int status = 0;
        siginfo_t info;
        long waited;
        long detached;
        long reaped;
        int index;

        /* Two tracees: each stop is consumed by the wait that reports it, so the
         * `wait4(2)` and the `waitid(2)` observation need one child each. */
        for (index = 0; index < 2; index++) {
            forked[index] = fork();
            if (forked[index] == 0) {
                if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) _exit(1);
                if (raise(SIGSTOP) != 0) _exit(1);
                _exit(0);
            }
        }
        if (forked[0] < 0 || forked[1] < 0) {
            check("ptrace-traceme-fork", 0);
        } else {
            /* `wait4(2)` with no option bits at all. A stopped tracee reports
             * WIFSTOPPED with the stop signal. */
            child = forked[0];
            errno = 0;
            waited = syscall(SYS_wait4, child, &status, 0, NULL);
            check("wait4-traceme-stop-without-wuntraced",
                  waited == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
            /* `waitid(2)` without `WSTOPPED` sees it too, and the report is
             * CLD_TRAPPED rather than CLD_STOPPED because the forced `ptrace`
             * also selects `why` (kernel/exit.c:1414). */
            child = forked[1];
            memset(&info, 0, sizeof(info));
            errno = 0;
            waited = syscall(SYS_waitid, P_PID, child, &info, WEXITED, NULL);
            check("waitid-traceme-stop-is-cld-trapped",
                  waited == 0 && info.si_code == CLD_TRAPPED
                      && info.si_status == SIGSTOP && info.si_pid == child);
            /* Detaching resumes a tracee, so its exit is an ordinary one. */
            errno = 0;
            detached = syscall(SYS_ptrace, PTRACE_DETACH, forked[0], 0, 0);
            detached |= syscall(SYS_ptrace, PTRACE_DETACH, forked[1], 0, 0);
            if (detached == 0) {
                reaped = waitpid(forked[0], NULL, 0) == forked[0]
                    && waitpid(forked[1], NULL, 0) == forked[1];
            } else {
                reaped = 0;
            }
            check("ptrace-tracee-resumes-and-reaps-after-detach", reaped);
        }
        for (index = 0; index < 2; index++) {
            if (forked[index] > 0) collect(forked[index]);
        }
    }
    done();

    if (failed) return 1;
    puts("THEKERNEL_WAIT_ABI_DIFFERENTIAL_OK");
    return 0;
}
