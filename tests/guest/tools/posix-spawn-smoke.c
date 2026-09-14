#define _GNU_SOURCE

/* TheKernel guest: can a process start a child the way glibc's posix_spawn does?
 *
 * This exists because of a specific piece of reconnaissance, not as a general
 * smoke test.  A distribution C compiler driver does not compile anything
 * itself: it launches `cc1`, `as` and `collect2` through glibc's posix_spawn,
 * which on x86_64 Linux uses
 *
 *     clone3({flags = CLONE_VM | CLONE_VFORK | CLONE_CLEAR_SIGHAND,
 *             stack = <caller-supplied>, stack_size = 0x9000})
 *
 * That is vfork semantics -- the parent is suspended and the child shares its
 * address space -- plus a stack the caller supplies and an explicit instruction
 * to clear the signal dispositions the child inherits.  The kernel implements
 * all of it, but no guest case had ever run this shape: the vfork case in the
 * portable set exercises bare vfork() with the child's own stack, which is a
 * different code path.  Since the alternative was discovering it after staging
 * eighty megabytes of compiler, it is checked here first, with no payload.
 *
 * Two independent checks, because they can fail separately:
 *
 *   1. the raw clone3 shape above, called directly.  This is the kernel
 *      contract, with nothing between the test and the syscall.
 *   2. glibc's posix_spawnp, which is what a compiler actually calls.  It is
 *      the same primitive plus glibc's own bookkeeping, and a failure here with
 *      (1) passing would point at glibc's setup rather than at the syscall.
 *
 * The child in both checks reports through a pipe and exits with a chosen
 * status, so "the child ran" is never inferred from the parent's return value
 * alone: a parent that returned success without the child doing anything would
 * still fail.
 *
 * Stable markers:
 *   THEKERNEL_POSIX_SPAWN_OK
 *   THEKERNEL_POSIX_SPAWN_FAIL <operation> condition=<c> k=v ... errno=<n> (<message>)
 *   THEKERNEL_POSIX_SPAWN_<STAGE> k=v ...     greppable progress
 */

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <spawn.h>
#include <sys/syscall.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "the posix_spawn probe is x86_64-only"
#endif

/* clone3 is newer than some of the headers this is built against -- it is
 * compiled once with the host glibc and once with the guest musl -- so the
 * values that a header may not define are spelled out here from the kernel's
 * UAPI.  The ones a header does define are used as they come, because getting
 * them wrong silently would be worse than a compile error. */
#if !defined(SYS_clone3)
#define SYS_clone3 435
#endif
#if !defined(CLONE_CLEAR_SIGHAND)
#define CLONE_CLEAR_SIGHAND 0x100000000ULL
#endif
#define CLONE3_ARGS_SIZE 88U

/* The clone3 argument struct, spelled out rather than taken from a header: the
 * size is checked by the kernel against the value passed beside it, and the
 * field offsets are the ABI being tested. */
struct clone3_args {
    uint64_t flags;
    uint64_t pidfd;
    uint64_t child_tid;
    uint64_t parent_tid;
    uint64_t exit_signal;
    uint64_t stack;
    uint64_t stack_size;
    uint64_t tls;
    uint64_t set_tid;
    uint64_t set_tid_size;
    uint64_t cgroup;
};

/* The stack glibc hands a vfork child.  Not a guess: 0x9000 is the size
 * posix_spawn passes on x86_64, and a child that only calls execve needs
 * almost none of it -- but the kernel still has to accept the pointer. */
#define SPAWN_STACK_BYTES 0x9000U

/* The child's exit status, chosen so it cannot be confused with a crash or with
 * the parent's own status. */
#define CHILD_STATUS 42

/* Byte the child writes to prove it reached its own code in the parent's
 * address space.  CLONE_VM means the child shares the parent's memory, so a
 * shared variable is not evidence -- only a write to a file descriptor is. */
#define CHILD_TOKEN 'S' 

/* ------------------------------------------------------------------ output */

static void emit(const char *format, ...) __attribute__((format(printf, 1, 2)));
static void emit(const char *format, ...)
{
    va_list arguments;

    va_start(arguments, format);
    vfprintf(stdout, format, arguments);
    va_end(arguments);
    fputc('\n', stdout);
    fflush(stdout);
}

static void fail(const char *operation, const char *condition, const char *format, ...)
    __attribute__((format(printf, 3, 4)));
static void fail(const char *operation, const char *condition, const char *format, ...)
{
    va_list arguments;
    int saved = errno;

    fprintf(stdout, "THEKERNEL_POSIX_SPAWN_FAIL %s condition=%s ", operation, condition);
    va_start(arguments, format);
    vfprintf(stdout, format, arguments);
    va_end(arguments);
    fprintf(stdout, " errno=%d (%s)\n", saved, strerror(saved));
    fflush(stdout);
}

/* CLONE_VM means the child shares the parent's memory, so this is how the child
 * reports why it could not write its token.  Without it a child that failed
 * before the write is indistinguishable from one that never ran: both leave the
 * parent reading end-of-file, which is exactly the ambiguity this probe exists
 * to remove. */
