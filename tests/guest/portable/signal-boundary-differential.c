#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
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
    puts("THEKERNEL_SIGNAL_BOUNDARY_PASS");
    return 0;
}
