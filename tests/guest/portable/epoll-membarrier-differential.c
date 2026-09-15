/* Portable ABI differential for the epoll/membarrier/restart-syscall cells.
 *
 * Every assertion below is a property that Linux 7.2.3 and TheKernel must agree
 * on, so the same records are emitted on both guests.  Where the two kernels
 * legitimately differ, the difference is described in the comment and the
 * record is left out on purpose:
 *
 *   - EPOLL_CTL_ADD with EPOLLEXCLUSIVE is admitted by Linux and refused by
 *     TheKernel with EPERM, because this kernel has no source-side registry to
 *     arbitrate exclusive wake ownership.  Only the absence of EINVAL is
 *     asserted here; tests/guest/portable/epoll-smoke.c pins the exact errno
 *     for the TheKernel guest.
 *   - MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ is implemented by the oracle and
 *     absent from TheKernel's QUERY mask, so no RSEQ command is called.
 *
 * The record protocol is the one tools/qemu_runner/abi_differential.py checks:
 * one case line, one assert line per assertion, one result line.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#ifndef EPOLLEXCLUSIVE
#define EPOLLEXCLUSIVE (1U << 28)
#endif
#ifndef EPOLLWAKEUP
#define EPOLLWAKEUP (1U << 29)
#endif
#ifndef EPOLLMSG
#define EPOLLMSG 0x0400
#endif

/* Keep the x86_64 Linux numbers available with older headers. */
#ifndef SYS_restart_syscall
#ifdef __NR_restart_syscall
#define SYS_restart_syscall __NR_restart_syscall
#else
#define SYS_restart_syscall 219
#endif
#endif
#ifndef SYS_membarrier
#ifdef __NR_membarrier
#define SYS_membarrier __NR_membarrier
#else
#define SYS_membarrier 324
#endif
#endif

#ifndef MEMBARRIER_CMD_QUERY
#define MEMBARRIER_CMD_QUERY 0
#define MEMBARRIER_CMD_GLOBAL 1
#define MEMBARRIER_CMD_GLOBAL_EXPEDITED 2
#define MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED 4
#define MEMBARRIER_CMD_PRIVATE_EXPEDITED 8
#define MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED 16
#define MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE 32
#define MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE 64
#define MEMBARRIER_CMD_GET_REGISTRATIONS 512
#endif

/* MEMBARRIER_CMD_FLAG_CPU.  In this ABI only MEMBARRIER_CMD_PRIVATE_EXPEDITED_
 * RSEQ accepts it, and that command is not implemented here, so every call that
 * passes the flag must be rejected with EINVAL. */
#define MEMBARRIER_FLAG_CPU 1u
#define MEMBARRIER_ALL_COMMANDS 1023L

#define CASE_NAME "epoll-membarrier.portable-differential"
#define SUCCESS_MARKER "THEKERNEL_EPOLL_MEMBARRIER_OK"

#define ASSERTION_FORMAT "THEKERNEL_ABI_ASSERT " CASE_NAME " %s pass"

/* Long enough that a recomputed relative timeout is clearly distinguishable
 * from the original absolute deadline, short enough for the suite budget. */
#define POLL_TIMEOUT_MS 1200
#define POLL_ALARM_MS 700
#define SLEEP_TIMEOUT_MS 600
#define SLEEP_ALARM_MS 100

/* Must mirror the checks `main()` runs one for one: a record for an assertion
 * that was not actually verified would be worse than no record. */
static const char *const assertions[] = {
    "EPOLLEXCLUSIVE_ADD_ADMISSION",
    "EPOLLEXCLUSIVE_ADD_BITS",
    "EPOLLEXCLUSIVE_MOD_BEFORE_LOOKUP",
    "EPOLLEXCLUSIVE_NESTED_EINVAL",
    "EPOLLEXCLUSIVE_DEL_IGNORES_MASK",
    "EPOLLMSG_INTEREST_ACCEPTED",
    "EPOLLWAKEUP_INTEREST_ACCEPTED",
    "MEMBARRIER_QUERY_MASK",
    "MEMBARRIER_FLAG_CPU_RULE",
    "MEMBARRIER_GLOBAL_COMMANDS",
    "MEMBARRIER_REGISTRATION_STATE",
    "MEMBARRIER_UNKNOWN_COMMAND",
    "RESTART_NO_BLOCK_EINTR",
    "NANOSLEEP_REM_WRITE_BACK",
    "NANOSLEEP_SA_RESTART_EINTR",
    "NANOSLEEP_RESTART_BLOCK_DEADLINE",
    "PPOLL_REMAINING_WRITE_BACK",
    "PPOLL_SA_RESTART_EINTR",
    "PPOLL_WITHOUT_RESTART_BLOCK",
    "SELECT_REMAINING_WRITE_BACK",
    "POLL_RESTART_BLOCK_DEADLINE",
};

