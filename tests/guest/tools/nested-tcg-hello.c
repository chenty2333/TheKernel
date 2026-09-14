#define _GNU_SOURCE

/* TheKernel guest-toolchain: can a system emulator that runs *inside* the
 * guest execute code in the guest's own userspace?
 *
 * This is Phase 2a.  A static-musl qemu-system-x86_64 is staged in the guest
 * rootfs together with a freestanding kernel image.  This helper forks the
 * emulator, lets it boot that image under TCG, and then requires four
 * separate things to be true at once:
 *
 *   1. the emulator is a static x86_64 ELF: no PT_INTERP, no DT_NEEDED.
 *      A dynamically linked emulator would prove only that this kernel has a
 *      working loader, which is Phase 1's claim, not this one.
 *   2. the inner kernel's banner arrives on the emulator's serial channel,
 *      matched on its INNER_-prefixed markers, so "the emulator started" is
 *      never mistaken for "the inner code ran".
 *   3. the inner kernel powers the machine off with an ACPI S5 transition and
 *      the emulator process exits 0 as a result.  The inner image deliberately
 *      has NO isa-debug-exit device configured, so a forced-exit shortcut
 *      cannot produce this status and a wedged inner boot shows up as the
 *      timeout below rather than as a passing case.
 *   4. the whole thing finishes inside a bounded deadline.  The emulator is
 *      reaped on every failure path; a leftover emulator would corrupt every
 *      later case in the suite.
 *
 * The inner workload runs entirely in the *guest's* userspace under TCG.  No
 * kernel-space emulator is involved, and this says nothing about whether the
 * guest could run the image under hardware virtualisation -- TheKernel exposes
 * no nested-VMX interface, and none is claimed here.
 *
 * Stable markers:
 *   THEKERNEL_NESTED_TCG_HELLO_OK
 *   THEKERNEL_NESTED_TCG_HELLO_FAIL <operation> condition=<c> k=v ... errno=<n> (<message>)
 *   THEKERNEL_NESTED_TCG_HELLO_INNER: <line>       captured inner serial line
 *   THEKERNEL_NESTED_TCG_HELLO_<STAGE> k=v ...     greppable progress
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
#error "the nested TCG smoke is x86_64-only"
#endif

/* ELF x86_64 ABI values, spelled out locally: this helper is built once with
 * the host glibc for validation and once with the guest musl for the rootfs,
 * so it must not depend on a header either sysroot may or may not ship. */
#define TK_ELF_HEADER_BYTES 64U
#define TK_ELF_DYN_BYTES 16U
#define TK_ELF_MAGIC0 0x7fU
#define TK_ELF_CLASS_OFFSET 4U
#define TK_ELF_CLASS32 1U
#define TK_ELF_CLASS64 2U
#define TK_ELF_DATA_OFFSET 5U
#define TK_ELF_DATA_LSB 1U
#define TK_ELF_MACHINE_OFFSET 18U
#define TK_EM_386 3U
#define TK_EM_X86_64 62U
#define TK_ELF_TYPE_OFFSET 16U
#define TK_ET_EXEC 2
#define TK_ET_DYN 3

/*
 * ELF32 and ELF64 share almost nothing about their header layout.  Only
 * e_ident, e_type, e_machine and e_version agree; from e_version onwards the
 * two diverge:
 *
 *              ELF32   ELF64
 *   e_phoff      28      32
 *   e_shoff      32      40
 *   e_phentsize  42      54
 *   e_phnum      44      56
 *   e_shentsize  46      58
 *   e_shnum      48      60
 *
 * The entry layouts differ as well (p_offset at 4 versus 8, p_filesz at 16
 * versus 32), as do the section header entries.
 *
 * Using one class's offsets for both is exactly the mistake an earlier
 * revision of this file made: it read a 32-bit image's e_shentsize as its
 * e_phentsize, the walk "succeeded", and it reported a load size 10,000x too
 * large.  Both layouts are spelled out here in full for that reason.
 */