static volatile int child_write_errno;

/* The descriptor the child must write to.
 *
 * CLONE_VM means the child shares the parent's memory, so this is visible to
 * both -- written before the clone, read after it.  Passing a value to a clone
 * child through memory rather than through a register is the difference between
 * a probe that tests the kernel and one that tests the compiler: the child
 * begins with no frame and no usable stack, so nothing may depend on how a
 * compiler chose to allocate a register. */
static int child_write_fd;

/* The child's body.  Reached only from the assembly below, which is why the
 * compiler cannot see a call to it. */
__attribute__((used)) static void child_body(void)
{
    unsigned char byte = CHILD_TOKEN;
    ssize_t written = write(child_write_fd, &byte, 1);

    if (written != 1) {
        child_write_errno = errno != 0 ? errno : EIO;
    }
    _exit(CHILD_STATUS);
}

/* Entered with the child's stack already in place and nothing on it. */
__attribute__((noreturn, naked, used)) static void child_entry(void)
{
    __asm__ volatile("movq %rsp, %rdi\n\tcall child_body\n\thlt");
}

/* clone3, called the way a C library has to call it.
 *
 * This is the whole point of the probe and the reason it is written in assembly
 * rather than as `syscall(SYS_clone3, ...)`.  The child must start running on
 * the stack named in the arguments, and it must never touch the parent's stack
 * -- but a C-level call returns through the parent's stack frame, and the
 * *caller's* frame is compiled around the assumption that the stack it returns
 * to is the one it was entered on.  Switching stacks after such a call returns
 * leaves the wrapper's own epilogue popping from a stack the child has already
 * abandoned, which faults at the exact top of the new stack and reads as a
 * kernel bug.  Measured while writing this: doing the switch after glibc's
 * `syscall()` returned gave SIGSEGV at si_addr = stack base + stack_size on
 * both an 0x9000 and a 1 MiB stack, with the faulting instruction the `ret` of
 * glibc's syscall wrapper.
 *
 * So the syscall, the test for the child, and the stack switch all happen here,
 * between one `syscall` instruction and the jump that follows it, with no
 * compiler-generated code in between.  This is also what glibc's own
 * posix_spawn does, for the same reason.
 *
 * Arguments: rdi = the argument struct, rsi = the child's stack top. */
__attribute__((naked)) static long raw_clone3(
    __attribute__((unused)) struct clone3_args *args,
    __attribute__((unused)) unsigned char *stack_top)
{
    __asm__ volatile(
        /* rbx is callee-saved and this function has no frame to store it in,
         * so it is kept on the parent's stack -- which is safe, because the
         * child abandons that stack before anything else happens. */
        "pushq %rbx\n\t"
        "movq %rsi, %rbx\n\t"
        "movl $435, %eax\n\t" /* SYS_clone3 */
        "movl $88, %esi\n\t"  /* sizeof(struct clone3_args) */
        "syscall\n\t"
        "testq %rax, %rax\n\t"
        "jne 1f\n\t"
        "movq %rbx, %rsp\n\t" /* the child: adopt the supplied stack */
        "jmp child_entry\n\t"
        "1:\n\t"
        "popq %rbx\n\t"
        "ret\n\t");
}

static int check_clone3(void)
{
    struct clone3_args args;
    unsigned char *stack;
    int pipe_fds[2];
    pid_t child;
    int status = 0;
    unsigned char token = 0;
    ssize_t got;

    if (pipe(pipe_fds) != 0) {
        return -1;
    }
    /* Allocated rather than a static array because the child runs on it and the
     * parent must not reuse it.  mmap-backed memory, so the stack is usable
     * from the child without any further setup. */
    stack = (unsigned char *)malloc(SPAWN_STACK_BYTES);
    if (stack == NULL) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return -1;
    }

    memset(&args, 0, sizeof(args));
    args.flags = CLONE_VM | CLONE_VFORK | CLONE_CLEAR_SIGHAND;
    /* The kernel grows the stack down from the top, so the pointer is the high
     * end and the size is what lies below it. */
    args.stack = (uint64_t)(uintptr_t)(stack + SPAWN_STACK_BYTES);
    args.stack_size = SPAWN_STACK_BYTES;
    args.exit_signal = SIGCHLD;

    child_write_fd = pipe_fds[1];

    long result = raw_clone3(&args, stack + SPAWN_STACK_BYTES);

    if (result < 0) {
        int saved = errno;

        free(stack);
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = saved;
        return -1;
    }
    /* There is no `if (result == 0)` branch here on purpose: in the child,
     * raw_clone3 never returns -- it jumps to the child body on the new stack.
     * A branch for it would be unreachable code that implies the stack switch
     * happens in C. */
    child = (pid_t)result;

    close(pipe_fds[1]);
    /* CLONE_VFORK means the parent was suspended until the child exited, so the
     * data is already in the pipe; a short read here would mean the vfork
     * semantics were not honoured. */
    got = read(pipe_fds[0], &token, 1);
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
        continue;
    }
    close(pipe_fds[0]);
    free(stack);

    if (got != 1 || token != CHILD_TOKEN) {
        emit("THEKERNEL_POSIX_SPAWN_CLONE3_IO got=%zd token=0x%02x child_errno=%d (%s)",
             got, token, child_write_errno, strerror(child_write_errno));
        errno = child_write_errno != 0 ? child_write_errno : EIO;
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != CHILD_STATUS) {
        emit("THEKERNEL_POSIX_SPAWN_CLONE3_WAIT status=%d exited=%d code=%d", status,
             WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : -1);
        errno = ECHILD;
        return -1;
    }
    return 0;
}

