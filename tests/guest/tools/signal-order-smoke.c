#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SI_TKILL
#define SI_TKILL (-6)
#endif

#define LOG_CAPACITY 32
#define VALUE_UNSET (-1)

/* Delivery log filled only from signal handlers. A single sig_atomic_t
 * cursor plus plain int slots is async-signal-safe here because every
 * handler in this program runs on the one main thread: nested handlers
 * (SA_NODEFER) still execute sequentially on that thread's stack. */
static volatile sig_atomic_t log_count;
static volatile int log_signo[LOG_CAPACITY];
static volatile int log_code[LOG_CAPACITY];
static volatile int log_value[LOG_CAPACITY];
static volatile long log_pid[LOG_CAPACITY];
static volatile long log_uid[LOG_CAPACITY];

static volatile sig_atomic_t handler_depth;
static volatile sig_atomic_t handler_max_depth;
static volatile sig_atomic_t defer_pending_seen;

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_SIGORDER_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_SIGORDER_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static void marker(const char *value) {
    puts(value);
    fflush(stdout);
}

static void reset_log(void) {
    log_count = 0;
    for (int index = 0; index < LOG_CAPACITY; ++index) {
        log_signo[index] = 0;
        log_code[index] = 0;
        log_value[index] = VALUE_UNSET;
        log_pid[index] = -1;
        log_uid[index] = -1;
    }
}

static void record_handler(int signo, siginfo_t *info, void *context) {
    (void)context;
    int slot = log_count;
    if (slot < LOG_CAPACITY) {
        log_signo[slot] = signo;
        log_code[slot] = info->si_code;
        /* si_value is only meaningful for queued (SI_QUEUE/SI_TIMER)
         * senders; keep the sentinel otherwise so ordering assertions
         * cannot accidentally match stack garbage. */
        log_value[slot] = info->si_code == SI_QUEUE
                              ? info->si_value.sival_int
                              : VALUE_UNSET;
        log_pid[slot] = (long)info->si_pid;
        log_uid[slot] = (long)info->si_uid;
    }
    log_count = slot + 1;
}

static int install_handler_masked(int signo, int extra_flags,
                                  const sigset_t *handler_mask) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = record_handler;
    action.sa_flags = SA_SIGINFO | extra_flags;
    if (handler_mask != NULL) {
        action.sa_mask = *handler_mask;
    } else {
        sigemptyset(&action.sa_mask);
    }
    if (sigaction(signo, &action, NULL) != 0) {
        return fail("sigaction-install");
    }
    return 0;
}

static int install_handler(int signo, int extra_flags) {
    return install_handler_masked(signo, extra_flags, NULL);
}

static int block_signal_set(const sigset_t *set) {
    if (sigprocmask(SIG_BLOCK, set, NULL) != 0) {
        return fail("sigprocmask-block");
    }
    return 0;
}

static int unblock_signal_set(const sigset_t *set) {
    if (sigprocmask(SIG_UNBLOCK, set, NULL) != 0) {
        return fail("sigprocmask-unblock");
    }
    return 0;
}

static int queue_value(int signo, int value) {
    union sigval payload;
    payload.sival_int = value;
    if (sigqueue(getpid(), signo, payload) != 0) {
        return fail("sigqueue");
    }
    return 0;
}

static int expect_entry(int slot, int signo, int code, int value,
                        const char *stage) {
    if (log_signo[slot] != signo) {
        return fail_value(stage, log_signo[slot], signo);
    }
    if (log_code[slot] != code) {
        return fail_value(stage, log_code[slot], code);
    }
    if (log_value[slot] != value) {
        return fail_value(stage, log_value[slot], value);
    }
    return 0;
}

static int wait_for_exit(pid_t child, const char *stage) {
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        errno = ECHILD;
        return fail(stage);
    }
    return 0;
}

static int run_exit_case(const char *stage, int (*test)(void)) {
    pid_t child = fork();
    if (child < 0) {
        return fail(stage);
    }
    if (child == 0) {
        exit(test());
    }
    return wait_for_exit(child, stage);
}