#define TK_E32_PHOFF 28U
#define TK_E32_SHOFF 32U
#define TK_E32_PHENTSIZE 42U
#define TK_E32_PHNUM 44U
#define TK_E32_SHENTSIZE 46U
#define TK_E32_SHNUM 48U
#define TK_E32_PHDR_BYTES 32U
#define TK_E32_PHDR_OFFSET 4U
#define TK_E32_PHDR_FILESZ 16U
#define TK_E64_PHOFF 32U
#define TK_E64_SHOFF 40U
#define TK_E64_PHENTSIZE 54U
#define TK_E64_PHNUM 56U
#define TK_E64_SHENTSIZE 58U
#define TK_E64_SHNUM 60U
#define TK_E64_PHDR_BYTES 56U
#define TK_E64_PHDR_OFFSET 8U
#define TK_E64_PHDR_FILESZ 32U
#define TK_E64_SHDR_BYTES 64U
/* Identical in both classes, so these keep a single name each. */
#define TK_ELF_PHDR_TYPE_OFFSET 0U
#define TK_ELF_PHDR_BYTES TK_E64_PHDR_BYTES
#define TK_ELF_SH_TYPE_OFFSET 4U
#define TK_ELF_SH_OFFSET_OFFSET 24U
#define TK_ELF_SH_SIZE_OFFSET 32U
#define TK_ELF_SH_ENTSIZE_OFFSET 56U
#define TK_PT_LOAD 1U
#define TK_PT_DYNAMIC 2U
#define TK_PT_INTERP 3U
#define TK_SHT_DYNAMIC 6U
#define TK_SHT_DYNSYM 11U
#define TK_DT_NULL 0
#define TK_DT_NEEDED 1

/* The interface under test.  Both knobs exist ONLY so this exact binary can be
 * validated on the host against a host-built payload; the guest runs with the
 * compiled-in defaults, which are the paths its rootfs installs. */
#define QEMU_ENV "THEKERNEL_NESTED_QEMU"
#define KERNEL_ENV "THEKERNEL_NESTED_KERNEL"
#define DEFAULT_QEMU "/opt/thekernel-tools/bin/qemu-system-x86_64"
#define DEFAULT_KERNEL "/opt/thekernel-tools/payloads/hello-acpi.elf"

/* Inner machine sizing.  The inner kernel is freestanding and needs a few MiB,
 * but QEMU's own TCG start-up cost dominates, so the inner RAM is kept small
 * on purpose: every MiB here is a MiB the outer guest does not have. */
#define INNER_MEMORY "256"
#define INNER_ACCEL "tcg"
#define INNER_CPU_MODEL "qemu64"

/* Deadlines.  Measured on the host at ~0.10 s for this image, so the budget is
 * three orders of magnitude of headroom for a TCG-under-TCG guest; it exists
 * to bound a wedged emulator, not to measure performance.  The runner's own
 * case timeout must exceed NESTED_TIMEOUT_MS plus the kill grace. */
#define NESTED_TIMEOUT_MS 20000
#define KILL_GRACE_MS 5000
#define POLL_SLICE_MS 100

#define PATH_BUFFER_BYTES 4096U
/* Retained inner transcript.  The banner is ~800 bytes; anything beyond this
 * is truncated from the front so the tail -- where a failure would be -- is
 * what gets printed. */
#define TRANSCRIPT_BYTES 8192U
/* Upper bound on bytes accepted from the emulator, so a runaway emulator
 * cannot make this helper allocate or spin without limit. */
#define TRANSCRIPT_HARD_LIMIT (1U << 20)

/* Multiboot v1 header magic, which QEMU scans for in the first 8192 bytes of a
 * -kernel image.  Checked so a wrong or truncated artifact fails here instead
 * of inside the emulator. */
#define MULTIBOOT_SEARCH_BYTES 8192U
#define MULTIBOOT_MAGIC 0x1BADB002U

/* Markers the inner image prints.  This helper requires all of them, and it is
 * the *combination* that carries the meaning: OK alone would still be printed
 * by an image that then hung. */
#define INNER_OK_MARKER "INNER_HELLO_OK"
#define INNER_ACPI_MARKER "INNER_HELLO_ACPI "
#define INNER_KIND_MARKER "INNER_HELLO_SHUTDOWN_KIND=acpi-s5"
#define INNER_MODE_MARKER "INNER_CPU_MODE=x86_64-long-mode"

/* Exit statuses the forked child uses when it cannot become the emulator;
 * they are distinct from every status QEMU itself could return. */
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

    fprintf(stdout, "THEKERNEL_NESTED_TCG_HELLO_FAIL %s condition=%s ", operation, condition);
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

