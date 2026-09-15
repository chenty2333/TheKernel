#define _GNU_SOURCE

/* TheKernel guest-toolchain: can a C compiler that ships inside the guest
 * build a program against the guest's own libc, and does that program run?
 *
 * This is the end-to-end claim the guest toolchain has to earn before anything
 * downstream (a system emulator, a package build, a JIT) can be attempted at
 * all.  The compiler in the guest is not a demo: it must link against the musl
 * sysroot installed next to it in the same rootfs, and the program it produces
 * must run on this kernel without a loader the guest does not have.
 *
 * Each stage is checked separately, so a failure names the stage that broke
 * instead of only reporting "the compiler is broken":
 *
 *   1. the compiler and its -B runtime directory are located
 *   2. a real libc program is written into a fresh scratch directory
 *   3. the compiler is forked; its output is captured and it must exit 0
 *   4. the product is parsed as ELF: no PT_INTERP, no DT_NEEDED
 *   5. the product is forked; it must print its token once and exit 42
 *   6. a deliberately broken source must make the compiler exit non-zero
 *
 * Stage 6 is the negative control.  Without it, "the compile succeeded" would
 * be satisfied just as well by a compiler that accepts anything and emits
 * anything, so the positive result would carry no information.
 *
 * Stable markers:
 *   THEKERNEL_COMPILER_SMOKE_OK
 *   THEKERNEL_COMPILER_SMOKE_FAIL <operation> condition=<c> k=v ... errno=<n> (<message>)
 *   THEKERNEL_COMPILER_SMOKE_TCC_OUTPUT: <line>       captured compiler output
 *   THEKERNEL_COMPILER_SMOKE_PROGRAM_OUTPUT: <line>   captured program stdout
 *   THEKERNEL_COMPILER_SMOKE_<STAGE> k=v ...          greppable progress
 */

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "the compiler smoke is x86_64-only"
#endif

/* ELF x86_64 ABI values.  These are Linux ABI numbers, not libc-private
 * declarations, so they are spelled out locally: this helper is built once
 * with the host's glibc for validation and once with the guest's musl for the
 * rootfs, and it must not depend on a header the guest sysroot may not ship. */
#define TK_ELF_HEADER_BYTES 64U
#define TK_ELF_PHDR_BYTES 56U
#define TK_ELF_DYN_BYTES 16U
#define TK_ELF_MAGIC0 0x7fU
#define TK_ELF_CLASS_OFFSET 4U
#define TK_ELF_CLASS64 2U
#define TK_ELF_DATA_OFFSET 5U
#define TK_ELF_DATA_LSB 1U
#define TK_ELF_TYPE_OFFSET 16U
#define TK_ET_EXEC 2
#define TK_ET_DYN 3
#define TK_ELF_PHOFF_OFFSET 32U
#define TK_ELF_PHENTSIZE_OFFSET 54U
#define TK_ELF_PHNUM_OFFSET 56U
#define TK_ELF_PHDR_TYPE_OFFSET 0U
#define TK_ELF_PHDR_OFFSET_OFFSET 8U
#define TK_ELF_PHDR_FILESZ_OFFSET 32U
#define TK_PT_DYNAMIC 2U
#define TK_PT_INTERP 3U
#define TK_DT_NULL 0
#define TK_DT_NEEDED 1

/* The interface under test.  Both knobs exist ONLY so this exact binary can be
 * validated on the host against a host-built payload; the guest must run with
 * the compiled-in defaults, which are the paths its rootfs installs. */
#define COMPILER_ENV "THEKERNEL_TCC"
#define SYSROOT_ENV "THEKERNEL_TCC_SYSROOT"
#define DEFAULT_COMPILER "/usr/bin/tcc"
#define DEFAULT_SYSROOT "/usr/lib/tcc"

/* Scratch location.  Only the base has to exist; the per-run working directory
 * is created here and is never required to pre-exist. */
#define SCRATCH_BASE_ENV "TMPDIR"
#define DEFAULT_SCRATCH_BASE "/tmp"
#define SCRATCH_TEMPLATE "compiler-smoke.XXXXXX"

/* Deadlines exist so a wedged compiler cannot hang the whole suite; the budget
 * itself is generous because the guest may be running under emulation. */
#define COMPILE_TIMEOUT_MS 60000
#define RUN_TIMEOUT_MS 30000
#define KILL_GRACE_MS 5000
#define POLL_SLICE_MS 200

#define PATH_BUFFER_BYTES 4096U
#define CHILD_OUTPUT_BYTES 8192U

/* Exit statuses the forked child uses when it cannot become the target
 * program; they are distinct from any status the compiler itself returns for
 * a rejected source file. */
#define CHILD_SETUP_EXIT_STATUS 126
#define CHILD_EXEC_EXIT_STATUS 127

/* The compiled program formats this token at run time and exits with
 * PROGRAM_EXIT_STATUS.  The numeric part is 1*1 + 2*2 + ... + 7*7, computed by
 * a loop in the generated program, so the token as a whole never appears in
 * the generated source or binary as a string the program could print without
 * doing the arithmetic: a stale, empty or wrong-but-successful compile cannot
 * match it.  The two observations (stdout and exit status) are checked
 * separately, because either one alone could be produced by accident. */
