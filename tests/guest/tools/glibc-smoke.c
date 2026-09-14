#define _GNU_SOURCE

/* TheKernel guest-toolchain: does a dynamically linked glibc program run in the
 * guest?
 *
 * This is Phase 3's first milestone, and it is deliberately a *staging* test
 * before it is a kernel test.  Reconnaissance against the kernel's ELF loader
 * found that the loader contract was already implemented -- PT_INTERP is read
 * out of the file, the interpreter is mapped and started with AT_BASE pointing
 * at it, and the auxiliary vector was measured equal to Linux's on every entry
 * glibc reads.  What the guest image lacked was the loader, the shared libc and
 * any dynamic binary at all.  So the interesting failure here is an ENOENT on
 * /lib64/ld-linux-x86-64.so.2, which is a staging bug wearing a kernel bug's
 * clothes, and the case has to say which one it is.
 *
 * Two separate claims, reported separately:
 *
 *   1. the program is *actually* dynamically linked.  Its PT_INTERP names the
 *      loader this payload stages, and it has at least one DT_NEEDED.  Without
 *      this the case could pass on a static binary and certify nothing, which
 *      is exactly the trap a "does it exit 0" check cannot see.
 *   2. the loader ran.  The program's own markers arrive on stdout, and the
 *      first of them carries AT_BASE -- the interpreter's base address, which
 *      only the kernel can supply and which is non-zero only when the program
 *      was started through an interpreter.  So the markers prove the loader
 *      contract rather than merely that some bytes were printed.
 *
 * The child is reaped on every path.  A leftover process would corrupt every
 * later case in the suite.
 *
 * Stable markers:
 *   THEKERNEL_GLIBC_SMOKE_OK
 *   THEKERNEL_GLIBC_SMOKE_FAIL <operation> condition=<c> k=v ... errno=<n> (<message>)
 *   THEKERNEL_GLIBC_SMOKE_CHILD: <line>        captured child output line
 *   THEKERNEL_GLIBC_SMOKE_<STAGE> k=v ...      greppable progress
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
#error "the glibc smoke case is x86_64-only"
#endif

#define PROGRAM_ENV "THEKERNEL_GLIBC_SMOKE_PROGRAM"
/* The loader is named twice on purpose.  `loader` is the *guest* path, which is
 * what PT_INTERP must say and therefore what the comparison is about.  `root`
 * is where the staging tree lives when the same binary is run outside the
 * guest, so that the on-disk checks can be exercised without booting.  In the
 * guest the root is empty and the two coincide. */
#define LOADER_ENV "THEKERNEL_GLIBC_SMOKE_LOADER"
#define ROOT_ENV "THEKERNEL_GLIBC_SMOKE_ROOT"
#define DEFAULT_PROGRAM "/opt/thekernel-tools/bin/glibc-smoke"
#define DEFAULT_LOADER "/lib64/ld-linux-x86-64.so.2"

/* A dynamic program that has been mis-staged fails immediately, so this is a
 * bound on a wedged process rather than a performance budget.  The suite's case
 * timeout is set above it. */
#define SMOKE_TIMEOUT_MS 20000
#define KILL_GRACE_MS 5000
#define POLL_SLICE_MS 50

#define TRANSCRIPT_BYTES 16384U
#define TRANSCRIPT_HARD_LIMIT (1U << 20)
#define TRANSCRIPT_LINE_BYTES 512U
#define PATH_BUFFER_BYTES 4096U

/* ELF values, spelled out locally: this helper is compiled once with the host
 * glibc for validation and once with the guest musl for the rootfs, and it must
 * not depend on a header either sysroot may or may not ship. */
#define ELF_HEADER_BYTES 64U
#define ELF_CLASS64 2U
#define ELF_MACHINE_OFFSET 18U
#define EM_X86_64 62U
#define ELF_PHOFF64 32U
#define ELF_PHENTSIZE64 54U
#define ELF_PHNUM64 56U
#define PHDR_BYTES64 56U
#define PHDR_TYPE_OFFSET 0U
#define PT_LOAD 1U
#define PT_INTERP 3U
#define PT_DYNAMIC 2U
#define DT_NULL 0
#define DT_NEEDED 1
#define DT_STRTAB 5
#define SHT_DYNAMIC 6U
#define SHDR_BYTES64 64U
#define ELF_SHOFF64 40U
#define ELF_SHENTSIZE64 58U
#define ELF_SHNUM64 60U
#define SH_TYPE_OFFSET 4U
#define SH_OFFSET_OFFSET 24U
#define SH_SIZE_OFFSET 32U
#define SH_ENTSIZE_OFFSET 56U
#define SH_LINK_OFFSET 40U

