#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <signal.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#define BAD ((void *)(uintptr_t)1)
#define AUTO (1U << 31)
struct action { uintptr_t handler, flags, restorer; uint64_t mask; };
struct altstack { uintptr_t sp; unsigned flags, pad; size_t size; };
_Static_assert(sizeof(struct action) == 32, "native rt_sigaction");
_Static_assert(sizeof(struct altstack) == 24, "native stack_t");
static const char *active;
static void begin(const char *name) { active = name; printf("THEKERNEL_ABI_CASE %s\n", name); }
static void check(int good, const char *name) {
    if (!good) { fprintf(stderr, "THEKERNEL_SIGNAL_BOUNDARY_FAIL %s %s errno=%d\n", active, name, errno); exit(1); }
}
static void mark(const char *name) { printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name); }
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
#define ERROR(call, err, name) do { errno=0; long rc=(call); check(rc == -1 && errno == (err), name); } while (0)
static long sa(int sig, const void *in, void *out, size_t len) { return syscall(SYS_rt_sigaction, sig, in, out, len); }
static long alt(const void *in, void *out) { return syscall(SYS_sigaltstack, in, out); }
static unsigned char first[65536], second[65536];

/*
 * Helpers for the delivery, masking and identity cases below.
 *
 * Every case leaves SIGUSR1 ignored and unblocked when it finishes, so a
 * signal that is still in flight can never terminate a later case, and each
 * case installs the disposition and mask it needs before it starts.
 */
static unsigned long bit_of(int sig) { return 1UL << (unsigned)(sig - 1); }
static long mask_op(int how, const void *set, void *old) { return syscall(SYS_rt_sigprocmask, how, set, old, (size_t)8); }
static long mask_op_size(int how, const void *set, void *old, size_t size) { return syscall(SYS_rt_sigprocmask, how, set, old, size); }
static long pend_op(void *set, size_t size) { return syscall(SYS_rt_sigpending, set, size); }
static long susp_op(const void *set) { return syscall(SYS_rt_sigsuspend, set, (size_t)8); }
static long twait_op(const void *set, void *info, const void *ts) { return syscall(SYS_rt_sigtimedwait, set, info, ts, (size_t)8); }
static long queue_op(int pid, int signo, const void *info) { return syscall(SYS_rt_sigqueueinfo, pid, signo, info); }
static long kill_op(int pid, int signo) { return syscall(SYS_kill, pid, signo); }
static long tkill_op(int tid, int signo) { return syscall(SYS_tkill, tid, signo); }
static long tgkill_op(int tgid, int tid, int signo) { return syscall(SYS_tgkill, tgid, tid, signo); }
static long pause_op(void) { return syscall(SYS_pause); }
static long sleep_ms(long ms) { struct timespec ts = {.tv_sec = 0, .tv_nsec = ms * 1000000L}; return syscall(SYS_nanosleep, &ts, NULL); }
static pid_t self_pid(void) { return (pid_t)syscall(SYS_getpid); }
static pid_t self_tid(void) { return (pid_t)syscall(SYS_gettid); }

static unsigned long mask_now(void) { unsigned long mask = 0; check(mask_op(SIG_BLOCK, NULL, &mask) == 0, "mask-query"); return mask; }
static void mask_install(unsigned long mask) { check(mask_op(SIG_SETMASK, &mask, NULL) == 0, "mask-install"); }
static long mask_install_raw(unsigned long mask) { return mask_op(SIG_SETMASK, &mask, NULL); }

static void *shared_page(void)
{
    void *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    check(page != MAP_FAILED && page != NULL, "shared-page");
    memset(page, 0, 4096);
    return page;
}

static int reap(pid_t pid)
{
    int status = 0;
    check(pid > 0 && waitpid(pid, &status, 0) == pid, "reap");
    return status;
}

static volatile sig_atomic_t hits;
static volatile unsigned long handler_mask;
static volatile int handler_code, handler_pid, handler_uid, handler_value;
static void count_handler(int sig) { (void)sig; hits++; }
static void mask_probe_handler(int sig)
{
    unsigned long mask = 0;
    (void)sig;
    if (syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &mask, (size_t)8) == 0) handler_mask = mask;
    hits++;
}
static void info_handler(int sig, siginfo_t *info, void *context)
{
    (void)sig;
    (void)context;
    handler_code = info->si_code;
    handler_pid = info->si_pid;
    handler_uid = info->si_uid;
    handler_value = info->si_value.sival_int;
    hits++;
}

static void install_handler(int sig, void (*plain)(int), void (*info)(int, siginfo_t *, void *), int flags, int mask_sig)
{
    struct sigaction act;
    memset(&act, 0, sizeof act);
    if (info != NULL) { act.sa_sigaction = info; act.sa_flags = SA_SIGINFO | flags; }
    else { act.sa_handler = plain; act.sa_flags = flags; }
    sigemptyset(&act.sa_mask);
    if (mask_sig != 0) sigaddset(&act.sa_mask, mask_sig);
    check(sigaction(sig, &act, NULL) == 0, "install-handler");
}

