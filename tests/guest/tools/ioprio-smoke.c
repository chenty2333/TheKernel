#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_ioprio_get
#define SYS_ioprio_get 252
#endif
#ifndef SYS_ioprio_set
#define SYS_ioprio_set 251
#endif
#ifndef SYS_clone
#define SYS_clone 56
#endif
#ifndef SYS_exit
#define SYS_exit 60
#endif
#ifndef SYS_sched_setscheduler
#define SYS_sched_setscheduler 144
#endif
#ifndef SYS_sched_getscheduler
#define SYS_sched_getscheduler 145
#endif

#ifndef CLONE_IO
#define CLONE_IO 0x80000000ULL
#endif

#define IOPRIO_CLASS_SHIFT 13U
#define IOPRIO_CLASS_NONE 0U
#define IOPRIO_CLASS_RT 1U
#define IOPRIO_CLASS_BE 2U
#define IOPRIO_CLASS_IDLE 3U
#define IOPRIO_WHO_PROCESS 1U
#define IOPRIO_WHO_PGRP 2U
#define IOPRIO_WHO_USER 3U

static unsigned short ioprio_value(unsigned int class, unsigned int data)
{
    return (unsigned short)((class << IOPRIO_CLASS_SHIFT) | (data & 0x1fffU));
}