/* The program's own markers.  Required as a set: START alone would be printed
 * by a program that then died, and OK alone would be printed by a program that
 * was started without a loader if the value checks were skipped. */
#define MARKER_START "THEKERNEL_GLIBC_SMOKE_START"
#define MARKER_LOADER "THEKERNEL_GLIBC_SMOKE_LOADER"
#define MARKER_OK "THEKERNEL_GLIBC_SMOKE_OK"

#define CHILD_SETUP_EXIT_STATUS 126
#define CHILD_EXEC_EXIT_STATUS 127

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

    fprintf(stdout, "THEKERNEL_GLIBC_SMOKE_FAIL %s condition=%s ", operation, condition);
    va_start(arguments, format);
    vfprintf(stdout, format, arguments);
    va_end(arguments);
    fprintf(stdout, " errno=%d (%s)\n", saved, strerror(saved));
    fflush(stdout);
}

/* -------------------------------------------------------------------- time */

static int64_t monotonic_ms(void)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    return (int64_t)now.tv_sec * 1000 + (int64_t)now.tv_nsec / 1000000;
}

/* ---------------------------------------------------------------- ELF read */

static int read_exact_at(int fd, uint64_t offset, void *buffer, size_t bytes)
{
    unsigned char *cursor = (unsigned char *)buffer;
    size_t done = 0;

    while (done < bytes) {
        ssize_t got = pread(fd, cursor + done, bytes - done, (off_t)(offset + done));

        if (got < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (got == 0) {
            errno = EINVAL;
            return -1;
        }
        done += (size_t)got;
    }
    return 0;
}

static uint16_t load_u16(const unsigned char *bytes)
{
    return (uint16_t)(bytes[0] | ((uint16_t)bytes[1] << 8));
}

static uint32_t load_u32(const unsigned char *bytes)
{
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
}

static uint64_t load_u64(const unsigned char *bytes)
{
    return (uint64_t)load_u32(bytes) | ((uint64_t)load_u32(bytes + 4) << 32);
}

/* What the program says about how it must be started.  `interp` is the
 * PT_INTERP string, which the kernel resolves verbatim; `needed` counts
 * DT_NEEDED entries, which is the difference between "statically linked and
 * lying" and a program the loader has to do work for. */
struct elf_facts {
    int is_64;
    int machine;
    int has_interp;
    int needed_count;
    char interp[PATH_BUFFER_BYTES];
};

static int inspect_program(const char *path, struct elf_facts *facts)
{
    unsigned char header[ELF_HEADER_BYTES];
    unsigned char phdr[PHDR_BYTES64];
    uint64_t phoff = 0, shoff = 0, strtab = 0, dynamic_offset = 0, dynamic_bytes = 0;
    size_t dynamic_entry = 0;
    uint16_t phentsize = 0, phnum = 0, shentsize = 0, shnum = 0;
    int fd;
    int result;

    memset(facts, 0, sizeof(*facts));
    fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    if (read_exact_at(fd, 0, header, sizeof(header)) != 0) {
        close(fd);
        return -1;
    }
    if (memcmp(header, "\x7f"
                       "ELF",
               4) != 0) {
        close(fd);
        errno = ENOEXEC;
        return -1;
    }
    if (header[4] != ELF_CLASS64) {
        close(fd);
        errno = ENOEXEC;
        return -1;
    }
    facts->is_64 = 1;
    facts->machine = load_u16(header + ELF_MACHINE_OFFSET);

    phoff = load_u64(header + ELF_PHOFF64);
    phentsize = load_u16(header + ELF_PHENTSIZE64);
    phnum = load_u16(header + ELF_PHNUM64);
    shoff = load_u64(header + ELF_SHOFF64);
    shentsize = load_u16(header + ELF_SHENTSIZE64);
    shnum = load_u16(header + ELF_SHNUM64);

    if (phentsize != PHDR_BYTES64) {
        close(fd);
        errno = ENOEXEC;
        return -1;
    }
    for (uint16_t index = 0; index < phnum; ++index) {
        if (read_exact_at(fd, phoff + (uint64_t)index * phentsize, phdr,
                          sizeof(phdr)) != 0) {
            close(fd);
            return -1;
        }
        uint32_t type = load_u32(phdr + PHDR_TYPE_OFFSET);

        if (type == PT_INTERP) {
            uint64_t offset = load_u64(phdr + 8);
            uint64_t filesz = load_u64(phdr + 32);

            if (filesz == 0 || filesz > sizeof(facts->interp)) {
                close(fd);
                errno = ENOEXEC;
                return -1;
            }
            if (read_exact_at(fd, offset, facts->interp, (size_t)filesz) != 0) {
                close(fd);
                return -1;
            }
            facts->interp[filesz - 1] = '\0';
            facts->has_interp = 1;
        } else if (type == PT_DYNAMIC) {
            dynamic_offset = load_u64(phdr + 8);
            dynamic_bytes = load_u64(phdr + 32);
        }
    }

    /* The string table's address comes from a DT_STRTAB in the dynamic
     * section, but DT_NEEDED entries can be counted without it: the section
     * headers give the dynamic section directly and DT_NEEDED tags are
     * self-describing.  Counting them is the point -- resolving the names is
     * not needed to tell a dynamic program from a static one. */
    if (dynamic_bytes == 0 && shnum > 0 && shentsize == SHDR_BYTES64) {
        unsigned char shdr[SHDR_BYTES64];

        for (uint16_t index = 0; index < shnum; ++index) {
            if (read_exact_at(fd, shoff + (uint64_t)index * shentsize, shdr,
                              sizeof(shdr)) != 0) {
                close(fd);
                return -1;
            }
            if (load_u32(shdr + SH_TYPE_OFFSET) == SHT_DYNAMIC) {
                dynamic_offset = load_u64(shdr + SH_OFFSET_OFFSET);
                dynamic_bytes = load_u64(shdr + SH_SIZE_OFFSET);
                dynamic_entry = (size_t)load_u64(shdr + SH_ENTSIZE_OFFSET);
                break;
            }
        }
    }
    if (dynamic_entry == 0) {
        dynamic_entry = 16;
    }
    if (dynamic_bytes > 0) {
        for (uint64_t offset = 0; offset + dynamic_entry <= dynamic_bytes;
             offset += dynamic_entry) {
            unsigned char entry[16];

            if (read_exact_at(fd, dynamic_offset + offset, entry, sizeof(entry)) != 0) {
                close(fd);
                return -1;
            }
            int64_t tag = (int64_t)load_u64(entry);

            if (tag == DT_NULL) {
                break;
            }
            if (tag == DT_NEEDED) {
                facts->needed_count += 1;
            }
            if (tag == DT_STRTAB) {
                strtab = load_u64(entry + 8);
            }
        }
    }
    (void)strtab;
    result = 0;
    close(fd);
    return result;
}

/* -------------------------------------------------------------- transcript */

struct transcript {
    char buffer[TRANSCRIPT_BYTES];
    size_t length;
    size_t total;
    size_t line_start;
    int saw_start;
    int saw_ok;
    unsigned long loader_base;
    int saw_loader_line;
    int truncated;
};

static void transcript_init(struct transcript *t)
{
    memset(t, 0, sizeof(*t));
}

static void transcript_flush_line(struct transcript *t);

static void transcript_push(struct transcript *t, char byte)
{
    if (t->length == sizeof(t->buffer)) {
        size_t drop = sizeof(t->buffer) / 2;

        memmove(t->buffer, t->buffer + drop, sizeof(t->buffer) - drop);
        t->length -= drop;
        t->line_start = t->line_start >= drop ? t->line_start - drop : 0;
        t->truncated = 1;
    }
    t->buffer[t->length++] = byte;
    t->total++;

    if (byte == '\n') {
        transcript_flush_line(t);
    }
}

/* Report each line as it completes, so a partial transcript is visible even if
 * the child is killed at the deadline. */
static void transcript_flush_line(struct transcript *t)
{
    size_t line_bytes = t->length - t->line_start;
    char *line = t->buffer + t->line_start;

    while (line_bytes > 0 &&
           (line[line_bytes - 1] == '\n' || line[line_bytes - 1] == '\r')) {
        line_bytes--;
    }
    if (line_bytes > 0) {
        char saved = line[line_bytes];

        line[line_bytes] = '\0';
        if (strstr(line, MARKER_START) != NULL) {
            t->saw_start = 1;
        }
        if (strstr(line, MARKER_OK) != NULL) {
            t->saw_ok = 1;
        }
        if (strstr(line, MARKER_LOADER) != NULL) {
            /* The loader line carries AT_BASE, which is the value the kernel
             * supplied and which is zero when no interpreter was involved. */
            const char *base = strstr(line, "base=0x");

            if (base != NULL) {
                t->loader_base = strtoul(base + 7, NULL, 16);
                t->saw_loader_line = 1;
            }
        }
        printf("THEKERNEL_GLIBC_SMOKE_CHILD: %s\n", line);
        fflush(stdout);
        line[line_bytes] = saved;
    }
    t->line_start = t->length;
}

/* ---------------------------------------------------------------- child run */

struct child {
    pid_t pid;
    int fd;
};

static void child_reap(struct child *child)
{
    int status;

    if (child->pid <= 0) {
        return;
    }
    kill(-child->pid, SIGKILL);
    while (waitpid(child->pid, &status, 0) < 0 && errno == EINTR) {
        continue;
    }
    if (child->fd >= 0) {
        close(child->fd);
        child->fd = -1;
    }
    child->pid = -1;
}

static int run_program(struct child *child, const char *program, struct transcript *t,
                       int64_t *exit_status, int64_t *elapsed_ms)
{
    int pipe_fds[2];
    int64_t started, deadline;
    int drained = 0;

    if (pipe(pipe_fds) != 0) {
        return -1;
    }
    started = monotonic_ms();
    if (started < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return -1;
    }
    deadline = started + SMOKE_TIMEOUT_MS;

    child->pid = fork();
    if (child->pid < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return -1;
    }
    if (child->pid == 0) {
        char *const arguments[] = { (char *)program, NULL };

        (void)setpgid(0, 0);
        close(pipe_fds[0]);
        if (dup2(pipe_fds[1], STDOUT_FILENO) < 0 ||
            dup2(pipe_fds[1], STDERR_FILENO) < 0) {
            _exit(CHILD_SETUP_EXIT_STATUS);
        }
        if (pipe_fds[1] > STDERR_FILENO) {
            close(pipe_fds[1]);
        }
        execv(program, arguments);
        fprintf(stderr, "THEKERNEL_GLIBC_SMOKE_EXEC_FAIL program=%s errno=%d (%s)\n",
                program, errno, strerror(errno));
        fflush(stderr);
        _exit(CHILD_EXEC_EXIT_STATUS);
    }

    close(pipe_fds[1]);
    child->fd = pipe_fds[0];
    (void)setpgid(child->pid, child->pid);

    for (;;) {
        struct pollfd descriptor;
        int64_t now = monotonic_ms();
        int ready;

        if (now < 0) {
            child_reap(child);
            return -1;
        }
        if (now >= deadline) {
            child_reap(child);
            *elapsed_ms = now - started;
            return 1;
        }
        descriptor.fd = child->fd;
        descriptor.events = drained ? 0 : POLLIN;
        descriptor.revents = 0;
        ready = poll(&descriptor, 1, POLL_SLICE_MS);
        if (ready < 0) {
            if (errno == EINTR) {
                continue;
            }
            child_reap(child);
            return -1;
        }
        if (ready > 0 && (descriptor.revents & (POLLIN | POLLHUP | POLLERR)) != 0) {
            char chunk[1024];
            ssize_t bytes = read(child->fd, chunk, sizeof(chunk));

            if (bytes > 0) {
                for (ssize_t index = 0; index < bytes; ++index) {
                    if (t->total < TRANSCRIPT_HARD_LIMIT) {
                        transcript_push(t, chunk[index]);
                    }
                }
            } else if (bytes == 0) {
                drained = 1;
            } else if (errno != EINTR && errno != EAGAIN && errno != EWOULDBLOCK) {
                child_reap(child);
                return -1;
            }
        }
        {
            int status = 0;
            pid_t done = waitpid(child->pid, &status, WNOHANG);

            if (done == child->pid) {
                for (;;) {
                    char chunk[1024];
                    ssize_t bytes = read(child->fd, chunk, sizeof(chunk));

                    if (bytes > 0) {
                        for (ssize_t index = 0; index < bytes; ++index) {
                            if (t->total < TRANSCRIPT_HARD_LIMIT) {
                                transcript_push(t, chunk[index]);
                            }
                        }
                        continue;
                    }
                    if (bytes < 0 && errno == EINTR) {
                        continue;
                    }
                    break;
                }
                if (WIFEXITED(status)) {
                    *exit_status = WEXITSTATUS(status);
                } else if (WIFSIGNALED(status)) {
                    *exit_status = -WTERMSIG(status);
                } else {
                    *exit_status = -1;
                }
                *elapsed_ms = monotonic_ms() - started;
                close(child->fd);
                child->fd = -1;
                child->pid = -1;
                return 0;
            }
            if (done < 0 && errno != EINTR && errno != ECHILD) {
                child_reap(child);
                return -1;
            }
        }
    }
}

/* -------------------------------------------------------------------- main */

int main(void)
{
    const char *program = getenv(PROGRAM_ENV);
    const char *loader = getenv(LOADER_ENV);
    const char *root = getenv(ROOT_ENV);
    char loader_on_disk[PATH_BUFFER_BYTES];
    struct elf_facts facts;
    struct transcript t;
    struct child child = { -1, -1 };
    int64_t exit_status = -1, elapsed_ms = -1;
    int outcome;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (program == NULL || program[0] == '\0') {
        program = DEFAULT_PROGRAM;
    }
    if (loader == NULL || loader[0] == '\0') {
        loader = DEFAULT_LOADER;
    }
    if (root != NULL && root[0] != '\0') {
        snprintf(loader_on_disk, sizeof(loader_on_disk), "%s%s", root, loader);
    } else {
        snprintf(loader_on_disk, sizeof(loader_on_disk), "%s", loader);
    }

    emit("THEKERNEL_GLIBC_SMOKE_INTERFACE program=%s loader=%s timeout_ms=%d", program,
         loader, SMOKE_TIMEOUT_MS);

    /* Stage 1: the program must really be dynamic, and it must name the loader
     * this payload staged.  Both are checked before anything runs, because a
     * static program would pass every check below while proving nothing. */
    if (inspect_program(program, &facts) != 0) {
        fail("inspect-program", "readable-elf64", "path=%s", program);
        return 1;
    }
    if (!facts.has_interp) {
        fail("inspect-program", "has-pt-interp", "path=%s", program);
        return 1;
    }
    if (facts.needed_count == 0) {
        fail("inspect-program", "has-dt-needed", "path=%s", program);
        return 1;
    }
    emit("THEKERNEL_GLIBC_SMOKE_PROGRAM interp=%s needed=%d machine=%d",
         facts.interp, facts.needed_count, facts.machine);
    if (strcmp(facts.interp, loader) != 0) {
        fail("inspect-program", "interp-is-staged-loader", "interp=%s loader=%s",
             facts.interp, loader);
        return 1;
    }
    /* And the loader it names has to be there: this is the failure the milestone
     * is expected to hit first, so it is reported as its own condition rather
     * than as a mysterious exit status later. */
    if (access(loader_on_disk, R_OK) != 0) {
        fail("stage-loader", "staged-and-readable", "path=%s guest_path=%s",
             loader_on_disk, loader);
        return 1;
    }

    /* Stage 2: run it.  This is the claim under test. */
    transcript_init(&t);
    outcome = run_program(&child, program, &t, &exit_status, &elapsed_ms);
    if (outcome < 0) {
        fail("run-program", "fork-and-read", "path=%s", program);
        return 1;
    }
    if (outcome > 0) {
        fail("run-program", "bounded-deadline", "elapsed_ms=%lld timeout_ms=%d",
             (long long)elapsed_ms, SMOKE_TIMEOUT_MS);
        return 1;
    }

    emit("THEKERNEL_GLIBC_SMOKE_EXIT status=%lld elapsed_ms=%lld", (long long)exit_status,
         (long long)elapsed_ms);

    if (!t.saw_start) {
        fail("run-program", "program-started", "marker=%s", MARKER_START);
        return 1;
    }
    if (!t.saw_loader_line) {
        fail("run-program", "loader-reported", "marker=%s", MARKER_LOADER);
        return 1;
    }
    /* AT_BASE zero means the kernel started the program directly and no
     * interpreter ran.  The program reports it rather than checking it, because
     * only the program can read the vector the kernel built. */
    if (t.loader_base == 0) {
        fail("run-program", "loader-base-nonzero", "marker=%s", MARKER_LOADER);
        return 1;
    }
    emit("THEKERNEL_GLIBC_SMOKE_LOADER_BASE base=0x%lx", t.loader_base);
    if (!t.saw_ok) {
        fail("run-program", "program-ok-marker", "marker=%s", MARKER_OK);
        return 1;
    }
    if (exit_status != 0) {
        fail("run-program", "exit-zero", "status=%lld elapsed_ms=%lld",
             (long long)exit_status, (long long)elapsed_ms);
        return 1;
    }

    emit("THEKERNEL_GLIBC_SMOKE_OK elapsed_ms=%lld lines=%llu loader_base=0x%lx",
         (long long)elapsed_ms, (unsigned long long)t.total, t.loader_base);
    return 0;
}