/* Return one signal to a quiet, ignorable state: pending copies are dropped
 * when the ignored disposition is unblocked. */
static void quiet_signal(int sig)
{
    struct sigaction act;
    unsigned long mask = bit_of(sig);
    memset(&act, 0, sizeof act);
    act.sa_handler = SIG_IGN;
    check(sigaction(sig, &act, NULL) == 0, "quiet-action");
    check(mask_op(SIG_UNBLOCK, &mask, NULL) == 0, "quiet-unblock");
}

/* Enter rt_sigreturn(2) with RSP set to `sp`, so the frame the kernel reads is
 * the caller-controlled region at [sp - 8, sp - 8 + sizeof(struct rt_sigframe)).
 * Never returns: the frame is invalid, so the kernel raises SIGSEGV. */
static void __attribute__((noreturn)) sigreturn_at(void *sp)
{
    __asm__ volatile("mov %[sp], %%rsp\n\t"
                     "mov $15, %%eax\n\t"
                     "syscall\n\t"
                     "ud2\n\t"
                     :
                     : [sp] "r"(sp)
                     : "rax", "rcx", "r11", "memory");
    __builtin_unreachable();
}

/*
 * Per-thread delivery probe for tkill(2).
 *
 * The target thread blocks SIGUSR1 and reports what its own rt_sigpending
 * shows, while the main thread stays unblocked with the same handler
 * installed. A signal sent with tkill must be pending in the target alone and
 * must be handled there, never on the main thread.
 */
enum {
    TK_TID = 0,
    TK_PENDING,
    TK_PRIVATE,
    TK_HANDLED,
    TK_DECOY,
    TK_RELEASE,
    TK_OTHER_TID,
    TK_FAIL,
};
static volatile unsigned long *thread_report;
static void thread_probe_handler(int sig)
{
    (void)sig;
    if (thread_report == NULL) return;
    if ((pid_t)thread_report[TK_TID] == (pid_t)syscall(SYS_gettid)) thread_report[TK_HANDLED]++;
    else thread_report[TK_DECOY]++;
    thread_report[TK_OTHER_TID] = (unsigned long)(unsigned)syscall(SYS_gettid);
}
static void *tkill_target(void *arg)
{
    unsigned long set = bit_of(SIGUSR1);
    unsigned long seen = 0;
    (void)arg;
    if (mask_op(SIG_SETMASK, &set, NULL) != 0) { thread_report[TK_FAIL] = 1; return NULL; }
    thread_report[TK_TID] = (unsigned long)(unsigned)syscall(SYS_gettid);
    for (int i = 0; i < 2000; ++i) {
        if (pend_op(&seen, 8) == 0 && (seen & bit_of(SIGUSR1)) != 0) { thread_report[TK_PENDING] = 1; break; }
        sleep_ms(1);
    }
    for (int i = 0; i < 2000 && thread_report[TK_RELEASE] == 0; ++i) sleep_ms(1);
    if (mask_op(SIG_UNBLOCK, &set, NULL) != 0) { thread_report[TK_FAIL] = 2; return NULL; }
    sleep_ms(5);
    return NULL;
}