/* ------------------------------------------------------------- ELF reading */

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

static uint16_t load_u16(const unsigned char *p)
{
    return (uint16_t)((uint16_t)p[0] | ((uint16_t)p[1] << 8));
}

static uint32_t load_u32(const unsigned char *p)
{
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
           ((uint32_t)p[3] << 24);
}

static uint64_t load_u64(const unsigned char *p)
{
    return (uint64_t)load_u32(p) | ((uint64_t)load_u32(p + 4) << 32);
}

/*
 * Inspect an ELF executable well enough to answer the questions this case
 * depends on, and to report what it found.  Both ELF classes occur here and
 * neither is an accident, so the class and machine are reported rather than
 * required to be one particular pair:
 *
 *   the emulator    -- ELF64 / EM_X86_64, ET_EXEC, static (no PT_INTERP, no
 *                      DT_NEEDED).  A dynamic emulator would only prove that
 *                      this kernel has a loader, which is Phase 1's claim.
 *   the inner image -- ELF32 / EM_386.  QEMU's Multiboot v1 loader reloads a
 *                      -kernel file with I386_ELF_MACHINE and rejects an
 *                      EM_X86_64 image outright, so the 32-bit container is
 *                      what makes the 64-bit payload loadable at all.
 *
 * Returns 0 on success and fills the report; -1 with errno set otherwise.
 */
struct elf_report {
    int is_static;
    int is_exec;
    int is_64;              /* ELFCLASS64 rather than ELFCLASS32            */
    int machine;            /* e_machine                                     */
    int has_interp;
    int needed_count;
    uint64_t dynamic_bytes;
    uint64_t load_segments;
    uint64_t load_bytes;
};