static unsigned int ioprio_class(unsigned short value)
{
    return ((unsigned int)value >> IOPRIO_CLASS_SHIFT) & 7U;
}

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_IOPRIO_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected)
{
    fprintf(stderr,
            "THEKERNEL_IOPRIO_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static long ioprio_get_call(unsigned int which, int who)
{
    return syscall(SYS_ioprio_get, which, who);
}

static long ioprio_set_call(unsigned int which, int who, unsigned int value)
{
    return syscall(SYS_ioprio_set, which, who, value);
}

static long sched_setscheduler_call(pid_t pid, int policy,
                                    const struct sched_param *param)
{
    return syscall(SYS_sched_setscheduler, pid, policy, param);
}

static long sched_getscheduler_call(pid_t pid)
{
    return syscall(SYS_sched_getscheduler, pid);
}

static int expect_get(const char *stage, unsigned int which, int who,
                      unsigned short expected)
{
    errno = 0;
    long result = ioprio_get_call(which, who);
    int saved_errno = errno;
    if (result != (long)expected) {
        errno = saved_errno;
        return fail_value(stage, result, (long)expected);
    }
    return 0;
}

static int expect_errno_get(const char *stage, unsigned int which, int who,
                            int expected_errno)
{
    errno = 0;
    long result = ioprio_get_call(which, who);
    int saved_errno = errno;
    if (result != -1 || saved_errno != expected_errno) {
        errno = saved_errno;
        return fail_value(stage, result, -1);
    }
    return 0;
}

static int expect_errno_set(const char *stage, unsigned int which, int who,
                            unsigned int value, int expected_errno)
{
    errno = 0;
    long result = ioprio_set_call(which, who, value);
    int saved_errno = errno;
    if (result != -1 || saved_errno != expected_errno) {
        errno = saved_errno;
        return fail_value(stage, result, -1);
    }
    return 0;
}

static int set_process(const char *stage, unsigned short value)
{
    errno = 0;
    if (ioprio_set_call(IOPRIO_WHO_PROCESS, 0, value) != 0) {
        return fail(stage);
    }
    return 0;
}

static int wait_success(pid_t child, const char *stage)
{
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail(stage);
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        if (WIFSIGNALED(status)) {
            fprintf(stderr, "THEKERNEL_IOPRIO_FAIL %s signal=%d\n", stage,
                    WTERMSIG(status));
        } else {
            fprintf(stderr, "THEKERNEL_IOPRIO_FAIL %s status=0x%x\n", stage,
                    status);
        }
        return 1;
    }
    return 0;
}

static int check_exec(void)
{
    return expect_get("exec-inheritance", IOPRIO_WHO_PROCESS, 0,
                       ioprio_value(IOPRIO_CLASS_BE, 3));
}

static int test_fork_and_exec_inheritance(const char *self)
{
    pid_t child;

    if (set_process("set-fork-priority", ioprio_value(IOPRIO_CLASS_BE, 3))) {
        return 1;
    }
    child = fork();
    if (child < 0) {
        return fail("fork-inheritance");
    }
    if (child == 0) {
        _exit(expect_get("fork-inheritance", IOPRIO_WHO_PROCESS, 0,
                         ioprio_value(IOPRIO_CLASS_BE, 3)));
    }
    if (wait_success(child, "fork-inheritance-wait")) {
        return 1;
    }

    child = fork();
    if (child < 0) {
        return fail("exec-inheritance-fork");
    }
    if (child == 0) {
        execl(self, self, "--check-exec", (char *)NULL);
        fprintf(stderr, "THEKERNEL_IOPRIO_FAIL exec-inheritance errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    return wait_success(child, "exec-inheritance-wait");
}

static int test_clone_io(void)
{
    pid_t child;

    if (set_process("set-clone-io-parent", ioprio_value(IOPRIO_CLASS_BE, 4))) {
        return 1;
    }
    child = (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_IO | SIGCHLD), 0,
                           0, 0, 0);
    if (child < 0) {
        return fail("clone-io");
    }
    if (child == 0) {
        if (set_process("set-clone-io-child", ioprio_value(IOPRIO_CLASS_BE, 2))) {
            _exit(1);
        }
        _exit(0);
    }
    if (wait_success(child, "clone-io-wait")) {
        return 1;
    }
    return expect_get("clone-io-shared-context", IOPRIO_WHO_PROCESS, 0,
                      ioprio_value(IOPRIO_CLASS_BE, 2));
}

static int test_clone_io_empty_parent(void)
{
    pid_t child;

    /* A task which has never set an explicit priority has no Linux
     * io_context.  CLONE_IO must not manufacture one in the parent merely by
     * sharing that empty state with the child. */
    child = (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_IO | SIGCHLD), 0,
                           0, 0, 0);
    if (child < 0) {
        return fail("clone-io-empty");
    }
    if (child == 0) {
        if (set_process("set-clone-io-empty-child",
                        ioprio_value(IOPRIO_CLASS_BE, 2))) {
            _exit(1);
        }
        _exit(0);
    }
    if (wait_success(child, "clone-io-empty-wait")) {
        return 1;
    }
    return expect_get("clone-io-empty-parent", IOPRIO_WHO_PROCESS, 0, 0);
}

static int test_zombie_ioprio(void)
{
    pid_t child;
    siginfo_t info;

    child = fork();
    if (child < 0) {
        return fail("zombie-fork");
    }
    if (child == 0) {
        if (set_process("set-zombie-priority",
                        ioprio_value(IOPRIO_CLASS_BE, 3))) {
            _exit(1);
        }
        _exit(0);
    }
    memset(&info, 0, sizeof(info));
    if (waitid(P_PID, child, &info, WEXITED | WNOWAIT) != 0) {
        return fail("zombie-waitid");
    }
    if (expect_get("zombie-get-before-reap", IOPRIO_WHO_PROCESS, child, 0)) {
        (void)waitpid(child, NULL, 0);
        return 1;
    }
    errno = 0;
    if (ioprio_set_call(IOPRIO_WHO_PROCESS, child,
                        ioprio_value(IOPRIO_CLASS_BE, 2)) != 0) {
        (void)waitpid(child, NULL, 0);
        return fail("zombie-set-before-reap");
    }
    if (expect_get("zombie-get-after-set", IOPRIO_WHO_PROCESS, child, 0)) {
        (void)waitpid(child, NULL, 0);
        return 1;
    }
    if (waitpid(child, NULL, 0) != child) {
        return fail("zombie-reap");
    }
    if (expect_errno_get("zombie-get-after-reap", IOPRIO_WHO_PROCESS, child,
                         ESRCH) ||
        expect_errno_set("zombie-set-after-reap", IOPRIO_WHO_PROCESS, child,
                         ioprio_value(IOPRIO_CLASS_BE, 2), ESRCH)) {
        return 1;
    }
    return 0;
}

static int test_zombie_group_scheduler(void)
{
    struct sched_param param = { .sched_priority = 0 };
    siginfo_t info;
    pid_t child;

    child = fork();
    if (child < 0) {
        return fail("zombie-group-fork");
    }
    if (child == 0) {
        if (setpgid(0, 0) != 0) {
            _exit(fail("zombie-group-child-setpgid"));
        }
        if (setpriority(PRIO_PROCESS, 0, 19) != 0) {
            _exit(fail("zombie-group-child-setpriority"));
        }
        if (sched_setscheduler_call(0, SCHED_IDLE, &param) != 0) {
            _exit(fail("zombie-group-child-sched-setscheduler"));
        }
        int policy = (int)sched_getscheduler_call(0);
        if (policy != SCHED_IDLE) {
            _exit(fail_value("zombie-group-child-sched-getscheduler",
                             policy, SCHED_IDLE));
        }
        _exit(0);
    }

    memset(&info, 0, sizeof(info));
    if (waitid(P_PID, child, &info, WEXITED | WNOWAIT) != 0) {
        (void)waitpid(child, NULL, 0);
        return fail("zombie-group-waitid");
    }
    if (info.si_code != CLD_EXITED) {
        (void)waitpid(child, NULL, 0);
        return fail_value("zombie-group-child-code", info.si_code,
                          CLD_EXITED);
    }
    if (info.si_status != 0) {
        (void)waitpid(child, NULL, 0);
        return fail_value("zombie-group-child-status", info.si_status, 0);
    }
    if (expect_get("zombie-group-idle", IOPRIO_WHO_PGRP, (int)child,
                   ioprio_value(IOPRIO_CLASS_IDLE, 7))) {
        (void)waitpid(child, NULL, 0);
        return 1;
    }
    if (waitpid(child, NULL, 0) != child) {
        return fail("zombie-group-reap");
    }
    return 0;
}

struct exec_oldtid_args {
    const char *self;
};

/*
 * The glibc clone() wrapper rejects CLONE_THREAD for this static helper.  A
 * raw clone syscall returns in the child at the supplied stack pointer, so
 * place the function and argument there and finish the child with _exit's
 * syscall ABI rather than depending on a libc/pthread trampoline.
 */
static __attribute__((noinline, noclone)) long
raw_clone_thread(unsigned long flags, void *child_stack)
{
    long result;

    __asm__ volatile(
        "mov %[flags], %%rdi\n\t"
        "mov %[child_stack], %%rsi\n\t"
        "xor %%edx, %%edx\n\t"
        "xor %%r10d, %%r10d\n\t"
        "xor %%r8d, %%r8d\n\t"
        "mov %[sys_clone], %%eax\n\t"
        "syscall\n\t"
        "test %%rax, %%rax\n\t"
        "jnz 1f\n\t"
        "pop %%rsi\n\t"
        "pop %%rdi\n\t"
        "call *%%rsi\n\t"
        "mov %%eax, %%edi\n\t"
        "mov %[sys_exit], %%eax\n\t"
        "syscall\n\t"
        "ud2\n\t"
        "1:\n\t"
        : "=a"(result)
        : [flags] "r"(flags), [child_stack] "r"(child_stack),
          [sys_clone] "i"(SYS_clone), [sys_exit] "i"(SYS_exit)
        : "cc", "rcx", "rdi", "rsi", "rdx", "r8", "r10", "r11",
          "memory");

    if (result < 0) {
        errno = (int)-result;
        return -1;
    }
    return result;
}

static int exec_oldtid_thread(void *opaque)
{
    struct exec_oldtid_args *args = opaque;
    char tid[32];
    int old_tid = (int)syscall(SYS_gettid);

    snprintf(tid, sizeof(tid), "%d", old_tid);
    execl(args->self, args->self, "--check-exec-oldtid", tid, (char *)NULL);
    fprintf(stderr, "THEKERNEL_IOPRIO_FAIL exec-oldtid errno=%d (%s)\n",
            errno, strerror(errno));
    _exit(127);
}

static int check_exec_oldtid(const char *value)
{
    char *end = NULL;
    long old_tid = strtol(value, &end, 10);

    if (end == value || *end != '\0' || old_tid <= 0 || old_tid > INT32_MAX) {
        errno = EINVAL;
        return fail("exec-oldtid-argument");
    }
    if (expect_errno_get("exec-oldtid-get", IOPRIO_WHO_PROCESS,
                         (int)old_tid, ESRCH) ||
        expect_errno_set("exec-oldtid-set", IOPRIO_WHO_PROCESS,
                         (int)old_tid, ioprio_value(IOPRIO_CLASS_BE, 2),
                         ESRCH)) {
        return 1;
    }
    return 0;
}

static int test_exec_oldtid(const char *self)
{
    enum { STACK_SIZE = 1 << 20 };
    pid_t supervisor;

    supervisor = fork();
    if (supervisor < 0) {
        return fail("exec-oldtid-fork");
    }
    if (supervisor == 0) {
        void *stack = mmap(NULL, STACK_SIZE, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        struct exec_oldtid_args args = { .self = self };
        uintptr_t *thread_stack;
        pid_t thread;

        if (stack == MAP_FAILED) {
            _exit(1);
        }
        /*
         * The raw child begins with rsp at thread_stack.  Keep that entry
         * point 16-byte aligned; after the two pops below, the call-site rsp
         * is aligned for the SysV ABI and the callee owns its own red zone.
         */
        thread_stack = (uintptr_t *)((uintptr_t)stack + STACK_SIZE);
        thread_stack = (uintptr_t *)((uintptr_t)thread_stack & ~(uintptr_t)0xf);
        *--thread_stack = (uintptr_t)&args;
        *--thread_stack = (uintptr_t)exec_oldtid_thread;
        thread = (pid_t)raw_clone_thread(
            CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND |
                CLONE_THREAD | CLONE_SYSVSEM,
            thread_stack);
        if (thread < 0) {
            int saved_errno = errno;

            munmap(stack, STACK_SIZE);
            errno = saved_errno;
            _exit(fail("exec-oldtid-clone"));
        }
        /* A non-leader exec terminates the other thread.  If the exec path
         * unexpectedly fails, bound the supervisor rather than hanging the
         * complete differential run. */
        sleep(2);
        _exit(1);
    }
    return wait_success(supervisor, "exec-oldtid-wait");
}

struct exited_leader_args {
    int leader_tid;
};

static int exited_leader_thread(void *opaque)
{
    struct exited_leader_args *args = opaque;
    const struct timespec timeout = { .tv_sec = 2 };
    int tid;

    if (setpriority(PRIO_PROCESS, 0, 19) != 0 ||
        set_process("exited-leader-sibling-none", 0)) {
        return 1;
    }
    while ((tid = __atomic_load_n(&args->leader_tid, __ATOMIC_ACQUIRE)) != 0) {
        /* clear_child_tid is shared, so use FUTEX_WAIT (not PRIVATE). */
        if (syscall(SYS_futex, &args->leader_tid, 0, tid, &timeout, NULL, 0) < 0 &&
            errno != EAGAIN && errno != EINTR) {
            return fail("exited-leader-wait");
        }
    }
    /* The leader's old BE/0 context was released on exit. Its retained
     * nice-0 default BE/4 still wins over the sibling's nice-19 BE/7. */
    if (expect_get("exited-leader-group-default", IOPRIO_WHO_PGRP, 0,
                   ioprio_value(IOPRIO_CLASS_BE, 4))) {
        return 1;
    }
    return 0;
}

static int test_exited_leader_group(void)
{
    enum { STACK_SIZE = 1 << 20 };
    pid_t child = fork();
    if (child < 0) {
        return fail("exited-leader-fork");
    }
    if (child == 0) {
        struct exited_leader_args args = { .leader_tid = (int)syscall(SYS_gettid) };
        void *stack = mmap(NULL, STACK_SIZE, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        uintptr_t *thread_stack;
        if (stack == MAP_FAILED || setpgid(0, 0) != 0 ||
            setpriority(PRIO_PROCESS, 0, 0) != 0 ||
            set_process("exited-leader-before-exit", ioprio_value(IOPRIO_CLASS_BE, 0))) {
            _exit(1);
        }
        syscall(SYS_set_tid_address, &args.leader_tid);
        thread_stack = (uintptr_t *)((uintptr_t)stack + STACK_SIZE);
        thread_stack = (uintptr_t *)((uintptr_t)thread_stack & ~(uintptr_t)0xf);
        *--thread_stack = (uintptr_t)&args;
        *--thread_stack = (uintptr_t)exited_leader_thread;
        if (raw_clone_thread(CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND |
                             CLONE_THREAD | CLONE_SYSVSEM, thread_stack) < 0) {
            _exit(fail("exited-leader-clone"));
        }
        syscall(SYS_exit, 0);
        __builtin_unreachable();
    }
    return wait_success(child, "exited-leader-wait-child");
}

static int test_process_group_highest(void)
{
    int start[2];
    int ready[2];
    int release[2];
    char byte;
    pid_t child;

    if (pipe(start) != 0 || pipe(ready) != 0 || pipe(release) != 0) {
        return fail("pgrp-pipe");
    }
    if (set_process("set-pgrp-parent", ioprio_value(IOPRIO_CLASS_BE, 6))) {
        close(ready[0]);
        close(ready[1]);
        close(release[0]);
        close(release[1]);
        return 1;
    }

    child = fork();
    if (child < 0) {
        return fail("pgrp-fork");
    }
    if (child == 0) {
        close(start[1]);
        close(ready[0]);
        close(release[1]);
        if (read(start[0], &byte, 1) != 1 ||
            set_process("set-pgrp-child", ioprio_value(IOPRIO_CLASS_BE, 1)) ||
            write(ready[1], "R", 1) != 1 ||
            read(release[0], &byte, 1) != 1) {
            _exit(1);
        }
        close(start[0]);
        close(ready[1]);
        close(release[0]);
        _exit(0);
    }

    close(start[0]);
    close(ready[1]);
    close(release[0]);
    if (setpgid(child, child) != 0 || write(start[1], "S", 1) != 1) {
        close(start[1]);
        close(ready[0]);
        close(release[1]);
        (void)kill(child, SIGKILL);
        (void)waitpid(child, NULL, 0);
        return fail("set-child-process-group");
    }
    close(start[1]);
    if (read(ready[0], &byte, 1) != 1 || byte != 'R') {
        close(ready[0]);
        close(release[1]);
        return fail("pgrp-ready");
    }
    close(ready[0]);
    if (expect_get("pgrp-highest", IOPRIO_WHO_PGRP, (int)child,
                   ioprio_value(IOPRIO_CLASS_BE, 1))) {
        close(release[1]);
        return 1;
    }
    if (write(release[1], "X", 1) != 1) {
        close(release[1]);
        return fail("pgrp-release");
    }
    close(release[1]);
    return wait_success(child, "pgrp-wait");
}

static int test_user_and_errors(void)
{
    long user_priority;

    errno = 0;
    user_priority = ioprio_get_call(IOPRIO_WHO_USER, (int)getuid());
    if (user_priority < 0 ||
        ioprio_class((unsigned short)user_priority) > IOPRIO_CLASS_IDLE) {
        return fail("user-highest");
    }
    if (expect_errno_get("invalid-get-which", 0, 0, EINVAL) ||
        expect_errno_set("invalid-set-which", 0, 0,
                         ioprio_value(IOPRIO_CLASS_BE, 0), EINVAL) ||
        expect_errno_get("missing-pgrp", IOPRIO_WHO_PGRP, 0x7fffffff, ESRCH) ||
        expect_errno_set("missing-pgrp-set", IOPRIO_WHO_PGRP, 0x7fffffff,
                         ioprio_value(IOPRIO_CLASS_BE, 0), ESRCH) ||
        expect_errno_set("invalid-class", IOPRIO_WHO_PROCESS, 0,
                         ioprio_value(4, 0), EINVAL) ||
        expect_errno_set("none-with-level", IOPRIO_WHO_PROCESS, 0,
                         ioprio_value(IOPRIO_CLASS_NONE, 1), EINVAL)) {
        return 1;
    }

    /* Direct syscalls retain non-level data bits in CLASS_NONE, as does
     * Linux's current ioprio_check_cap()/io_context path. */
    if (set_process("set-none-hint", ioprio_value(IOPRIO_CLASS_NONE, 0x100)) ||
        expect_get("get-none-hint", IOPRIO_WHO_PROCESS, 0,
                   ioprio_value(IOPRIO_CLASS_NONE, 0x100)) ||
        set_process("reset-none", 0)) {
        return 1;
    }
    return 0;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--check-exec") == 0) {
        return check_exec();
    }
    if (argc == 3 && strcmp(argv[1], "--check-exec-oldtid") == 0) {
        return check_exec_oldtid(argv[2]);
    }
    if (argc == 2 && strcmp(argv[1], "--linux-host") != 0) {
        errno = EINVAL;
        return fail("arguments");
    }
    if (argc > 2) {
        errno = EINVAL;
        return fail("arguments");
    }

    if (test_clone_io_empty_parent() ||
        set_process("set-initial", ioprio_value(IOPRIO_CLASS_BE, 4)) ||
        expect_get("get-process", IOPRIO_WHO_PROCESS, 0,
                   ioprio_value(IOPRIO_CLASS_BE, 4)) ||
        test_fork_and_exec_inheritance(argv[0]) || test_clone_io() ||
        test_zombie_ioprio() || test_zombie_group_scheduler() ||
        test_exec_oldtid(argv[0]) || test_exited_leader_group() ||
        test_process_group_highest() || test_user_and_errors() ||
        set_process("reset-final", 0) ||
        expect_get("get-final-none", IOPRIO_WHO_PROCESS, 0, 0)) {
        return 1;
    }
    puts("THEKERNEL_IOPRIO_DIFFERENTIAL_OK");
    return 0;
}
