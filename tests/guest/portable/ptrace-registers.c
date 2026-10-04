#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stddef.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile unsigned long word = 11;
static pid_t child;
static int fail(const char *name) {
    fprintf(stderr, "ptrace-registers: %s errno=%d\n", name, errno);
    if (child > 0) { kill(child, SIGKILL); waitpid(child, NULL, 0); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long request(long op, unsigned long addr, void *data) {
    return syscall(SYS_ptrace, op, child, addr, data);
}
int main(void) {
    int status;
    child = fork();
    CHECK(child >= 0);
    if (child == 0) {
        if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0)) _exit(1);
        long result;
        pid_t self = getpid();
        __asm__ volatile ("syscall" : "=a"(result) : "a"((long)SYS_kill), "D"((long)self), "S"((long)SIGSTOP) : "rcx", "r11", "memory");
        _exit(result == 77 && word == 22 ? 0 : 2);
    }
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status));
    struct user_regs_struct regs, original;
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0);
    original = regs;
    CHECK(regs.orig_rax == SYS_kill && regs.rip && regs.rsp);
    uint64_t first = 0;
    struct iovec iov = { &first, sizeof(first) };
    CHECK(request(PTRACE_GETREGSET, 1, &iov) == 0 && iov.iov_len == 8 && first == regs.r15);
    iov.iov_base = NULL; iov.iov_len = 0;
    CHECK(request(PTRACE_GETREGSET, 1, &iov) == 0 && iov.iov_len == 0);
    iov.iov_len = 7; errno = 0;
    CHECK(request(PTRACE_GETREGSET, 1, &iov) == -1 && errno == EINVAL);
    CHECK(request(PTRACE_PEEKUSER, offsetof(struct user_regs_struct, rip), &first) == 0 && first == regs.rip);
    CHECK(request(PTRACE_PEEKDATA, (unsigned long)&word, &first) == 0 && first == 11);
    CHECK(request(PTRACE_POKEDATA, (unsigned long)&word, (void *)22) == 0);
    regs.fs_base = 1UL << 47; errno = 0;
    CHECK(request(PTRACE_SETREGS, 0, &regs) == -1 && errno == EIO);
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0 && memcmp(&original, &regs, sizeof(regs)) == 0);
    regs.rax = 66;
    CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_POKEUSER, offsetof(struct user_regs_struct, rax), (void *)77) == 0);
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0 && regs.rax == 77);
    CHECK(request(PTRACE_DETACH, 0, NULL) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = 0;
    puts("PTRACE_REGISTERS_OK");
    return 0;
}