/* ------------------------------------------------ check 2: glibc's wrapper */

/* posix_spawnp runs a program from PATH.  The child here is /bin/sh, which the
 * image already has for every payload, and the program it runs writes to the
 * same pipe the parent reads.  Using the shell rather than a purpose-built
 * helper keeps the case payload-free. */
static int check_posix_spawnp(void)
{
    posix_spawn_file_actions_t actions;
    posix_spawnattr_t attributes;
    char *const argv[] = { (char *)"sh", (char *)"-c",
                           (char *)"printf '<' ; exit " "42", NULL };
    char *const envp[] = { (char *)"PATH=/bin:/usr/bin:/sbin:/usr/sbin", NULL };
    int pipe_fds[2];
    pid_t child;
    int status = 0;
    char buffer[16];
    ssize_t got = 0;
    int result;

    if (pipe(pipe_fds) != 0) {
        return -1;
    }
    result = posix_spawn_file_actions_init(&actions);
    if (result != 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = result;
        return -1;
    }
    /* The child writes to the pipe while the parent waits on it, so the
     * write end is inherited and the read end is not.  A child that inherited
     * the read end would keep the pipe open and the read below would never see
     * end-of-file. */
    (void)posix_spawn_file_actions_adddup2(&actions, pipe_fds[1], STDOUT_FILENO);
    (void)posix_spawn_file_actions_addclose(&actions, pipe_fds[0]);
    result = posix_spawnattr_init(&attributes);
    if (result == 0) {
        /* POSIX_SPAWN_SETSIGDEF with an empty set is what glibc's own callers
         * use to get CLONE_CLEAR_SIGHAND: the child must not inherit the
         * parent's handlers. */
        sigset_t empty;

        sigemptyset(&empty);
        (void)posix_spawnattr_setsigdefault(&attributes, &empty);
        (void)posix_spawnattr_setflags(&attributes, POSIX_SPAWN_SETSIGDEF);
    }

    result = posix_spawnp(&child, "sh", &actions, &attributes, argv, envp);
    posix_spawn_file_actions_destroy(&actions);
    posix_spawnattr_destroy(&attributes);
    close(pipe_fds[1]);
    if (result != 0) {
        close(pipe_fds[0]);
        errno = result;
        return -1;
    }

    /* Read until end of file: the child writes one byte and exits, so a read
     * that blocks forever would be a child that never ran or never closed the
     * pipe. */
    for (;;) {
        ssize_t bytes = read(pipe_fds[0], buffer + got, sizeof(buffer) - 1 - (size_t)got);

        if (bytes > 0) {
            got += bytes;
            if ((size_t)got >= sizeof(buffer) - 1) {
                break;
            }
            continue;
        }
        if (bytes < 0 && errno == EINTR) {
            continue;
        }
        break;
    }
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
        continue;
    }
    close(pipe_fds[0]);
    buffer[got] = '\0';

    if (got != 1 || buffer[0] != '<') {
        emit("THEKERNEL_POSIX_SPAWN_SPAWNP_IO got=%zd text=\"%s\"", got, buffer);
        errno = EIO;
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != CHILD_STATUS) {
        emit("THEKERNEL_POSIX_SPAWN_SPAWNP_WAIT status=%d exited=%d code=%d", status,
             WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : -1);
        errno = ECHILD;
        return -1;
    }
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    emit("THEKERNEL_POSIX_SPAWN_INTERFACE clone3_flags=0x%llx stack_size=0x%x",
         (unsigned long long)(CLONE_VM | CLONE_VFORK | CLONE_CLEAR_SIGHAND),
         SPAWN_STACK_BYTES);

    if (check_clone3() != 0) {
        fail("clone3-vfork-shape", "raw-clone3-child-ran",
             "flags=0x%llx",
             (unsigned long long)(CLONE_VM | CLONE_VFORK | CLONE_CLEAR_SIGHAND));
        return 1;
    }
    emit("THEKERNEL_POSIX_SPAWN_CLONE3_OK status=%d", CHILD_STATUS);

    if (check_posix_spawnp() != 0) {
        fail("posix-spawnp", "spawned-child-ran", "program=sh");
        return 1;
    }
    emit("THEKERNEL_POSIX_SPAWN_SPAWNP_OK status=%d", CHILD_STATUS);

    emit("THEKERNEL_POSIX_SPAWN_OK");
    return 0;
}
