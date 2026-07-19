#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_seccomp
#if defined(__x86_64__)
#define SYS_seccomp 317
#else
#define SYS_seccomp 277
#endif
#endif

#ifndef SECCOMP_RET_KILL_PROCESS
#define SECCOMP_RET_KILL_PROCESS 0x80000000U
#endif
#ifndef SECCOMP_RET_KILL_THREAD
#define SECCOMP_RET_KILL_THREAD 0x00000000U
#endif
#ifndef SECCOMP_RET_TRAP
#define SECCOMP_RET_TRAP 0x00030000U
#endif
#ifndef SECCOMP_RET_ERRNO
#define SECCOMP_RET_ERRNO 0x00050000U
#endif
#ifndef SECCOMP_RET_LOG
#define SECCOMP_RET_LOG 0x7ffc0000U
#endif
#ifndef SECCOMP_RET_TRACE
#define SECCOMP_RET_TRACE 0x7ff00000U
#endif
#ifndef SECCOMP_RET_USER_NOTIF
#define SECCOMP_RET_USER_NOTIF 0x7fc00000U
#endif
#ifndef SECCOMP_RET_ALLOW
#define SECCOMP_RET_ALLOW 0x7fff0000U
#endif
#ifndef SECCOMP_RET_DATA
#define SECCOMP_RET_DATA 0x0000ffffU
#endif

#ifndef SECCOMP_SET_MODE_STRICT
#define SECCOMP_SET_MODE_STRICT 0U
#endif
#ifndef SECCOMP_SET_MODE_FILTER
#define SECCOMP_SET_MODE_FILTER 1U
#endif
#ifndef SECCOMP_GET_ACTION_AVAIL
#define SECCOMP_GET_ACTION_AVAIL 2U
#endif
#ifndef SECCOMP_GET_NOTIF_SIZES
#define SECCOMP_GET_NOTIF_SIZES 3U
#endif
#ifndef SECCOMP_FILTER_FLAG_TSYNC
#define SECCOMP_FILTER_FLAG_TSYNC 1U
#endif
#ifndef SECCOMP_MODE_FILTER
#define SECCOMP_MODE_FILTER 2U
#endif
#ifndef SYS_SECCOMP
#define SYS_SECCOMP 1
#endif

#if defined(__riscv) && __riscv_xlen == 64
#define EXPECTED_AUDIT_ARCH 0xc00000f3U
#elif defined(__loongarch_lp64)
#define EXPECTED_AUDIT_ARCH 0xc0000102U
#elif defined(__x86_64__)
#define EXPECTED_AUDIT_ARCH 0xc000003eU
#else
#error unsupported seccomp smoke-test architecture
#endif

#define UNKNOWN_SYSCALL_NR 0x3fffffffL
#define TRAP_ARGUMENT_SENTINEL 0x13579L
#define FULL_FILTER_LENGTH 4096U
#define FINAL_FILTER_LENGTH 4036U

#if defined(__x86_64__)
#define EXPECTED_TRAP_ROLLBACK SYS_getppid
#else
#define EXPECTED_TRAP_ROLLBACK TRAP_ARGUMENT_SENTINEL
#endif

static const char *self_path;
static int require_exact_path_limit;
static volatile sig_atomic_t trap_seen;
static volatile sig_atomic_t trap_valid;
static _Atomic pid_t kill_scope_tid;
static _Atomic int kill_scope_returned;

static int status_seccomp_fields_are_exact(unsigned int expected_filters);
static int current_thread_seccomp_fields_are_exact(
    unsigned int expected_filters);

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_SECCOMP_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_SECCOMP_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static void marker(const char *value) {
    puts(value);
    fflush(stdout);
}

static int set_no_new_privs(void) {
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) {
        return fail("set-no-new-privs");
    }
    if (prctl(PR_GET_NO_NEW_PRIVS, 0UL, 0UL, 0UL, 0UL) != 1) {
        return fail("get-no-new-privs");
    }
    return 0;
}