#define PROGRAM_TOKEN "COMPILER-SMOKE-VALUE-140"
#define PROGRAM_EXIT_STATUS 42

/* The program the guest compiler must build.  It uses standard headers and the
 * C library on purpose -- <stdio.h>, <stdlib.h>, <string.h>, a heap
 * allocation, a formatted integer and a string copy -- so that compiling it
 * genuinely exercises the sysroot's headers, startup objects and libc archive
 * rather than only the compiler's own code generator. */
static const char program_source[] =
    "#include <stdio.h>\n"
    "#include <stdlib.h>\n"
    "#include <string.h>\n"
    "\n"
    "int main(void) {\n"
    "    const size_t capacity = 64U;\n"
    "    unsigned long total = 0UL;\n"
    "    unsigned long index;\n"
    "    char *message;\n"
    "    char *copy;\n"
    "\n"
    "    message = malloc(capacity);\n"
    "    copy = malloc(capacity);\n"
    "    if (message == NULL || copy == NULL) {\n"
    "        fputs(\"COMPILER-SMOKE-MALLOC-FAILED\\n\", stdout);\n"
    "        free(message);\n"
    "        free(copy);\n"
    "        return 1;\n"
    "    }\n"
    "    for (index = 1UL; index <= 7UL; ++index) {\n"
    "        total += index * index;\n"
    "    }\n"
    "    snprintf(message, capacity, \"COMPILER-SMOKE-VALUE-%lu\", total);\n"
    "    memcpy(copy, message, strlen(message) + 1U);\n"
    "    printf(\"%s\\n\", copy);\n"
    "    free(copy);\n"
    "    free(message);\n"
    "    return 42;\n"
    "}\n";

/* Deliberately invalid C: an empty initializer expression is a syntax error
 * and the returned call names a function that is never declared.  A compiler
 * that accepts this file cannot be trusted when it accepts the real one. */
static const char negative_source[] =
    "#include <stdio.h>\n"
    "\n"
    "int main(void) {\n"
    "    int value = ;\n"
    "    return undeclared_helper(value);\n"
    "}\n";

struct toolchain {
    const char *compiler;
    const char *sysroot;
};

struct scratch_paths {
    char directory[PATH_BUFFER_BYTES];
    char source[PATH_BUFFER_BYTES];
    char program[PATH_BUFFER_BYTES];
    char negative_source[PATH_BUFFER_BYTES];
    char negative_program[PATH_BUFFER_BYTES];
};

/* Everything a forked child can fail at, so a failure names the operation that
 * did not hold rather than only an exit status. */
enum child_failure {
    CHILD_OK = 0,
    CHILD_ARGUMENT_FAILED,
    CHILD_PIPE_FAILED,
    CHILD_FORK_FAILED,
    CHILD_WAIT_FAILED,
    CHILD_READ_FAILED,
    CHILD_TIMED_OUT,
    CHILD_KILL_FAILED,
};

static const char *child_failure_condition(enum child_failure failure) {
    switch (failure) {
        case CHILD_OK:
            return "child-ran-to-completion";
        case CHILD_ARGUMENT_FAILED:
            return "-B-argument-fits-the-path-buffer";
        case CHILD_PIPE_FAILED:
            return "pipe(2)-returns-0";
        case CHILD_FORK_FAILED:
            return "fork(2)-returns-a-child-pid";
        case CHILD_WAIT_FAILED:
            return "waitpid(2)-returns-the-child-pid";
        case CHILD_READ_FAILED:
            return "read(2)-from-the-child-pipe-succeeds";
        case CHILD_TIMED_OUT:
            return "child-exits-within-timeout-ms";
        case CHILD_KILL_FAILED:
            return "child-is-reaped-after-sigkill";
    }
    return "unknown-failure";
}

struct child_capture {
    char output[CHILD_OUTPUT_BYTES];
    size_t output_bytes;
    int truncated;
    int reaped;
    int wait_status;
    enum child_failure failure;
    int failure_errno;
};

/* Required failure.  The house convention is one greppable line naming the
 * operation, the violated semantic condition and the raw observation, plus the
 * errno/strerror pair of whichever syscall failed.  Semantic failures set
 * errno = 0 first, so they read errno=0 (Success) instead of a stale value. */
static int __attribute__((format(printf, 2, 3)))
failf(const char *operation, const char *format, ...) {
    const int saved_errno = errno;
    va_list arguments;
    fprintf(stderr, "THEKERNEL_COMPILER_SMOKE_FAIL %s ", operation);
    va_start(arguments, format);
    vfprintf(stderr, format, arguments);
    va_end(arguments);
    fprintf(stderr, " errno=%d (%s)\n", saved_errno, strerror(saved_errno));
    return EXIT_FAILURE;
}

static const char *resolve_knob(const char *name, const char *fallback) {
    const char *value = getenv(name);
    if (value != NULL && value[0] != '\0') {
        return value;
    }
    return fallback;
}

/* snprintf with an explicit truncation check.  A silently truncated path would
 * make the probe exercise a different file than the one it reports, which is
 * exactly the kind of wrong-but-passing outcome this smoke exists to prevent. */
