#define _GNU_SOURCE

/* TheKernel guest-toolchain: does a real Linux distribution boot inside the
 * guest, in the guest's own userspace, under an emulator that is itself a
 * guest process?
 *
 * This is Phase 2b, and it is the point of the plan.  Phase 2a shows a
 * freestanding kernel booting under TCG; this shows Alpine Linux -- a real
 * distribution kernel with its own musl/BusyBox userland -- doing the same.
 * Nothing here is written for this test except the initramfs, and that is
 * assembled from the release's own minirootfs.
 *
 * Four conditions, reported separately so a failure says which one broke:
 *
 *   1. the emulator is a static x86_64 executable.  A dynamic one would prove
 *      only that this kernel has a loader, which Phase 1 already claims.
 *   2. the inner machine reaches userland.  The kernel's own serial output
 *      carries INNER_ markers, including /init reporting itself as PID 1, so a
 *      kernel that panicked or fell to an emergency shell fails here instead
 *      of being mistaken for a boot.
 *   3. the inner OS performs a normal shutdown and the emulator exits 0 as a
 *      result.  The image is booted with no isa-debug-exit device, so a forced
 *      exit cannot produce status 0; Alpine's poweroff reaches ACPI S5 and
 *      QEMU exits normally.
 *   4. all of it finishes inside a deadline sized from measurement.  The
 *      emulator's whole process group is reaped on every failure path, because
 *      a leftover emulator would corrupt every later case in the suite.
 *
 * Stable markers:
 *   THEKERNEL_NESTED_LINUX_BOOT_OK
 *   THEKERNEL_NESTED_LINUX_BOOT_FAIL <operation> condition=<c> k=v ... errno=<n> (<message>)
 *   THEKERNEL_NESTED_LINUX_BOOT_INNER: <line>       captured inner serial line
 *   THEKERNEL_NESTED_LINUX_BOOT_<STAGE> k=v ...     greppable progress
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
#error "the nested Linux boot case is x86_64-only"
#endif

/* The prefix every marker of this case carries.  The two nested cases use
 * distinct prefixes so neither one's output can be read as the other's. */
#define CATEGORY "NESTED_LINUX_BOOT"

/* The payload's paths.  Overridable, so the same binary can be pointed at a
 * different build without recompiling -- which is what makes it testable. */
#define QEMU_ENV "THEKERNEL_NESTED_QEMU"
#define KERNEL_ENV "THEKERNEL_NESTED_ALPINE_KERNEL"
#define INITRD_ENV "THEKERNEL_NESTED_ALPINE_INITRD"
#define QEMU_DATA_ENV "THEKERNEL_NESTED_QEMU_DATA"
#define DEFAULT_QEMU "/opt/thekernel-tools/bin/qemu-system-x86_64"
#define DEFAULT_KERNEL "/opt/thekernel-tools/payloads/vmlinuz-virt"
#define DEFAULT_INITRD "/opt/thekernel-tools/payloads/alpine-initramfs.cpio.gz"
/* QEMU loads its firmware data from a directory found relative to the binary,
 * and it needs it even for `-kernel`: on pc/q35 it still executes SeaBIOS
 * before the kernel.  A missing data directory is not a warning -- the
 * emulator exits before loading anything. */
#define DEFAULT_QEMU_DATA "/opt/thekernel-tools/share"

/* The inner machine.  `pc` is the machine this combination was measured on;
 * q35 works on the host but has not been measured under nested TCG.  No VGA
 * and no NIC: neither is used, both cost boot time, and leaving them out keeps
 * the option ROMs off the critical path.  One inner CPU on purpose -- more
 * would multiply the emulation cost without testing anything new. */
#define INNER_MACHINE "pc"
#define INNER_MEMORY "256"
#define INNER_CPUS "1"
#define INNER_ACCEL "tcg"
/* console=ttyS0 routes the kernel console, and therefore PID 1's stdout, to the
 * emulator's -serial stdio.  rdinit=/init makes our script PID 1 directly,
 * with no init system and no boot media.  quiet suppresses the ~200-line
 * kernel log: worth ~5% of the boot, and not what is being tested.  There is
 * deliberately no root= and no modloop= -- the initramfs is the root. */