static long install_program_raw(struct sock_filter *instructions,
                                unsigned short length) {
    struct sock_fprog program = {
        .len = length,
        .filter = instructions,
    };
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program);
}

static int install_program(struct sock_filter *instructions,
                           unsigned short length) {
    if (install_program_raw(instructions, length) != 0) {
        return fail("install-filter");
    }
    return 0;
}

static int install_action(long syscall_number, uint32_t action) {
    struct sock_filter instructions[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, EXPECTED_AUDIT_ARCH, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (uint32_t)syscall_number, 0,
                 1),
        BPF_STMT(BPF_RET | BPF_K, action),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    return install_program(instructions,
                           (unsigned short)(sizeof(instructions) /
                                            sizeof(instructions[0])));
}

static int expect_errno_syscall(long number, int expected_errno) {
    errno = 0;
    long result = syscall(number, 0UL, 0UL, 0UL, 0UL, 0UL, 0UL);
    if (result != -1 || errno != expected_errno) {
        return fail_value("unexpected-syscall-errno", result, -1);
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

static int test_api(void) {
    if (prctl(PR_GET_SECCOMP, 0x11UL, 0x22UL, 0x33UL, 0x44UL) != 0) {
        return fail("pr-get-seccomp-disabled");
    }
    errno = 0;
    if (prctl(PR_SET_SECCOMP,
              (1UL << 32) | (unsigned long)SECCOMP_MODE_FILTER, 0UL, 0UL,
              0UL) != -1 ||
        errno != EINVAL) {
        return fail("pr-set-seccomp-high-mode-bits");
    }
    errno = 0;
    if (prctl(PR_SET_SECCOMP, 1UL << 32, 0UL, 0UL, 0UL) != -1 ||
        errno != EINVAL) {
        return fail("pr-set-seccomp-high-strict-mode-bits");
    }

    const uint32_t actions[] = {
        SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD,
        SECCOMP_RET_TRAP,         SECCOMP_RET_ERRNO,
        SECCOMP_RET_LOG,          SECCOMP_RET_ALLOW,
    };
    for (size_t index = 0; index < sizeof(actions) / sizeof(actions[0]);
         ++index) {
        uint32_t action = actions[index];
        if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0U, &action) !=
            0) {
            return fail("get-action-available");
        }
    }

    unsigned char unaligned_query[sizeof(uint32_t) + 1] = {0};
    uint32_t unaligned_action = SECCOMP_RET_ALLOW;
    memcpy(unaligned_query + 1, &unaligned_action, sizeof(unaligned_action));
    if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0U,
                unaligned_query + 1) != 0) {
        return fail("get-action-unaligned");
    }

    uint32_t unknown = 0x12340000U;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0U, &unknown) != -1 ||
        errno != EOPNOTSUPP) {
        return fail("get-action-unknown");
    }
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 1U,
                (void *)(uintptr_t)1) != -1 ||
        errno != EINVAL) {
        return fail("get-action-flags-precedence");
    }

    marker("THEKERNEL_SECCOMP_API_OK");
    return 0;
}

static int test_filter_error_order(void) {
    if (set_no_new_privs()) {
        return 1;
    }

    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, NULL) != -1 ||
        errno != EFAULT) {
        return fail("null-fprog");
    }

    struct sock_fprog program = {
        .len = 0,
        .filter = NULL,
    };
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program) != -1 ||
        errno != EINVAL) {
        return fail("empty-filter");
    }
    program.len = 4097;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program) != -1 ||
        errno != EINVAL) {
        return fail("oversize-filter");
    }
    program.len = 1;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program) != -1 ||
        errno != EINVAL) {
        return fail("null-filter-pointer");
    }
    program.filter = (struct sock_filter *)(uintptr_t)1;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program) != -1 ||
        errno != EFAULT) {
        return fail("invalid-filter-pointer");
    }

    marker("THEKERNEL_SECCOMP_FILTER_ERRORS_OK");
    return 0;
}