static int inspect_elf(const char *path, struct elf_report *report)
{
    unsigned char header[TK_ELF_HEADER_BYTES];
    uint64_t phoff, shoff, phentsize, phnum, shentsize, shnum;
    uint64_t interp_segments = 0, load_segments = 0, load_bytes = 0;
    uint64_t dynamic_vaddr = 0, dynamic_size = 0;
    unsigned char segment[TK_ELF_PHDR_BYTES];
    unsigned char section[64];
    uint64_t index;
    int machine;
    int fd;

    memset(report, 0, sizeof(*report));

    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    if (read_exact_at(fd, 0, header, sizeof(header)) != 0) {
        close(fd);
        return -1;
    }
    if (header[0] != TK_ELF_MAGIC0 ||
        (header[TK_ELF_CLASS_OFFSET] != TK_ELF_CLASS32 &&
         header[TK_ELF_CLASS_OFFSET] != TK_ELF_CLASS64) ||
        header[TK_ELF_DATA_OFFSET] != TK_ELF_DATA_LSB) {
        close(fd);
        errno = ENOEXEC;
        return -1;
    }
    report->is_64 = header[TK_ELF_CLASS_OFFSET] == TK_ELF_CLASS64;
    machine = load_u16(header + TK_ELF_MACHINE_OFFSET);
    report->machine = machine;
    report->is_exec = load_u16(header + TK_ELF_TYPE_OFFSET) == TK_ET_EXEC;

    if (!report->is_64) {
        /* A 32-bit container has no dynamic section worth walking: its
         * PT_DYNAMIC/DT_NEEDED would describe a 32-bit loader this kernel
         * cannot run anyway, and the inner image is a flat Multiboot payload.
         * Static is decided from PT_INTERP alone for this class. */
        uint32_t phoff32 = load_u32(header + TK_E32_PHOFF);
        uint16_t phentsize32 = load_u16(header + TK_E32_PHENTSIZE);
        uint16_t phnum32 = load_u16(header + TK_E32_PHNUM);

        if (phentsize32 < TK_E32_PHDR_BYTES) {
            close(fd);
            errno = ENOEXEC;
            return -1;
        }
        for (index = 0; index < phnum32; index++) {
            uint32_t type;

            if (read_exact_at(fd, phoff32 + index * phentsize32, segment,
                              TK_E32_PHDR_BYTES) != 0) {
                close(fd);
                return -1;
            }
            type = load_u32(segment + TK_ELF_PHDR_TYPE_OFFSET);
            if (type == TK_PT_INTERP) {
                interp_segments++;
            } else if (type == TK_PT_LOAD) {
                load_segments++;
                load_bytes += load_u32(segment + TK_E32_PHDR_FILESZ);
            }
        }
        report->has_interp = interp_segments != 0;
        report->load_segments = load_segments;
        report->load_bytes = load_bytes;
        report->is_static = !report->has_interp;
        close(fd);
        return 0;
    }

    phoff = load_u64(header + TK_E64_PHOFF);
    phentsize = load_u16(header + TK_E64_PHENTSIZE);
    phnum = load_u16(header + TK_E64_PHNUM);
    if (phentsize < TK_E64_PHDR_BYTES) {
        close(fd);
        errno = ENOEXEC;
        return -1;
    }
    for (index = 0; index < phnum; index++) {
        uint32_t type;

        if (read_exact_at(fd, phoff + index * phentsize, segment, TK_E64_PHDR_BYTES) != 0) {
            close(fd);
            return -1;
        }
        type = load_u32(segment + TK_ELF_PHDR_TYPE_OFFSET);
        if (type == TK_PT_INTERP) {
            interp_segments++;
        } else if (type == TK_PT_LOAD) {
            load_segments++;
            load_bytes += load_u64(segment + TK_E64_PHDR_FILESZ);
        } else if (type == TK_PT_DYNAMIC) {
            dynamic_vaddr = load_u64(segment + TK_E64_PHDR_OFFSET);
            dynamic_size = load_u64(segment + TK_E64_PHDR_FILESZ);
        }
    }
    report->has_interp = interp_segments != 0;
    report->load_segments = load_segments;
    report->load_bytes = load_bytes;

    /* A PT_DYNAMIC segment is where DT_NEEDED would live.  Read it through the
     * file offset, which for a non-relocatable link equals the virtual
     * address; a static binary has no such segment at all, which is the
     * stronger and more common case. */
    shoff = load_u64(header + TK_E64_SHOFF);
    shentsize = load_u16(header + TK_E64_SHENTSIZE);
    shnum = load_u16(header + TK_E64_SHNUM);
    if (shentsize >= 40 && shnum > 0 && shoff != 0) {
        for (index = 0; index < shnum; index++) {
            uint32_t type;

            if (read_exact_at(fd, shoff + index * shentsize, section, 40) != 0) {
                close(fd);
                return -1;
            }
            type = load_u32(section + TK_ELF_SH_TYPE_OFFSET);
            if (type == TK_SHT_DYNAMIC) {
                dynamic_vaddr = load_u64(section + TK_ELF_SH_OFFSET_OFFSET);
                dynamic_size = load_u64(section + TK_ELF_SH_SIZE_OFFSET);
            }
        }
    }

    if (dynamic_size >= TK_ELF_DYN_BYTES) {
        uint64_t offset = 0;

        report->dynamic_bytes = dynamic_size;
        while (offset + TK_ELF_DYN_BYTES <= dynamic_size) {
            unsigned char entry[TK_ELF_DYN_BYTES];
            int64_t tag;

            if (read_exact_at(fd, dynamic_vaddr + offset, entry, sizeof(entry)) != 0) {
                close(fd);
                return -1;
            }
            tag = (int64_t)load_u64(entry);
            if (tag == TK_DT_NULL) {
                break;
            }
            if (tag == TK_DT_NEEDED) {
                report->needed_count++;
            }
            offset += TK_ELF_DYN_BYTES;
        }
    }

    report->is_static = !report->has_interp && report->needed_count == 0;
    close(fd);
    return 0;
}

/* Reject a -kernel image QEMU would refuse, before forking an emulator. */
static int has_multiboot_header(const char *path)
{
    static const unsigned char magic[4] = {
        (unsigned char)(MULTIBOOT_MAGIC & 0xFFU),
        (unsigned char)((MULTIBOOT_MAGIC >> 8) & 0xFFU),
        (unsigned char)((MULTIBOOT_MAGIC >> 16) & 0xFFU),
        (unsigned char)((MULTIBOOT_MAGIC >> 24) & 0xFFU),
    };
    unsigned char window[MULTIBOOT_SEARCH_BYTES];
    size_t got = 0;
    int fd;

    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    while (got < sizeof(window)) {
        ssize_t bytes = read(fd, window + got, sizeof(window) - got);

        if (bytes < 0) {
            if (errno == EINTR) {
                continue;
            }
            close(fd);
            return -1;
        }
        if (bytes == 0) {
            break;
        }
        got += (size_t)bytes;
    }
    close(fd);
    if (got < sizeof(magic)) {
        return 0;
    }
    for (size_t offset = 0; offset + sizeof(magic) <= got; offset += 4) {
        if (memcmp(window + offset, magic, sizeof(magic)) == 0) {
            return 1;
        }
    }
    return 0;
}