static volatile sig_atomic_t handler_runs;
static volatile sig_atomic_t handler_restarts;
static volatile sig_atomic_t restart_result;

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_EPOLL_MEMBARRIER_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected)
{
    fprintf(stderr,
            "THEKERNEL_EPOLL_MEMBARRIER_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static long long now_ms(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return -1;
    }
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void alarm_handler(int signo)
{
    (void)signo;
    ++handler_runs;
    if (handler_restarts) {
        /* The only syscall a restart-block test may issue from the handler:
         * every other syscall ends the restart block's life, so the point of
         * these cases is that restart_syscall() still finds it armed. */
        restart_result = (sig_atomic_t)syscall(SYS_restart_syscall);
    }
}

static int install_handler(int flags)
{
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = alarm_handler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = flags;
    if (sigaction(SIGALRM, &action, NULL) != 0) {
        return fail("sigaction");
    }
    return 0;
}

static int set_alarm_ms(long milliseconds, const char *stage)
{
    struct itimerval timer;
    memset(&timer, 0, sizeof(timer));
    timer.it_value.tv_sec = milliseconds / 1000;
    timer.it_value.tv_usec = (milliseconds % 1000) * 1000;
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
        return fail(stage);
    }
    return 0;
}

static int clear_alarm(void)
{
    struct itimerval timer;
    memset(&timer, 0, sizeof(timer));
    return setitimer(ITIMER_REAL, &timer, NULL);
}

/* ------------------------------------------------------------------ epoll_ctl */

static int check_epoll_admission(void)
{
    int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    int event_fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    int nested_fd = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event exclusive = {.events = EPOLLIN | EPOLLEXCLUSIVE};
    struct epoll_event plain = {.events = EPOLLIN};
    struct epoll_event bits = {.events = EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT};
    struct epoll_event pri = {.events = EPOLLIN | EPOLLEXCLUSIVE | EPOLLPRI};
    struct epoll_event msg = {.events = EPOLLIN | EPOLLMSG};
    struct epoll_event wakeup = {.events = EPOLLIN | EPOLLWAKEUP};
    long result;

    exclusive.data.u32 = 1;
    plain.data.u32 = 1;
    bits.data.u32 = 1;
    pri.data.u32 = 1;
    msg.data.u32 = 1;
    wakeup.data.u32 = 1;
    if (epoll_fd < 0 || event_fd < 0 || nested_fd < 0) {
        return fail("epoll-create");
    }

    /* EPOLL_CTL_DEL carries no event: `ep_op_has_event(DEL)` is false, so the
     * mask is never inspected and a flag-carrying one is accepted. */
    if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, event_fd, &plain) != 0) {
        return fail("epoll-add-plain");
    }
    if (epoll_ctl(epoll_fd, EPOLL_CTL_DEL, event_fd, &exclusive) != 0) {
        return fail("epoll-del-ignores-mask");
    }

    /* The admitted request: Linux registers it, TheKernel reports EPERM for the
     * capability it cannot provide.  EINVAL would mean the admission table
     * disagreed about the bit combination itself. */
    errno = 0;
    result = epoll_ctl(epoll_fd, EPOLL_CTL_ADD, event_fd, &exclusive);
    if (!(result == 0 || (result == -1 && errno == EPERM))) {
        return fail_value("epoll-add-exclusive", result, 0);
    }
    if (result == 0 && epoll_ctl(epoll_fd, EPOLL_CTL_DEL, event_fd, NULL) != 0) {
        return fail("epoll-del-after-add");
    }

    /* EPOLL_CTL_MOD never carries the flag, and the check happens before the
     * item lookup: an unregistered descriptor still reports EINVAL, not ENOENT. */
    errno = 0;
    result = epoll_ctl(epoll_fd, EPOLL_CTL_MOD, event_fd, &exclusive);
    if (result != -1 || errno != EINVAL) {
        return fail_value("epoll-mod-exclusive-before-lookup", result, -1);
    }

    /* EPOLLEXCLUSIVE_OK_BITS is EPOLLIN|EPOLLOUT|EPOLLERR|EPOLLHUP|EPOLLWAKEUP|
     * EPOLLET|EPOLLEXCLUSIVE; EPOLLONESHOT and EPOLLPRI are outside it. */
    errno = 0;
    result = epoll_ctl(epoll_fd, EPOLL_CTL_ADD, event_fd, &bits);
    if (result != -1 || errno != EINVAL) {
        return fail_value("epoll-add-exclusive-oneshot", result, -1);
    }
    errno = 0;
    result = epoll_ctl(epoll_fd, EPOLL_CTL_ADD, event_fd, &pri);
    if (result != -1 || errno != EINVAL) {
        return fail_value("epoll-add-exclusive-pri", result, -1);
    }

    /* Nested exclusive wakeups are unsupported. */
    errno = 0;
    result = epoll_ctl(epoll_fd, EPOLL_CTL_ADD, nested_fd, &exclusive);
    if (result != -1 || errno != EINVAL) {
        return fail_value("epoll-add-exclusive-nested", result, -1);
    }

    /* Neither EPOLLMSG nor EPOLLWAKEUP is validated away: the mask is stored
     * verbatim (EPOLLWAKEUP first cleared without CAP_BLOCK_SUSPEND) and only
     * ever intersected with what the target's ->poll() reports. */
    if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, event_fd, &plain) != 0) {
        return fail("epoll-add-msg");
    }
    if (epoll_ctl(epoll_fd, EPOLL_CTL_MOD, event_fd, &msg) != 0) {
        return fail("epoll-mod-msg");
    }
    if (epoll_ctl(epoll_fd, EPOLL_CTL_MOD, event_fd, &wakeup) != 0) {
        return fail("epoll-mod-wakeup");
    }

    close(nested_fd);
    close(event_fd);
    close(epoll_fd);
    return 0;
}