static int test_unaligned_filter(void) {
    if (set_no_new_privs()) {
        return 1;
    }

    struct sock_filter allow =
        (struct sock_filter)BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    unsigned char instruction_bytes[sizeof(allow) + 1] = {0};
    memcpy(instruction_bytes + 1, &allow, sizeof(allow));

    struct sock_fprog program = {
        .len = 1,
        .filter = (struct sock_filter *)(instruction_bytes + 1),
    };
    unsigned char header_bytes[sizeof(program) + 1] = {0};
    memcpy(header_bytes + 1, &program, sizeof(program));
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U,
                header_bytes + 1) != 0) {
        return fail("unaligned-filter-install");
    }

    marker("THEKERNEL_SECCOMP_UNALIGNED_OK");
    return 0;
}

static int test_filter_fast_paths(void) {
    if (set_no_new_privs() ||
        install_action(SYS_getpid, SECCOMP_RET_ERRNO | EACCES)) {
        return 1;
    }
    if (expect_errno_syscall(SYS_getpid, EACCES)) {
        return 1;
    }

    if (install_action(SYS_clock_gettime, SECCOMP_RET_ERRNO | EPERM)) {
        return 1;
    }
    struct timespec time_value;
    errno = 0;
    if (syscall(SYS_clock_gettime, 0, &time_value) != -1 || errno != EPERM) {
        return fail("clock-gettime-fast-path");
    }

    if (install_action(UNKNOWN_SYSCALL_NR, SECCOMP_RET_ERRNO | EBUSY) ||
        expect_errno_syscall(UNKNOWN_SYSCALL_NR, EBUSY)) {
        return 1;
    }

    marker("THEKERNEL_SECCOMP_FILTER_OK");
    marker("THEKERNEL_SECCOMP_ERRNO_OK");
    marker("THEKERNEL_SECCOMP_FASTPATH_OK");
    marker("THEKERNEL_SECCOMP_UNKNOWN_OK");
    return 0;
}

static int test_errno_zero(void) {
    if (set_no_new_privs() ||
        install_action(SYS_getpid, SECCOMP_RET_ERRNO)) {
        return 1;
    }
    errno = 0;
    long result = syscall(SYS_getpid);
    if (result != 0 || errno != 0) {
        return fail_value("errno-zero", result, 0);
    }
    marker("THEKERNEL_SECCOMP_ERRNO_ZERO_OK");
    return 0;
}

static int test_log_allows(void) {
    if (set_no_new_privs() ||
        install_action(SYS_getpid, SECCOMP_RET_LOG)) {
        return 1;
    }
    errno = 0;
    if (syscall(SYS_getpid) <= 0 || errno != 0) {
        return fail("log-action-allow");
    }
    marker("THEKERNEL_SECCOMP_LOG_OK");
    return 0;
}

static void trap_handler(int signo, siginfo_t *info, void *context) {
    (void)context;
    trap_seen = 1;
    trap_valid = signo == SIGSYS && info != NULL &&
                 info->si_code == SYS_SECCOMP && info->si_errno == 0x1234 &&
                 info->si_syscall == SYS_getppid &&
                 info->si_arch == EXPECTED_AUDIT_ARCH &&
                 info->si_call_addr != NULL;
}

static int test_trap(void) {
    struct sigaction action = {
        .sa_sigaction = trap_handler,
        .sa_flags = SA_SIGINFO,
    };
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGSYS, &action, NULL) != 0 || set_no_new_privs() ||
        install_action(SYS_getppid, SECCOMP_RET_TRAP | 0x1234U)) {
        return fail("trap-setup");
    }

    errno = 0;
    long result = syscall(SYS_getppid, TRAP_ARGUMENT_SENTINEL);
    /* Linux rolls the syscall frame back before SIGSYS. RV64/LoongArch64
     * expose the original a0 while x86_64 exposes the original syscall number
     * after the handler returns; either result proves getppid was skipped. */
    if (!trap_seen || !trap_valid) {
        return fail("trap-payload");
    }
    if (result != EXPECTED_TRAP_ROLLBACK) {
        return fail_value("trap-frame-rollback", result,
                          EXPECTED_TRAP_ROLLBACK);
    }
    marker("THEKERNEL_SECCOMP_TRAP_OK");
    marker("THEKERNEL_SECCOMP_TRAP_ROLLBACK_OK");
    return 0;
}

