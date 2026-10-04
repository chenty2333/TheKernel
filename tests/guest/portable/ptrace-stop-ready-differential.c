#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t child;
static volatile sig_atomic_t timed_out;
static void timeout_handler(int signo) { (void)signo; timed_out = 1; }
static int fail(const char *what) {
    fprintf(stderr, "ptrace-stop-ready: %s errno=%d timeout=%d\n", what, errno, (int)timed_out);
    if (child > 0) { kill(child, SIGKILL); waitpid(child, NULL, __WALL); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long request(long op) { return syscall(SYS_ptrace, op, child, 0, 0); }
static int one_case(void) {
    volatile int *go = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(go != MAP_FAILED); *go = 0;
    int ready[2], effect[2]; CHECK(pipe(ready) == 0 && pipe(effect) == 0);
    CHECK(fcntl(effect[0], F_SETFL, O_NONBLOCK) == 0);
    child = fork(); CHECK(child >= 0);
    if (!child) {
        if (write(ready[1], "r", 1) != 1) _exit(2);
        while (!*go) __asm__ volatile("pause" ::: "memory");
        if (write(effect[1], "ran", 3) != 3) _exit(3);
        _exit(0);
    }
    close(ready[1]); close(effect[1]); char byte;
    CHECK(read(ready[0], &byte, 1) == 1); close(ready[0]);
    CHECK(request(PTRACE_SEIZE) == 0 && request(PTRACE_INTERRUPT) == 0);
    int status;
    CHECK(waitpid(child, &status, __WALL) == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGTRAP && (status >> 16) == 128);
    /* NO GETREGS, GETSIGINFO, PEEK or other inactivity barrier here. A reported
     * stop itself promises that this release cannot execute the next write. */
    *go = 1;
    usleep(10000);
    char buffer[8]; errno = 0;
    CHECK(read(effect[0], buffer, sizeof(buffer)) == -1 && errno == EAGAIN);
    CHECK(request(PTRACE_CONT) == 0);
    CHECK(waitpid(child, &status, __WALL) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = 0;
    CHECK(read(effect[0], buffer, sizeof(buffer)) == 3 && memcmp(buffer, "ran", 3) == 0);
    close(effect[0]); munmap((void *)go, 4096); return 0;
}
int main(void) {
    puts("THEKERNEL_ABI_CASE ptrace-stop-ready.raw-differential");
    struct sigaction action; memset(&action, 0, sizeof(action)); action.sa_handler = timeout_handler;
    CHECK(sigaction(SIGALRM, &action, NULL) == 0); alarm(15);
    for (int round = 0; round < 16; ++round) if (one_case() != 0) return 1;
    alarm(0);
    puts("THEKERNEL_ABI_ASSERT ptrace-stop-ready.raw-differential REPORT_PRECEDES_NO_FURTHER_USER_EFFECTS pass");
    puts("THEKERNEL_ABI_RESULT ptrace-stop-ready.raw-differential pass");
    puts("THEKERNEL_PTRACE_STOP_READY_OK"); return 0;
}