/* ---------------------------------------------------------------- membarrier */

static long membarrier_call(int command, unsigned int flags, int cpu_id)
{
    errno = 0;
    return syscall(SYS_membarrier, command, flags, cpu_id);
}

static int expect_membarrier(int command, unsigned int flags, int cpu_id,
                             long expected, int expected_errno,
                             const char *stage)
{
    long result = membarrier_call(command, flags, cpu_id);
    int saved_errno = errno;
    if (expected == 0) {
        if (result != 0) {
            errno = saved_errno;
            return fail_value(stage, result, 0);
        }
        return 0;
    }
    if (result != -1 || saved_errno != expected_errno) {
        errno = saved_errno;
        return fail_value(stage, result, -1);
    }
    return 0;
}

static int check_membarrier(void)
{
    const long global_commands = MEMBARRIER_CMD_GLOBAL |
                                 MEMBARRIER_CMD_GLOBAL_EXPEDITED |
                                 MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED;
    const long private_commands = MEMBARRIER_CMD_PRIVATE_EXPEDITED |
                                  MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
                                  MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE |
                                  MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE |
                                  MEMBARRIER_CMD_GET_REGISTRATIONS;
    long query;
    long registrations;

    /* QUERY is the command mask, so it never advertises MEMBARRIER_CMD_QUERY
     * itself and never sets a bit outside MEMBARRIER_CMD_BITMASK. */
    query = membarrier_call(MEMBARRIER_CMD_QUERY, 0, 0);
    if (query < 0 || (query & (global_commands | private_commands)) !=
                         (global_commands | private_commands) ||
        (query & ~MEMBARRIER_ALL_COMMANDS) != 0 ||
        (query & MEMBARRIER_CMD_QUERY) != 0) {
        return fail_value("membarrier-query-mask", query,
                          global_commands | private_commands);
    }

    /* MEMBARRIER_CMD_FLAG_CPU is accepted by MEMBARRIER_CMD_PRIVATE_EXPEDITED_
     * RSEQ only, which this kernel does not implement, and cpu_id is ignored
     * without the flag. */
    if (expect_membarrier(MEMBARRIER_CMD_QUERY, MEMBARRIER_FLAG_CPU, 0, -1,
                          EINVAL, "membarrier-query-cpu-flag") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_GLOBAL, MEMBARRIER_FLAG_CPU, 0, -1,
                          EINVAL, "membarrier-global-cpu-flag") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_GET_REGISTRATIONS,
                          MEMBARRIER_FLAG_CPU, 0, -1, EINVAL,
                          "membarrier-registrations-cpu-flag") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED,
                          MEMBARRIER_FLAG_CPU, 0, -1, EINVAL,
                          "membarrier-private-cpu-flag") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE,
                          MEMBARRIER_FLAG_CPU, 0, -1, EINVAL,
                          "membarrier-sync-core-cpu-flag") != 0) {
        return 1;
    }

    /* The private expedited commands need a registration; the global pair and
     * MEMBARRIER_CMD_GLOBAL need none and take no capability. */
    if (expect_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1, -1, EPERM,
                          "membarrier-private-unregistered") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0, -1, -1,
                          EPERM, "membarrier-sync-core-unregistered") != 0) {
        return 1;
    }
    if (expect_membarrier(MEMBARRIER_CMD_GLOBAL, 0, -1, 0, 0,
                          "membarrier-global") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_GLOBAL_EXPEDITED, 0, -1, 0, 0,
                          "membarrier-global-expedited") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED, 0, -1, 0, 0,
                          "membarrier-register-global") != 0) {
        return 1;
    }
    registrations = membarrier_call(MEMBARRIER_CMD_GET_REGISTRATIONS, 0, -1);
    if (registrations < 0 ||
        (registrations & MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED) == 0) {
        return fail_value("membarrier-registrations-global", registrations,
                          MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED);
    }

    /* Registering one private mode must not authorize the others, and the
     * reported mask is the set of registration commands issued so far. */
    if (expect_membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED, 0, -1, 0,
                          0, "membarrier-register-private") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1, 0, 0,
                          "membarrier-private-registered") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE,
                          0, -1, 0, 0, "membarrier-register-sync-core") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0, -1, 0,
                          0, "membarrier-sync-core-registered") != 0) {
        return 1;
    }
    registrations = membarrier_call(MEMBARRIER_CMD_GET_REGISTRATIONS, 0, -1);
    if (registrations < 0 ||
        (registrations & (MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED |
                          MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
                          MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE)) !=
            (MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED |
             MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
             MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE)) {
        return fail_value("membarrier-registrations-mask", registrations,
                          MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED |
                              MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
                              MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE);
    }

    /* Unknown commands, multi-bit command values, and negative commands are
     * EINVAL, and flag validation precedes the command table. */
    if (expect_membarrier(1 << 20, 0, -1, -1, EINVAL,
                          "membarrier-unknown-command") != 0 ||
        expect_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED |
                              MEMBARRIER_CMD_GET_REGISTRATIONS,
                          0, -1, -1, EINVAL, "membarrier-command-bits") != 0 ||
        expect_membarrier(-1, 0, -1, -1, EINVAL,
                          "membarrier-negative-command") != 0) {
        return 1;
    }
    return 0;
}

