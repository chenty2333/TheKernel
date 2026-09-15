#define _GNU_SOURCE

/* TheKernel guest-toolchain: does a real distribution C compiler work in the
 * guest?
 *
 * This is Phase 3's second milestone.  Unlike the tcc case, the compiler here
 * is not a self-contained binary: `gcc` is a *driver* that locates and runs
 * cc1, as and collect2 as separate processes, reading glibc's headers, gcc's
 * private headers, glibc's startup objects and a dozen shared libraries along
 * the way.  So this case is as much a staging test as a kernel test, and the
 * two things it must not do are conflate them or pass for the wrong reason.
 *
 * What it checks, in the order the failures actually occur:
 *
 *   1. the driver runs at all -- `gcc --version` executes and prints a version.
 *      A missing cc1 or a missing shared library fails here, before any
 *      compile, and this step says which.
 *   2. a two-translation-unit program using pthreads and libm compiles and
 *      links.  This is the step that exercises the private headers (stddef.h
 *      lives in gcc's own directory, not /usr/include), the startup objects,
 *      the pthread and libm linker scripts, and the *_asneeded.so files.  All
 *      of those are link-time failures with a file tree that looks complete.
 *   3. the *product* runs, and its output is checked separately from its exit
 *      status.  A compiler that emits a binary which exits 0 without computing
 *      anything would pass an exit-status-only check.
 *
 * The compile itself runs with a deliberately minimal environment: PATH is set
 * because the driver searches it for `as` and `ld` (measured: with PATH unset
 * the compile dies with "collect2: fatal error: cannot find 'ld'"), and TMPDIR
 * points at the scratch directory so the intermediate files land somewhere the
 * guest can write.  Nothing else is inherited, so a compile that works here
 * works from any caller.
 *
 * Stable markers:
 *   THEKERNEL_GCC_SMOKE_OK
 *   THEKERNEL_GCC_SMOKE_FAIL <operation> condition=<c> k=v ... [errno=<n> (<message>)]
 *   THEKERNEL_GCC_SMOKE_CHILD: <line>          captured child output line
 *   THEKERNEL_GCC_SMOKE_<STAGE> k=v ...        greppable progress
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
#error "the gcc smoke case is x86_64-only"
#endif

#define GCC_ENV "THEKERNEL_GCC_SMOKE_GCC"
#define DEFAULT_GCC "/usr/bin/gcc"
#define WORKDIR_ENV "THEKERNEL_GCC_SMOKE_WORKDIR"

/* The compiler is a factor of a hundred slower here than on a host, so the
 * deadline is sized from what the guest measures rather than from a host
 * figure.  The suite's case timeout is set above this. */
#define COMPILE_TIMEOUT_MS 240000
#define PROGRAM_TIMEOUT_MS 30000
#define VERSION_TIMEOUT_MS 30000
#define POLL_SLICE_MS 50

#define TRANSCRIPT_BYTES 32768U

/* Room for a guest path plus the longest suffix appended to one below.  Sized
 * with the suffix in mind rather than exactly: a buffer that can hold PATH_MAX
 * but not PATH_MAX plus "/helper.c" makes the compiler warn, and silencing that
 * warning would be silencing a real truncation. */
#define PATH_BUFFER_BYTES 4096U
#define SCRIPT_BUFFER_BYTES (PATH_BUFFER_BYTES + 64U)

/* The program the guest compiles.  It is split across two translation units on
 * purpose: one unit would be compiled by a driver that never actually ran cc1
 * twice, and the link step is where the startup objects and the library linker
 * scripts are exercised.
 *
 * `folded` is computed by the helper unit through a function pointer reached
 * from the other unit, so the value proves both objects were linked and run.
 * `runtime` is computed by the main unit directly.  They must agree. */
static const char *const SOURCE_HELPER =
    "#include <math.h>\n"
    "#include <stdlib.h>\n"
    "double guest_fold(double value);\n"
    "double guest_fold(double value) {\n"
    "    double *result = malloc(sizeof *result);\n"
    "    if (result == NULL) return -1.0;\n"
    "    *result = sqrt(value) + 1.0;\n"
    "    double answer = *result;\n"
    "    free(result);\n"
    "    return answer;\n"
    "}\n";