/* Standard signals do not queue: three sends while blocked collapse into
 * exactly one pending instance, delivered once after unblock. */
static int test_standard_coalesce(void) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);

    reset_log();
    if (install_handler(SIGUSR1, 0) || block_signal_set(&set)) {
        return 1;
    }
    for (int round = 0; round < 3; ++round) {
        if (kill(getpid(), SIGUSR1) != 0) {
            return fail("std-coalesce-kill");
        }
    }
    if (unblock_signal_set(&set)) {
        return 1;
    }
    if (log_count != 1) {
        return fail_value("std-coalesce-count", log_count, 1);
    }
    if (log_signo[0] != SIGUSR1 || log_code[0] != SI_USER) {
        return fail_value("std-coalesce-entry", log_signo[0], SIGUSR1);
    }

    sigset_t pending;
    if (sigpending(&pending) != 0) {
        return fail("std-coalesce-pending-query");
    }
    if (sigismember(&pending, SIGUSR1) != 0) {
        return fail_value("std-coalesce-residual", 1, 0);
    }

    marker("THEKERNEL_SIGORDER_STD_COALESCE_OK");
    return 0;
}

/* Real-time signals queue every instance and deliver them in FIFO enqueue
 * order for the same signal number, carrying the sender's sigval. */
static int test_rt_fifo(void) {
    const int rt = SIGRTMIN;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, rt);

    reset_log();
    if (install_handler(rt, 0) || block_signal_set(&set) ||
        queue_value(rt, 101) || queue_value(rt, 102) ||
        queue_value(rt, 103) || unblock_signal_set(&set)) {
        return 1;
    }
    if (log_count != 3) {
        return fail_value("rt-fifo-count", log_count, 3);
    }
    if (expect_entry(0, rt, SI_QUEUE, 101, "rt-fifo-first") ||
        expect_entry(1, rt, SI_QUEUE, 102, "rt-fifo-second") ||
        expect_entry(2, rt, SI_QUEUE, 103, "rt-fifo-third")) {
        return 1;
    }

    marker("THEKERNEL_SIGORDER_RT_FIFO_OK");
    return 0;
}

static int make_cross_priority_pending(sigset_t *set, int rt0, int rt1,
                                       int rt2) {
    sigemptyset(set);
    sigaddset(set, SIGUSR1);
    sigaddset(set, rt0);
    sigaddset(set, rt1);
    sigaddset(set, rt2);
    if (block_signal_set(set)) {
        return 1;
    }
    /* Enqueue order (rt2, rt0, rt0, usr1, rt1) is deliberately scrambled
     * against the expected dequeue order. */
    if (queue_value(rt2, 31) || queue_value(rt0, 11) ||
        queue_value(rt0, 12)) {
        return 1;
    }
    if (kill(getpid(), SIGUSR1) != 0) {
        return fail("cross-priority-kill");
    }
    return queue_value(rt1, 21);
}

/* Linux dequeues the lowest pending signal number first, so a pending
 * standard signal precedes every RT signal, RT signals dequeue in
 * ascending number, and equal RT numbers keep FIFO enqueue order. To
 * observe that dequeue order through handler entries, every handler's
 * sa_mask must block the whole test set: each frame then runs to
 * completion before the kernel dequeues the next signal. */