/* ---------------------------------------------------------- restart lifetime
 *
 * Only interruptions that run a handler are testable here.  A "no handler"
 * interruption cannot be produced portably: Linux drops a default-ignored
 * signal before it is ever queued (`prepare_signal()` ->
 * `sig_handler_ignored()` -> `sig_kernel_ignore()`, so SIGCHLD/SIGURG/SIGWINCH
 * with SIG_DFL never arrive) and never wakes a single-threaded task for a
 * signal it blocks (`complete_signal()`: `thread_group_empty(p)` returns
 * without `signal_wake_up()`).  Every signal that does interrupt a sleeping
 * syscall here therefore has a handler, and `restart_syscall()` is called from
 * inside it -- which is exactly the lifetime the ledger has to get right. */

/* The ledger is consumed by an explicit restart_syscall(); Linux's rt_sigreturn
 * clears `restart_block.fn` on the way back from the handler, so a restart
 * attempted after the handler has returned always reports EINTR. */
static int check_restart_without_block(void)
{
    long result = syscall(SYS_restart_syscall);
    if (result != -1 || errno != EINTR) {
        return fail_value("restart-without-block", result, -1);
    }
    return 0;
}

/* ppoll/select return -ERESTARTNOHAND and arm no restart block at all, so a
 * handler's restart_syscall() finds none and reports EINTR. */