static void *inherited_thread(void *unused) {
    (void)unused;
    errno = 0;
    long result = syscall(SYS_getpid);
    if (result != -1 || errno != EACCES ||
        install_action(SYS_getppid, SECCOMP_RET_ERRNO | EPERM) ||
        expect_errno_syscall(SYS_getppid, EPERM) ||
        current_thread_seccomp_fields_are_exact(2)) {
        return (void *)(uintptr_t)1;
    }
    return NULL;
}

static int seccomp_fields_at_path_are_exact(const char *path,
                                            unsigned int expected_filters) {
    char buffer[4096];
    char filters_field[64];
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return fail("proc-status-open");
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (count <= 0) {
        return fail("proc-status-read");
    }
    buffer[count] = '\0';
    int field_length = snprintf(filters_field, sizeof(filters_field),
                                "Seccomp_filters:\t%u\n", expected_filters);
    if (field_length <= 0 || (size_t)field_length >= sizeof(filters_field)) {
        errno = EOVERFLOW;
        return fail("proc-status-seccomp-format");
    }
    if (strstr(buffer, "Seccomp:\t2\n") == NULL ||
        strstr(buffer, filters_field) == NULL) {
        errno = EPROTO;
        return fail("proc-status-seccomp-fields");
    }
    return 0;
}

static int status_seccomp_fields_are_exact(unsigned int expected_filters) {
    return seccomp_fields_at_path_are_exact("/proc/self/status",
                                            expected_filters);
}

static int current_thread_seccomp_fields_are_exact(
    unsigned int expected_filters) {
    char path[96];
    long tid = syscall(SYS_gettid);
    int path_length = snprintf(path, sizeof(path),
                               "/proc/self/task/%ld/status", tid);
    if (tid <= 0 || path_length <= 0 ||
        (size_t)path_length >= sizeof(path)) {
        errno = EOVERFLOW;
        return fail("proc-thread-status-path");
    }
    return seccomp_fields_at_path_are_exact(path, expected_filters);
}

static int test_inheritance(void) {
    if (set_no_new_privs() ||
        install_action(SYS_getpid, SECCOMP_RET_ERRNO | EACCES)) {
        return 1;
    }

    pthread_t thread;
    if (pthread_create(&thread, NULL, inherited_thread, NULL) != 0) {
        return fail("pthread-create");
    }
    void *thread_result = NULL;
    if (pthread_join(thread, &thread_result) != 0 || thread_result != NULL) {
        errno = EPROTO;
        return fail("pthread-inherit");
    }
    errno = 0;
    if (syscall(SYS_getppid) <= 0 || errno != 0) {
        return fail("thread-append-isolation");
    }
    marker("THEKERNEL_SECCOMP_THREAD_APPEND_ISOLATION_OK");

    pid_t child = fork();
    if (child < 0) {
        return fail("inherit-fork");
    }
    if (child == 0) {
        if (expect_errno_syscall(SYS_getpid, EACCES) ||
            install_action(SYS_getppid, SECCOMP_RET_ERRNO | EPERM) ||
            expect_errno_syscall(SYS_getppid, EPERM) ||
            status_seccomp_fields_are_exact(2)) {
            _exit(1);
        }
        _exit(0);
    }
    if (wait_for_exit(child, "inherit-child") != 0) {
        return 1;
    }
    errno = 0;
    if (syscall(SYS_getppid) <= 0) {
        return fail("child-append-isolation");
    }
    marker("THEKERNEL_SECCOMP_FORK_APPEND_ISOLATION_OK");

    if (prctl(PR_GET_SECCOMP, 1UL, 2UL, 3UL, 4UL) !=
            SECCOMP_MODE_FILTER ||
        status_seccomp_fields_are_exact(1)) {
        return fail("proc-seccomp-mode");
    }

    marker("THEKERNEL_SECCOMP_INHERIT_OK");
    marker("THEKERNEL_SECCOMP_PROC_OK");
    return 0;
}