static int test_cross_priority(void) {
    const int rt0 = SIGRTMIN;
    const int rt1 = SIGRTMIN + 1;
    const int rt2 = SIGRTMIN + 2;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, rt0);
    sigaddset(&set, rt1);
    sigaddset(&set, rt2);

    reset_log();
    if (install_handler_masked(SIGUSR1, 0, &set) ||
        install_handler_masked(rt0, 0, &set) ||
        install_handler_masked(rt1, 0, &set) ||
        install_handler_masked(rt2, 0, &set) ||
        make_cross_priority_pending(&set, rt0, rt1, rt2) ||
        unblock_signal_set(&set)) {
        return 1;
    }
    if (log_count != 5) {
        return fail_value("cross-priority-count", log_count, 5);
    }
    if (log_signo[0] != SIGUSR1 || log_code[0] != SI_USER) {
        return fail_value("cross-priority-standard-first", log_signo[0],
                          SIGUSR1);
    }
    if (expect_entry(1, rt0, SI_QUEUE, 11, "cross-priority-rt0-first") ||
        expect_entry(2, rt0, SI_QUEUE, 12, "cross-priority-rt0-second") ||
        expect_entry(3, rt1, SI_QUEUE, 21, "cross-priority-rt1") ||
        expect_entry(4, rt2, SI_QUEUE, 31, "cross-priority-rt2")) {
        return 1;
    }

    marker("THEKERNEL_SIGORDER_CROSS_PRIORITY_BOUNDARY "
           "sequence=usr1,rt0:11,rt0:12,rt1:21,rt2:31");
    marker("THEKERNEL_SIGORDER_CROSS_PRIORITY_OK");
    return 0;
}

/* Underdocumented Linux behavior the man pages are silent on: with empty
 * handler sa_masks, one kernel exit dequeues every deliverable pending
 * signal and stacks a frame per DISTINCT signal number before any handler
 * instruction runs. The topmost frame runs first, so handler ENTRY order
 * across distinct numbers is the reverse of the ascending dequeue order.
 * Same-number instances still enter in FIFO order, because each dequeue
 * auto-blocks its own number: the second rt0 instance stays pending until
 * the first rt0 handler frame is torn down by sigreturn. */
static int test_stacked_reversal(void) {
    const int rt0 = SIGRTMIN;
    const int rt1 = SIGRTMIN + 1;
    const int rt2 = SIGRTMIN + 2;
    sigset_t set;

    reset_log();
    if (install_handler(SIGUSR1, 0) || install_handler(rt0, 0) ||
        install_handler(rt1, 0) || install_handler(rt2, 0) ||
        make_cross_priority_pending(&set, rt0, rt1, rt2) ||
        unblock_signal_set(&set)) {
        return 1;
    }
    if (log_count != 5) {
        return fail_value("stacked-reversal-count", log_count, 5);
    }
    if (expect_entry(0, rt2, SI_QUEUE, 31, "stacked-reversal-rt2") ||
        expect_entry(1, rt1, SI_QUEUE, 21, "stacked-reversal-rt1") ||
        expect_entry(2, rt0, SI_QUEUE, 11, "stacked-reversal-rt0-first") ||
        expect_entry(3, rt0, SI_QUEUE, 12, "stacked-reversal-rt0-second")) {
        return 1;
    }
    if (log_signo[4] != SIGUSR1 || log_code[4] != SI_USER) {
        return fail_value("stacked-reversal-standard-last", log_signo[4],
                          SIGUSR1);
    }

    marker("THEKERNEL_SIGORDER_STACKED_REVERSAL_BOUNDARY "
           "sequence=rt2:31,rt1:21,rt0:11,rt0:12,usr1");
    marker("THEKERNEL_SIGORDER_STACKED_REVERSAL_OK");
    return 0;
}

/* sigtimedwait with a zero timeout reports EAGAIN when nothing in the set
 * is pending, and dequeues pending RT signals in the same ascending-number
 * FIFO order as asynchronous delivery, returning the queued si_value. */
