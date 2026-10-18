#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>
struct shared {
    atomic_int ready, leave[2], allow_process_exit;
    atomic_uint count[2];
    pid_t tid[2];
};
static struct shared *shared;
static pid_t target;
static int fail(const char *what) {
    fprintf(stderr, "ptrace-threads: %s errno=%d target=%d tids=%d,%d\n", what, errno, target, shared->tid[0], shared->tid[1]);
    if (target > 0) { kill(target, SIGKILL); waitpid(target, NULL, __WALL); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static void *worker(void *argument) {
    unsigned index = (unsigned)(uintptr_t)argument;
    shared->tid[index] = (pid_t)syscall(SYS_gettid);
    atomic_fetch_add(&shared->ready, 1);
    while (!atomic_load(&shared->leave[index])) {
        atomic_fetch_add(&shared->count[index], 1);
        __asm__ volatile("pause" ::: "memory");
    }
    return NULL;
}
static long request(long op, pid_t tid, unsigned long addr, void *data) {
    return syscall(SYS_ptrace, op, tid, addr, data);
}
static int stopped(pid_t tid) {
    int status;
    return waitpid(tid, &status, __WALL) == tid && WIFSTOPPED(status) &&
           WSTOPSIG(status) == SIGTRAP && (status >> 16) == PTRACE_EVENT_STOP;
}
static int exited(pid_t tid, int code) {
    int status;
    return waitpid(tid, &status, __WALL) == tid && WIFEXITED(status) && WEXITSTATUS(status) == code;
}

/* Wait for a resumed worker to run while proving its ptrace-stopped peer stays still. */
static int wait_for_counter_progress(unsigned running, unsigned running_start,
                                     unsigned stopped, unsigned stopped_value) {
    while (atomic_load(&shared->count[running]) == running_start) {
        if (atomic_load(&shared->count[stopped]) != stopped_value) return 0;
        /* The test's existing 15s alarm bounds this wait; a sleep alone is not a scheduling guarantee. */
        usleep(1000);
    }
    return atomic_load(&shared->count[stopped]) == stopped_value;
}

static const char *self_path;
static void *exec_worker(void *unused) {
    (void)unused;
    shared->tid[0] = (pid_t)syscall(SYS_gettid);
    atomic_store(&shared->ready, 1);
    while (!atomic_load(&shared->leave[0])) sched_yield();
    char *args[] = {(char *)self_path, "--thread-exec-child", NULL};
    char *env[] = {NULL};
    long result = syscall(SYS_execve, self_path, args, env);
    fprintf(stderr,
            "ptrace-threads: nonleader-exec execve returned %ld errno=%d\n",
            result, errno);
    _exit(31);
}
static int nonleader_exec(void) {
    shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(shared != MAP_FAILED);
    fprintf(stderr, "ptrace-threads: nonleader-exec begin\n");
    target = fork(); CHECK(target >= 0);
    if (!target) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, exec_worker, NULL)) _exit(32);
        for (;;) pause();
    }
    while (!atomic_load(&shared->ready)) sched_yield();
    pid_t worker_tid = shared->tid[0]; CHECK(worker_tid != target);
    fprintf(stderr,
            "ptrace-threads: nonleader-exec worker ready target=%d worker_tid=%d\n",
            target, worker_tid);
    /* The leader is asleep in pause, not executing a user spin loop. */
    CHECK(request(PTRACE_SEIZE, target, 0, NULL) == 0);
    CHECK(request(PTRACE_INTERRUPT, target, 0, NULL) == 0 && stopped(target));
    fprintf(stderr, "ptrace-threads: nonleader-exec leader stopped\n");
    struct user_regs_struct leader_regs;
    CHECK(request(PTRACE_GETREGS, target, 0, &leader_regs) == 0 && leader_regs.rip);
    CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    CHECK(request(PTRACE_SEIZE, worker_tid, 0, (void *)(uintptr_t)PTRACE_O_TRACEEXEC) == 0);
    fprintf(stderr, "ptrace-threads: nonleader-exec worker INTERRUPT begin\n");
    CHECK(request(PTRACE_INTERRUPT, worker_tid, 0, NULL) == 0);
    fprintf(stderr, "ptrace-threads: nonleader-exec worker INTERRUPT done; waiting stop\n");
    CHECK(stopped(worker_tid));
    fprintf(stderr, "ptrace-threads: nonleader-exec worker stopped\n");
    atomic_store(&shared->leave[0], 1);
    CHECK(request(PTRACE_CONT, worker_tid, 0, NULL) == 0);
    fprintf(stderr, "ptrace-threads: nonleader-exec worker resumed; waiting exec stop\n");
    int status = 0;
    pid_t reported = waitpid(target, &status, __WALL);
    if (reported != target || !WIFSTOPPED(status) || WSTOPSIG(status) != SIGTRAP ||
        (status >> 16) != PTRACE_EVENT_EXEC) {
        fprintf(stderr,
                "ptrace-threads: nonleader-exec unexpected wait result pid=%d status=0x%x "
                "stopped=%d signal=%d event=%d\n",
                reported, status, WIFSTOPPED(status), WIFSTOPPED(status) ? WSTOPSIG(status) : 0,
                (status >> 16));
        return fail("nonleader-exec PTRACE_EVENT_EXEC wait");
    }
    fprintf(stderr, "ptrace-threads: nonleader-exec adopted leader\n");
    unsigned long old_tid = 0;
    CHECK(request(PTRACE_GETEVENTMSG, target, 0, &old_tid) == 0 && old_tid == (unsigned long)worker_tid);
    struct user_regs_struct regs;
    CHECK(request(PTRACE_GETREGS, target, 0, &regs) == 0 && regs.rip && regs.fs_base == 0);
    CHECK(request(PTRACE_CONT, target, 0, NULL) == 0 && exited(target, 23));
    errno = 0; CHECK(waitpid(target, &status, WNOHANG | __WALL) == -1 && errno == ECHILD);
    target = 0;
    CHECK(munmap(shared, 4096) == 0);
    return 0;
}

