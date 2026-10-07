#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/sched.h>

struct shared { volatile int go, seen; };
static pid_t target, descendant;
static volatile sig_atomic_t timed_out;
static void timeout_handler(int signo) { (void)signo; timed_out = 1; }
static int fail(const char *what) {
    fprintf(stderr, "ptrace-fork: %s errno=%d timeout=%d target=%d child=%d\n", what, errno, (int)timed_out, target, descendant);
    if (descendant > 0) kill(descendant, SIGKILL);
    if (target > 0) { kill(target, SIGKILL); waitpid(target, NULL, __WALL); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static __attribute__((always_inline)) inline long raw(long nr, long a, long b, long c) {
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}
static long request(long op, pid_t pid, unsigned long addr, void *data) {
    return syscall(SYS_ptrace, op, pid, addr, data);
}
static int stopped(pid_t pid, int signal, int event) {
    int status;
    return waitpid(pid, &status, __WALL) == pid && WIFSTOPPED(status) && WSTOPSIG(status) == signal && (status >> 16) == event;
}
static int target_done(void) {
    int status;
    for (int count = 0; count < 5; ++count) {
        if (waitpid(target, &status, __WALL) != target) return 0;
        if (WIFEXITED(status) && WEXITSTATUS(status) == 0) { target = 0; return 1; }
        if (!WIFSTOPPED(status) || WSTOPSIG(status) != SIGCHLD || request(PTRACE_CONT, target, 0, NULL) != 0) return 0;
    }
    return 0;
}
/* kind 0=fork, 1=non-thread clone with exit_signal=0, 2=vfork,
 * 3=CLONE_UNTRACED, 4=seized fork. No libc child-side fork setup intervenes. */
static int run_case(int kind) {
    struct shared *shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(shared != MAP_FAILED);
    int ready[2], output[2]; CHECK(pipe(ready) == 0 && pipe(output) == 0);
    target = fork(); CHECK(target >= 0);
    if (!target) {
        if (kind == 4) {
            if (raw(SYS_write, ready[1], (long)"r", 1) != 1) _exit(2);
            while (!shared->go) __asm__ volatile("pause" ::: "memory");
        } else {
            if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) _exit(3);
            raw(SYS_kill, raw(SYS_getpid, 0, 0, 0), SIGSTOP, 0);
        }
        long child_pid;
        if (kind == 1) child_pid = raw(SYS_clone, 0, 0, 0);
        else if (kind == 2) child_pid = raw(SYS_vfork, 0, 0, 0);
        else if (kind == 3) child_pid = raw(SYS_clone, CLONE_UNTRACED | SIGCHLD, 0, 0);
        else child_pid = raw(SYS_fork, 0, 0, 0);
        if (child_pid < 0) _exit(4);
        if (!child_pid) {
            shared->seen = 1;
            raw(SYS_write, output[1], (long)"kid", 3);
            raw(SYS_exit_group, 17, 0, 0); __builtin_unreachable();
        }
        int status;
        if (syscall(SYS_wait4, child_pid, &status, __WALL, NULL) != child_pid ||
            !WIFEXITED(status) || WEXITSTATUS(status) != 17) _exit(5);
        raw(SYS_exit_group, 0, 0, 0); __builtin_unreachable();
    }
    close(ready[1]); close(output[1]);
    unsigned long options = PTRACE_O_TRACEFORK | PTRACE_O_TRACEVFORK | PTRACE_O_TRACECLONE | PTRACE_O_TRACESYSGOOD;
    if (kind == 4) {
        char byte; CHECK(read(ready[0], &byte, 1) == 1);
        CHECK(request(PTRACE_SEIZE, target, 0, (void *)options) == 0);
        CHECK(request(PTRACE_INTERRUPT, target, 0, NULL) == 0);
        CHECK(stopped(target, SIGTRAP, 128)); shared->go = 1;

    } else {
        CHECK(stopped(target, SIGSTOP, 0));
        CHECK(request(PTRACE_SETOPTIONS, target, 0, (void *)options) == 0);
    }
    close(ready[0]);
    if (kind == 4) {
        CHECK(request(PTRACE_SYSCALL, target, 0, NULL) == 0);
        CHECK(stopped(target, SIGTRAP | 0x80, 0));
        struct user_regs_struct entry;
        CHECK(request(PTRACE_GETREGS, target, 0, &entry) == 0);
        CHECK(entry.orig_rax == SYS_fork);
    }
    CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    if (kind != 3) {
        int event = kind == 1 ? 3 : kind == 2 ? 2 : 1;
        CHECK(stopped(target, SIGTRAP, event));
        unsigned long message; CHECK(request(PTRACE_GETEVENTMSG, target, 0, &message) == 0 && message > 0);
        descendant = (pid_t)message;
        struct user_regs_struct regs;
        CHECK(request(PTRACE_GETREGS, target, 0, &regs) == 0 && (long)regs.rax == -ENOSYS);
        CHECK(regs.orig_rax == (unsigned long)(kind == 1 ? SYS_clone : kind == 2 ? SYS_vfork : SYS_fork));
        CHECK(stopped(descendant, kind == 4 ? SIGTRAP : SIGSTOP, kind == 4 ? 128 : 0));
        CHECK(shared->seen == 0);
        CHECK(request(PTRACE_GETREGS, descendant, 0, &regs) == 0 && regs.rax == 0);
        siginfo_t info;
        CHECK(request(PTRACE_GETSIGINFO, descendant, 0, &info) == 0);
        if (kind == 4) CHECK(info.si_signo == SIGTRAP && info.si_code == (SIGTRAP | (128 << 8)));
        else CHECK(info.si_signo == SIGSTOP && info.si_code == SI_USER && info.si_pid == 0);
        CHECK(request(PTRACE_SYSCALL, descendant, 0, NULL) == 0 && stopped(descendant, SIGTRAP | 0x80, 0));
        CHECK(shared->seen == 1);
        CHECK(request(PTRACE_GETREGS, descendant, 0, &regs) == 0 && regs.orig_rax == SYS_write && regs.rdx == 3);
        CHECK(request(PTRACE_SYSCALL, descendant, 0, NULL) == 0 && stopped(descendant, SIGTRAP | 0x80, 0));
        CHECK(request(PTRACE_GETREGS, descendant, 0, &regs) == 0 && regs.rax == 3);
        CHECK(request(PTRACE_DETACH, descendant, 0, NULL) == 0); descendant = 0;
        CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    }
    CHECK(target_done());
    char data[8]; CHECK(read(output[0], data, sizeof(data)) == 3 && memcmp(data, "kid", 3) == 0);
    CHECK(shared->seen == 1); close(output[0]); munmap(shared, 4096);
    return 0;
}
int main(void) {
    puts("THEKERNEL_ABI_CASE ptrace-fork.raw-differential");
    struct sigaction action; memset(&action, 0, sizeof(action)); action.sa_handler = timeout_handler;
    CHECK(sigaction(SIGALRM, &action, NULL) == 0); alarm(15);
    for (int kind = 0; kind < 5; ++kind) {
        printf("PTRACE_FORK_CASE_BEGIN kind=%d\n", kind);
        if (run_case(kind) != 0) return 1;
    }
    alarm(0);
    puts("THEKERNEL_ABI_ASSERT ptrace-fork.raw-differential AUTOMATIC_INHERITANCE_AND_INITIAL_CHILD_STOP pass");
    puts("THEKERNEL_ABI_RESULT ptrace-fork.raw-differential pass");
    puts("THEKERNEL_PTRACE_FORK_OK"); return 0;
}