static const char *const SOURCE_MAIN =
    "#include <math.h>\n"
    "#include <pthread.h>\n"
    "#include <stdio.h>\n"
    "#include <stdlib.h>\n"
    "double guest_fold(double value);\n"
    "static double input_value;\n"
    "static double thread_result;\n"
    "static void *worker(void *arg) {\n"
    "    (void)arg;\n"
    "    thread_result = guest_fold(input_value);\n"
    "    return NULL;\n"
    "}\n"
    "int main(void) {\n"
    "    pthread_t thread;\n"
    "    input_value = 2.0;\n"
    "    if (pthread_create(&thread, NULL, worker, NULL) != 0) return 1;\n"
    "    pthread_join(thread, NULL);\n"
    "    printf(\"THEKERNEL_GUEST_GCC_PROGRAM folded=%.6f runtime=%.6f\\n\",\n"
    "           thread_result, sqrt(input_value));\n"
    "    return (thread_result > 2.41 && thread_result < 2.42) ? 0 : 2;\n"
    "}\n";

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

    fprintf(stdout, "THEKERNEL_GCC_SMOKE_FAIL %s condition=%s ", operation, condition);
    va_start(arguments, format);
    vfprintf(stdout, format, arguments);
    va_end(arguments);
    /* The errno tail is printed only when errno is nonzero: several conditions
     * below fail on a value the case computed (an exit status, a missing
     * marker) rather than on a syscall, and there a stale errno from an
     * unrelated earlier call would read as evidence it is not. */
    if (saved != 0) {
        fprintf(stdout, " errno=%d (%s)", saved, strerror(saved));
    }
    fputc('\n', stdout);
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

/* -------------------------------------------------------------- transcript */

struct transcript {
    char buffer[TRANSCRIPT_BYTES];
    size_t length;
    size_t total;
    size_t line_start;
    int saw_version;
    int saw_program_marker;
    int saw_marker_value;
    unsigned truncated;
};

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
        if (strstr(line, "gcc (GCC)") != NULL || strstr(line, "gcc version") != NULL) {
            t->saw_version = 1;
        }
        if (strstr(line, "THEKERNEL_GUEST_GCC_PROGRAM") != NULL) {
            t->saw_program_marker = 1;
            if (strstr(line, "folded=2.414214") != NULL &&
                strstr(line, "runtime=1.414214") != NULL) {
                t->saw_marker_value = 1;
            }
        }
        printf("THEKERNEL_GCC_SMOKE_CHILD: %s\n", line);
        fflush(stdout);
        line[line_bytes] = saved;
    }
    t->line_start = t->length;
}

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

static void transcript_init(struct transcript *t)
{
    memset(t, 0, sizeof(*t));
}

/* ---------------------------------------------------------------- child run */

struct run {
    int64_t exit_status;
    int64_t elapsed_ms;
    int timed_out;
};

/* The child is killed by process group so that a compiler's own children die
 * with it.  When setpgid failed in both parent and child the group does not
 * exist and the group kill fails with ESRCH; then the child itself is killed
 * directly, because a waitpid on a live child would hang past the deadline
 * the kill was enforcing. */
static void kill_child(pid_t child)
{
    if (kill(-child, SIGKILL) != 0) {
        kill(child, SIGKILL);
    }
}

/* Run one program to completion, capturing its output.
 *
 * The child is placed in its own process group and the group is killed on the
 * deadline, because a compiler that has already started cc1 and as has children
 * of its own: killing only the driver would leave them running and corrupt
 * every later case in the suite. */
