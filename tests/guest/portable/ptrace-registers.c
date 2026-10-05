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
        uint64_t vector[2] = { 12, 34 };
        __asm__ volatile ("movdqu %0, %%xmm15" : : "m"(vector) : "xmm15");
        __asm__ volatile ("syscall" : "=a"(result) : "a"((long)SYS_kill), "D"((long)self), "S"((long)SIGSTOP) : "rcx", "r11", "memory");
        __asm__ volatile ("movdqu %%xmm15, %0" : "=m"(vector));
        unsigned short cs, ss, ds, es, fs, gs;
        __asm__ volatile ("mov %%cs, %0" : "=r"(cs));
        __asm__ volatile ("mov %%ss, %0" : "=r"(ss));
        __asm__ volatile ("mov %%ds, %0" : "=r"(ds));
        __asm__ volatile ("mov %%es, %0" : "=r"(es));
        __asm__ volatile ("mov %%fs, %0" : "=r"(fs));
        __asm__ volatile ("mov %%gs, %0" : "=r"(gs));
        sigset_t mask;
        if (sigprocmask(SIG_BLOCK, NULL, &mask) != 0) _exit(3);
        _exit(cs == 0x33 && ss == 0x2b && ds == 0x2b && es == 0x2b && fs == 0x2b && gs == 0x2b && result == 77 && word == 22 && vector[0] == 33 && vector[1] == 34 &&
              sigismember(&mask, SIGUSR1) && !sigismember(&mask, SIGKILL) && !sigismember(&mask, SIGSTOP) ? 0 : 2);
    }
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status));
    struct user_regs_struct regs, original;
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0);
    original = regs;
    CHECK(regs.cs == 0x33 && regs.ss == 0x2b);
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
    struct user_fpregs_struct fp;
    CHECK(request(PTRACE_GETFPREGS, 0, &fp) == 0);
    uint64_t vector[2];
    memcpy(vector, &fp.xmm_space[60], sizeof(vector));
    CHECK(vector[0] == 12 && vector[1] == 34);
    unsigned int mxcsr = fp.mxcsr;
    fp.mxcsr = ~0U; errno = 0;
    CHECK(request(PTRACE_SETFPREGS, 0, &fp) == -1 && errno == EINVAL);
    fp.mxcsr = mxcsr; vector[0] = 33;
    memcpy(&fp.xmm_space[60], vector, sizeof(vector));
    CHECK(request(PTRACE_SETFPREGS, 0, &fp) == 0);
    iov.iov_base = &fp; iov.iov_len = sizeof(fp);
    CHECK(request(PTRACE_GETREGSET, 2, &iov) == 0 && iov.iov_len == sizeof(fp));
    memcpy(vector, &fp.xmm_space[60], sizeof(vector));
    CHECK(vector[0] == 33 && vector[1] == 34);
    unsigned char xstate[32768];
    iov.iov_base = xstate; iov.iov_len = sizeof(xstate);
    long xs = request(PTRACE_GETREGSET, 0x202, &iov);
    if (xs == 0) {
        CHECK(iov.iov_len >= 576 && iov.iov_len <= sizeof(xstate));
        CHECK(request(PTRACE_SETREGSET, 0x202, &iov) == 0);
        uint64_t invalid = UINT64_MAX;
        memcpy(xstate + 512, &invalid, sizeof(invalid)); errno = 0;
        CHECK(request(PTRACE_SETREGSET, 0x202, &iov) == -1 && errno == EINVAL);
        iov.iov_len = 8; errno = 0;
        CHECK(request(PTRACE_SETREGSET, 0x202, &iov) == -1 && errno == EFAULT);
    } else { CHECK(errno == ENODEV); }
    uint64_t mask = UINT64_MAX;
    CHECK(request(0x420b, 8, &mask) == 0);
    mask = 0;
    CHECK(request(0x420a, 8, &mask) == 0);
    CHECK(mask == (UINT64_MAX & ~(1ULL << (SIGKILL - 1)) & ~(1ULL << (SIGSTOP - 1))));
    errno = 0;
    CHECK(request(0x420a, 7, NULL) == -1 && errno == EINVAL);
    CHECK(kill(child, SIGUSR1) == 0);
    union sigval value = { .sival_int = 55 };
    CHECK(sigqueue(child, SIGRTMIN + 1, value) == 0);
    value.sival_int = 66;
    CHECK(sigqueue(child, SIGRTMIN, value) == 0);
    CHECK(syscall(SYS_tgkill, child, child, SIGUSR2) == 0);
    struct { uint64_t off; uint32_t flags; int32_t nr; } peek = { 0, 1, 8 };
    siginfo_t infos[8];
    CHECK(request(0x4209, (unsigned long)&peek, infos) == 3);
    CHECK(infos[0].si_signo == SIGUSR1 && infos[1].si_signo == SIGRTMIN + 1 && infos[2].si_signo == SIGRTMIN);
    CHECK(infos[1].si_value.sival_int == 55 && infos[2].si_value.sival_int == 66);
    peek.off = 1; peek.nr = 1;
    CHECK(request(0x4209, (unsigned long)&peek, infos) == 1 && infos[0].si_signo == SIGRTMIN + 1);
    peek.off = 0; peek.flags = 0; peek.nr = 8;
    CHECK(request(0x4209, (unsigned long)&peek, infos) == 1 && infos[0].si_signo == SIGUSR2);
    CHECK(request(0x4209, (unsigned long)&peek, infos) == 1 && infos[0].si_signo == SIGUSR2);
    peek.flags = 2; errno = 0;
    CHECK(request(0x4209, (unsigned long)&peek, infos) == -1 && errno == EINVAL);
    peek.flags = 0; peek.nr = -1; errno = 0;
    CHECK(request(0x4209, (unsigned long)&peek, infos) == -1 && errno == EINVAL);
    peek.off = UINT64_MAX; peek.nr = 1;
    CHECK(request(0x4209, (unsigned long)&peek, infos) == 0);
    regs.ds = 0x2b; regs.es = 0x2b; regs.fs = 0x2b; regs.gs = 0x2b;
    regs.rax = 66;
    CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_POKEUSER, offsetof(struct user_regs_struct, rax), (void *)77) == 0);
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0 && regs.rax == 77);
    CHECK(request(PTRACE_DETACH, 0, NULL) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = 0;
    child = fork(); CHECK(child >= 0);
    if (!child) {
        if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0)) _exit(1);
        raise(SIGSTOP); word = 44; word = 55; _exit(word != 55);
    }
    CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status));
    CHECK(request(PTRACE_POKEUSER, offsetof(struct user, u_debugreg[0]), (void *)&word) == 0);
    CHECK(request(PTRACE_POKEUSER, offsetof(struct user, u_debugreg[7]), (void *)0x90001) == 0);
    for (unsigned long expected = 44; expected <= 55; expected += 11) {
        CHECK(request(PTRACE_CONT, 0, NULL) == 0);
        CHECK(waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == SIGTRAP);
        siginfo_t info; CHECK(request(PTRACE_GETSIGINFO, 0, &info) == 0);
        CHECK(info.si_code == TRAP_HWBKPT);
        struct user_regs_struct trap_regs; CHECK(request(PTRACE_GETREGS, 0, &trap_regs) == 0);
        CHECK((uintptr_t)info.si_addr == trap_regs.rip);
        unsigned long actual = 0;
        CHECK(request(PTRACE_PEEKDATA, (unsigned long)&word, &actual) == 0 && actual == expected);
        CHECK(request(PTRACE_PEEKUSER, offsetof(struct user, u_debugreg[6]), &actual) == 0 && (actual & 1));
    }
    CHECK(request(PTRACE_POKEUSER, offsetof(struct user, u_debugreg[7]), NULL) == 0);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status)); child = 0;
    puts("PTRACE_REGISTERS_OK");
    return 0;
}