static int test_exec_persistence(void) {
    if (set_no_new_privs() ||
        install_action(SYS_getpid, SECCOMP_RET_ERRNO | EACCES)) {
        return 1;
    }
    char *const arguments[] = {(char *)self_path, (char *)"--exec-probe",
                               NULL};
    execv(self_path, arguments);
    return fail("exec-persistence-execv");
}

static int exec_probe(void) {
    if (expect_errno_syscall(SYS_getpid, EACCES) ||
        prctl(PR_GET_SECCOMP, 0UL, 0UL, 0UL, 0UL) !=
            SECCOMP_MODE_FILTER) {
        return fail("exec-persistence-state");
    }
    marker("THEKERNEL_SECCOMP_EXEC_OK");
    return 0;
}

static int test_strict(void) {
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 1U, NULL) != -1 ||
        errno != EINVAL) {
        return fail("strict-nonzero-flags");
    }
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0U,
                (void *)(uintptr_t)1) != -1 ||
        errno != EINVAL) {
        return fail("strict-nonnull-uargs");
    }
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0U, NULL) != 0) {
        return fail("strict-install");
    }
    static const char value[] = "THEKERNEL_SECCOMP_STRICT_OK\n";
    if (syscall(SYS_write, STDOUT_FILENO, value, sizeof(value) - 1) !=
        (long)(sizeof(value) - 1)) {
        syscall(SYS_exit, 2);
    }
    syscall(SYS_exit, 0);
    return 3;
}

static int test_strict_kill(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("strict-kill-fork");
    }
    if (child == 0) {
        if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0U, NULL) != 0) {
            _exit(2);
        }
        (void)syscall(SYS_getpid);
        _exit(3);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFSIGNALED(status) ||
        WTERMSIG(status) != SIGKILL) {
        errno = EPROTO;
        return fail("strict-kill-status");
    }
    marker("THEKERNEL_SECCOMP_STRICT_KILL_OK");
    return 0;
}

static int test_unsupported_lifecycles(void) {
    if (!require_exact_path_limit) {
        marker("THEKERNEL_SECCOMP_UNSUPPORTED_PORTABLE_OK");
        return 0;
    }

    uint32_t action = SECCOMP_RET_TRACE;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0U, &action) != -1 ||
        errno != EOPNOTSUPP) {
        return fail("unsupported-trace-query");
    }
    action = SECCOMP_RET_USER_NOTIF;
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0U, &action) != -1 ||
        errno != EOPNOTSUPP) {
        return fail("unsupported-user-notif-query");
    }
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 0U, NULL) != -1 ||
        errno != EOPNOTSUPP) {
        return fail("unsupported-notif-sizes");
    }
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 1U,
                (void *)(uintptr_t)1) != -1 ||
        errno != EINVAL) {
        return fail("unsupported-notif-flags-precedence");
    }
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER,
                SECCOMP_FILTER_FLAG_TSYNC, (void *)(uintptr_t)1) != -1 ||
        errno != EINVAL) {
        return fail("unsupported-filter-flags-precedence");
    }

    if (set_no_new_privs() ||
        install_action(SYS_getpid, SECCOMP_RET_TRACE | 0x1234U)) {
        return 1;
    }
    errno = 0;
    if (syscall(SYS_getpid) != -1 || errno != ENOSYS) {
        return fail("trace-without-owner");
    }
    if (install_action(SYS_getppid, SECCOMP_RET_USER_NOTIF | 0x5678U)) {
        return 1;
    }
    errno = 0;
    if (syscall(SYS_getppid) != -1 || errno != ENOSYS) {
        return fail("user-notif-without-owner");
    }
    marker("THEKERNEL_SECCOMP_UNSUPPORTED_OK");
    return 0;
}