static int __attribute__((format(printf, 3, 4)))
format_checked(char *out, size_t out_size, const char *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    const int written = vsnprintf(out, out_size, format, arguments);
    va_end(arguments);
    if (written < 0 || (size_t)written >= out_size) {
        out[0] = '\0';
        return -1;
    }
    return 0;
}

static int join_path(char *out, size_t out_size, const char *directory,
                     const char *name) {
    const size_t length = strlen(directory);
    const char *separator =
        (length > 0U && directory[length - 1U] == '/') ? "" : "/";
    return format_checked(out, out_size, "%s%s%s", directory, separator, name);
}

/* Monotonic milliseconds for the child deadlines.  CLOCK_MONOTONIC is not
 * expected to fail; falling back to a coarse clock keeps the wait bounded
 * instead of turning a failure into an unbounded one. */
static long long monotonic_ms(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) == 0) {
        return (long long)now.tv_sec * 1000LL +
               (long long)(now.tv_nsec / 1000000L);
    }
    return (long long)time(NULL) * 1000LL;
}

static ssize_t read_exact_at(int fd, uint64_t offset, unsigned char *buffer,
                             size_t length) {
    size_t done = 0U;
    while (done < length) {
        const ssize_t bytes =
            pread(fd, buffer + done, length - done, (off_t)(offset + done));
        if (bytes > 0) {
            done += (size_t)bytes;
            continue;
        }
        if (bytes == 0) {
            break; /* short file: the caller compares against `length` */
        }
        if (errno == EINTR) {
            continue;
        }
        return -1;
    }
    return (ssize_t)done;
}

static uint16_t load_u16_le(const unsigned char *bytes) {
    return (uint16_t)((uint16_t)bytes[0] | (uint16_t)((uint16_t)bytes[1] << 8));
}

static uint32_t load_u32_le(const unsigned char *bytes) {
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
}

static uint64_t load_u64_le(const unsigned char *bytes) {
    uint64_t value = 0U;
    unsigned int index;
    for (index = 0U; index < 8U; ++index) {
        value |= (uint64_t)bytes[index] << (8U * index);
    }
    return value;
}

static size_t count_occurrences(const char *haystack, const char *needle) {
    const size_t needle_length = strlen(needle);
    size_t count = 0U;
    if (needle_length == 0U) {
        return 0U;
    }
    for (const char *cursor = haystack;
         (cursor = strstr(cursor, needle)) != NULL; cursor += needle_length) {
        count += 1U;
    }
    return count;
}

static void describe_wait_status(int status, char *out, size_t out_size) {
    if (WIFEXITED(status)) {
        (void)snprintf(out, out_size, "exit=%d", WEXITSTATUS(status));
        return;
    }
    if (WIFSIGNALED(status)) {
        (void)snprintf(out, out_size, "signal=%d", WTERMSIG(status));
        return;
    }
    (void)snprintf(out, out_size, "raw=%d", status);
}

/* Returns the child's exit status, or -1 when it did not exit normally.  A
 * signal death is therefore never mistaken for exit status 0. */
static int child_exit_status(const struct child_capture *capture) {
    if (capture->reaped == 0 || !WIFEXITED(capture->wait_status)) {
        return -1;
    }
    return WEXITSTATUS(capture->wait_status);
}

static int write_file(const char *operation, const char *path,
                      const char *contents) {
    const size_t length = strlen(contents);
    size_t done = 0U;

    errno = 0;
    const int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) {
        return failf(operation,
                     "condition=open(source,O_WRONLY|O_CREAT|O_TRUNC)-returns-fd "
                     "path=%s",
                     path);
    }
    while (done < length) {
        const ssize_t bytes = write(fd, contents + done, length - done);
        if (bytes > 0) {
            done += (size_t)bytes;
            continue;
        }
        if (bytes < 0 && errno == EINTR) {
            continue;
        }
        const int saved_errno = errno;
        (void)close(fd);
        errno = saved_errno;
        return failf(operation,
                     "condition=write(source)-accepts-all-bytes path=%s bytes=%zu",
                     path, length);
    }
    errno = 0;
    if (close(fd) != 0) {
        return failf(operation, "condition=close(source)-returns-0 path=%s",
                     path);
    }
    return EXIT_SUCCESS;
}

/* The forked child.  It never returns: it either becomes the target program or
 * exits with a status the parent can distinguish from the program's own. */
static void child_run(char *const argv[], int write_fd, int merge_stderr) {
    const int null_fd = open("/dev/null", O_RDONLY);
    if (null_fd >= 0) {
        /* A compiler that decides to read stdin must see EOF, not the suite's
         * terminal.  A failure here only means the inherited stdin is used. */
        (void)dup2(null_fd, STDIN_FILENO);
        if (null_fd > STDERR_FILENO) {
            (void)close(null_fd);
        }
    }
    if (dup2(write_fd, STDOUT_FILENO) < 0) {
        _exit(CHILD_SETUP_EXIT_STATUS);
    }
    if (merge_stderr != 0 && dup2(write_fd, STDERR_FILENO) < 0) {
        _exit(CHILD_SETUP_EXIT_STATUS);
    }
    if (write_fd > STDERR_FILENO) {
        (void)close(write_fd);
    }
    execv(argv[0], argv);
    fprintf(stderr, "THEKERNEL_COMPILER_SMOKE_EXEC_FAIL path=%s errno=%d (%s)\n",
            argv[0], errno, strerror(errno));
    _exit(CHILD_EXEC_EXIT_STATUS);
}