#define INNER_APPEND "console=ttyS0 rdinit=/init quiet"

/* Deadlines.  This inner boot takes ~1.6-1.9 s on host TCG and was measured at
 * 54-86 s under nested TCG, a factor of 30-50.  The design budget of 120 s for
 * the inner phase comes from that measurement; 180 s keeps it with room for a
 * slower host.  Sizing this from the host figure would fail every correct run.
 * The suite's case timeout is set above this plus the kill grace. */
#define NESTED_TIMEOUT_MS 180000
#define KILL_GRACE_MS 5000
#define POLL_SLICE_MS 100

#define PATH_BUFFER_BYTES 4096U
/* A quiet Alpine boot is ~30 lines, but a boot that goes wrong prints far more
 * and that output is the evidence.  Kept generous, truncated from the front. */
#define TRANSCRIPT_BYTES 262144U
#define TRANSCRIPT_HARD_LIMIT (4U << 20)
#define TRANSCRIPT_LINE_BYTES 512U

/* The Linux x86 boot protocol puts "HdrS" at offset 0x202 of a bzImage, which
 * is what QEMU accepts for -kernel here.  Checking it is the cheapest way to
 * tell a kernel image from an arbitrary file before spending 180 s finding out
 * the hard way. */
/* Exit statuses the forked child uses when it cannot become the emulator.
 * Chosen to be distinct from every status QEMU itself could return, so a
 * failure to start is never read as a failing inner boot. */
#define CHILD_SETUP_EXIT_STATUS 126
#define CHILD_EXEC_EXIT_STATUS 127

#define BZIMAGE_MAGIC_OFFSET 0x202U
#define BZIMAGE_MAGIC "HdrS"
#define BZIMAGE_PROBE_BYTES 0x210U

/* Markers the inner Alpine init prints.  All are required: the combination is
 * what distinguishes "Alpine reached its own userland and shut down normally"
 * from "a kernel printed something". */
#define INNER_RELEASE_MARKER "INNER_ALPINE_RELEASE "
#define INNER_KERNEL_MARKER "INNER_KERNEL "
#define INNER_INIT_PID_MARKER "INNER_INIT_PID 1"
#define INNER_USERLAND_MARKER "INNER_USERLAND_OK"
#define INNER_SHUTDOWN_MARKER "INNER_SHUTDOWN_BEGIN"

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

    fprintf(stdout, "THEKERNEL_" CATEGORY "_FAIL %s condition=%s ", operation, condition);
    va_start(arguments, format);
    vfprintf(stdout, format, arguments);
    va_end(arguments);
    fprintf(stdout, " errno=%d (%s)\n", saved, strerror(saved));
    fflush(stdout);
}
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
/* -------------------------------------------------------------------- time */

static int64_t monotonic_ms(void)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    return (int64_t)now.tv_sec * 1000 + (int64_t)now.tv_nsec / 1000000;
}
/* -------------------------------------------------------------- transcript */

struct transcript {
    char buffer[TRANSCRIPT_BYTES];
    size_t length;          /* bytes retained in buffer                      */
    size_t total;           /* bytes seen, including discarded ones          */
    size_t line_start;      /* index in buffer where the pending line starts */
    int truncated;
    /* Each nested case looks for its own markers, so the meaning of a line is
     * supplied by the case rather than baked in here. */
    void (*sink)(const char *line, void *context);
    void *sink_context;
    size_t lines;           /* complete lines seen, kept or not              */
};

static void transcript_init(struct transcript *t)
{
    memset(t, 0, sizeof(*t));
}