static int test_prctl_strict_ignores_filter(void) {
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT, 1UL, 0UL, 0UL) != 0) {
        return fail("prctl-strict-install");
    }
    static const char value[] = "THEKERNEL_SECCOMP_PRCTL_STRICT_OK\n";
    if (syscall(SYS_write, STDOUT_FILENO, value, sizeof(value) - 1) !=
        (long)(sizeof(value) - 1)) {
        syscall(SYS_exit, 2);
    }
    syscall(SYS_exit, 0);
    return 3;
}

static void *kill_scope_worker(void *should_die) {
    if ((uintptr_t)should_die != 0) {
        pid_t tid = (pid_t)syscall(SYS_gettid);
        atomic_store_explicit(&kill_scope_tid, tid, memory_order_release);
        (void)syscall(SYS_getpid);
        atomic_store_explicit(&kill_scope_returned, 1,
                              memory_order_release);
        return (void *)(uintptr_t)0xbad;
    }
    return (void *)(uintptr_t)0x600d;
}

static int wait_for_seccomp_killed_tid(pid_t tgid, uint32_t action) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
    for (unsigned int attempt = 0; attempt < 5000; ++attempt) {
        pid_t tid = atomic_load_explicit(&kill_scope_tid,
                                         memory_order_acquire);
        if (tid > 0) {
            errno = 0;
            long result = syscall(SYS_tgkill, tgid, tid, 0);
            if (result == -1 && errno == ESRCH) {
                return action == SECCOMP_RET_KILL_THREAD ? 0 : -1;
            }
            if (result == -1 && errno != ESRCH) {
                return -1;
            }
        }
        if (atomic_load_explicit(&kill_scope_returned,
                                 memory_order_acquire) != 0) {
            return -1;
        }
        (void)nanosleep(&delay, NULL);
    }
    return -1;
}

static void run_kill_scope_child(uint32_t action,
                                 int append_kill_thread_filter) {
    pid_t tgid = (pid_t)syscall(SYS_getpid);
    if (tgid <= 0) {
        _exit(1);
    }
    atomic_store_explicit(&kill_scope_tid, 0, memory_order_relaxed);
    atomic_store_explicit(&kill_scope_returned, 0, memory_order_relaxed);
    if (set_no_new_privs() || install_action(SYS_getpid, action)) {
        _exit(2);
    }
    /* Match Linux's selftest: a later KILL_THREAD filter must not downgrade
     * an already-installed KILL_PROCESS decision for the same syscall. */
    if (append_kill_thread_filter &&
        install_action(SYS_getpid, SECCOMP_RET_KILL_THREAD)) {
        _exit(3);
    }
    pthread_t thread;
    void *thread_result = NULL;
    if (pthread_create(&thread, NULL, kill_scope_worker, NULL) != 0 ||
        pthread_join(thread, &thread_result) != 0 ||
        thread_result != (void *)(uintptr_t)0x600d) {
        _exit(4);
    }
    if (pthread_create(&thread, NULL, kill_scope_worker,
                       (void *)(uintptr_t)1) != 0) {
        _exit(5);
    }
    /* A seccomp kernel kill bypasses libc's pthread-exit bookkeeping.  musl
     * therefore cannot safely join this pthread.  Observe the Linux-visible
     * task identity instead: the worker publishes its TID before the filtered
     * syscall, and tgkill(..., 0) must eventually report ESRCH.  The ordinary
     * worker above remains the control for libc join and clear-child-tid. */
    if (wait_for_seccomp_killed_tid(tgid, action) != 0) {
        _exit(6);
    }
    /* Only KILL_THREAD may leave this spawner alive. */
    _exit(42);
}

static int wait_for_kill_scope(pid_t child, uint32_t action,
                               const char *stage) {
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail(stage);
    }
    if (action == SECCOMP_RET_KILL_THREAD) {
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 42) {
            errno = EPROTO;
            return fail(stage);
        }
    } else if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSYS) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

static int run_kill_scope_case(uint32_t action,
                               int append_kill_thread_filter,
                               const char *stage) {
    pid_t child = fork();
    if (child < 0) {
        return fail(stage);
    }
    if (child == 0) {
        run_kill_scope_child(action, append_kill_thread_filter);
    }
    return wait_for_kill_scope(child, action, stage);
}