static void capture_append(struct child_capture *capture,
                           const unsigned char *bytes, size_t length) {
    const size_t room = (CHILD_OUTPUT_BYTES - 1U) - capture->output_bytes;
    const size_t stored = length < room ? length : room;
    memcpy(capture->output + capture->output_bytes, bytes, stored);
    capture->output_bytes += stored;
    if (stored < length) {
        capture->truncated = 1;
    }
    capture->output[capture->output_bytes] = '\0';
}

/* A killed child is still reaped with a bound of its own: an unreapable child
 * has to be reported, not waited on forever. */
static void kill_child_bounded(pid_t child, struct child_capture *capture) {
    if (kill(child, SIGKILL) != 0 && errno != ESRCH) {
        capture->failure = CHILD_KILL_FAILED;
        capture->failure_errno = errno;
        return;
    }
    const long long deadline = monotonic_ms() + (long long)KILL_GRACE_MS;
    for (;;) {
        int status = 0;
        const pid_t observed = waitpid(child, &status, WNOHANG);
        if (observed == child) {
            capture->reaped = 1;
            capture->wait_status = status;
            return;
        }
        if (observed < 0 && errno != EINTR) {
            capture->failure = CHILD_WAIT_FAILED;
            capture->failure_errno = errno;
            return;
        }
        if (monotonic_ms() >= deadline) {
            capture->failure = CHILD_KILL_FAILED;
            capture->failure_errno = ETIMEDOUT;
            return;
        }
        const struct timespec slice = {0, 10000000L}; /* 10 ms */
        (void)nanosleep(&slice, NULL);
    }
}

/* Drains the pipe and reaps the child under one deadline.  Both halves matter:
 * the child may block writing to a full pipe while the parent waits for it to
 * exit, so "wait then read" would deadlock on a compiler that prints a lot. */
static void wait_and_capture(pid_t child, int read_fd, int timeout_ms,
                             struct child_capture *capture) {
    const long long deadline = monotonic_ms() + (long long)timeout_ms;
    int saw_eof = 0;

    for (;;) {
        for (;;) {
            unsigned char chunk[512];
            const ssize_t bytes = read(read_fd, chunk, sizeof(chunk));
            if (bytes > 0) {
                capture_append(capture, chunk, (size_t)bytes);
                /* The deadline is re-checked on the drain path too: a child
                 * that writes faster than we read would otherwise keep the pipe
                 * non-empty forever and the wait would never expire. */
                if (monotonic_ms() >= deadline) {
                    capture->failure = CHILD_TIMED_OUT;
                    capture->failure_errno = ETIMEDOUT;
                    break;
                }
                continue;
            }
            if (bytes == 0) {
                saw_eof = 1;
                break;
            }
            if (errno == EINTR) {
                continue;
            }
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                break;
            }
            capture->failure = CHILD_READ_FAILED;
            capture->failure_errno = errno;
            break;
        }
        if (capture->failure != CHILD_OK) {
            break;
        }

        if (capture->reaped == 0) {
            int status = 0;
            const pid_t observed = waitpid(child, &status, WNOHANG);
            if (observed == child) {
                capture->reaped = 1;
                capture->wait_status = status;
                continue; /* drain what the child wrote just before exiting */
            }
            if (observed < 0 && errno != EINTR) {
                capture->failure = CHILD_WAIT_FAILED;
                capture->failure_errno = errno;
                break;
            }
        } else if (saw_eof != 0) {
            break;
        }

        const long long remaining = deadline - monotonic_ms();
        if (remaining <= 0) {
            capture->failure = CHILD_TIMED_OUT;
            capture->failure_errno = ETIMEDOUT;
            break;
        }
        if (saw_eof != 0) {
            /* The pipe is closed but the process is still alive: there is
             * nothing left to poll, so sleep out a slice of the deadline. */
            const struct timespec slice = {0, (long)POLL_SLICE_MS * 1000000L};
            (void)nanosleep(&slice, NULL);
        } else {
            struct pollfd descriptor;
            descriptor.fd = read_fd;
            descriptor.events = POLLIN;
            descriptor.revents = 0;
            const int slice = remaining < (long long)POLL_SLICE_MS
                                  ? (int)remaining
                                  : POLL_SLICE_MS;
            if (poll(&descriptor, 1, slice) < 0 && errno != EINTR) {
                capture->failure = CHILD_READ_FAILED;
                capture->failure_errno = errno;
                break;
            }
        }
    }

    if (capture->failure == CHILD_TIMED_OUT) {
        kill_child_bounded(child, capture);
    }
}

/* Runs one program with captured stdout (and, when asked, stderr as well) under
 * a deadline.  Errors are reported through the capture, never by blocking. */