static void transcript_set_sink(struct transcript *t, void (*sink)(const char *, void *),
                                void *context)
{
    t->sink = sink;
    t->sink_context = context;
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
        t->lines += 1;
        if (t->sink != NULL) {
            t->sink(line, t->sink_context);
        }
        printf("THEKERNEL_" CATEGORY "_INNER: %s\n", line);
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
                        const char *initrd, const char *data, struct transcript *t,
                        int64_t *exit_status, int64_t *elapsed_ms)
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
            /* The data directory, explicitly: without it QEMU cannot find the
             * firmware it executes even for -kernel. */
            (char *)"-L", (char *)data,
            (char *)"-machine", (char *)INNER_MACHINE,
            (char *)"-accel", (char *)INNER_ACCEL,
            (char *)"-m", memory_argument,
            (char *)"-smp", (char *)INNER_CPUS,
            /* No VGA and no NIC.  Unused, and both cost boot time; leaving
             * them out also keeps the video and network option ROMs out of the
             * critical path.  Note there is still no isa-debug-exit: the only
             * way out of this machine is the guest shutting it down. */
            (char *)"-vga", (char *)"none",
            (char *)"-nic", (char *)"none",
            (char *)"-display", (char *)"none",
            (char *)"-serial", (char *)"stdio",
            (char *)"-no-reboot",
            (char *)"-kernel", (char *)kernel,
            (char *)"-initrd", (char *)initrd,
            (char *)"-append", (char *)INNER_APPEND,
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


/* ------------------------------------------------------- bzImage probing */

/* QEMU accepts a bzImage for -kernel.  The Linux boot protocol puts "HdrS" at
 * offset 0x202, which is enough to tell a kernel image from a truncated
 * download or from an initramfs passed in the wrong argument, and it fails in
 * milliseconds instead of after the emulator has spent its whole deadline
 * failing to load something. */
static int check_bzimage(const char *path, uint64_t *bytes)
{
    unsigned char header[BZIMAGE_PROBE_BYTES];
    struct stat info;
    int fd, result;

    fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    if (fstat(fd, &info) != 0) {
        close(fd);
        return -1;
    }
    result = read_exact_at(fd, 0, header, sizeof(header));
    close(fd);
    if (result != 0) {
        return -1;
    }
    *bytes = (uint64_t)info.st_size;
    if (memcmp(header + BZIMAGE_MAGIC_OFFSET, BZIMAGE_MAGIC, 4) != 0) {
        errno = ENOEXEC;
        return -1;
    }
    return 0;
}

/* -------------------------------------------------------------- transcript */

/* The inner lines carry this case's whole verdict, so the checks run as the
 * lines arrive rather than after the boot: what is required is that specific
 * markers appeared, and that is a property of the bytes seen, not of the file
 * they were written to. */
struct marker_state {
    int release;
    int kernel;
    int init_pid;
    int userland;
    int shutdown;
    char release_value[128];
    char kernel_value[128];
};

static void note_line(const char *line, void *context)
{
    struct marker_state *state = (struct marker_state *)context;

    if (strncmp(line, INNER_RELEASE_MARKER, strlen(INNER_RELEASE_MARKER)) == 0) {
        state->release = 1;
        snprintf(state->release_value, sizeof(state->release_value), "%s",
                 line + strlen(INNER_RELEASE_MARKER));
    }
    if (strncmp(line, INNER_KERNEL_MARKER, strlen(INNER_KERNEL_MARKER)) == 0) {
        state->kernel = 1;
        snprintf(state->kernel_value, sizeof(state->kernel_value), "%s",
                 line + strlen(INNER_KERNEL_MARKER));
    }
    if (strcmp(line, INNER_INIT_PID_MARKER) == 0) {
        state->init_pid = 1;
    }
    if (strcmp(line, INNER_USERLAND_MARKER) == 0) {
        state->userland = 1;
    }
    if (strcmp(line, INNER_SHUTDOWN_MARKER) == 0) {
        state->shutdown = 1;
    }
}

/* -------------------------------------------------------------------- main */