int main(void) {
    begin("rt_sigaction.raw-differential");
    struct action action = {.handler = (uintptr_t)SIG_IGN}, old = {0}, saved = {0};
    ERROR(sa(0, BAD, BAD, 0), EINVAL, "size-first"); mark("SIZE_BEFORE_COPY");
    int signals[] = {0, 65, SIGKILL, SIGSTOP};
    for (unsigned i=0; i<sizeof(signals)/sizeof(signals[0]); ++i)
        ERROR(sa(signals[i], BAD, NULL, 8), EFAULT, "copy-before-signo");
    mark("COPY_BEFORE_SIGNO");
    ERROR(sa(0, &action, NULL, 8), EINVAL, "invalid-signo");
    ERROR(sa(SIGKILL, &action, NULL, 8), EINVAL, "kill-replace");
    ERROR(sa(SIGSTOP, &action, NULL, 8), EINVAL, "stop-replace"); mark("INVALID_REPLACEMENT");
    check(sa(SIGKILL, NULL, &old, 8) == 0 && sa(SIGSTOP, NULL, &old, 8) == 0, "unmodifiable-query");
    mark("KILL_STOP_QUERY");
    check(sa(SIGUSR1, NULL, &saved, 8) == 0, "save");
    ERROR(sa(SIGUSR1, &action, BAD, 8), EFAULT, "old-fault");
    check(sa(SIGUSR1, NULL, &old, 8) == 0 && old.handler == (uintptr_t)SIG_IGN, "committed");
    check(sa(SIGUSR1, &saved, NULL, 8) == 0, "restore"); mark("COMMIT_BEFORE_OLD_COPY"); done();

    begin("sigaltstack.raw-differential");
    struct altstack initial = {0}, query = {0};
    check(alt(NULL, &initial) == 0, "initial");
    struct altstack one = {.sp=(uintptr_t)first, .size=sizeof(first)};
    struct altstack two = {.sp=(uintptr_t)second, .size=sizeof(second)};
    check(alt(&one, NULL) == 0, "setup");
    ERROR(alt(&two, BAD), EFAULT, "old-fault");
    check(alt(NULL, &query) == 0 && query.sp == two.sp && query.size == two.size, "committed"); mark("COMMIT_BEFORE_OLD_COPY");
    ERROR(alt(BAD, &query), EFAULT, "new-fault");
    check(alt(NULL, &query) == 0 && query.sp == two.sp, "new-fault-preserves"); mark("BAD_NEW_PRESERVES");
    one.flags=SS_ONSTACK;
    check(alt(&one, NULL) == 0 && alt(NULL, &query) == 0 && query.sp == one.sp && query.flags == 0, "onstack-input"); mark("ACCEPT_ONSTACK");
    struct altstack wrap = {.sp=UINTPTR_MAX-8, .size=65536};
    check(alt(&wrap, NULL) == 0 && alt(NULL, &query) == 0 && query.sp == wrap.sp && query.size == wrap.size, "wrap-stored"); mark("WRAPPING_GEOMETRY_STORED");
    struct altstack disable = {.sp=1, .flags=SS_DISABLE|AUTO, .size=1};
    check(alt(&disable, NULL) == 0 && alt(NULL, &query) == 0 && query.sp == 0 && query.size == 0 && query.flags == (SS_DISABLE|AUTO), "disable-auto"); mark("DISABLE_AUTODISARM");
    one.flags=0;
    check(alt(&one, NULL) == 0, "overlap-setup");
    struct altstack overlap=two;
    check(alt(&overlap, &overlap) == 0 && overlap.sp == one.sp && alt(NULL, &query) == 0 && query.sp == two.sp, "overlap"); mark("OVERLAPPING_INPUT_OUTPUT");
    struct altstack invalid = {.sp=(uintptr_t)first, .flags=SS_ONSTACK|SS_DISABLE, .size=sizeof(first)};
    ERROR(alt(&invalid, BAD), EINVAL, "flags-before-old"); mark("INVALID_FLAGS_BEFORE_OLD_COPY");
    check(alt(&initial, NULL) == 0, "restore-stack"); done();
    begin("rt_tgsigqueueinfo.raw-differential");
    ERROR(syscall(SYS_rt_tgsigqueueinfo, 0, 0, SIGUSR1, BAD), EFAULT, "copy-before-zero-ids");
    ERROR(syscall(SYS_rt_tgsigqueueinfo, -1, -1, SIGUSR1, BAD), EFAULT, "copy-before-negative-ids");
    mark("COPY_BEFORE_INVALID_IDS");
    siginfo_t info = {0};
    ERROR(syscall(SYS_rt_tgsigqueueinfo, 0, 0, SIGUSR1, &info), EINVAL, "ids-before-code");
    mark("INVALID_IDS_BEFORE_CODE");
    ERROR(syscall(SYS_rt_tgsigqueueinfo, 0, 0, 65, BAD), EFAULT, "copy-before-invalid-signo");
    mark("COPY_BEFORE_INVALID_SIGNO");
    done();
    begin("restart_syscall.raw-differential");
    ERROR(syscall(SYS_restart_syscall), EINTR, "no-restart-block");
    mark("NO_PENDING_BLOCK_EINTR");
    done();

    /*
     * rt_sigprocmask(2): the native set size is validated before either
     * pointer, the input copy precedes `how` decoding, SIGKILL/SIGSTOP are
     * dropped silently, the output copy carries the entry snapshot and a
     * faulting output copy happens after the new mask is committed.
     */
    begin("rt_sigprocmask.raw-differential");
    {
        unsigned long saved = mask_now();
        unsigned long set, old;
        size_t sizes[] = {0, 4, 9, 16, 4096};
        for (unsigned i = 0; i < sizeof(sizes) / sizeof(sizes[0]); ++i)
            ERROR(mask_op_size(SIG_BLOCK, BAD, BAD, sizes[i]), EINVAL, "size-first");
        mark("SIZE_BEFORE_POINTER");

        set = bit_of(SIGUSR1);
        ERROR(mask_op(99, BAD, NULL), EFAULT, "copy-before-how");
        ERROR(mask_op(99, &set, NULL), EINVAL, "how-invalid");
        mark("COPY_BEFORE_HOW");

        old = 0;
        check(mask_op(99, NULL, &old) == 0 && old == saved, "query-ignores-how");
        mark("QUERY_IGNORES_HOW");

        set = bit_of(SIGKILL) | bit_of(SIGSTOP) | bit_of(SIGUSR1);
        check(mask_op(SIG_SETMASK, &set, NULL) == 0 && mask_now() == bit_of(SIGUSR1), "kill-stop-drop");
        set = bit_of(SIGUSR2);
        check(mask_op(SIG_BLOCK, &set, NULL) == 0 && mask_now() == (bit_of(SIGUSR1) | bit_of(SIGUSR2)), "block");
        set = bit_of(SIGUSR1);
        check(mask_op(SIG_UNBLOCK, &set, NULL) == 0 && mask_now() == bit_of(SIGUSR2), "unblock");
        mark("KILL_STOP_DROPPED_AND_COMPOSED");

        set = bit_of(SIGUSR1);
        check(mask_op(SIG_SETMASK, &set, &set) == 0 && set == bit_of(SIGUSR2) && mask_now() == bit_of(SIGUSR1), "aliasing");
        mark("ALIASING_ENTRY_SNAPSHOT");

        set = bit_of(SIGUSR2);
        ERROR(mask_op(SIG_BLOCK, &set, BAD), EFAULT, "old-fault-commits");
        check((mask_now() & bit_of(SIGUSR2)) != 0, "old-fault-state");
        mark("OLD_FAULT_AFTER_COMMIT");

        mask_install(saved);
        done();
    }

    /*
     * rt_sigreturn(2): the frame the kernel reads is the handler frame it
     * published, so the mask in force inside a handler is
     * (entry mask | sa_mask | delivered signal) and the mask restored when the
     * handler returns is the entry mask. An invalid frame is fatal SIGSEGV.
     */
    begin("rt_sigreturn.raw-differential");
    {
        unsigned long saved = mask_now();
        void *page;
        pid_t child;
        int status;

        mask_install(0);
        install_handler(SIGUSR1, mask_probe_handler, NULL, 0, SIGUSR2);
        hits = 0;
        handler_mask = 0;
        mask_install(bit_of(SIGUSR1));
        check(kill_op(self_pid(), SIGUSR1) == 0, "arm");
        check(mask_op(SIG_UNBLOCK, &(unsigned long){bit_of(SIGUSR1)}, NULL) == 0, "unblock");
        check(hits == 1, "handler-entered");
        check(handler_mask == (bit_of(SIGUSR1) | bit_of(SIGUSR2)), "handler-mask");
        check(mask_now() == 0, "restored-mask");
        mark("FRAME_MASK_RESTORED");
        quiet_signal(SIGUSR1);
        mask_install(saved);

        page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        check(page != MAP_FAILED && page != NULL, "frame-page");
        memset(page, 0, 4096);
        child = fork();
        if (child == 0) {
            sigreturn_at((char *)page + 0x800);
            _exit(3);
        }
        check(child > 0, "sigreturn-fork");
        status = reap(child);
        check(WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV, "bad-frame-status");
        mark("BAD_FRAME_FATAL_SIGSEGV");
        done();
    }

    /*
     * pause(2): a caught signal completes the wait with -EINTR even for an
     * SA_RESTART handler, and a fatal signal terminates the waiter instead of
     * returning.
     */
    begin("pause.raw-differential");
    {
        unsigned long saved = mask_now();
        pid_t child;
        int status;
        long rc;
        int saved_errno;

        install_handler(SIGUSR1, count_handler, NULL, SA_RESTART, 0);
        hits = 0;
        mask_install(bit_of(SIGUSR1));
        child = fork();
        if (child == 0) {
            for (int i = 0; i < 400; ++i) {
                if (sleep_ms(5) != 0) _exit(4);
                if (kill_op((int)syscall(SYS_getppid), SIGUSR1) != 0) _exit(5);
            }
            _exit(0);
        }
        check(child > 0, "pause-fork");
        check(mask_op(SIG_UNBLOCK, &(unsigned long){bit_of(SIGUSR1)}, NULL) == 0, "pause-unblock");
        errno = 0;
        rc = pause_op();
        saved_errno = errno;
        check(kill_op(child, SIGKILL) == 0, "pause-stop-sender");
        reap(child);
        check(rc == -1 && saved_errno == EINTR && hits >= 1, "pause-eintr");
        mark("EINTR_AFTER_HANDLER");
        quiet_signal(SIGUSR1);

        child = fork();
        if (child == 0) {
            if (mask_install_raw(0) != 0) _exit(6);
            pause_op();
            syscall(SYS_exit, 7);
        }
        check(child > 0, "pause-fatal-fork");
        check(sleep_ms(50) == 0, "pause-fatal-sleep");
        check(kill_op(child, SIGTERM) == 0, "pause-fatal-kill");
        status = reap(child);
        check(WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM, "pause-fatal-status");
        mark("FATAL_SIGNAL_TERMINATES_WAITER");
        mask_install(saved);
        done();
    }

    /*
     * kill(2): INT_MIN is the one pid that cannot be negated, the target is
     * resolved before the signal number is validated, the null signal is a
     * permission and existence probe, SI_USER records the caller, and a
     * negative pid addresses a process group.
     */
    begin("kill.raw-differential");
    {
        siginfo_t info;
        struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
        unsigned long set = bit_of(SIGUSR1);
        volatile unsigned long *report;
        pid_t child;
        int status;

        ERROR(kill_op(INT_MIN, SIGUSR1), ESRCH, "int-min");
        mark("INT_MIN_ESRCH");

        ERROR(kill_op(INT_MAX, 65), ESRCH, "lookup-before-signo");
        ERROR(kill_op((int)self_pid(), 65), EINVAL, "signo-above-range");
        ERROR(kill_op((int)self_pid(), -1), EINVAL, "signo-negative");
        mark("LOOKUP_BEFORE_SIGNO");

        check(kill_op((int)self_pid(), 0) == 0, "null-self");
        check(kill_op(0, 0) == 0, "null-group");
        check(kill_op(-1, 0) == 0, "null-broadcast");
        ERROR(kill_op(INT_MAX, 0), ESRCH, "null-missing");
        ERROR(kill_op(-INT_MAX, 0), ESRCH, "null-group-missing");
        mark("NULL_SIGNAL_PROBES");

        memset(&info, 0, sizeof info);
        mask_install(bit_of(SIGUSR1));
        check(kill_op((int)self_pid(), SIGUSR1) == 0, "si-user-send");
        check(twait_op(&set, &info, &zero) == SIGUSR1, "si-user-wait");
        check(info.si_code == SI_USER && info.si_pid == (int)self_pid() && info.si_uid == getuid(), "si-user-record");
        mark("SI_USER_SENDER_RECORDED");
        quiet_signal(SIGUSR1);

        report = shared_page();
        hits = 0;
        handler_code = 0;
        handler_pid = 0;
        child = fork();
        if (child == 0) {
            struct sigaction act;
            if (setpgid(0, 0) != 0) _exit(11);
            memset(&act, 0, sizeof act);
            act.sa_sigaction = info_handler;
            act.sa_flags = SA_SIGINFO;
            sigemptyset(&act.sa_mask);
            if (sigaction(SIGUSR1, &act, NULL) != 0) _exit(12);
            if (mask_install_raw(0) != 0) _exit(13);
            report[0] = 1;
            for (int i = 0; i < 2000 && hits == 0; ++i) sleep_ms(1);
            report[1] = (unsigned long)hits;
            report[2] = (unsigned long)handler_code;
            report[3] = (unsigned long)(unsigned)handler_pid;
            _exit(hits == 0 ? 14 : 0);
        }
        check(child > 0, "group-fork");
        for (int i = 0; i < 2000 && report[0] == 0; ++i) check(sleep_ms(1) == 0, "group-ready-sleep");
        check(report[0] == 1, "group-child-ready");
        check(kill_op(-(int)child, 0) == 0, "group-probe");
        ERROR(kill_op(-INT_MAX, 0), ESRCH, "group-missing");
        ERROR(kill_op(-INT_MAX, 65), ESRCH, "group-lookup-before-signo");
        check(kill_op(-(int)child, SIGUSR1) == 0, "group-deliver");
        status = reap(child);
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "group-child-status");
        check(report[1] == 1 && report[2] == (unsigned long)SI_USER
              && report[3] == (unsigned long)(unsigned)self_pid(), "group-observed");
        mark("PROCESS_GROUP_DELIVERY");

        child = fork();
        if (child == 0) syscall(SYS_exit, 0);
        check(child > 0, "zombie-fork");
        check(waitpid(child, &status, 0) == child, "zombie-reap");
        ERROR(kill_op((int)child, 0), ESRCH, "reaped-missing");
        mark("REAPED_TARGET_ESRCH");
        mask_install(mask_now());
        done();
    }

    /*
     * rt_sigpending(2): a size above the native set is rejected before any
     * pointer is touched, a shorter size copies exactly that many bytes, and
     * only signals that are both pending and blocked are reported.
     */
    begin("rt_sigpending.raw-differential");
    {
        unsigned long saved = mask_now();
        unsigned long full = 0, after = 0;
        unsigned char buffer[16];

        ERROR(pend_op(BAD, 9), EINVAL, "size-nine");
        ERROR(pend_op(BAD, 16), EINVAL, "size-sixteen");
        ERROR(pend_op(BAD, 4096), EINVAL, "size-page");
        mark("SIZE_ABOVE_SET_EINVAL");

        check(pend_op(BAD, 0) == 0, "zero-size-no-copy");
        mark("ZERO_SIZE_NO_COPY");

        mask_install(bit_of(SIGUSR1));
        check(kill_op((int)self_pid(), SIGUSR1) == 0, "arm");
        check(pend_op(&full, 8) == 0, "full");
        memset(buffer, 0xAA, sizeof buffer);
        check(pend_op(buffer, 1) == 0 && buffer[0] == (unsigned char)(full & 0xff) && buffer[1] == 0xAA, "short-one");
        memset(buffer, 0xAA, sizeof buffer);
        check(pend_op(buffer, 2) == 0 && buffer[0] == (unsigned char)(full & 0xff)
              && buffer[1] == (unsigned char)(full >> 8) && buffer[2] == 0xAA, "short-two");
        mark("SHORT_SIZE_PARTIAL_COPY");

        check((full & bit_of(SIGUSR1)) != 0 && (full & bit_of(SIGUSR2)) == 0, "blocked-pending");
        mark("BLOCKED_PENDING_VISIBLE");

        ERROR(pend_op(BAD, 8), EFAULT, "fault");
        check(pend_op(&after, 8) == 0 && (after & bit_of(SIGUSR1)) != 0, "fault-preserves");
        mark("FAULT_PRESERVES_PENDING");

        install_handler(SIGUSR1, count_handler, NULL, 0, 0);
        hits = 0;
        check(mask_op(SIG_UNBLOCK, &(unsigned long){bit_of(SIGUSR1)}, NULL) == 0, "deliver");
        check(hits == 1 && pend_op(&after, 8) == 0 && (after & bit_of(SIGUSR1)) == 0, "delivered-clears");
        mark("DELIVERED_CLEARS_PENDING");
        quiet_signal(SIGUSR1);
        mask_install(saved);
        done();
    }

    /*
     * rt_sigtimedwait(2): the native set size is validated first, the timeout
     * is copied and validated before the wait, an empty selection with a zero
     * timeout is EAGAIN, a selected pending signal is dequeued with its
     * siginfo, and that dequeue precedes the output copy.
     */
    begin("rt_sigtimedwait.raw-differential");
    {
        unsigned long saved = mask_now();
        unsigned long empty = 0, set = bit_of(SIGUSR1), left = 0;
        struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
        struct timespec negative = {.tv_sec = -1, .tv_nsec = 0};
        struct timespec huge = {.tv_sec = 0, .tv_nsec = 1000000000L};
        struct timespec backwards = {.tv_sec = 0, .tv_nsec = -1};
        struct timespec short_wait = {.tv_sec = 0, .tv_nsec = 20000000L};
        siginfo_t info;
        size_t sizes[] = {0, 4, 9, 16};
        for (unsigned i = 0; i < sizeof(sizes) / sizeof(sizes[0]); ++i)
            ERROR(syscall(SYS_rt_sigtimedwait, BAD, BAD, BAD, sizes[i]), EINVAL, "size-first");
        mark("SIZE_BEFORE_COPY");

        ERROR(twait_op(BAD, NULL, NULL), EFAULT, "these-copy");
        ERROR(twait_op(&set, NULL, BAD), EFAULT, "timeout-copy");
        mark("COPY_BEFORE_TIMEOUT");

        ERROR(twait_op(&set, NULL, &negative), EINVAL, "timeout-negative");
        ERROR(twait_op(&set, NULL, &huge), EINVAL, "timeout-huge");
        ERROR(twait_op(&set, NULL, &backwards), EINVAL, "timeout-backwards");
        mark("INVALID_TIMEOUT_EINVAL");

        errno = 0;
        check(twait_op(&empty, NULL, &zero) == -1 && errno == EAGAIN, "empty-eagain");
        mark("EMPTY_SELECTION_EAGAIN");

        mask_install(bit_of(SIGUSR1));
        check(kill_op((int)self_pid(), SIGUSR1) == 0, "arm");
        memset(&info, 0, sizeof info);
        check(twait_op(&set, &info, &zero) == SIGUSR1, "dequeue");
        check(info.si_signo == SIGUSR1 && info.si_code == SI_USER && info.si_pid == (int)self_pid()
              && info.si_uid == getuid(), "dequeue-record");
        mark("DEQUEUE_PENDING_SIGINFO");

        check(kill_op((int)self_pid(), SIGUSR1) == 0, "arm-again");
        ERROR(twait_op(&set, BAD, &zero), EFAULT, "info-fault");
        check(pend_op(&left, 8) == 0 && (left & bit_of(SIGUSR1)) == 0, "info-fault-consumed");
        mark("INFO_FAULT_AFTER_DEQUEUE");

        errno = 0;
        check(twait_op(&set, NULL, &short_wait) == -1 && errno == EAGAIN, "timeout-eagain");
        mark("TIMEOUT_EXPIRES_EAGAIN");
        quiet_signal(SIGUSR1);
        mask_install(saved);
        done();
    }

    /*
     * rt_sigqueueinfo(2): the siginfo is copied before the target is resolved,
     * a non-negative si_code may not impersonate another sender, the null
     * signal probes existence, and the queued record reaches the target
     * verbatim.
     */
    begin("rt_sigqueueinfo.raw-differential");
    {
        unsigned long saved = mask_now();
        unsigned long set = bit_of(SIGUSR1);
        struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
        siginfo_t info, delivered;

        ERROR(queue_op(INT_MAX, 65, BAD), EFAULT, "copy-before-target");
        ERROR(queue_op(0, SIGUSR1, BAD), EFAULT, "copy-before-zero");
        mark("COPY_BEFORE_TARGET");

        memset(&info, 0, sizeof info);
        info.si_code = SI_USER;
        ERROR(queue_op(INT_MAX, SIGUSR1, &info), EPERM, "user-impersonation");
        info.si_code = SI_TKILL;
        ERROR(queue_op(INT_MAX, SIGUSR1, &info), EPERM, "tkill-impersonation");
        info.si_code = SI_QUEUE;
        ERROR(queue_op(INT_MAX, SIGUSR1, &info), ESRCH, "queue-lookup");
        ERROR(queue_op(INT_MAX, 65, &info), ESRCH, "lookup-before-signo");
        check(queue_op((int)self_pid(), 0, &info) == 0, "null-signal");
        mark("IMPERSONATION_BEFORE_LOOKUP");

        info.si_code = SI_QUEUE;
        info.si_pid = 4242;
        info.si_uid = 43;
        info.si_value.sival_int = 0x5a5a;
        mask_install(bit_of(SIGUSR1));
        check(queue_op((int)self_pid(), SIGUSR1, &info) == 0, "queue-self");
        memset(&delivered, 0, sizeof delivered);
        check(twait_op(&set, &delivered, &zero) == SIGUSR1, "queue-wait");
        check(delivered.si_code == SI_QUEUE && delivered.si_pid == 4242 && delivered.si_uid == 43
              && delivered.si_value.sival_int == 0x5a5a, "queue-verbatim");
        mark("QUEUED_RECORD_VERBATIM");

        ERROR(queue_op((int)self_pid(), 65, &info), EINVAL, "signo-above-range");
        ERROR(queue_op((int)self_pid(), -1, &info), EINVAL, "signo-negative");
        mark("INVALID_SIGNO_EINVAL");
        info.si_code = SI_USER;
        ERROR(queue_op(INT_MAX, 65, &info), EPERM, "impersonation-before-signo");
        mark("IMPERSONATION_BEFORE_SIGNO");
        quiet_signal(SIGUSR1);
        mask_install(saved);
        done();
    }

    /*
     * rt_sigsuspend(2): the set size is exact and the input is copied before
     * the wait, a signal that the replacement mask unblocks completes the wait
     * with -EINTR, and the caller's mask is restored when it returns.
     */
    begin("rt_sigsuspend.raw-differential");
    {
        unsigned long saved = mask_now();
        unsigned long empty = 0;
        long rc;
        int saved_errno;

        ERROR(syscall(SYS_rt_sigsuspend, BAD, 0), EINVAL, "size-zero");
        ERROR(syscall(SYS_rt_sigsuspend, BAD, 4), EINVAL, "size-four");
        ERROR(syscall(SYS_rt_sigsuspend, BAD, 16), EINVAL, "size-sixteen");
        ERROR(susp_op(BAD), EFAULT, "copy");
        mark("SIZE_AND_COPY_RULES");

        mask_install(bit_of(SIGUSR1));
        install_handler(SIGUSR1, mask_probe_handler, NULL, 0, SIGUSR2);
        hits = 0;
        handler_mask = 0;
        check(kill_op((int)self_pid(), SIGUSR1) == 0, "arm");
        errno = 0;
        rc = susp_op(&empty);
        saved_errno = errno;
        check(rc == -1 && saved_errno == EINTR, "sigsuspend-eintr");
        check(hits == 1 && handler_mask == (bit_of(SIGUSR1) | bit_of(SIGUSR2)), "sigsuspend-handler");
        mark("ATOMIC_SWAP_EINTR");
        check(mask_now() == bit_of(SIGUSR1), "sigsuspend-restore");
        mark("RESTORES_CALLER_MASK");
        quiet_signal(SIGUSR1);
        mask_install(saved);
        done();
    }

    /*
     * tkill(2): a non-positive tid and a signal outside the native range are
     * EINVAL, the target is resolved before the signal number is validated,
     * the null signal probes existence, and delivery is per-task: a signal
     * sent to a thread that blocks it stays private to that thread.
     */
    begin("tkill.raw-differential");
    {
        volatile unsigned long *report = shared_page();
        unsigned long main_set = 0;
        pthread_t thread;

        ERROR(tkill_op(0, SIGUSR1), EINVAL, "zero-tid");
        ERROR(tkill_op(-1, SIGUSR1), EINVAL, "negative-tid");
        ERROR(tkill_op(0, 65), EINVAL, "zero-tid-bad-signo");
        mark("NONPOSITIVE_TID_EINVAL");

        ERROR(tkill_op(INT_MAX, 65), ESRCH, "lookup-before-signo");
        ERROR(tkill_op((int)self_tid(), 65), EINVAL, "signo-above-range");
        ERROR(tkill_op((int)self_tid(), -1), EINVAL, "signo-negative");
        mark("LOOKUP_BEFORE_SIGNO");

        check(tkill_op((int)self_tid(), 0) == 0, "null-self");
        ERROR(tkill_op(INT_MAX, 0), ESRCH, "null-missing");
        mark("NULL_SIGNAL_PROBES");

        thread_report = report;
        install_handler(SIGUSR1, thread_probe_handler, NULL, 0, 0);
        mask_install(0);
        if (pthread_create(&thread, NULL, tkill_target, NULL) != 0) check(0, "thread-create");
        for (int i = 0; i < 2000 && report[TK_TID] == 0; ++i) check(sleep_ms(1) == 0, "thread-ready-sleep");
        check(report[TK_TID] != 0, "thread-ready");
        check(tkill_op((int)report[TK_TID], SIGUSR1) == 0, "targeted-send");
        for (int i = 0; i < 2000 && report[TK_PENDING] == 0; ++i) check(sleep_ms(1) == 0, "thread-pending-sleep");
        check(report[TK_PENDING] == 1, "thread-pending");
        check(pend_op(&main_set, 8) == 0 && (main_set & bit_of(SIGUSR1)) == 0, "thread-private");
        report[TK_PRIVATE] = 1;
        report[TK_RELEASE] = 1;
        check(pthread_join(thread, NULL) == 0, "thread-join");
        check(report[TK_FAIL] == 0 && report[TK_HANDLED] == 1 && report[TK_DECOY] == 0
              && report[TK_OTHER_TID] == report[TK_TID], "thread-handled");
        mark("THREAD_PRIVATE_DELIVERY");
        quiet_signal(SIGUSR1);
        thread_report = NULL;
        done();
    }

    /*
     * tgkill(2): both ids must be positive, the thread id must belong to the
     * thread group, the target lookup precedes signal validation, and the
     * delivered record carries SI_TKILL.
     */
    begin("tgkill.raw-differential");
    {
        unsigned long saved = mask_now();
        siginfo_t info;
        struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
        unsigned long set = bit_of(SIGUSR1);
        pid_t child;

        ERROR(tgkill_op(0, 0, SIGUSR1), EINVAL, "zero-both");
        ERROR(tgkill_op((int)self_pid(), 0, SIGUSR1), EINVAL, "zero-tid");
        ERROR(tgkill_op(0, (int)self_tid(), SIGUSR1), EINVAL, "zero-tgid");
        ERROR(tgkill_op(-1, -1, 65), EINVAL, "negative-both");
        mark("NONPOSITIVE_IDS_EINVAL");

        ERROR(tgkill_op((int)self_pid(), INT_MAX, 65), ESRCH, "lookup-before-signo");
        ERROR(tgkill_op(INT_MAX, (int)self_tid(), 65), ESRCH, "lookup-tgid-before-signo");
        ERROR(tgkill_op((int)self_pid(), (int)self_tid(), 65), EINVAL, "signo-above-range");
        mark("LOOKUP_BEFORE_SIGNO");

        child = fork();
        if (child == 0) {
            pause_op();
            syscall(SYS_exit, 8);
        }
        check(child > 0, "tgkill-fork");
        check(sleep_ms(30) == 0, "tgkill-sleep");
        ERROR(tgkill_op((int)self_pid(), (int)child, 0), ESRCH, "tgid-mismatch");
        ERROR(tgkill_op((int)child, (int)self_tid(), 0), ESRCH, "tid-mismatch");
        ERROR(tgkill_op((int)self_pid(), INT_MAX, 0), ESRCH, "missing-thread");
        check(kill_op(child, SIGKILL) == 0, "tgkill-cleanup");
        reap(child);
        mark("TGID_MUST_MATCH_TARGET");

        check(tgkill_op((int)self_pid(), (int)self_tid(), 0) == 0, "null-self");
        mark("NULL_SIGNAL_PROBE");

        memset(&info, 0, sizeof info);
        mask_install(bit_of(SIGUSR1));
        check(tgkill_op((int)self_pid(), (int)self_tid(), SIGUSR1) == 0, "send");
        check(twait_op(&set, &info, &zero) == SIGUSR1, "wait");
        check(info.si_code == SI_TKILL && info.si_pid == (int)self_pid() && info.si_uid == getuid(), "tkill-record");
        mark("SI_TKILL_RECORD");
        quiet_signal(SIGUSR1);
        mask_install(saved);
        done();
    }

    puts("THEKERNEL_SIGNAL_BOUNDARY_PASS");
    return 0;
}
