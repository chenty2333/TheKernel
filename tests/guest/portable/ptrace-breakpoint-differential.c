#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
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

extern int breakpoint_target(void);
__asm__(".text\n .global breakpoint_target\n breakpoint_target: mov $7,%eax\n ret\n");
static pid_t child;
static volatile sig_atomic_t timed_out;
static void timeout_handler(int signo) { (void)signo; timed_out = 1; }
static int fail(const char *what) {
    fprintf(stderr, "ptrace-breakpoint: %s errno=%d timeout=%d\n", what, errno, (int)timed_out);
    if (child > 0) { kill(child, SIGKILL); waitpid(child, NULL, __WALL); }
    return 1;
}
#define CHECK(x) do { if (!(x)) return fail(#x); } while (0)
static long request(long op, unsigned long addr, void *data) { return syscall(SYS_ptrace, op, child, addr, data); }
static int stopped(int signo) {
    int status;
    return waitpid(child, &status, __WALL) == child && WIFSTOPPED(status) && WSTOPSIG(status) == signo;
}
static int file_offset(int fd, unsigned long address, off_t *offset) {
    Elf64_Ehdr header;
    if (pread(fd, &header, sizeof(header), 0) != sizeof(header) || header.e_phentsize != sizeof(Elf64_Phdr)) return 0;
    for (int i = 0; i < header.e_phnum; ++i) {
        Elf64_Phdr program;
        if (pread(fd, &program, sizeof(program), header.e_phoff + i * sizeof(program)) != sizeof(program)) return 0;
        if (program.p_type == PT_LOAD && address >= program.p_vaddr && address - program.p_vaddr + 8 <= program.p_filesz) {
            *offset = program.p_offset + address - program.p_vaddr; return 1;
        }
    }
    return 0;
}
int main(int argc, char **argv) {
    (void)argc;
    puts("THEKERNEL_ABI_CASE ptrace-breakpoint.raw-differential");
    struct sigaction action; memset(&action, 0, sizeof(action)); action.sa_handler = timeout_handler;
    CHECK(sigaction(SIGALRM, &action, NULL) == 0); alarm(15);
    unsigned long address = (unsigned long)breakpoint_target;
    unsigned long original; memcpy(&original, (void *)address, sizeof(original));
    CHECK((original & 0xff) == 0xb8);
    int fd = open(argv[0], O_RDONLY); CHECK(fd >= 0);
    off_t offset; CHECK(file_offset(fd, address, &offset));
    off_t page_offset = offset & ~4095L;
    unsigned char *shared = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_SHARED, fd, page_offset);
    CHECK(shared != MAP_FAILED);
    unsigned long shared_address = (unsigned long)shared + (offset - page_offset);
    child = fork(); CHECK(child >= 0);
    if (!child) {
        if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) _exit(2);
        syscall(SYS_kill, getpid(), SIGSTOP);
        _exit(breakpoint_target() == 7 ? 0 : 3);
    }
    CHECK(stopped(SIGSTOP));
    errno = 0;
    CHECK(request(PTRACE_POKETEXT, shared_address, (void *)((original & ~0xffUL) | 0xcc)) == -1 && errno == EIO);
    CHECK(request(PTRACE_POKETEXT, address, (void *)((original & ~0xffUL) | 0xcc)) == 0);
    unsigned long peeked;
    CHECK(request(PTRACE_PEEKTEXT, address, &peeked) == 0 && (peeked & 0xff) == 0xcc);
    unsigned long parent_word, file_word, shared_word;
    memcpy(&parent_word, (void *)address, sizeof(parent_word));
    memcpy(&shared_word, (void *)shared_address, sizeof(shared_word));
    CHECK(pread(fd, &file_word, sizeof(file_word), offset) == sizeof(file_word));
    CHECK(parent_word == original && file_word == original && shared_word == original);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0 && stopped(SIGTRAP));
    struct user_regs_struct regs; CHECK(request(PTRACE_GETREGS, 0, &regs) == 0 && regs.rip == address + 1);
    siginfo_t info; CHECK(request(PTRACE_GETSIGINFO, 0, &info) == 0 && info.si_signo == SIGTRAP && info.si_code == SI_KERNEL);
    CHECK(request(PTRACE_POKEDATA, address, (void *)original) == 0);
    regs.rip = address; CHECK(request(PTRACE_SETREGS, 0, &regs) == 0);
    CHECK(request(PTRACE_SINGLESTEP, 0, NULL) == 0 && stopped(SIGTRAP));
    CHECK(request(PTRACE_GETREGS, 0, &regs) == 0 && regs.rip == address + 5 && regs.rax == 7);
    CHECK(request(PTRACE_GETSIGINFO, 0, &info) == 0 && info.si_code == TRAP_TRACE);
    CHECK(request(PTRACE_CONT, 0, NULL) == 0);
    int status; CHECK(waitpid(child, &status, __WALL) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = 0; alarm(0); close(fd); munmap(shared, 4096);
    CHECK(breakpoint_target() == 7);
    puts("THEKERNEL_ABI_ASSERT ptrace-breakpoint.raw-differential PRIVATE_TEXT_PATCH_TRAP_STEP_AND_RESTORE pass");
    puts("THEKERNEL_ABI_RESULT ptrace-breakpoint.raw-differential pass");
    puts("THEKERNEL_PTRACE_BREAKPOINT_OK"); return 0;
}