static int run_capture(struct transcript *t, int64_t timeout_ms, struct run *run,
                       const char *program, char *const argv[], char *const envp[])
{
    int pipe_fds[2];
    int64_t started, deadline;
    pid_t child;
    int drained = 0;
    int status = 0;

    memset(run, 0, sizeof(*run));
    if (pipe(pipe_fds) != 0) {
        return -1;
    }
    started = monotonic_ms();
    if (started < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return -1;
    }
    deadline = started + timeout_ms;

    child = fork();
    if (child < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return -1;
    }
    if (child == 0) {
        (void)setpgid(0, 0);
        close(pipe_fds[0]);
        if (dup2(pipe_fds[1], STDOUT_FILENO) < 0 ||
            dup2(pipe_fds[1], STDERR_FILENO) < 0) {
            _exit(126);
        }
        if (pipe_fds[1] > STDERR_FILENO) {
            close(pipe_fds[1]);
        }
        execve(program, argv, envp);
        fprintf(stderr, "THEKERNEL_GCC_SMOKE_EXEC_FAIL program=%s errno=%d (%s)\n",
                program, errno, strerror(errno));
        fflush(stderr);
        _exit(127);
    }

    close(pipe_fds[1]);
    (void)setpgid(child, child);

    for (;;) {
        struct pollfd descriptor;
        int64_t now = monotonic_ms();
        int ready;

        if (now < 0) {
            kill_child(child);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
                continue;
            }
            close(pipe_fds[0]);
            return -1;
        }
        if (now >= deadline) {
            kill_child(child);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
                continue;
            }
            run->timed_out = 1;
            run->elapsed_ms = now - started;
            close(pipe_fds[0]);
            return 0;
        }
        /* Once the pipe has been drained it sits at POLLHUP, which poll reports
         * regardless of the requested events: polling the descriptor anyway
         * would return at once and spin at 100% CPU until the child exits.
         * With nothing left to read, poll on no descriptors purely for the
         * bounded slice. */
        if (drained) {
            ready = poll(NULL, 0, POLL_SLICE_MS);
        } else {
            descriptor.fd = pipe_fds[0];
            descriptor.events = POLLIN;
            descriptor.revents = 0;
            ready = poll(&descriptor, 1, POLL_SLICE_MS);
        }
        if (ready < 0) {
            if (errno == EINTR) {
                continue;
            }
            kill_child(child);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
                continue;
            }
            close(pipe_fds[0]);
            return -1;
        }
        if (ready > 0 && (descriptor.revents & (POLLIN | POLLHUP | POLLERR)) != 0) {
            char chunk[1024];
            ssize_t bytes = read(pipe_fds[0], chunk, sizeof(chunk));

            if (bytes > 0) {
                for (ssize_t index = 0; index < bytes; ++index) {
                    transcript_push(t, chunk[index]);
                }
            } else if (bytes == 0) {
                drained = 1;
            } else if (errno != EINTR && errno != EAGAIN && errno != EWOULDBLOCK) {
                kill_child(child);
                while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
                    continue;
                }
                close(pipe_fds[0]);
                return -1;
            }
        }
        {
            pid_t done = waitpid(child, &status, WNOHANG);

            if (done == child) {
                /* Drain what is left before reporting, so the last line of a
                 * failing compile is never lost.  The drain is bounded by what
                 * remains of the deadline: a grandchild that inherited the
                 * write end would otherwise keep this read blocking after the
                 * child is gone. */
                for (;;) {
                    char chunk[1024];
                    ssize_t bytes;
                    int64_t remaining = deadline - monotonic_ms();
                    struct pollfd drain_fd;
                    int drain_ready;

                    if (remaining <= 0) {
                        break;
                    }
                    drain_fd.fd = pipe_fds[0];
                    drain_fd.events = POLLIN;
                    drain_fd.revents = 0;
                    drain_ready = poll(&drain_fd, 1, (int)remaining);
                    if (drain_ready < 0 && errno == EINTR) {
                        continue;
                    }
                    if (drain_ready <= 0) {
                        break;
                    }
                    bytes = read(pipe_fds[0], chunk, sizeof(chunk));

                    if (bytes > 0) {
                        for (ssize_t index = 0; index < bytes; ++index) {
                            transcript_push(t, chunk[index]);
                        }
                        continue;
                    }
                    if (bytes < 0 && errno == EINTR) {
                        continue;
                    }
                    break;
                }
                run->elapsed_ms = monotonic_ms() - started;
                if (WIFEXITED(status)) {
                    run->exit_status = WEXITSTATUS(status);
                } else if (WIFSIGNALED(status)) {
                    run->exit_status = -WTERMSIG(status);
                } else {
                    run->exit_status = -1;
                }
                close(pipe_fds[0]);
                return 0;
            }
            if (done < 0 && errno != EINTR && errno != ECHILD) {
                kill_child(child);
                close(pipe_fds[0]);
                return -1;
            }
        }
    }
}