static int test_kill_scope(void) {
    if (run_kill_scope_case(SECCOMP_RET_KILL_THREAD, 0,
                            "kill-thread-scope")) {
        return 1;
    }
    marker("THEKERNEL_SECCOMP_KILL_THREAD_OK");

    if (run_kill_scope_case(SECCOMP_RET_KILL_PROCESS, 1,
                            "kill-process-scope")) {
        return 1;
    }
    marker("THEKERNEL_SECCOMP_KILL_PROCESS_OK");

    if (run_kill_scope_case(0xaaaa0000U, 0, "kill-unknown-scope")) {
        return 1;
    }
    marker("THEKERNEL_SECCOMP_KILL_UNKNOWN_OK");
    marker("THEKERNEL_SECCOMP_KILL_SCOPE_OK");
    return 0;
}

static int test_resource_boundary_impl(int emit_marker) {
    if (set_no_new_privs()) {
        return 1;
    }

    struct sock_filter *program =
        calloc(FULL_FILTER_LENGTH, sizeof(*program));
    if (program == NULL) {
        return fail("path-program-allocation");
    }
    for (unsigned int index = 0; index + 1 < FULL_FILTER_LENGTH; ++index) {
        program[index] = (struct sock_filter)BPF_STMT(BPF_LD | BPF_IMM, 0);
    }
    program[FULL_FILTER_LENGTH - 1] =
        (struct sock_filter)BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);

    /* Linux v6.12 charges the unblinded cBPF-to-eBPF execution length, not
     * the source length: this shape costs source_len + 4 (three prologue
     * instructions and RET_K becoming MOV+EXIT). Seven 4096-source filters
     * plus six existing-ancestor penalties cost 28724. The eighth filter has
     * source length 4036, execution charge 4040, and adds the seventh four-unit
     * ancestor penalty: 28724 + 4040 + 4 == the exact 32768 path limit. */
    for (unsigned int count = 0; count < 7; ++count) {
        errno = 0;
        if (install_program_raw(program, FULL_FILTER_LENGTH) != 0) {
            if (!require_exact_path_limit && errno == ENOMEM) {
                free(program);
                if (emit_marker) {
                    marker("THEKERNEL_SECCOMP_RESOURCE_PORTABLE_OK");
                }
                return 0;
            }
            free(program);
            return fail_value("full-path-install", count, 7);
        }
    }
    program[FINAL_FILTER_LENGTH - 1] =
        (struct sock_filter)BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    errno = 0;
    if (install_program_raw(program, FINAL_FILTER_LENGTH) != 0) {
        if (!require_exact_path_limit && errno == ENOMEM) {
            free(program);
            if (emit_marker) {
                marker("THEKERNEL_SECCOMP_RESOURCE_PORTABLE_OK");
            }
            return 0;
        }
        free(program);
        return fail("final-path-install");
    }

    struct sock_fprog overflow = {
        .len = 1,
        .filter = &program[FULL_FILTER_LENGTH - 1],
    };
    errno = 0;
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &overflow) != -1 ||
        errno != ENOMEM) {
        free(program);
        return fail("path-limit-overflow");
    }
    if (require_exact_path_limit &&
        (status_seccomp_fields_are_exact(8) || syscall(SYS_getpid) <= 0)) {
        free(program);
        return fail("path-limit-rollback");
    }
    free(program);
    if (emit_marker) {
        if (require_exact_path_limit) {
            marker("THEKERNEL_SECCOMP_RESOURCE_ROLLBACK_OK");
        }
        marker(require_exact_path_limit
                   ? "THEKERNEL_SECCOMP_RESOURCE_OK"
                   : "THEKERNEL_SECCOMP_RESOURCE_PORTABLE_OK");
    }
    return 0;
}

static int test_resource_boundary(void) {
    return test_resource_boundary_impl(1);
}