/* -------------------------------------------------------------- transcript */

struct transcript {
    char buffer[TRANSCRIPT_BYTES];
    size_t length;          /* bytes retained in buffer                      */
    size_t total;           /* bytes seen, including discarded ones          */
    size_t line_start;      /* index in buffer where the pending line starts */
    int saw_ok;
    int saw_acpi;
    int saw_kind;
    int saw_mode;
    int truncated;
};

static void transcript_init(struct transcript *t)
{
    memset(t, 0, sizeof(*t));
}

static void transcript_flush_line(struct transcript *t);

/* Append one byte, discarding the oldest bytes when the window is full. */
static void transcript_push(struct transcript *t, char byte)
{
    if (t->length == sizeof(t->buffer)) {
        size_t drop = sizeof(t->buffer) / 2;

        memmove(t->buffer, t->buffer + drop, sizeof(t->buffer) - drop);
        t->length -= drop;
        if (t->line_start >= drop) {
            t->line_start -= drop;
        } else {
            /* The pending line begins in the discarded region; treat what is
             * left as its tail rather than pretending it is complete. */
            t->line_start = 0;
        }
        t->truncated = 1;
    }
    t->buffer[t->length++] = byte;
    t->total++;

    if (byte == '\n') {
        transcript_flush_line(t);
    }
}

/* A line is only reported once it is complete, so a partial write never looks
 * like a marker. */
static void transcript_flush_line(struct transcript *t)
{
    size_t line_bytes = t->length - t->line_start;
    char *line = t->buffer + t->line_start;

    while (line_bytes > 0 && (line[line_bytes - 1] == '\n' || line[line_bytes - 1] == '\r')) {
        line_bytes--;
    }
    if (line_bytes > 0) {
        char saved = line[line_bytes];

        line[line_bytes] = '\0';
        if (strstr(line, INNER_OK_MARKER) != NULL) {
            t->saw_ok = 1;
        }
        if (strstr(line, INNER_ACPI_MARKER) != NULL) {
            t->saw_acpi = 1;
        }
        if (strstr(line, INNER_KIND_MARKER) != NULL) {
            t->saw_kind = 1;
        }
        if (strstr(line, INNER_MODE_MARKER) != NULL) {
            t->saw_mode = 1;
        }
        printf("THEKERNEL_NESTED_TCG_HELLO_INNER: %s\n", line);
        fflush(stdout);
        line[line_bytes] = saved;
    }
    t->line_start = t->length;
}

/* ---------------------------------------------------------------- emulator */

struct child {
    pid_t pid;
    int fd;             /* read end of the serial pipe */
};

static void child_reap(struct child *child)
{
    int status;
    int attempts;

    if (child->pid <= 0) {
        return;
    }
    /* The emulator owns a process group: kill the group, not just the leader,
     * so no helper thread process survives into the next case. */
    kill(-child->pid, SIGKILL);
    for (attempts = 0; attempts < 100; attempts++) {
        pid_t done = waitpid(child->pid, &status, WNOHANG);

        if (done == child->pid || (done < 0 && errno == ECHILD)) {
            break;
        }
        if (done < 0 && errno != EINTR) {
            break;
        }
        usleep(1000);
    }
    if (child->fd >= 0) {
        close(child->fd);
        child->fd = -1;
    }
    child->pid = -1;
}

/*
 * Run the emulator to completion, keeping the serial transcript.  Returns:
 *    0  the emulator exited and its status was collected
 *    1  the deadline expired and the emulator was killed
 *   -1  a setup or read error; errno is set
 */
static int run_emulator(struct child *child, const char *qemu, const char *kernel,
                        struct transcript *t, int64_t *exit_status, int64_t *elapsed_ms)
{
    int pipe_fds[2];
    char memory_argument[64];
    int64_t started, now, deadline;
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
    deadline = started + NESTED_TIMEOUT_MS;
    snprintf(memory_argument, sizeof(memory_argument), "%s", INNER_MEMORY);