/* ------------------------------------------------------------------ writing */

static int write_file(const char *path, const char *text)
{
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    size_t bytes = strlen(text);
    size_t done = 0;

    if (fd < 0) {
        return -1;
    }
    while (done < bytes) {
        ssize_t written = write(fd, text + done, bytes - done);

        if (written < 0) {
            if (errno == EINTR) {
                continue;
            }
            close(fd);
            return -1;
        }
        done += (size_t)written;
    }
    if (close(fd) != 0) {
        return -1;
    }
    return 0;
}

/* -------------------------------------------------------------------- main */

/* The environment the compiler runs with, kept as explicit strings so that the
 * case's dependencies are visible in the source rather than inherited from
 * whatever the suite happened to export. */
static char *const COMPILER_ENV[] = {
    (char *)"PATH=/usr/bin:/bin:/usr/sbin:/sbin",
    (char *)"TMPDIR=/tmp",
    (char *)"LC_ALL=C",
    NULL,
};

/* A program that only needs the loader the payload staged. */
static char *const PROGRAM_ENV[] = {
    (char *)"PATH=/usr/bin:/bin",
    NULL,
};

int main(void)
{
    const char *gcc = getenv(GCC_ENV);
    const char *workdir = getenv(WORKDIR_ENV);
    char scratch[PATH_BUFFER_BYTES];
    char helper_path[SCRIPT_BUFFER_BYTES];
    char main_path[SCRIPT_BUFFER_BYTES];
    char program_path[SCRIPT_BUFFER_BYTES];
    char tmpdir[SCRIPT_BUFFER_BYTES];
    struct transcript t;
    struct run run;
    int64_t version_ms = 0, compile_ms = 0, program_ms = 0;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (gcc == NULL || gcc[0] == '\0') {
        gcc = DEFAULT_GCC;
    }

    emit("THEKERNEL_GCC_SMOKE_INTERFACE gcc=%s compile_timeout_ms=%d", gcc,
         COMPILE_TIMEOUT_MS);

    if (access(gcc, X_OK) != 0) {
        fail("stage-driver", "present-and-executable", "path=%s", gcc);
        return 1;
    }

    /* Stage 1: a scratch directory the guest can write.  /tmp is created by the
     * init before any case runs, but the case does not assume it: a test that
     * needs a writable directory should say so when it cannot get one. */
    if (workdir != NULL && workdir[0] != '\0') {
        snprintf(scratch, sizeof(scratch), "%s", workdir);
        if (mkdir(scratch, 0700) != 0 && errno != EEXIST) {
            fail("prepare", "scratch-directory", "path=%s", scratch);
            return 1;
        }
    } else {
        snprintf(scratch, sizeof(scratch), "/tmp/thekernel-gcc-smoke.XXXXXX");
        if (mkdtemp(scratch) == NULL) {
            fail("prepare", "scratch-directory", "template=%s", scratch);
            return 1;
        }
    }
    snprintf(tmpdir, sizeof(tmpdir), "TMPDIR=%s", scratch);
    snprintf(helper_path, sizeof(helper_path), "%s/helper.c", scratch);
    snprintf(main_path, sizeof(main_path), "%s/main.c", scratch);
    snprintf(program_path, sizeof(program_path), "%s/program", scratch);

    if (write_file(helper_path, SOURCE_HELPER) != 0 ||
        write_file(main_path, SOURCE_MAIN) != 0) {
        fail("prepare", "write-source", "directory=%s", scratch);
        return 1;
    }


    /* Stage 2: the driver itself.  A missing cc1 or a missing shared library
     * fails here, and this is the step that says so rather than leaving it to
     * be inferred from a compile that produced no diagnostics. */
    {
        char *const argv[] = { (char *)gcc, (char *)"--version", NULL };

        transcript_init(&t);
        if (run_capture(&t, VERSION_TIMEOUT_MS, &run, gcc, argv, COMPILER_ENV) != 0) {
            fail("run-driver", "fork-and-read", "path=%s", gcc);
            return 1;
        }
        version_ms = run.elapsed_ms;
        if (run.timed_out) {
            fail("run-driver", "bounded-deadline", "elapsed_ms=%lld timeout_ms=%d",
                 (long long)version_ms, VERSION_TIMEOUT_MS);
            return 1;
        }
        if (run.exit_status != 0) {
            fail("run-driver", "exit-zero", "status=%lld",
                 (long long)run.exit_status);
            return 1;
        }
        if (!t.saw_version) {
            fail("run-driver", "printed-version", "path=%s", gcc);
            return 1;
        }
        emit("THEKERNEL_GCC_SMOKE_VERSION elapsed_ms=%lld", (long long)version_ms);
    }

    /* Stage 3: the compile.  This is the claim under test. */
    {
        char *const argv[] = { (char *)gcc, (char *)"-O2", (char *)"-pthread",
                               (char *)"-o", program_path, main_path, helper_path,
                               (char *)"-lm", NULL };
        /* TMPDIR is REPLACED, not appended: glibc's getenv returns the first
         * match, so a second TMPDIR entry after COMPILER_ENV[1]'s TMPDIR=/tmp
         * would be dead and the intermediates would land in /tmp instead of
         * the scratch directory. */
        char *const env[] = { COMPILER_ENV[0], tmpdir, COMPILER_ENV[2], NULL };

        transcript_init(&t);
        emit("THEKERNEL_GCC_SMOKE_COMPILE_BEGIN");
        if (run_capture(&t, COMPILE_TIMEOUT_MS, &run, gcc, argv, env) != 0) {
            fail("compile", "fork-and-read", "path=%s", gcc);
            return 1;
        }
        compile_ms = run.elapsed_ms;
        emit("THEKERNEL_GCC_SMOKE_COMPILE_EXIT status=%lld elapsed_ms=%lld lines=%llu truncated=%u",
             (long long)run.exit_status, (long long)compile_ms,
             (unsigned long long)t.total, t.truncated);
        if (run.timed_out) {
            fail("compile", "bounded-deadline", "elapsed_ms=%lld timeout_ms=%d",
                 (long long)compile_ms, COMPILE_TIMEOUT_MS);
            return 1;
        }
        if (run.exit_status != 0) {
            fail("compile", "exit-zero", "status=%lld elapsed_ms=%lld",
                 (long long)run.exit_status, (long long)compile_ms);
            return 1;
        }
    }
    if (access(program_path, X_OK) != 0) {
        fail("compile", "produced-executable", "path=%s", program_path);
        return 1;
    }

    /* Stage 4: run the product.  Output and exit status are checked separately:
     * a program that exits 0 without computing anything would pass on status
     * alone, and one that prints the right numbers while crashing would pass on
     * output alone. */
    {
        char *const argv[] = { program_path, NULL };

        transcript_init(&t);
        if (run_capture(&t, PROGRAM_TIMEOUT_MS, &run, program_path, argv,
                        PROGRAM_ENV) != 0) {
            fail("run-program", "fork-and-read", "path=%s", program_path);
            return 1;
        }
        program_ms = run.elapsed_ms;
        if (run.timed_out) {
            fail("run-program", "bounded-deadline", "elapsed_ms=%lld timeout_ms=%d",
                 (long long)program_ms, PROGRAM_TIMEOUT_MS);
            return 1;
        }
        if (!t.saw_program_marker) {
            fail("run-program", "printed-marker", "path=%s", program_path);
            return 1;
        }
        if (!t.saw_marker_value) {
            fail("run-program", "computed-expected-values", "path=%s", program_path);
            return 1;
        }
        if (run.exit_status != 0) {
            fail("run-program", "exit-zero", "status=%lld", (long long)run.exit_status);
            return 1;
        }
    }

    emit("THEKERNEL_GCC_SMOKE_OK version_ms=%lld compile_ms=%lld program_ms=%lld",
         (long long)version_ms, (long long)compile_ms, (long long)program_ms);
    return 0;
}
