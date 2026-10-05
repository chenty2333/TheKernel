#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
struct shared { volatile int first_wait, final_wait; volatile pid_t target; };
static pid_t target, descendant;
static volatile sig_atomic_t usr1_seen;
static void usr1_handler(int signo) { (void)signo; usr1_seen++; }
static int fail(const char *what) {
    fprintf(stderr, "ptrace-exit: %s errno=%d target=%d child=%d\n", what, errno, target, descendant);
    if (descendant > 0) kill(descendant, SIGKILL);
    if (target > 0) { kill(target, SIGKILL); waitpid(target, NULL, __WALL); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long request(long op, pid_t pid, unsigned long addr, void *data) {
    return syscall(SYS_ptrace, op, pid, addr, data);
}
static int stopped(pid_t pid, int signo, int event) {
    int status;
    return waitpid(pid, &status, __WALL) == pid && WIFSTOPPED(status) &&
           WSTOPSIG(status) == signo && (status >> 16) == event;
}
static int peek_exit(pid_t pid, int code) {
    siginfo_t info; memset(&info, 0, sizeof(info));
    return waitid(P_PID, pid, &info, WEXITED | WNOWAIT | __WALL) == 0 &&
           info.si_pid == pid && info.si_code == CLD_EXITED && info.si_status == code;
}
static int target_exit(pid_t pid, int code) {
    int status;
    for (int count = 0; count < 8; ++count) {
        if (waitpid(pid, &status, __WALL) != pid) return 0;
        if (WIFEXITED(status)) return WEXITSTATUS(status) == code;
        if (!WIFSTOPPED(status) || WSTOPSIG(status) != SIGCHLD ||
            request(PTRACE_CONT, pid, 0, NULL) != 0) return 0;
    }
    return 0;
}
static int fork_handoff(int tracer_dies) {
    struct shared *shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(shared != MAP_FAILED);
    pid_t worker = 0;
    if (tracer_dies) { worker = fork(); CHECK(worker >= 0); }
    if (!worker) {
        target = fork(); CHECK(target >= 0);
        if (!target) {
            if (request(PTRACE_TRACEME, 0, 0, NULL) != 0) _exit(2);
            raise(SIGSTOP);
            pid_t child = syscall(SYS_fork);
            if (child < 0) _exit(3);
            if (!child) _exit(17);
            int status = 0;
            if (!tracer_dies) {
                shared->first_wait = (int)syscall(SYS_wait4, child, &status, WNOHANG, NULL);
                raise(SIGSTOP);
            }
            if (syscall(SYS_wait4, child, &status, 0, NULL) != child ||
                !WIFEXITED(status) || WEXITSTATUS(status) != 17) _exit(4);
            shared->final_wait = 17; _exit(0);
        }
        shared->target = target;
        CHECK(stopped(target, SIGSTOP, 0));
        CHECK(request(PTRACE_SETOPTIONS, target, 0, (void *)(uintptr_t)PTRACE_O_TRACEFORK) == 0);
        CHECK(request(PTRACE_CONT, target, 0, NULL) == 0 && stopped(target, SIGTRAP, PTRACE_EVENT_FORK));
        unsigned long message;
        CHECK(request(PTRACE_GETEVENTMSG, target, 0, &message) == 0 && message > 0);
        descendant = (pid_t)message;
        CHECK(stopped(descendant, SIGSTOP, 0));
        CHECK(request(PTRACE_CONT, descendant, 0, NULL) == 0);
        CHECK(peek_exit(descendant, 17) && peek_exit(descendant, 17));
        if (tracer_dies) _exit(0);
        CHECK(request(PTRACE_CONT, target, 0, NULL) == 0 && stopped(target, SIGSTOP, 0));
        CHECK(shared->first_wait == 0);
        CHECK(peek_exit(descendant, 17));
        int status;
        CHECK(waitpid(descendant, &status, __WALL) == descendant && WIFEXITED(status) && WEXITSTATUS(status) == 17);
        errno = 0;
        CHECK(waitpid(descendant, &status, WNOHANG | __WALL) == -1 && errno == ECHILD);
        descendant = 0;
        CHECK(request(PTRACE_CONT, target, 0, NULL) == 0 && target_exit(target, 0)); target = 0;
    } else {
        int status;
        CHECK(waitpid(worker, &status, 0) == worker && WIFEXITED(status) && WEXITSTATUS(status) == 0);
        target = shared->target;
        CHECK(target > 0 && waitpid(target, &status, 0) == target && WIFEXITED(status) && WEXITSTATUS(status) == 0);
        target = 0;
    }
    CHECK(shared->final_wait == 17);
    CHECK(munmap(shared, 4096) == 0);
    return 0;
}
static int direct_parent(int exit_signal) {
    usr1_seen = 0;
    target = (pid_t)syscall(SYS_clone, exit_signal, 0, 0, 0, 0); CHECK(target >= 0);
    if (!target) {
        if (request(PTRACE_TRACEME, 0, 0, NULL) != 0) _exit(2);
        raise(SIGSTOP); _exit(23);
    }
    CHECK(stopped(target, SIGSTOP, 0));
    CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    CHECK(peek_exit(target, 23) && peek_exit(target, 23));
    CHECK(target_exit(target, 23)); target = 0;
    CHECK((exit_signal == SIGUSR1 && usr1_seen == 1) || (exit_signal != SIGUSR1 && usr1_seen == 0));
    return 0;
}
static int direct_tracer_death(int ignore_children) {
    struct shared *shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(shared != MAP_FAILED);
    pid_t worker = fork(); CHECK(worker >= 0);
    if (!worker) {
        if (ignore_children) signal(SIGCHLD, SIG_IGN);
        target = fork(); CHECK(target >= 0);
        if (!target) {
            if (request(PTRACE_TRACEME, 0, 0, NULL) != 0) _exit(2);
            raise(SIGSTOP); _exit(23);
        }
        shared->target = target;
        CHECK(stopped(target, SIGSTOP, 0));
        CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
        CHECK(peek_exit(target, 23) && peek_exit(target, 23));
        _exit(0);
    }
    int status;
    CHECK(waitpid(worker, &status, 0) == worker && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    target = shared->target; CHECK(target > 0);
    if (ignore_children) {
        errno = 0;
        CHECK(waitpid(target, &status, WNOHANG) == -1 && errno == ECHILD);
    } else {
        CHECK(waitpid(target, &status, 0) == target && WIFEXITED(status) && WEXITSTATUS(status) == 23);
    }
    target = 0; CHECK(munmap(shared, 4096) == 0); return 0;
}
struct thread_tracer_result { pid_t child; };
static int thread_tracer_body(struct thread_tracer_result *result) {
    target = fork(); CHECK(target >= 0);
    if (!target) {
        if (request(PTRACE_TRACEME, 0, 0, NULL) != 0) _exit(2);
        raise(SIGSTOP); _exit(23);
    }
    result->child = target;
    CHECK(stopped(target, SIGSTOP, 0));
    CHECK(request(PTRACE_CONT, target, 0, NULL) == 0);
    CHECK(peek_exit(target, 23) && peek_exit(target, 23));
    return 0; /* Only the tracer task exits; its process remains alive. */
}
static void *thread_tracer(void *argument) {
    return (void *)(uintptr_t)thread_tracer_body(argument);
}
static int nonfinal_tracer_thread_death(void) {
    struct thread_tracer_result result = {0};
    pthread_t thread; void *returned;
    CHECK(pthread_create(&thread, NULL, thread_tracer, &result) == 0);
    CHECK(pthread_join(thread, &returned) == 0 && (uintptr_t)returned == 0);
    int status; CHECK(result.child > 0);
    CHECK(waitpid(result.child, &status, 0) == result.child && WIFEXITED(status) && WEXITSTATUS(status) == 23);
    target = 0; return 0;
}
int main(void) {
    CHECK(prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0);
    alarm(20);
    puts("THEKERNEL_ABI_CASE ptrace-exit.raw-differential");
    struct sigaction action; memset(&action, 0, sizeof(action));
    action.sa_handler = usr1_handler; action.sa_flags = SA_RESTART;
    CHECK(sigaction(SIGUSR1, &action, NULL) == 0);
    if (fork_handoff(0) || fork_handoff(1) || direct_parent(SIGCHLD) ||
        direct_parent(SIGUSR1) || direct_parent(0) ||
        direct_tracer_death(0) || direct_tracer_death(1) ||
        nonfinal_tracer_thread_death()) return 1;
    alarm(0);
    puts("THEKERNEL_ABI_ASSERT ptrace-exit.raw-differential TRACER_FIRST_EXIT_WNOWAIT_AND_NATURAL_PARENT_HANDOFF pass");
    puts("THEKERNEL_ABI_RESULT ptrace-exit.raw-differential pass");
    puts("THEKERNEL_PTRACE_EXIT_HANDOFF_OK"); return 0;
}