    child->pid = fork();
    if (child->pid < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return -1;
    }
    if (child->pid == 0) {
        char *const arguments[] = {
            (char *)qemu,
            (char *)"-accel", (char *)INNER_ACCEL,
            (char *)"-cpu", (char *)INNER_CPU_MODEL,
            (char *)"-m", memory_argument,
            (char *)"-kernel", (char *)kernel,
            (char *)"-display", (char *)"none",
            (char *)"-serial", (char *)"stdio",
            (char *)"-no-reboot",
            (char *)"-nodefaults",
            NULL,
        };

        /* Own process group so the parent can reap the whole emulator. */
        (void)setpgid(0, 0);
        close(pipe_fds[0]);
        if (dup2(pipe_fds[1], STDOUT_FILENO) < 0 || dup2(pipe_fds[1], STDERR_FILENO) < 0) {
            _exit(CHILD_SETUP_EXIT_STATUS);
        }
        if (pipe_fds[1] > STDERR_FILENO) {
            close(pipe_fds[1]);
        }
        execv(qemu, arguments);
        _exit(CHILD_EXEC_EXIT_STATUS);
    }

    close(pipe_fds[1]);
    child->fd = pipe_fds[0];
    /* Parent-side too, to close the race where the child is killed before it
     * reaches its own setpgid. */
    (void)setpgid(child->pid, child->pid);

