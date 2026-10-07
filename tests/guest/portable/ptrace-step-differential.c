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

extern const unsigned char step_breakpoint[], step_start[], step_syscall[], step_store[], step_next[];
__asm__(".text\n"
        ".global step_breakpoint, step_start, step_syscall, step_store, step_next\n"
        "step_breakpoint: int3\n"
        "step_start: nop\n"
        "step_syscall: syscall\n"
        "step_store: mov %rax,(%r12)\n"
        "step_next: nop\n"
        "mov $231,%eax\n xor %edi,%edi\n syscall\n ud2\n");
static pid_t child;
static volatile sig_atomic_t timed_out;
static void timeout_handler(int signo) { (void)signo; timed_out = 1; }
static int fail(const char *what) {
    fprintf(stderr, "ptrace-step: %s errno=%d timeout=%d\n", what, errno, (int)timed_out);
    if (child > 0) { kill(child, SIGKILL); waitpid(child, NULL, 0); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long request(long op, unsigned long addr, void *data) {
    return syscall(SYS_ptrace, op, child, addr, data);
}
static int stopped(int signal) {
    int status;
    return waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == signal;
}
static int spawn(long *result, const unsigned char *ip, int user_tf) {
    child = fork(); if (child < 0) return 0;
    if (!child) {
        if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) _exit(2);
        syscall(SYS_kill, getpid(), SIGSTOP); _exit(3);
    }
    if (!stopped(SIGSTOP)) return 0;
    struct user_regs_struct regs;
    if (request(PTRACE_GETREGS, 0, &regs) != 0) return 0;
    regs.rip = (unsigned long)step_breakpoint; regs.orig_rax = -1;
    if (request(PTRACE_SETREGS, 0, &regs) != 0 || request(PTRACE_CONT, 0, NULL) != 0 || !stopped(SIGTRAP)) return 0;
    if (request(PTRACE_GETREGS, 0, &regs) != 0) return 0;
    regs.rip = (unsigned long)ip; regs.rax = SYS_getpid;
    regs.orig_rax = -1; regs.r12 = (unsigned long)result;
    regs.eflags = (regs.eflags & ~0x100UL) | (user_tf ? 0x100UL : 0);
    return request(PTRACE_SETREGS, 0, &regs) == 0;
}
static int trace_stop_current(const unsigned char *ip, long value, int tf_visible, int code) {
    siginfo_t info;
    if (request(PTRACE_GETSIGINFO, 0, &info) != 0 || info.si_signo != SIGTRAP ||
        info.si_code != code || (unsigned long)info.si_addr != (unsigned long)ip) { fprintf(stderr, "step siginfo: signo=%d code=%d addr=%p expected=%p\n", info.si_signo, info.si_code, info.si_addr, ip); return 0; }
    struct user_regs_struct regs;
    if (request(PTRACE_GETREGS, 0, &regs) != 0 || regs.rip != (unsigned long)ip ||
        (long)regs.rax != value || !!(regs.eflags & 0x100) != tf_visible) { fprintf(stderr, "step regs: rip=%lx/%lx rax=%ld/%ld flags=%lx TF=%d\n", (unsigned long)regs.rip, (unsigned long)ip, (long)regs.rax, value, (unsigned long)regs.eflags, tf_visible); return 0; }
    unsigned char syscall_info[88]; memset(syscall_info, 0xa5, sizeof(syscall_info));
    if (request(PTRACE_GET_SYSCALL_INFO, sizeof(syscall_info), syscall_info) != 24 || syscall_info[0] != 0) return 0;
    return 1;
}
static int trace_stop(const unsigned char *ip, long value, int tf_visible, int code) {
    return stopped(SIGTRAP) && trace_stop_current(ip, value, tf_visible, code);
}
static int exited(void) {
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 0;
    child = 0; return 1;
}
int main(void) {
    puts("THEKERNEL_ABI_CASE ptrace-step.raw-differential");
    long *result = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(result != MAP_FAILED);
    struct sigaction action; memset(&action, 0, sizeof(action)); action.sa_handler = timeout_handler;
    CHECK(sigaction(SIGALRM, &action, NULL) == 0); alarm(10);
    CHECK(spawn(result, step_start, 0));
    CHECK(request(PTRACE_SINGLESTEP, 0, NULL) == 0 && trace_stop(step_syscall, SYS_getpid, 0, TRAP_TRACE));
    CHECK(*result == 0);
    CHECK(request(PTRACE_SINGLESTEP, 0, NULL) == 0 && trace_stop(step_store, child, 0, TRAP_BRKPT));
    CHECK(*result == 0); /* syscall step must stop before following MOV. */
    long expected_pid = child;
    CHECK(request(PTRACE_CONT, 0, NULL) == 0 && exited());
    CHECK(*result == expected_pid);
    *result = 0;
    CHECK(spawn(result, step_store, 0));
    CHECK(request(PTRACE_SINGLESTEP, 0, NULL) == 0 && trace_stop(step_next, SYS_getpid, 0, TRAP_TRACE));
    CHECK(*result == SYS_getpid);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0 && exited());
    *result = 0;
    CHECK(spawn(result, step_syscall, 0));
    CHECK(request(PTRACE_SETOPTIONS, 0, (void *)PTRACE_O_TRACESYSGOOD) == 0);
    CHECK(request(PTRACE_SYSEMU_SINGLESTEP, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    struct user_regs_struct regs;
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0 && regs.orig_rax == SYS_getpid && (long)regs.rax == -ENOSYS);
    regs.rax = 555; CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_SYSEMU_SINGLESTEP, 0, NULL) == 0 && stopped(SIGTRAP));
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0);
    /* Native x86 Linux also reports one same-IP TRAP_TRACE after resuming this
     * syscall entry. Admit only that observed stop, and prove it did not store. */
    if (regs.rip == (unsigned long)step_store) {
        CHECK(*result == 0 && trace_stop_current(step_store, 555, 0, TRAP_TRACE));
        CHECK(request(PTRACE_SYSEMU_SINGLESTEP, 0, NULL) == 0 && stopped(SIGTRAP));
    }
    CHECK(trace_stop_current(step_next, 555, 0, TRAP_TRACE));
    CHECK(*result == 555); /* No syscall-exit stop; the next instruction really ran. */
    CHECK(request(PTRACE_CONT, 0, NULL) == 0 && exited());
    *result = 0;
    CHECK(spawn(result, step_start, 1));
    CHECK(request(PTRACE_CONT, 0, NULL) == 0 && trace_stop(step_syscall, SYS_getpid, 1, TRAP_TRACE));
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0); regs.eflags &= ~0x100UL;
    CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0 && exited());
    CHECK(*result > 0 && *result != 555);
    alarm(0); munmap(result, 4096);
    puts("THEKERNEL_ABI_ASSERT ptrace-step.raw-differential INSTRUCTION_SYSCALL_AND_EMULATION_STEPS pass");
    puts("THEKERNEL_ABI_RESULT ptrace-step.raw-differential pass");
    puts("THEKERNEL_PTRACE_STEP_OK"); return 0;
}