static void spawn_capture(char *const argv[], int merge_stderr, int timeout_ms,
                          struct child_capture *capture) {
    memset(capture, 0, sizeof(*capture));

    int pipe_fds[2];
    errno = 0;
    if (pipe(pipe_fds) != 0) {
        capture->failure = CHILD_PIPE_FAILED;
        capture->failure_errno = errno;
        return;
    }

    /* Flush before forking so the probe's own progress output is not copied
     * into the child's pipe buffer and reported as child output. */
    (void)fflush(stdout);
    (void)fflush(stderr);

    errno = 0;
    const pid_t child = fork();
    if (child < 0) {
        const int saved_errno = errno;
        (void)close(pipe_fds[0]);
        (void)close(pipe_fds[1]);
        capture->failure = CHILD_FORK_FAILED;
        capture->failure_errno = saved_errno;
        return;
    }
    if (child == 0) {
        (void)close(pipe_fds[0]);
        child_run(argv, pipe_fds[1], merge_stderr);
    }

    (void)close(pipe_fds[1]);
    errno = 0;
    if (fcntl(pipe_fds[0], F_SETFL, O_NONBLOCK) != 0) {
        const int saved_errno = errno;
        (void)close(pipe_fds[0]);
        capture->failure = CHILD_READ_FAILED;
        capture->failure_errno = saved_errno;
        return;
    }
    wait_and_capture(child, pipe_fds[0], timeout_ms, capture);
    (void)close(pipe_fds[0]);
}

static int fail_child(const char *operation,
                      const struct child_capture *capture, int timeout_ms) {
    char status_text[32];
    describe_wait_status(capture->wait_status, status_text,
                         sizeof(status_text));
    errno = capture->failure_errno;
    return failf(operation,
                 "condition=%s timeout_ms=%d status=%s reaped=%d output_bytes=%zu "
                 "truncated=%d",
                 child_failure_condition(capture->failure), timeout_ms,
                 status_text, capture->reaped, capture->output_bytes,
                 capture->truncated);
}

/* Replays captured output into the KTAP log, one prefixed line per line, so a
 * failure stays diagnosable from the log alone. */
static void dump_child_output(const char *marker,
                              const struct child_capture *capture) {
    const char *cursor = capture->output;
    while (*cursor != '\0') {
        const char *newline = strchr(cursor, '\n');
        size_t length = (newline == NULL) ? strlen(cursor)
                                          : (size_t)(newline - cursor);
        while (length > 0U && cursor[length - 1U] == '\r') {
            length -= 1U;
        }
        printf("%s: %.*s\n", marker, (int)length, cursor);
        if (newline == NULL) {
            break;
        }
        cursor = newline + 1;
    }
}

struct elf_report {
    const char *type_name;
    uint64_t file_size;
    int has_dynamic;
};

static const char *elf_type_name(uint16_t type) {
    if (type == (uint16_t)TK_ET_EXEC) {
        return "EXEC";
    }
    if (type == (uint16_t)TK_ET_DYN) {
        return "DYN";
    }
    return "UNKNOWN";
}

/* Walks the program headers and the dynamic section of an already-open file.
 * This is the check that proves the guest toolchain linked against the guest's
 * musl instead of silently producing something that needs a loader (or a
 * shared library) the guest does not have. */
