#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t child;
static volatile sig_atomic_t timed_out;
static void timeout_handler(int signo) { (void)signo; timed_out = 1; }
static int fail(const char *what) {
    fprintf(stderr, "ptrace-exec: %s errno=%d timeout=%d\n", what, errno, (int)timed_out);
    if (child > 0) { kill(child, SIGKILL); waitpid(child, NULL, __WALL); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long request(long op, unsigned long addr, void *data) { return syscall(SYS_ptrace, op, child, addr, data); }
static int stopped(int signo, int event) {
    int status;
    return waitpid(child, &status, __WALL) == child && WIFSTOPPED(status) && WSTOPSIG(status) == signo && (status >> 16) == event;
}
static int run_case(const char *self, int seized, int trace_exec) {
    volatile int *go = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(go != MAP_FAILED); *go = 0;
    int ready[2], effect[2]; CHECK(pipe(ready) == 0 && pipe(effect) == 0);
    char descriptor[24]; snprintf(descriptor, sizeof(descriptor), "%d", effect[1]);
    child = fork(); CHECK(child >= 0);
    if (!child) {
        if (seized) {
            if (write(ready[1], "r", 1) != 1) _exit(2);
            while (!*go) __asm__ volatile("pause" ::: "memory");
        } else {
            if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) _exit(3);
            syscall(SYS_kill, getpid(), SIGSTOP);
        }
        char *args[] = {(char *)self, "--exec-child", descriptor, NULL};
        char *env[] = {NULL};
        syscall(SYS_execve, self, args, env); _exit(4);
    }
    close(ready[1]); close(effect[1]);
    unsigned long options = trace_exec ? PTRACE_O_TRACEEXEC : 0;
    if (seized) {
        char byte; CHECK(read(ready[0], &byte, 1) == 1);
        CHECK(request(PTRACE_SEIZE, 0, (void *)options) == 0 && request(PTRACE_INTERRUPT, 0, NULL) == 0);
        CHECK(stopped(SIGTRAP, 128)); *go = 1;
    } else {
        CHECK(stopped(SIGSTOP, 0));
        CHECK(request(PTRACE_SETOPTIONS, 0, (void *)options) == 0);
    }
    close(ready[0]);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0);
    if (trace_exec || !seized) {
        CHECK(stopped(SIGTRAP, trace_exec ? PTRACE_EVENT_EXEC : 0));
        siginfo_t info; CHECK(request(PTRACE_GETSIGINFO, 0, &info) == 0);
        CHECK(info.si_signo == SIGTRAP && info.si_pid == child && info.si_uid == getuid());
        CHECK(info.si_code == (trace_exec ? SIGTRAP | (PTRACE_EVENT_EXEC << 8) : SI_USER));
        if (trace_exec) {
            unsigned long old_pid; CHECK(request(PTRACE_GETEVENTMSG, 0, &old_pid) == 0 && old_pid == (unsigned long)child);
        }
        struct user_regs_struct regs; CHECK(request(PTRACE_GETREGS, 0, &regs) == 0);
        CHECK(regs.cs == 0x33 && regs.ss == 0x2b && regs.rip && regs.rsp);
        CHECK(regs.fs_base == 0 && regs.gs_base == 0 && regs.rdx == 0);
        unsigned char syscall_info[88];
        CHECK(request(PTRACE_GET_SYSCALL_INFO, sizeof(syscall_info), syscall_info) == 24 && syscall_info[0] == 0);
        CHECK(request(PTRACE_CONT, 0, NULL) == 0);
    }
    int status; CHECK(waitpid(child, &status, __WALL) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = 0; char output[8];
    CHECK(read(effect[0], output, sizeof(output)) == 4 && memcmp(output, "exec", 4) == 0);
    close(effect[0]); munmap((void *)go, 4096); return 0;
}
int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "--exec-child") == 0) {
        int fd = atoi(argv[2]);
        if (write(fd, "exec", 4) != 4) return 5;
        return 0;
    }
    puts("THEKERNEL_ABI_CASE ptrace-exec.raw-differential");
    struct sigaction action; memset(&action, 0, sizeof(action)); action.sa_handler = timeout_handler;
    CHECK(sigaction(SIGALRM, &action, NULL) == 0); alarm(15);
    for (int seized = 0; seized < 2; ++seized) {
        for (int trace_exec = 0; trace_exec < 2; ++trace_exec) if (run_case(argv[0], seized, trace_exec) != 0) return 1;
    }
    alarm(0);
    puts("THEKERNEL_ABI_ASSERT ptrace-exec.raw-differential EVENT_LEGACY_AND_SEIZED_EXEC_PROTOCOL pass");
    puts("THEKERNEL_ABI_RESULT ptrace-exec.raw-differential pass");
    puts("THEKERNEL_PTRACE_EXEC_OK"); return 0;
}
