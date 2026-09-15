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
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef P_PIDFD
#define P_PIDFD 3
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
#define MPOL_WEIGHTED_INTERLEAVE 4
#define MPOL_PREFERRED_MANY 5
#define MPOL_F_NODE (1 << 0)
#define MPOL_F_ADDR (1 << 1)
#define MPOL_F_MEMS_ALLOWED (1 << 2)
#define MPOL_MF_STRICT (1 << 0)
#define MPOL_MF_MOVE (1 << 1)
#define MPOL_MF_MOVE_ALL (1 << 2)
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
    kill(child, SIGSTOP);
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
            memset(page, 0, sizeof(page));
            errno = 0;
            long rc = syscall(SYS_sched_getattr, 0, page, sizeof(page), 0);
            uint32_t reported;
            memcpy(&reported, page, sizeof(reported));
            check("getattr-size-page-accepted", rc == 0 && reported == 56);
        }
        EXPECT_ERR("getattr-size-4097-einval", syscall(SYS_sched_getattr, 0, &attr, 4097, 0), EINVAL);
        EXPECT_ERR("getattr-null-attr", syscall(SYS_sched_getattr, 0, NULL, 48, 0), EINVAL);
        EXPECT_ERR("getattr-negative-pid", syscall(SYS_sched_getattr, -1, &attr, 48, 0), EINVAL);
        /* The flags argument is EINVAL unless the target is a deadline task and
         * the value is exactly SCHED_GETATTR_FLAG_DL_DYNAMIC. */
        EXPECT_ERR("getattr-unknown-flags", syscall(SYS_sched_getattr, 0, &attr, 48, 2), EINVAL);
        EXPECT_ERR("getattr-dl-dynamic-on-other",
                   syscall(SYS_sched_getattr, 0, &attr, 48, 1), EINVAL);
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
    /* `maxnode` is a bit count compared against PAGE_SIZE * BITS_PER_BYTE
     * *after* the decrement, so 32769 is the largest admitted window. 32770 is
     * EINVAL before the mask is read at all; 32769 is admitted and then faults
     * on the mask, which proves the bound is on `maxnode - 1`. */
    EXPECT_ERR("set_mempolicy-maxnode-too-long",
               syscall(SYS_set_mempolicy, MPOL_DEFAULT, (void *)1, 32770), EINVAL);
    EXPECT_ERR("set_mempolicy-maxnode-at-bound-faults",
               syscall(SYS_set_mempolicy, MPOL_DEFAULT, (void *)1, 32769), EFAULT);
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
    done();

    if (failed) return 1;
    puts("THEKERNEL_WAIT_ABI_DIFFERENTIAL_OK");
    return 0;
}