static int inspect_open_elf(int fd, const char *path,
                            struct elf_report *report) {
    struct stat info;
    errno = 0;
    if (fstat(fd, &info) != 0) {
        return failf("elf-stat", "condition=fstat(output)-returns-0 path=%s",
                     path);
    }
    if (info.st_size <= 0) {
        errno = 0;
        return failf("elf-size",
                     "condition=compiled-output-is-nonempty path=%s size=%lld",
                     path, (long long)info.st_size);
    }
    const uint64_t file_size = (uint64_t)info.st_size;
    report->file_size = file_size;

    unsigned char header[TK_ELF_HEADER_BYTES];
    errno = 0;
    if (read_exact_at(fd, 0U, header, sizeof(header)) !=
        (ssize_t)sizeof(header)) {
        return failf("elf-header",
                     "condition=file-holds-a-complete-elf-header path=%s "
                     "size=%llu",
                     path, (unsigned long long)file_size);
    }
    if (header[0] != TK_ELF_MAGIC0 || header[1] != 'E' || header[2] != 'L' ||
        header[3] != 'F') {
        char magic[16];
        (void)snprintf(magic, sizeof(magic), "%02x%02x%02x%02x", header[0],
                       header[1], header[2], header[3]);
        errno = 0;
        return failf("elf-magic",
                     "condition=output-starts-with-ELF-magic magic=0x%s path=%s",
                     magic, path);
    }
    if ((unsigned)header[TK_ELF_CLASS_OFFSET] != TK_ELF_CLASS64 ||
        (unsigned)header[TK_ELF_DATA_OFFSET] != TK_ELF_DATA_LSB) {
        errno = 0;
        return failf("elf-class",
                     "condition=output-is-64-bit-little-endian class=%u data=%u "
                     "path=%s",
                     (unsigned)header[TK_ELF_CLASS_OFFSET],
                     (unsigned)header[TK_ELF_DATA_OFFSET], path);
    }

    const uint16_t type = load_u16_le(header + TK_ELF_TYPE_OFFSET);
    report->type_name = elf_type_name(type);
    if (type != (uint16_t)TK_ET_EXEC && type != (uint16_t)TK_ET_DYN) {
        errno = 0;
        return failf("elf-type",
                     "condition=e-type-is-ET_EXEC-or-ET_DYN e_type=%u path=%s",
                     (unsigned)type, path);
    }

    const uint64_t phoff = load_u64_le(header + TK_ELF_PHOFF_OFFSET);
    const uint16_t phentsize = load_u16_le(header + TK_ELF_PHENTSIZE_OFFSET);
    const uint16_t phnum = load_u16_le(header + TK_ELF_PHNUM_OFFSET);
    if (phnum == 0U || phentsize < TK_ELF_PHDR_BYTES) {
        errno = 0;
        return failf("elf-program-headers",
                     "condition=program-header-table-is-present phnum=%u "
                     "phentsize=%u path=%s",
                     (unsigned)phnum, (unsigned)phentsize, path);
    }
    if (phoff > file_size ||
        (uint64_t)phnum * (uint64_t)phentsize > file_size - phoff) {
        errno = 0;
        return failf("elf-program-headers",
                     "condition=program-header-table-is-inside-the-file "
                     "phoff=%llu phnum=%u phentsize=%u size=%llu",
                     (unsigned long long)phoff, (unsigned)phnum,
                     (unsigned)phentsize, (unsigned long long)file_size);
    }

    uint64_t dynamic_offset = 0U;
    uint64_t dynamic_size = 0U;
    uint16_t index;
    for (index = 0U; index < phnum; ++index) {
        unsigned char phdr[TK_ELF_PHDR_BYTES];
        errno = 0;
        if (read_exact_at(fd, phoff + (uint64_t)index * (uint64_t)phentsize,
                          phdr, sizeof(phdr)) != (ssize_t)sizeof(phdr)) {
            return failf("elf-program-header",
                         "condition=program-header-entry-is-readable index=%u "
                         "path=%s",
                         (unsigned)index, path);
        }
        const uint32_t phdr_type = load_u32_le(phdr + TK_ELF_PHDR_TYPE_OFFSET);
        if (phdr_type == TK_PT_INTERP) {
            /* A PT_INTERP segment means run time needs a dynamic loader.  The
             * guest installs none for this toolchain, so such a product could
             * not be executed by the very system that produced it. */
            errno = 0;
            return failf("elf-interp",
                         "condition=absent-PT_INTERP-program-header index=%u "
                         "path=%s",
                         (unsigned)index, path);
        }
        if (phdr_type == TK_PT_DYNAMIC) {
            dynamic_offset = load_u64_le(phdr + TK_ELF_PHDR_OFFSET_OFFSET);
            dynamic_size = load_u64_le(phdr + TK_ELF_PHDR_FILESZ_OFFSET);
            report->has_dynamic = 1;
        }
    }

    if (report->has_dynamic != 0) {
        if (dynamic_offset > file_size ||
            dynamic_size > file_size - dynamic_offset) {
            errno = 0;
            return failf("elf-dynamic",
                         "condition=dynamic-segment-is-inside-the-file "
                         "offset=%llu size=%llu size=%llu",
                         (unsigned long long)dynamic_offset,
                         (unsigned long long)dynamic_size,
                         (unsigned long long)file_size);
        }
        uint64_t offset;
        for (offset = 0U; offset + TK_ELF_DYN_BYTES <= dynamic_size;
             offset += TK_ELF_DYN_BYTES) {
            unsigned char entry[TK_ELF_DYN_BYTES];
            errno = 0;
            if (read_exact_at(fd, dynamic_offset + offset, entry,
                              sizeof(entry)) != (ssize_t)sizeof(entry)) {
                return failf("elf-dynamic-entry",
                             "condition=dynamic-entry-is-readable offset=%llu",
                             (unsigned long long)offset);
            }
            const int64_t tag = (int64_t)load_u64_le(entry);
            if (tag == TK_DT_NULL) {
                break;
            }
            if (tag == TK_DT_NEEDED) {
                /* A DT_NEEDED entry names a shared object to load.  The guest
                 * ships none for this toolchain, so "statically linked" has to
                 * mean it: no loader, no dependency. */
                errno = 0;
                return failf("elf-needed",
                             "condition=absent-DT_NEEDED-entry offset=%llu "
                             "path=%s",
                             (unsigned long long)offset, path);
            }
        }
    }
    return EXIT_SUCCESS;
}

static int inspect_static_elf(const char *path, struct elf_report *report) {
    memset(report, 0, sizeof(*report));

    errno = 0;
    const int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return failf("elf-open", "condition=compiled-output-exists path=%s",
                     path);
    }
    const int result = inspect_open_elf(fd, path, report);
    const int saved_errno = errno;
    (void)close(fd);
    errno = saved_errno;
    return result;
}