int main(void)
{
    const char *qemu = getenv(QEMU_ENV);
    const char *kernel = getenv(KERNEL_ENV);
    const char *initrd = getenv(INITRD_ENV);
    const char *data = getenv(QEMU_DATA_ENV);
    struct marker_state markers;
    struct transcript t;
    struct child child = { -1, -1 };
    int64_t exit_status = -1, elapsed_ms = -1, initrd_bytes = 0;
    uint64_t kernel_bytes = 0;
    int outcome;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    memset(&markers, 0, sizeof(markers));

    if (qemu == NULL || qemu[0] == '\0') {
        qemu = DEFAULT_QEMU;
    }
    if (kernel == NULL || kernel[0] == '\0') {
        kernel = DEFAULT_KERNEL;
    }
    if (initrd == NULL || initrd[0] == '\0') {
        initrd = DEFAULT_INITRD;
    }
    if (data == NULL || data[0] == '\0') {
        data = DEFAULT_QEMU_DATA;
    }

    emit("THEKERNEL_" CATEGORY "_INTERFACE qemu=%s kernel=%s initrd=%s data=%s "
         "machine=%s append=\"%s\" timeout_ms=%d",
         qemu, kernel, initrd, data, INNER_MACHINE, INNER_APPEND, NESTED_TIMEOUT_MS);

    /* Stage 1: the inputs are what they claim to be.  All of this is checked
     * before anything is forked, because a missing or wrong file would
     * otherwise look like an inner-boot failure. */
    if (check_bzimage(kernel, &kernel_bytes) != 0) {
        fail("inspect-inner-kernel", "bzimage-hdrs-magic",
             "path=%s offset=0x%x magic=%s", kernel, BZIMAGE_MAGIC_OFFSET, BZIMAGE_MAGIC);
        return 1;
    }
    {
        struct stat info;

        if (stat(initrd, &info) != 0) {
            fail("inspect-inner-initramfs", "readable", "path=%s", initrd);
            return 1;
        }
        initrd_bytes = (int64_t)info.st_size;
    }
    if (initrd_bytes <= 0) {
        fail("inspect-inner-initramfs", "non-empty", "path=%s bytes=%lld",
             initrd, (long long)initrd_bytes);
        return 1;
    }
    emit("THEKERNEL_" CATEGORY "_INPUTS kernel_bytes=%lld initrd_bytes=%lld",
         (long long)kernel_bytes, (long long)initrd_bytes);

    /* Stage 2: run it.  This is the claim under test. */
    transcript_init(&t);
    transcript_set_sink(&t, note_line, &markers);
    outcome = run_emulator(&child, qemu, kernel, initrd, data, &t, &exit_status, &elapsed_ms);
    if (outcome < 0) {
        fail("run-emulator", "fork-and-read", "path=%s", qemu);
        return 1;
    }
    if (outcome > 0) {
        fail("run-emulator", "bounded-deadline", "elapsed_ms=%lld timeout_ms=%d",
             (long long)elapsed_ms, NESTED_TIMEOUT_MS);
        return 1;
    }

    emit("THEKERNEL_" CATEGORY "_SERIAL lines=%llu bytes=%llu truncated=%d",
         (unsigned long long)t.total, (unsigned long long)t.total, t.truncated);
    emit("THEKERNEL_" CATEGORY "_EXIT status=%lld elapsed_ms=%lld",
         (long long)exit_status, (long long)elapsed_ms);

    /* Stage 3: the four conditions, each reported separately so a failure says
     * which one broke instead of only "the emulator did not work". */
    if (!markers.release || !markers.kernel) {
        fail("inner-kernel", "alpine-kernel-identified",
             "release_marker=%d kernel_marker=%d", markers.release, markers.kernel);
        return 1;
    }
    emit("THEKERNEL_" CATEGORY "_ALPINE release=\"%s\" kernel=\"%s\"",
         markers.release_value, markers.kernel_value);
    if (!markers.init_pid) {
        fail("inner-userland", "init-is-pid-1", "marker=%s", INNER_INIT_PID_MARKER);
        return 1;
    }
    if (!markers.userland) {
        fail("inner-userland", "userland-reached", "marker=%s", INNER_USERLAND_MARKER);
        return 1;
    }
    if (!markers.shutdown) {
        fail("inner-shutdown", "shutdown-attempted", "marker=%s", INNER_SHUTDOWN_MARKER);
        return 1;
    }
    if (exit_status != 0) {
        /* A negative status is a signal.  The emulator only returns non-zero
         * here if the inner machine did not complete its poweroff. */
        fail("inner-shutdown", "emulator-exit-zero", "status=%lld elapsed_ms=%lld",
             (long long)exit_status, (long long)elapsed_ms);
        return 1;
    }

    emit("THEKERNEL_" CATEGORY "_OK elapsed_ms=%lld inner_lines=%llu release=\"%s\"",
         (long long)elapsed_ms, (unsigned long long)t.total, markers.release_value);
    return 0;
}