    for (;;) {
        struct pollfd descriptor;
        int ready;

        now = monotonic_ms();
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
                ssize_t index;

                for (index = 0; index < bytes; index++) {
                    if (t->total < TRANSCRIPT_HARD_LIMIT) {
                        transcript_push(t, chunk[index]);
                    }
                }
            } else if (bytes == 0) {
                /* End of file: the emulator closed its console.  Keep polling
                 * for the exit status rather than reading in a loop. */
                drained = 1;
            } else if (errno != EINTR && errno != EAGAIN && errno != EWOULDBLOCK) {
                child_reap(child);
                return -1;
            }
        }

        /* Reap on every pass: a fast inner boot exits before the pipe is even
         * drained, and waiting for EOF would burn the whole deadline. */
        {
            int status = 0;
            pid_t done = waitpid(child->pid, &status, WNOHANG);

            if (done == child->pid) {
                /* Drain whatever is still buffered, bounded by the deadline
                 * re-checked on every iteration. */
                for (;;) {
                    char chunk[1024];
                    ssize_t bytes;

                    now = monotonic_ms();
                    if (now < 0 || now >= deadline) {
                        break;
                    }
                    bytes = read(child->fd, chunk, sizeof(chunk));
                    if (bytes > 0) {
                        ssize_t index;

                        for (index = 0; index < bytes; index++) {
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
    const char *qemu = getenv(QEMU_ENV);
    const char *kernel = getenv(KERNEL_ENV);
    struct elf_report qemu_report, kernel_report;
    struct transcript t;
    struct child child = { -1, -1 };
    int64_t exit_status = -1, elapsed_ms = -1;
    int multiboot, outcome;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (qemu == NULL || qemu[0] == '\0') {
        qemu = DEFAULT_QEMU;
    }
    if (kernel == NULL || kernel[0] == '\0') {
        kernel = DEFAULT_KERNEL;
    }

    emit("THEKERNEL_NESTED_TCG_HELLO_INTERFACE qemu=%s kernel=%s timeout_ms=%d",
         qemu, kernel, NESTED_TIMEOUT_MS);

    /* Stage 1: the emulator must be a static x86_64 executable.  Checked
     * before running it, because a dynamic emulator's failure would otherwise
     * look like an inner-boot failure. */
    if (inspect_elf(qemu, &qemu_report) != 0) {
        fail("inspect-emulator", "readable-elf64", "path=%s", qemu);
        return 1;
    }
    emit("THEKERNEL_NESTED_TCG_HELLO_EMULATOR class=ELF64 machine=%d type=%s static=%d "
         "interp=%d needed=%d load_segments=%llu load_bytes=%llu",
         qemu_report.machine, qemu_report.is_exec ? "EXEC" : "DYN", qemu_report.is_static,
         qemu_report.has_interp, qemu_report.needed_count,
         (unsigned long long)qemu_report.load_segments,
         (unsigned long long)qemu_report.load_bytes);
    if (!qemu_report.is_64 || qemu_report.machine != TK_EM_X86_64) {
        fail("inspect-emulator", "elf64-x86_64", "class64=%d machine=%d",
             qemu_report.is_64, qemu_report.machine);
        return 1;
    }
    if (!qemu_report.is_static) {
        fail("inspect-emulator", "statically-linked",
             "interp=%d needed=%d load_bytes=%llu", qemu_report.has_interp,
             qemu_report.needed_count, (unsigned long long)qemu_report.load_bytes);
        return 1;
    }

    /* Stage 2: the inner image must be what the emulator can load. */
    if (inspect_elf(kernel, &kernel_report) != 0) {
        fail("inspect-inner-image", "readable-elf64", "path=%s", kernel);
        return 1;
    }
    multiboot = has_multiboot_header(kernel);
    emit("THEKERNEL_NESTED_TCG_HELLO_INNER_IMAGE class=%s machine=%d type=%s multiboot1=%d "
         "load_segments=%llu load_bytes=%llu",
         kernel_report.is_64 ? "ELF64" : "ELF32", kernel_report.machine,
         kernel_report.is_exec ? "EXEC" : "DYN", multiboot != 1 ? 0 : 1,
         (unsigned long long)kernel_report.load_segments,
         (unsigned long long)kernel_report.load_bytes);
    if (kernel_report.is_64 || kernel_report.machine != TK_EM_386) {
        fail("inspect-inner-image", "elf32-i386",
             "note=QEMU reloads a -kernel image with I386_ELF_MACHINE class64=%d machine=%d",
             kernel_report.is_64, kernel_report.machine);
        return 1;
    }
    if (multiboot != 1) {
        fail("inspect-inner-image", "multiboot1-header-present",
             "note=QEMU scans the first %u bytes for 0x%08x", MULTIBOOT_SEARCH_BYTES,
             MULTIBOOT_MAGIC);
        return 1;
    }

    /* Stage 3: run it.  This is the claim under test. */
    transcript_init(&t);
    outcome = run_emulator(&child, qemu, kernel, &t, &exit_status, &elapsed_ms);
    if (outcome < 0) {
        fail("run-emulator", "fork-and-read", "path=%s", qemu);
        return 1;
    }
    if (outcome > 0) {
        fail("run-emulator", "bounded-deadline", "elapsed_ms=%lld timeout_ms=%d",
             (long long)elapsed_ms, NESTED_TIMEOUT_MS);
        return 1;
    }

    emit("THEKERNEL_NESTED_TCG_HELLO_SERIAL lines=%llu bytes=%llu truncated=%d",
         (unsigned long long)t.total, (unsigned long long)t.total, t.truncated);
    emit("THEKERNEL_NESTED_TCG_HELLO_EXIT status=%lld elapsed_ms=%lld",
         (long long)exit_status, (long long)elapsed_ms);

    /* Stage 4: the four conditions, each reported separately so a failure says
     * which one broke instead of only "the emulator did not work". */
    if (!t.saw_ok) {
        fail("inner-banner", "inner-ok-marker", "marker=%s", INNER_OK_MARKER);
        return 1;
    }
    if (!t.saw_mode) {
        fail("inner-banner", "inner-long-mode-marker", "marker=%s", INNER_MODE_MARKER);
        return 1;
    }
    if (!t.saw_kind || !t.saw_acpi) {
        fail("inner-shutdown", "inner-acpi-s5-marker",
             "kind_marker=%d acpi_marker=%d marker=%s", t.saw_kind, t.saw_acpi,
             INNER_KIND_MARKER);
        return 1;
    }
    if (exit_status != 0) {
        /* A negative status is a signal; the emulator only reports a non-zero
         * status here if the guest could not complete the poweroff. */
        fail("inner-shutdown", "emulator-exit-zero", "status=%lld elapsed_ms=%lld",
             (long long)exit_status, (long long)elapsed_ms);
        return 1;
    }

    emit("THEKERNEL_NESTED_TCG_HELLO_OK elapsed_ms=%lld inner_lines=%llu",
         (long long)elapsed_ms, (unsigned long long)t.total);
    return 0;
}