static int create_scratch(struct scratch_paths *paths) {
    /* Only the base ($TMPDIR, or /tmp when it is unset) has to exist, and in
     * the guest it always does: the per-run working directory below it is
     * created here and is never required to pre-exist. */
    const char *base = resolve_knob(SCRATCH_BASE_ENV, DEFAULT_SCRATCH_BASE);
    char template_path[PATH_BUFFER_BYTES];

    if (join_path(template_path, sizeof(template_path), base,
                  SCRATCH_TEMPLATE) != 0) {
        errno = 0;
        return failf("scratch-template",
                     "condition=scratch-template-fits-the-path-buffer base=%s",
                     base);
    }
    errno = 0;
    if (mkdtemp(template_path) == NULL) {
        return failf("scratch-mkdtemp",
                     "condition=mkdtemp(base/compiler-smoke.XXXXXX)-returns-a-"
                     "directory base=%s",
                     base);
    }
    if (format_checked(paths->directory, sizeof(paths->directory), "%s",
                       template_path) != 0 ||
        join_path(paths->source, sizeof(paths->source), paths->directory,
                  "program.c") != 0 ||
        join_path(paths->program, sizeof(paths->program), paths->directory,
                  "program") != 0 ||
        join_path(paths->negative_source, sizeof(paths->negative_source),
                  paths->directory, "broken.c") != 0 ||
        join_path(paths->negative_program, sizeof(paths->negative_program),
                  paths->directory, "broken") != 0) {
        errno = 0;
        return failf("scratch-paths",
                     "condition=scratch-paths-fit-the-path-buffers directory=%s",
                     paths->directory);
    }
    return EXIT_SUCCESS;
}

/* Best effort, as the contract asks: the verdict never depends on the scratch
 * files, but the errno of a failed rmdir is returned so a guest tmpfs leak is
 * still visible in the log. */
static int remove_scratch(const struct scratch_paths *paths) {
    (void)unlink(paths->source);
    (void)unlink(paths->program);
    (void)unlink(paths->negative_source);
    (void)unlink(paths->negative_program);
    errno = 0;
    if (rmdir(paths->directory) != 0) {
        return errno;
    }
    return 0;
}

/* Runs the compiler under test with exactly the argument vector the guest is
 * specified to use: tcc -B<sysroot> -static -o <output> <source>.  The -B
 * directory is how the compiler finds its own startup objects, libc archive
 * and headers, so it is the whole reason the payload works from a /usr layout
 * that is not the one it was configured for. */
static void invoke_compiler(const struct toolchain *toolchain,
                            const char *source_path, const char *output_path,
                            int timeout_ms, struct child_capture *capture) {
    char b_sysroot[PATH_BUFFER_BYTES];
    if (format_checked(b_sysroot, sizeof(b_sysroot), "-B%s",
                       toolchain->sysroot) != 0) {
        memset(capture, 0, sizeof(*capture));
        capture->failure = CHILD_ARGUMENT_FAILED;
        capture->failure_errno = ENAMETOOLONG;
        return;
    }
    char *const argv[] = {
        (char *)toolchain->compiler, b_sysroot,         "-static", "-o",
        (char *)output_path,         (char *)source_path, NULL,
    };
    spawn_capture(argv, 1, timeout_ms, capture);
}