static int test_timedwait_order(void) {
    const int rt0 = SIGRTMIN;
    const int rt1 = SIGRTMIN + 1;
    const struct timespec zero_timeout = {0, 0};
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, rt0);
    sigaddset(&set, rt1);

    if (block_signal_set(&set)) {
        return 1;
    }
    errno = 0;
    if (sigtimedwait(&set, NULL, &zero_timeout) != -1 || errno != EAGAIN) {
        return fail("timedwait-empty-eagain");
    }
    marker("THEKERNEL_SIGORDER_TIMEDWAIT_EAGAIN_OK");

    if (queue_value(rt1, 55) || queue_value(rt0, 44) ||
        queue_value(rt0, 45)) {
        return 1;
    }

    const int expected[3][2] = {{0, 44}, {0, 45}, {1, 55}};
    for (int index = 0; index < 3; ++index) {
        siginfo_t info;
        memset(&info, 0, sizeof(info));
        int signo = expected[index][0] == 0 ? rt0 : rt1;
        int result = sigtimedwait(&set, &info, &zero_timeout);
        if (result != signo) {
            return fail_value("timedwait-dequeue-signo", result, signo);
        }
        if (info.si_code != SI_QUEUE ||
            info.si_value.sival_int != expected[index][1]) {
            return fail_value("timedwait-dequeue-value",
                              info.si_value.sival_int, expected[index][1]);
        }
    }
    errno = 0;
    if (sigtimedwait(&set, NULL, &zero_timeout) != -1 || errno != EAGAIN) {
        return fail("timedwait-drained-eagain");
    }

    marker("THEKERNEL_SIGORDER_TIMEDWAIT_ORDER_OK");
    return 0;
}

/* sigpending reflects the blocked pending set as a bitmask: a standard
 * signal raised twice still shows exactly one bit, an unraised blocked
 * signal shows none, and one dequeue clears the bit completely. */
static int test_pending_set(void) {
    const struct timespec zero_timeout = {0, 0};
    sigset_t set;
    sigset_t pending;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);

    if (block_signal_set(&set)) {
        return 1;
    }
    if (kill(getpid(), SIGUSR1) != 0 || kill(getpid(), SIGUSR1) != 0) {
        return fail("pending-set-kill");
    }
    if (sigpending(&pending) != 0) {
        return fail("pending-set-query");
    }
    if (sigismember(&pending, SIGUSR1) != 1) {
        return fail_value("pending-set-usr1", 0, 1);
    }
    if (sigismember(&pending, SIGUSR2) != 0) {
        return fail_value("pending-set-usr2", 1, 0);
    }

    sigset_t wait_set;
    sigemptyset(&wait_set);
    sigaddset(&wait_set, SIGUSR1);
    if (sigtimedwait(&wait_set, NULL, &zero_timeout) != SIGUSR1) {
        return fail("pending-set-dequeue");
    }
    if (sigpending(&pending) != 0) {
        return fail("pending-set-requery");
    }
    if (sigismember(&pending, SIGUSR1) != 0) {
        return fail_value("pending-set-coalesced", 1, 0);
    }

    marker("THEKERNEL_SIGORDER_PENDING_SET_OK");
    return 0;
}

/* si_code discriminates the sender API: kill() stamps SI_USER, sigqueue()
 * stamps SI_QUEUE, and tgkill() stamps SI_TKILL. SI_USER and SI_QUEUE must
 * also populate si_pid/si_uid with the sender's credentials. */
static int test_si_code(void) {
    reset_log();
    if (install_handler(SIGUSR1, 0)) {
        return 1;
    }
    long self_pid = (long)getpid();
    long self_uid = (long)getuid();

    if (kill(getpid(), SIGUSR1) != 0) {
        return fail("si-code-kill");
    }
    if (queue_value(SIGUSR1, 77)) {
        return 1;
    }
    if (syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid),
                SIGUSR1) != 0) {
        return fail("si-code-tgkill");
    }

    if (log_count != 3) {
        return fail_value("si-code-count", log_count, 3);
    }
    if (log_code[0] != SI_USER) {
        return fail_value("si-code-user", log_code[0], SI_USER);
    }
    if (log_pid[0] != self_pid || log_uid[0] != self_uid) {
        return fail_value("si-code-user-ids", log_pid[0], self_pid);
    }
    if (log_code[1] != SI_QUEUE || log_value[1] != 77) {
        return fail_value("si-code-queue", log_code[1], SI_QUEUE);
    }
    if (log_pid[1] != self_pid || log_uid[1] != self_uid) {
        return fail_value("si-code-queue-ids", log_pid[1], self_pid);
    }
    if (log_code[2] != SI_TKILL) {
        return fail_value("si-code-tkill", log_code[2], SI_TKILL);
    }

    marker("THEKERNEL_SIGORDER_SICODE_OK");
    return 0;
}

