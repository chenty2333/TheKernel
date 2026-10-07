#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t child;
static volatile sig_atomic_t timed_out;
static void timeout_handler(int signo) { (void)signo; timed_out = 1; }
static int fail(const char *name) {
    fprintf(stderr, "ptrace-listen: %s errno=%d timeout=%d\n", name, errno, (int)timed_out);
    if (child > 0) { kill(child, SIGKILL); waitpid(child, NULL, 0); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long request(long op, unsigned long addr, void *data) {
    return syscall(SYS_ptrace, op, child, addr, data);
}
static int stopped_as(int signo, int event) {
    int status;
    return waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == signo && (status >> 16) == event;
}
static int stopped(void) { return stopped_as(SIGTRAP, 128); }
int main(void) {
    puts("THEKERNEL_ABI_CASE ptrace-listen.raw-differential");
    volatile uint64_t *spin = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(spin != MAP_FAILED);
    int ready[2];
    CHECK(pipe(ready) == 0);
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = timeout_handler;
    CHECK(sigaction(SIGALRM, &action, NULL) == 0);
    alarm(10);
    child = fork(); CHECK(child >= 0);
    if (child == 0) {
        close(ready[0]);
        if (write(ready[1], "r", 1) != 1) _exit(2);
        for (;;) { ++*spin; __asm__ volatile ("pause" ::: "memory"); }
    }
    close(ready[1]); char byte;
    CHECK(read(ready[0], &byte, 1) == 1); close(ready[0]);
    CHECK(request(PTRACE_SEIZE, 0, NULL) == 0);
    CHECK(request(PTRACE_INTERRUPT, 0, NULL) == 0);
    CHECK(stopped());
    siginfo_t info;
    CHECK(request(PTRACE_GETSIGINFO, 0, &info) == 0 && info.si_signo == SIGTRAP && info.si_code == (SIGTRAP | (128 << 8)));
    CHECK(info.si_pid == child && info.si_uid == getuid());
    CHECK(request(PTRACE_LISTEN, 0, NULL) == 0);
    uint64_t before = *spin; usleep(10000); CHECK(*spin == before);
    struct user_regs_struct regs; errno = 0;
    CHECK(request(PTRACE_GETREGS, 0, &regs) == -1 && errno == ESRCH);
    int status;
    CHECK(waitpid(child, &status, WNOHANG) == 0);
    CHECK(request(PTRACE_INTERRUPT, 0, NULL) == 0);
    CHECK(stopped());
    CHECK(request(PTRACE_INTERRUPT, 0, NULL) == 0);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0);
    CHECK(stopped());
    CHECK(request(PTRACE_CONT, 0, NULL) == 0);
    CHECK(kill(child, SIGSTOP) == 0);
    CHECK(stopped_as(SIGSTOP, 0));
    CHECK(request(PTRACE_CONT, 0, (void *)(long)SIGSTOP) == 0);
    CHECK(stopped_as(SIGSTOP, 128));
    CHECK(request(PTRACE_GETSIGINFO, 0, &info) == 0 && info.si_signo == SIGSTOP && info.si_code == (SIGSTOP | (128 << 8)));
    CHECK(request(PTRACE_LISTEN, 0, NULL) == 0);
    CHECK(kill(child, SIGCONT) == 0);
    CHECK(stopped());
    CHECK(request(PTRACE_DETACH, 0, (void *)(long)SIGKILL) == 0);
    usleep(10000);
    CHECK(waitpid(child, &status, WNOHANG) == 0);
    CHECK(kill(child, SIGKILL) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
    child = 0; alarm(0); munmap((void *)spin, 4096);
    puts("THEKERNEL_ABI_ASSERT ptrace-listen.raw-differential LISTEN_RETRAP_AND_DEFERRED_INTERRUPT pass");
    puts("THEKERNEL_ABI_RESULT ptrace-listen.raw-differential pass");
    puts("THEKERNEL_PTRACE_LISTEN_OK");
    return 0;
}