static int run_probe(const struct toolchain *toolchain,
                     const struct scratch_paths *paths) {
    char status_text[32];

    /* Stage 1: write a real libc program into the scratch directory. */
    if (write_file("write-program-source", paths->source, program_source) !=
        EXIT_SUCCESS) {
        return EXIT_FAILURE;
    }
    printf("THEKERNEL_COMPILER_SMOKE_WROTE_SOURCE path=%s bytes=%zu\n",
           paths->source, strlen(program_source));

    /* Stage 2: compile it.  The compiler's own output is replayed into the log
     * before its exit status is judged, so a rejection is diagnosable. */
    struct child_capture compile_capture;
    invoke_compiler(toolchain, paths->source, paths->program,
                    COMPILE_TIMEOUT_MS, &compile_capture);
    dump_child_output("THEKERNEL_COMPILER_SMOKE_TCC_OUTPUT", &compile_capture);
    if (compile_capture.failure != CHILD_OK) {
        return fail_child("compile", &compile_capture, COMPILE_TIMEOUT_MS);
    }
    describe_wait_status(compile_capture.wait_status, status_text,
                         sizeof(status_text));
    if (child_exit_status(&compile_capture) != 0) {
        errno = 0;
        return failf("compile",
                     "condition=compiler-exits-0 status=%s output_bytes=%zu "
                     "output_truncated=%d",
                     status_text, compile_capture.output_bytes,
                     compile_capture.truncated);
    }
    printf("THEKERNEL_COMPILER_SMOKE_COMPILED path=%s\n", paths->program);

    /* Stage 3: the product is checked as an artifact, before it is run.  A
     * program that only works because the host happens to have a loader is not
     * a working guest toolchain. */
    struct elf_report report;
    if (inspect_static_elf(paths->program, &report) != EXIT_SUCCESS) {
        return EXIT_FAILURE;
    }
    printf("THEKERNEL_COMPILER_SMOKE_ELF type=%s size=%llu dynamic=%s "
           "interp=absent\n",
           report.type_name, (unsigned long long)report.file_size,
           report.has_dynamic != 0 ? "present" : "absent");

    /* Stage 4: run it.  Output and exit status are judged separately, because
     * either one alone can be produced by accident. */
    struct child_capture run_capture;
    char *const run_argv[] = {(char *)paths->program, NULL};
    spawn_capture(run_argv, 0, RUN_TIMEOUT_MS, &run_capture);
    dump_child_output("THEKERNEL_COMPILER_SMOKE_PROGRAM_OUTPUT", &run_capture);
    if (run_capture.failure != CHILD_OK) {
        return fail_child("run", &run_capture, RUN_TIMEOUT_MS);
    }
    if (run_capture.truncated != 0) {
        errno = 0;
        return failf("run-stdout",
                     "condition=program-stdout-is-captured-completely bytes=%zu",
                     run_capture.output_bytes);
    }
    describe_wait_status(run_capture.wait_status, status_text,
                         sizeof(status_text));
    if (!WIFEXITED(run_capture.wait_status)) {
        errno = 0;
        return failf("run-exit-status",
                     "condition=program-exits-normally status=%s", status_text);
    }
    if (WEXITSTATUS(run_capture.wait_status) != PROGRAM_EXIT_STATUS) {
        errno = 0;
        return failf("run-exit-status",
                     "condition=program-exit-status-equals-expected expected=%d "
                     "actual=%s",
                     PROGRAM_EXIT_STATUS, status_text);
    }
    const size_t token_count =
        count_occurrences(run_capture.output, PROGRAM_TOKEN);
    if (token_count != 1U) {
        errno = 0;
        return failf("run-stdout",
                     "condition=stdout-contains-token-exactly-once expected=1 "
                     "actual=%zu token=%s",
                     token_count, PROGRAM_TOKEN);
    }
    printf("THEKERNEL_COMPILER_SMOKE_RAN status=%s token=%s\n", status_text,
           PROGRAM_TOKEN);

    /* Stage 5: the negative control.  If the same compiler accepts a file that
     * is not valid C, then "the compile succeeded" above proves nothing about
     * the compiler, and this probe must say so instead of passing. */
    if (write_file("write-negative-source", paths->negative_source,
                   negative_source) != EXIT_SUCCESS) {
        return EXIT_FAILURE;
    }
    struct child_capture negative_capture;
    invoke_compiler(toolchain, paths->negative_source, paths->negative_program,
                    COMPILE_TIMEOUT_MS, &negative_capture);
    dump_child_output("THEKERNEL_COMPILER_SMOKE_TCC_OUTPUT", &negative_capture);
    if (negative_capture.failure != CHILD_OK) {
        return fail_child("negative-control", &negative_capture,
                          COMPILE_TIMEOUT_MS);
    }
    describe_wait_status(negative_capture.wait_status, status_text,
                         sizeof(status_text));
    if (child_exit_status(&negative_capture) == 0) {
        errno = 0;
        return failf("negative-control",
                     "condition=invalid-c-source-makes-compiler-exit-nonzero "
                     "status=%s",
                     status_text);
    }
    printf("THEKERNEL_COMPILER_SMOKE_NEGATIVE_CONTROL status=%s\n",
           status_text);

    return EXIT_SUCCESS;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    /* The two overrides below exist ONLY so this exact binary can be exercised
     * on the host against a host-built payload.  The guest must use the
     * compiled-in defaults: /usr/bin/tcc and /usr/lib/tcc are the paths its
     * rootfs installs, and a guest that silently needed an override would be
     * a rootfs bug this probe has to catch. */
    const struct toolchain toolchain = {
        .compiler = resolve_knob(COMPILER_ENV, DEFAULT_COMPILER),
        .sysroot = resolve_knob(SYSROOT_ENV, DEFAULT_SYSROOT),
    };
    printf("THEKERNEL_COMPILER_SMOKE_INTERFACE compiler=%s sysroot=%s\n",
           toolchain.compiler, toolchain.sysroot);

    struct scratch_paths paths;
    memset(&paths, 0, sizeof(paths));
    if (create_scratch(&paths) != EXIT_SUCCESS) {
        return EXIT_FAILURE;
    }
    printf("THEKERNEL_COMPILER_SMOKE_SCRATCH dir=%s\n", paths.directory);

    const int result = run_probe(&toolchain, &paths);

    /* Cleanup is best effort and must not change the verdict; a leftover
     * directory is reported so a guest tmpfs leak stays visible. */
    const int cleanup_errno = remove_scratch(&paths);
    if (result != EXIT_SUCCESS) {
        return EXIT_FAILURE;
    }
    if (cleanup_errno != 0) {
        fprintf(stderr,
                "THEKERNEL_COMPILER_SMOKE_CLEANUP_REMAINDER dir=%s errno=%d "
                "(%s)\n",
                paths.directory, cleanup_errno, strerror(cleanup_errno));
    } else {
        puts("THEKERNEL_COMPILER_SMOKE_CLEANED");
    }

    puts("THEKERNEL_COMPILER_SMOKE_OK");
    return EXIT_SUCCESS;
}