static void defer_probe_handler(int signo, siginfo_t *info, void *context) {
    (void)info;
    (void)context;
    int depth = handler_depth + 1;
    handler_depth = depth;
    if (depth > handler_max_depth) {
        handler_max_depth = depth;
    }
    log_count = log_count + 1;
    /* Re-raise once from the first entry only, so nesting depth is bounded
     * by construction whichever deferral policy is active. */
    if (log_count == 1) {
        (void)kill(getpid(), signo);
        sigset_t pending;
        if (sigpending(&pending) == 0 &&
            sigismember(&pending, signo) == 1) {
            defer_pending_seen = 1;
        }
    }
    handler_depth = depth - 1;
}

/* Without SA_NODEFER the kernel blocks the signal for the duration of its
 * own handler: a re-raise from inside stays pending until the handler
 * returns, so both entries run at depth 1. */
static int test_defer_default(void) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = defer_probe_handler;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR2, &action, NULL) != 0) {
        return fail("defer-default-sigaction");
    }

    reset_log();
    handler_depth = 0;
    handler_max_depth = 0;
    defer_pending_seen = 0;
    if (kill(getpid(), SIGUSR2) != 0) {
        return fail("defer-default-kill");
    }
    if (log_count != 2) {
        return fail_value("defer-default-count", log_count, 2);
    }
    if (handler_max_depth != 1) {
        return fail_value("defer-default-depth", handler_max_depth, 1);
    }
    if (defer_pending_seen != 1) {
        return fail_value("defer-default-pending", defer_pending_seen, 1);
    }

    marker("THEKERNEL_SIGORDER_DEFER_DEFAULT_OK");
    return 0;
}

/* With SA_NODEFER the re-raise is delivered immediately inside the running
 * handler: the nested entry pushes depth to exactly 2 and nothing remains
 * pending at the point of the re-raise. */
static int test_nodefer_nesting(void) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = defer_probe_handler;
    action.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR2, &action, NULL) != 0) {
        return fail("nodefer-sigaction");
    }

    reset_log();
    handler_depth = 0;
    handler_max_depth = 0;
    defer_pending_seen = 0;
    if (kill(getpid(), SIGUSR2) != 0) {
        return fail("nodefer-kill");
    }
    if (log_count != 2) {
        return fail_value("nodefer-count", log_count, 2);
    }
    if (handler_max_depth != 2) {
        return fail_value("nodefer-depth", handler_max_depth, 2);
    }
    if (defer_pending_seen != 0) {
        return fail_value("nodefer-pending", defer_pending_seen, 0);
    }

    marker("THEKERNEL_SIGORDER_NODEFER_NEST_OK");
    return 0;
}

int main(int argc, char **argv) {
    (void)argv;
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (argc != 1) {
        errno = EINVAL;
        return fail("unknown-option");
    }

    if (run_exit_case("standard-coalesce", test_standard_coalesce) ||
        run_exit_case("rt-fifo", test_rt_fifo) ||
        run_exit_case("cross-priority", test_cross_priority) ||
        run_exit_case("stacked-reversal", test_stacked_reversal) ||
        run_exit_case("timedwait-order", test_timedwait_order) ||
        run_exit_case("pending-set", test_pending_set) ||
        run_exit_case("si-code", test_si_code) ||
        run_exit_case("defer-default", test_defer_default) ||
        run_exit_case("nodefer-nesting", test_nodefer_nesting)) {
        return 1;
    }

    marker("THEKERNEL_SIGORDER_OK");
    return 0;
}