static int test_resource_release(void) {
    if (!require_exact_path_limit) {
        marker("THEKERNEL_SECCOMP_RESOURCE_RELEASE_PORTABLE_OK");
        return 0;
    }

    /* One exact-limit chain retains roughly 256 KiB of source programs. If
     * task exit leaves its immutable leaf attached to a scheduler Arc, 72
     * children exceed the fixed 16 MiB live-filter budget. The parent opens
     * each live child's proc status file before allowing exit and retains all
     * those FDs, deterministically pinning the old Task object past waitpid.
     * Success therefore proves explicit exit retirement, not prompt GC. */
    enum { RETAINED_TASKS = 72 };
    int retained_status[RETAINED_TASKS];
    unsigned int retained_count = 0;
    for (unsigned int iteration = 0; iteration < RETAINED_TASKS; ++iteration) {
        int ready_pipe[2];
        int exit_pipe[2];
        if (pipe(ready_pipe) != 0 || pipe(exit_pipe) != 0) {
            return fail("resource-release-pipe");
        }
        pid_t child = fork();
        if (child < 0) {
            return fail("resource-release-fork");
        }
        if (child == 0) {
            close(ready_pipe[0]);
            close(exit_pipe[1]);
            if (test_resource_boundary_impl(0)) {
                _exit(1);
            }
            char token = 'R';
            if (write(ready_pipe[1], &token, 1) != 1 ||
                read(exit_pipe[0], &token, 1) != 1) {
                _exit(2);
            }
            _exit(0);
        }
        close(ready_pipe[1]);
        close(exit_pipe[0]);
        char token = 0;
        if (read(ready_pipe[0], &token, 1) != 1 || token != 'R') {
            return fail("resource-release-ready");
        }
        char proc_path[64];
        int path_length = snprintf(proc_path, sizeof(proc_path),
                                   "/proc/%ld/status", (long)child);
        if (path_length <= 0 || (size_t)path_length >= sizeof(proc_path)) {
            errno = EOVERFLOW;
            return fail("resource-release-proc-path");
        }
        int status_fd = open(proc_path, O_RDONLY | O_CLOEXEC);
        if (status_fd < 0) {
            return fail("resource-release-proc-open");
        }
        retained_status[retained_count++] = status_fd;
        token = 'X';
        if (write(exit_pipe[1], &token, 1) != 1) {
            return fail("resource-release-go");
        }
        close(ready_pipe[0]);
        close(exit_pipe[1]);
        if (wait_for_exit(child, "resource-release-child")) {
            return 1;
        }
    }
    for (unsigned int index = 0; index < retained_count; ++index) {
        close(retained_status[index]);
    }
    marker("THEKERNEL_SECCOMP_EXIT_RECLAIM_OK");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (argc == 2 && strcmp(argv[1], "--exec-probe") == 0) {
        return exec_probe();
    }
    if (argc == 2 && strcmp(argv[1], "--thekernel") == 0) {
        require_exact_path_limit = 1;
    } else if (argc != 1) {
        errno = EINVAL;
        return fail("unknown-option");
    }
    if (argv[0] == NULL || argv[0][0] != '/') {
        errno = EINVAL;
        return fail("absolute-self-path-required");
    }
    self_path = argv[0];

    if (test_api() ||
        run_exit_case("filter-errors", test_filter_error_order) ||
        run_exit_case("unaligned-filter", test_unaligned_filter) ||
        run_exit_case("filter-fast-paths", test_filter_fast_paths) ||
        run_exit_case("errno-zero", test_errno_zero) ||
        run_exit_case("log-allow", test_log_allows) ||
        run_exit_case("trap", test_trap) ||
        run_exit_case("inheritance", test_inheritance) ||
        run_exit_case("exec-persistence", test_exec_persistence) ||
        run_exit_case("strict", test_strict) ||
        run_exit_case("prctl-strict", test_prctl_strict_ignores_filter) ||
        test_strict_kill() ||
        run_exit_case("unsupported", test_unsupported_lifecycles) ||
        test_kill_scope() ||
        test_resource_release() ||
        run_exit_case("resource-boundary", test_resource_boundary)) {
        return 1;
    }

    marker("THEKERNEL_SECCOMP_OK");
    return 0;
}