/* PTRACE_INTERRUPT of an in-kernel wait must not leak EINTR to a tracee
 * when CONT injects no signal/handler. This also exercises the restart edge
 * which strace-f reaches if child notification wins a wait wake race. */
static int interrupted_wait_without_handler(void) {
    shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(shared != MAP_FAILED);
    target = fork(); CHECK(target >= 0);
    if (!target) {
        atomic_store(&shared->ready, 1);
        while (!atomic_load(&shared->leave[0])) __asm__ volatile("pause" ::: "memory");
        pid_t grandchild = (pid_t)syscall(SYS_fork);
        if (grandchild < 0) _exit(33);
        if (!grandchild) {
            while (!atomic_load(&shared->leave[1])) __asm__ volatile("pause" ::: "memory");
            _exit(0);
        }
        int status;
        long result = syscall(SYS_wait4, grandchild, &status, 0, NULL);
        atomic_store(&shared->allow_process_exit, 1);
        _exit(result == grandchild && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 34);
    }
    while (!atomic_load(&shared->ready)) sched_yield();
    CHECK(request(PTRACE_SEIZE, target, 0, (void *)(uintptr_t)PTRACE_O_TRACESYSGOOD) == 0);
    CHECK(request(PTRACE_INTERRUPT, target, 0, NULL) == 0 && stopped(target));
    atomic_store(&shared->leave[0], 1);
    CHECK(request(PTRACE_SYSCALL, target, 0, NULL) == 0);
    struct user_regs_struct regs;
    for (;;) {
        int status;
        CHECK(waitpid(target, &status, __WALL) == target && WIFSTOPPED(status));
        CHECK(request(PTRACE_GETREGS, target, 0, &regs) == 0);
        if (regs.orig_rax == SYS_wait4 && (long)regs.rax == -ENOSYS) break;
        CHECK(request(PTRACE_SYSCALL, target, 0, NULL) == 0);
    }
    CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    usleep(20000);
    CHECK(request(PTRACE_INTERRUPT, target, 0, NULL) == 0 && stopped(target));
    CHECK(request(PTRACE_GETREGS, target, 0, &regs) == 0 && regs.orig_rax == SYS_wait4);
    CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    usleep(20000);
    int prematurely_returned = atomic_load(&shared->allow_process_exit);
    atomic_store(&shared->leave[1], 1);
    CHECK(!prematurely_returned);
    for (;;) {
        int status;
        CHECK(waitpid(target, &status, __WALL) == target);
        if (WIFEXITED(status)) { CHECK(WEXITSTATUS(status) == 0); break; }
        CHECK(WIFSTOPPED(status));
        CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    }
    target = 0;
    CHECK(munmap(shared, 4096) == 0);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--thread-exec-child") == 0) return 23;
    self_path = argv[0];
    puts("THEKERNEL_ABI_CASE ptrace-threads.raw-differential");
    shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) return 1;
    alarm(15);
    target = fork(); CHECK(target >= 0);
    if (!target) {
        pthread_t a, b;
        if (pthread_create(&a, NULL, worker, (void *)(uintptr_t)0) ||
            pthread_create(&b, NULL, worker, (void *)(uintptr_t)1)) _exit(2);
        if (pthread_join(a, NULL) || pthread_join(b, NULL)) _exit(3);
        while (!atomic_load(&shared->allow_process_exit)) sched_yield();
        _exit(23);
    }
    while (atomic_load(&shared->ready) != 2) sched_yield();
    pid_t a = shared->tid[0], b = shared->tid[1];
    CHECK(a != target && b != target && a != b);
    CHECK(request(PTRACE_SEIZE, a, 0, NULL) == 0);
    CHECK(request(PTRACE_INTERRUPT, a, 0, NULL) == 0 && stopped(a));
    unsigned a0 = atomic_load(&shared->count[0]), b0 = atomic_load(&shared->count[1]);
    CHECK(wait_for_counter_progress(1, b0, 0, a0));
    CHECK(request(PTRACE_SEIZE, b, 0, NULL) == 0);
    CHECK(request(PTRACE_INTERRUPT, b, 0, NULL) == 0 && stopped(b));
    struct user_regs_struct ar, br;
    CHECK(request(PTRACE_GETREGS, a, 0, &ar) == 0 && request(PTRACE_GETREGS, b, 0, &br) == 0);
    /* GDB resets DR7 on each newly discovered LWP, even for software-only debugging. */
    CHECK(request(PTRACE_POKEUSER, a, offsetof(struct user, u_debugreg[7]), NULL) == 0);
    unsigned long control = 1;
    CHECK(request(PTRACE_PEEKUSER, a, offsetof(struct user, u_debugreg[7]), &control) == 0 && control == 0);
    CHECK(ar.rsp != br.rsp && ar.fs_base != br.fs_base && ar.fs_base != 0 && br.fs_base != 0);
    b0 = atomic_load(&shared->count[1]);
    unsigned a_before_resume = atomic_load(&shared->count[0]);
    CHECK(a_before_resume == a0);
    CHECK(request(PTRACE_CONT, a, 0, NULL) == 0);
    CHECK(wait_for_counter_progress(0, a_before_resume, 1, b0));
    CHECK(request(PTRACE_INTERRUPT, a, 0, NULL) == 0 && stopped(a));
    atomic_store(&shared->leave[0], 1);
    CHECK(request(PTRACE_CONT, a, 0, NULL) == 0);
    siginfo_t observed;
    memset(&observed, 0, sizeof(observed));
    CHECK(waitid(P_PID, a, &observed, WEXITED | WNOWAIT | __WALL) == 0 &&
          observed.si_pid == a && observed.si_code == CLD_EXITED && observed.si_status == 0);
    memset(&observed, 0, sizeof(observed));
    CHECK(waitid(P_PID, a, &observed, WEXITED | WNOWAIT | __WALL) == 0 && observed.si_pid == a);
    CHECK(exited(a, 0));
    errno = 0; int status;
    CHECK(waitpid(a, &status, WNOHANG | __WALL) == -1 && errno == ECHILD);
    CHECK(request(PTRACE_GETREGS, b, 0, &br) == 0 && atomic_load(&shared->count[1]) == b0);
    atomic_store(&shared->leave[1], 1);
    CHECK(request(PTRACE_CONT, b, 0, NULL) == 0 && exited(b, 0));
    atomic_store(&shared->allow_process_exit, 1);
    CHECK(exited(target, 23)); target = 0;
    CHECK(munmap(shared, 4096) == 0);
    fprintf(stderr, "ptrace-threads: basic thread exits complete\n");
    fprintf(stderr, "ptrace-threads: nonleader-exec starting\n");
    CHECK(nonleader_exec() == 0);
    fprintf(stderr, "ptrace-threads: nonleader-exec complete\n");
    fprintf(stderr, "ptrace-threads: interrupted-wait starting\n");
    CHECK(interrupted_wait_without_handler() == 0);
    fprintf(stderr, "ptrace-threads: interrupted-wait complete\n");
    alarm(0);
    puts("THEKERNEL_ABI_ASSERT ptrace-threads.raw-differential TASK_EXACT_STOP_REGISTERS_RESUME_AND_EXIT pass");
    puts("THEKERNEL_ABI_RESULT ptrace-threads.raw-differential pass");
    puts("THEKERNEL_PTRACE_THREADS_OK"); return 0;
}