static int check_ppoll_without_restart_block(void)
{
    struct pollfd descriptor = {.fd = -1, .events = POLLIN, .revents = 0};
    struct timespec timeout = {.tv_sec = 1, .tv_nsec = 0};
    int pipe_fds[2];
    long result;

    if (pipe(pipe_fds) != 0) {
        return fail("ppoll-pipe");
    }
    descriptor.fd = pipe_fds[0];
    handler_runs = 0;
    handler_restarts = 1;
    restart_result = 0;
    if (install_handler(SA_RESTART) != 0 || set_alarm_ms(100, "ppoll-alarm") != 0) {
        return 1;
    }
    errno = 0;
    result = syscall(SYS_ppoll, &descriptor, 1, &timeout, NULL, 0);
    int saved_errno = errno;
    clear_alarm();
    if (result != -1 || saved_errno != EINTR || handler_runs != 1) {
        errno = saved_errno;
        return fail_value("ppoll-interrupted", result, -1);
    }
    if (restart_result != -1) {
        return fail_value("ppoll-restart-without-block", restart_result, -1);
    }
    /* The remaining time is written back before the EINTR return. */
    if (timeout.tv_sec < 0 || timeout.tv_nsec < 0 || timeout.tv_nsec >= 1000000000L ||
        timeout.tv_sec != 0 || timeout.tv_nsec == 0) {
        return fail_value("ppoll-remaining", timeout.tv_nsec, 1);
    }
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

/* select() has no restart block either, and its struct timeval is updated in
 * place on the interrupted return. */
static int check_select_remaining(void)
{
    struct timeval timeout = {.tv_sec = 1, .tv_usec = 0};
    fd_set read_set;
    int pipe_fds[2];
    long result;

    if (pipe(pipe_fds) != 0) {
        return fail("select-pipe");
    }
    FD_ZERO(&read_set);
    FD_SET(pipe_fds[0], &read_set);
    handler_runs = 0;
    handler_restarts = 0;
    if (install_handler(SA_RESTART) != 0 || set_alarm_ms(100, "select-alarm") != 0) {
        return 1;
    }
    errno = 0;
    result = syscall(SYS_select, pipe_fds[0] + 1, &read_set, NULL, NULL, &timeout);
    int saved_errno = errno;
    clear_alarm();
    if (result != -1 || saved_errno != EINTR || handler_runs != 1) {
        errno = saved_errno;
        return fail_value("select-interrupted", result, -1);
    }
    if (timeout.tv_sec != 0 || timeout.tv_usec <= 0 || timeout.tv_usec >= 1000000L) {
        return fail_value("select-remaining", timeout.tv_usec, 1);
    }
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

/* nanosleep() reports -ERESTART_RESTARTBLOCK: the original absolute deadline is
 * recorded before the sleep starts and its remaining time is copied out even
 * when a handler makes the call return EINTR. */
static int check_nanosleep_restart_block(void)
{
    struct timespec request = {.tv_sec = 0,
                               .tv_nsec = (long)SLEEP_TIMEOUT_MS * 1000000L};
    struct timespec remaining = {.tv_sec = -1, .tv_nsec = -1};
    long long started;
    long long finished;
    long result;

    /* First: the EINTR return and the remaining-time write-back. */
    handler_runs = 0;
    handler_restarts = 0;
    if (install_handler(SA_RESTART) != 0 ||
        set_alarm_ms(SLEEP_ALARM_MS, "nanosleep-alarm") != 0) {
        return 1;
    }
    started = now_ms();
    errno = 0;
    result = nanosleep(&request, &remaining);
    int saved_errno = errno;
    finished = now_ms();
    clear_alarm();
    if (result != -1 || saved_errno != EINTR || handler_runs != 1) {
        errno = saved_errno;
        return fail_value("nanosleep-interrupted", result, -1);
    }
    if (remaining.tv_sec < 0 || remaining.tv_nsec < 0 ||
        remaining.tv_nsec >= 1000000000L ||
        (remaining.tv_sec == 0 && remaining.tv_nsec == 0) ||
        remaining.tv_sec > request.tv_sec) {
        return fail_value("nanosleep-remaining", remaining.tv_nsec, 1);
    }
    if (finished - started >= SLEEP_TIMEOUT_MS) {
        /* SA_RESTART must not silently replay a restart-block sleep. */
        return fail_value("nanosleep-eintr-before-deadline", finished - started,
                          SLEEP_TIMEOUT_MS);
    }

    /* Second: a handler's restart_syscall() resumes the sleep at the original
     * absolute deadline, so the total wait is the requested interval, not the
     * interval plus the time already spent. */
    memset(&remaining, 0, sizeof(remaining));
    handler_runs = 0;
    handler_restarts = 1;
    restart_result = 1;
    if (install_handler(SA_RESTART) != 0 ||
        set_alarm_ms(SLEEP_ALARM_MS, "nanosleep-restart-alarm") != 0) {
        return 1;
    }
    started = now_ms();
    errno = 0;
    result = nanosleep(&request, &remaining);
    saved_errno = errno;
    finished = now_ms();
    clear_alarm();
    if (result != -1 || saved_errno != EINTR || handler_runs != 1) {
        errno = saved_errno;
        return fail_value("nanosleep-restart-interrupted", result, -1);
    }
    if (restart_result != 0) {
        return fail_value("nanosleep-restart-result", restart_result, 0);
    }
    if (finished - started < SLEEP_TIMEOUT_MS ||
        finished - started > SLEEP_TIMEOUT_MS + 300) {
        return fail_value("nanosleep-restart-deadline", finished - started,
                          SLEEP_TIMEOUT_MS);
    }
    return 0;
}

/* sys_poll() records the absolute end time in the restart block, so a handler's
 * restart_syscall() waits only for what is left of the original timeout. */
static int check_poll_restart_block(void)
{
    struct pollfd descriptor = {.fd = -1, .events = POLLIN, .revents = 0};
    int pipe_fds[2];
    long long started;
    long long finished;
    long result;

    if (pipe(pipe_fds) != 0) {
        return fail("poll-pipe");
    }
    descriptor.fd = pipe_fds[0];

    /* SA_RESTART must not replay poll() either. */
    handler_runs = 0;
    handler_restarts = 0;
    if (install_handler(SA_RESTART) != 0 || set_alarm_ms(100, "poll-alarm") != 0) {
        return 1;
    }
    errno = 0;
    result = poll(&descriptor, 1, POLL_TIMEOUT_MS);
    int saved_errno = errno;
    clear_alarm();
    if (result != -1 || saved_errno != EINTR || handler_runs != 1) {
        errno = saved_errno;
        return fail_value("poll-interrupted", result, -1);
    }

    handler_runs = 0;
    handler_restarts = 1;
    restart_result = 1;
    if (install_handler(SA_RESTART) != 0 ||
        set_alarm_ms(POLL_ALARM_MS, "poll-restart-alarm") != 0) {
        return 1;
    }
    started = now_ms();
    errno = 0;
    result = poll(&descriptor, 1, POLL_TIMEOUT_MS);
    saved_errno = errno;
    finished = now_ms();
    clear_alarm();
    if (result != -1 || saved_errno != EINTR || handler_runs != 1) {
        errno = saved_errno;
        return fail_value("poll-restart-interrupted", result, -1);
    }
    if (restart_result != 0) {
        return fail_value("poll-restart-result", restart_result, 0);
    }
    /* The restarted wait ends at the original deadline.  A recomputed relative
     * timeout would end POLL_ALARM_MS later instead. */
    if (finished - started < POLL_TIMEOUT_MS ||
        finished - started > POLL_TIMEOUT_MS + 300) {
        return fail_value("poll-restart-deadline", finished - started,
                          POLL_TIMEOUT_MS);
    }
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

int main(int argc, char **argv)
{
    int thekernel_mode = 0;
    int failed = 0;

    for (int index = 1; index < argc; ++index) {
        if (strcmp(argv[index], "--thekernel") == 0) {
            thekernel_mode = 1;
        } else if (strcmp(argv[index], "--linux-host") == 0) {
            thekernel_mode = 0;
        } else {
            errno = EINVAL;
            return fail("arguments");
        }
    }

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    /* Informational only: the differential runner treats every
     * THEKERNEL_ABI_ line as a counted record. */
    printf("THEKERNEL_EPOLL_MEMBARRIER_INFO mode=%s\n",
           thekernel_mode ? "thekernel" : "linux");

    failed |= check_epoll_admission();
    failed |= check_membarrier();
    failed |= check_restart_without_block();
    failed |= check_ppoll_without_restart_block();
    failed |= check_select_remaining();
    failed |= check_nanosleep_restart_block();
    failed |= check_poll_restart_block();
    if (failed) {
        fprintf(stderr, "THEKERNEL_EPOLL_MEMBARRIER_FAIL assertions\n");
        return 1;
    }

    puts("THEKERNEL_ABI_CASE " CASE_NAME);
    for (size_t index = 0; index < sizeof(assertions) / sizeof(assertions[0]);
         ++index) {
        printf(ASSERTION_FORMAT "\n", assertions[index]);
    }
    puts("THEKERNEL_ABI_RESULT " CASE_NAME " pass");
    puts(SUCCESS_MARKER);
    return 0;
}
