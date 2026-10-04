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

/* Exact bytes of the native GET_SYSCALL_INFO ABI, independent of libc headers. */
struct syscall_info {
    uint8_t op, reserved;
    uint16_t flags;
    uint32_t arch;
    uint64_t ip, sp;
    uint64_t payload[8];
};
struct results { long replaced, error, skipped, emulated; };
static pid_t child;
static volatile sig_atomic_t timed_out;
static void timeout_handler(int signo) { (void)signo; timed_out = 1; }
static int fail(const char *what) {
    fprintf(stderr, "ptrace-syscall: %s errno=%d timeout=%d\n", what, errno, (int)timed_out);
    if (child > 0) { kill(child, SIGKILL); waitpid(child, NULL, 0); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long raw(long nr, long a, long b, long c) {
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}
static long request(long op, unsigned long addr, void *data) {
    return syscall(SYS_ptrace, op, child, addr, data);
}
static int stopped(int signal) {
    int status;
    return waitpid(child, &status, 0) == child && WIFSTOPPED(status) && WSTOPSIG(status) == signal && (status >> 16) == 0;
}
static int info_is(struct syscall_info *info, int op, long nr_or_result, int error) {
    memset(info, 0xa5, sizeof(*info));
    long size = request(PTRACE_GET_SYSCALL_INFO, sizeof(*info), info);
    if (size != (op == 1 ? 80 : op == 2 ? 33 : 24) || info->op != op ||
        info->arch != 0xc000003e || !info->ip || !info->sp || info->reserved || info->flags) return 0;
    if (op && (long)info->payload[0] != nr_or_result) return 0;
    return op != 2 || ((unsigned char *)info)[32] == error;
}
int main(void) {
    puts("THEKERNEL_ABI_CASE ptrace-syscall.raw-differential");
    struct results *results = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(results != MAP_FAILED);
    struct sigaction action;
    memset(&action, 0, sizeof(action)); action.sa_handler = timeout_handler;
    CHECK(sigaction(SIGALRM, &action, NULL) == 0); alarm(10);
    int fds[2]; CHECK(pipe(fds) == 0);
    child = fork(); CHECK(child >= 0);
    if (!child) {
        if (raw(SYS_ptrace, PTRACE_TRACEME, 0, 0) != 0) _exit(2);
        raw(SYS_kill, raw(SYS_getpid, 0, 0, 0), SIGSTOP, 0);
        raw(SYS_write, fds[1], (long)"abcdef", 6);
        results->replaced = raw(SYS_getpid, 0, 0, 0);
        results->error = raw(SYS_close, -1, 0, 0);
        results->skipped = raw(SYS_write, fds[1], (long)"skip", 4);
        results->emulated = raw(SYS_write, fds[1], (long)"emu", 3);
        raw(SYS_exit_group, 0, 0, 0); _exit(3);
    }
    CHECK(stopped(SIGSTOP));
    struct syscall_info info;
    CHECK(info_is(&info, 0, 0, 0));
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0);
    CHECK(stopped(SIGTRAP));
    CHECK(info_is(&info, 0, 0, 0)); /* Without TRACESYSGOOD, never guess ENTRY. */
    CHECK(request(PTRACE_SETOPTIONS, 0, (void *)PTRACE_O_TRACESYSGOOD) == 0);
    CHECK(info_is(&info, 0, 0, 0)); /* The published stop keeps its provenance. */
    struct user_regs_struct regs;
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0);
    CHECK(regs.orig_rax == SYS_write && (long)regs.rax == -ENOSYS && regs.rdi == (unsigned)fds[1] && regs.rdx == 6);
    regs.rdx = 3; CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 2, 3, 0));
    siginfo_t siginfo;
    CHECK(request(PTRACE_GETSIGINFO, 0, &siginfo) == 0 && siginfo.si_signo == SIGTRAP && siginfo.si_code == (SIGTRAP | 0x80));
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 1, SYS_getpid, 0));
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0);
    regs.orig_rax = SYS_getppid; CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 2, getpid(), 0));
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0); regs.rax = 77;
    CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 1, SYS_close, 0) && (long)info.payload[1] == -1);
    struct syscall_info short_info; memset(&short_info, 0xa5, sizeof(short_info));
    CHECK(request(PTRACE_GET_SYSCALL_INFO, 1, &short_info) == 80 && short_info.op == 1 && short_info.reserved == 0xa5);
    CHECK(request(PTRACE_GET_SYSCALL_INFO, 0, NULL) == 80);
    unsigned long message;
    CHECK(request(PTRACE_GETEVENTMSG, 0, &message) == 0 && message == 1);
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 2, -EBADF, 1));
    CHECK(request(PTRACE_GETEVENTMSG, 0, &message) == 0 && message == 2);
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 1, SYS_write, 0) && info.payload[3] == 4);
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0); regs.orig_rax = -1; regs.rax = 55;
    CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_SYSCALL, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 2, 55, 0));
    CHECK(request(PTRACE_SYSEMU, 0, NULL) == 0 && stopped(SIGTRAP | 0x80));
    CHECK(info_is(&info, 1, SYS_write, 0) && info.payload[3] == 3);
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0); regs.rax = 66;
    CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0); /* CONT must not execute emulated write. */
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = 0; alarm(0);
    close(fds[1]); char buffer[20];
    CHECK(read(fds[0], buffer, sizeof(buffer)) == 3 && memcmp(buffer, "abc", 3) == 0);
    CHECK(results->replaced == 77 && results->error == -EBADF && results->skipped == 55 && results->emulated == 66);
    close(fds[0]); munmap(results, 4096);
    puts("THEKERNEL_ABI_ASSERT ptrace-syscall.raw-differential ENTRY_EXIT_MUTATION_AND_EMULATION pass");
    puts("THEKERNEL_ABI_RESULT ptrace-syscall.raw-differential pass");
    puts("THEKERNEL_PTRACE_SYSCALL_OK");
    return 0;
}
