#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <math.h>
#include <pwd.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/shm.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_SYSTEM_TEST_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static int ensure_dir(const char *path) {
    if (mkdir(path, 0755) == 0 || errno == EEXIST) {
        return 0;
    }
    return fail(path);
}

static int verify_core_filesystems(void) {
    if (ensure_dir("/dev") || ensure_dir("/proc") || ensure_dir("/sys") ||
        ensure_dir("/tmp") || ensure_dir("/var") || ensure_dir("/var/tmp") ||
        ensure_dir("/root")) {
        return 1;
    }
    if (chmod("/tmp", 01777) != 0 || chmod("/var/tmp", 01777) != 0) {
        return fail("chmod-runtime-dirs");
    }

    int fd = open("/dev/null", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        return fail("devfs-null-open");
    }
    if (close(fd) != 0) {
        return fail("devfs-null-close");
    }
    fd = open("/sys/devices/system/node/online", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return fail("sysfs-node-online-open");
    }
    if (close(fd) != 0) {
        return fail("sysfs-node-online-close");
    }
    return 0;
}

static int write_and_read_file(const char *path, const char *payload) {
    char buffer[64] = {0};
    const size_t length = strlen(payload);
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0644);
    if (fd < 0) {
        return fail("open");
    }
    if (write(fd, payload, length) != (ssize_t)length) {
        close(fd);
        return fail("write");
    }
    if (lseek(fd, 0, SEEK_SET) != 0) {
        close(fd);
        return fail("lseek");
    }
    if (read(fd, buffer, length) != (ssize_t)length) {
        close(fd);
        return fail("read");
    }
    if (close(fd) != 0 || memcmp(buffer, payload, length) != 0) {
        errno = EIO;
        return fail("file-contents");
    }
    return 0;
}

static int test_rootfs(void) {
    if (ensure_dir("/var") || ensure_dir("/var/tmp")) {
        return 1;
    }
    if (write_and_read_file("/var/tmp/thekernel-rootfs", "rootfs-ok\n")) {
        return 1;
    }
    if (rename("/var/tmp/thekernel-rootfs", "/var/tmp/thekernel-rootfs-renamed") != 0) {
        return fail("rename");
    }
    if (unlink("/var/tmp/thekernel-rootfs-renamed") != 0) {
        return fail("unlink");
    }
    return 0;
}

static int test_tmpfs(void) {
    if (ensure_dir("/tmp") || ensure_dir("/tmp/thekernel-system-test")) {
        return 1;
    }
    if (mount("tmpfs", "/tmp/thekernel-system-test", "tmpfs", 0, "size=4m") != 0) {
        return fail("tmpfs-mount");
    }
    if (write_and_read_file("/tmp/thekernel-system-test/payload", "tmpfs-ok\n")) {
        return 1;
    }
    if (unlink("/tmp/thekernel-system-test/payload") != 0) {
        return fail("tmpfs-unlink");
    }
    if (umount2("/tmp/thekernel-system-test", 0) != 0) {
        return fail("tmpfs-umount");
    }
    if (rmdir("/tmp/thekernel-system-test") != 0) {
        return fail("tmpfs-rmdir");
    }
    return 0;
}

static int test_sysv_shm(void) {
    int id = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
    if (id < 0)
        return fail("shmget");
    unsigned char *first = shmat(id, NULL, 0);
    unsigned char *second = (void *)-1;
    const char *error = "shmat-first";
    if (first == (void *)-1)
        goto out;
    first[0] = 17;
    error = "shm-rmid";
    if (shmctl(id, IPC_RMID, NULL) != 0)
        goto out;
    error = "shmat-after-rmid";
    second = shmat(id, NULL, SHM_RDONLY);
    if (second == (void *)-1 || second[0] != 17)
        goto out;
    error = "shmat-child";
    pid_t child = fork();
    if (child < 0)
        goto out;
    if (child == 0) {
        unsigned char *shared = shmat(id, NULL, 0);
        if (shared == (void *)-1 || shared[0] != 17)
            _exit(1);
        shared[0] = 23;
        _exit(shmdt(shared) != 0);
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0 || first[0] != 23 || second[0] != 23)
        goto out;
    error = "shmdt-final";
    if (shmdt(second) != 0)
        goto out;
    second = (void *)-1;
    if (shmdt(first) != 0)
        goto out;
    first = (void *)-1;
    error = "shmat-removed-id";
    second = shmat(id, NULL, 0);
    if (second != (void *)-1 || errno != EINVAL)
        goto out;
    return 0;
out:
    {
        int saved = errno;
        (void)shmctl(id, IPC_RMID, NULL);
        if (second != (void *)-1) (void)shmdt(second);
        if (first != (void *)-1) (void)shmdt(first);
        errno = saved;
        return fail(error);
    }
}

static int read_proc_cpu_times(unsigned long long ticks[2][4]) {
    FILE *file = fopen("/proc/stat", "r");
    if (file == NULL) return fail("proc-stat-open");
    char line[256];
    unsigned found = 0;
    while (fgets(line, sizeof(line), file) != NULL) {
        int row = strncmp(line, "cpu ", 4) == 0 ? 0 :
                  strncmp(line, "cpu0 ", 5) == 0 ? 1 : -1;
        if (row < 0) continue;
        if (sscanf(line + (row == 0 ? 4 : 5), "%llu %llu %llu %llu",
                   &ticks[row][0], &ticks[row][1], &ticks[row][2], &ticks[row][3]) != 4)
            break;
        found |= 1u << row;
    }
    int error = ferror(file);
    if (fclose(file) != 0 || error || found != 3) {
        errno = EPROTO;
        return fail("proc-stat-cpu-fields");
    }
    return 0;
}

static int check_busybox_identity_and_top(int top) {
    FILE *output = popen(top ? "/bin/busybox top -b -n 1" : "/bin/busybox whoami", "r");
    if (output == NULL) return fail("proc-busybox-popen");
    char line[512];
    unsigned seen = 0;
    while (fgets(line, sizeof(line), output) != NULL) {
        if (top) {
            if (strstr(line, "CPU:") != NULL) seen |= 1;
            if (strstr(line, "Load average:") != NULL) seen |= 2;
            if (strstr(line, "PID") != NULL && strstr(line, "COMMAND") != NULL) seen |= 4;
        } else if (strcmp(line, "root\n") == 0) seen = 7;
    }
    int error = ferror(output);
    int status = pclose(output);
    if (error || status == -1 || !WIFEXITED(status) || WEXITSTATUS(status) != 0 || seen != 7) {
        fprintf(stderr, "THEKERNEL_SYSTEM_TEST_FAIL busybox-%s status=%d output_fields=%u\n",
                top ? "top" : "whoami", status, seen);
        errno = EPROTO;
        return fail("proc-busybox-output");
    }
    return 0;
}

static int check_proc_cpu_and_identity(void) {
    unsigned long long before[2][4], after[2][4];
    if (read_proc_cpu_times(before)) return 1;
    struct timespec pause = { .tv_sec = 0, .tv_nsec = 250000000 };
    while (nanosleep(&pause, &pause) != 0)
        if (errno != EINTR) return fail("proc-stat-sample-delay");
    if (read_proc_cpu_times(after)) return 1;
    for (int row = 0; row < 2; ++row) {
        unsigned long long total_before = 0, total_after = 0;
        for (int field = 0; field < 4; ++field) {
            if (after[row][field] < before[row][field]) {
                errno = EPROTO;
                return fail("proc-stat-counter-regressed");
            }
            total_before += before[row][field];
            total_after += after[row][field];
        }
        if (total_after <= total_before) {
            errno = EPROTO;
            return fail("proc-stat-counter-stalled");
        }
    }
    FILE *load = fopen("/proc/loadavg", "r");
    if (load == NULL) return fail("proc-loadavg-open");
    double averages[3];
    unsigned running, total, last_pid;
    char extra;
    int fields = fscanf(load, "%lf %lf %lf %u/%u %u %c", &averages[0], &averages[1],
                        &averages[2], &running, &total, &last_pid, &extra);
    if (fclose(load) != 0 || fields != 6 || total == 0 || running > total ||
        !isfinite(averages[0]) || !isfinite(averages[1]) || !isfinite(averages[2]) ||
        averages[0] < 0 || averages[1] < 0 || averages[2] < 0) {
        errno = EPROTO;
        return fail("proc-loadavg-fields");
    }
    struct passwd *root = getpwuid(0);
    struct group *group = getgrgid(0);
    if (root == NULL || group == NULL || strcmp(root->pw_name, "root") != 0 ||
        root->pw_gid != 0 || strcmp(root->pw_dir, "/root") != 0 ||
        strcmp(group->gr_name, "root") != 0) {
        errno = EPROTO;
        return fail("proc-root-account-lookup");
    }
    return check_busybox_identity_and_top(0) || check_busybox_identity_and_top(1);
}

static int test_procfs(void) {
    char buffer[1024] = {0};
    int fd = open("/proc/meminfo", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return fail("proc-meminfo-open");
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    if (close(fd) != 0 || count <= 0) {
        return fail("proc-meminfo-read");
    }
    memset(buffer, 0, sizeof(buffer));
    fd = open("/proc/memory_pressure", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return fail("proc-memory-pressure-open");
    }
    count = read(fd, buffer, sizeof(buffer) - 1);
    if (close(fd) != 0 || count <= 0) {
        return fail("proc-memory-pressure-read");
    }
    if (strstr(buffer, "schema=thekernel-mm-pressure-v1\n") == NULL ||
        strstr(buffer, "low_watermark_pages=") == NULL ||
        strstr(buffer, "reclaimable_clean_file_pages=") == NULL ||
        strstr(buffer, "scan_budget_exhausted_files=") == NULL ||
        strstr(buffer, "snapshot_truncations=") == NULL) {
        errno = EPROTO;
        return fail("proc-memory-pressure-schema");
    }
    return check_proc_cpu_and_identity();
}

static int wait_for_success(pid_t child, const char *stage) {
    int status = 0;
    pid_t waited = waitpid(child, &status, 0);
    if (waited != child) {
        return fail(stage);
    }
    if (WIFSIGNALED(status)) {
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL %s signal=%d\n",
                stage, WTERMSIG(status));
        return 1;
    }
    if (!WIFEXITED(status)) {
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL %s wait-status=0x%x\n",
                stage, status);
        return 1;
    }
    if (WEXITSTATUS(status) == 4) {
        return 4;
    }
    if (WEXITSTATUS(status) != 0) {
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL %s exit-status=%d\n",
                stage, WEXITSTATUS(status));
        return 1;
    }
    return 0;
}

static int run_guest_program(const char *path, const char *argument,
                             const char *stage) {
    pid_t child = fork();
    if (child < 0) {
        return fail(stage);
    }
    if (child == 0) {
        if (argument == NULL) {
            execl(path, path, (char *)NULL);
        } else {
            execl(path, path, argument, (char *)NULL);
        }
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL %s-exec errno=%d (%s)\n",
                stage, errno, strerror(errno));
        _exit(127);
    }
    int result = wait_for_success(child, stage);
    if (result != 0) {
        return result;
    }
    return 0;
}

static int test_memory_pressure_reclaim(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-memory-pressure-smoke",
        NULL,
        "memory-pressure-child");
}

static int test_process_pipe_and_exec(void) {
    int fds[2];
    char byte = 0;
    if (pipe2(fds, O_CLOEXEC) != 0) {
        return fail("pipe2");
    }
    pid_t child = fork();
    if (child < 0) {
        return fail("fork");
    }
    if (child == 0) {
        close(fds[0]);
        if (write(fds[1], "K", 1) != 1) {
            _exit(2);
        }
        close(fds[1]);
        _exit(0);
    }
    close(fds[1]);
    if (read(fds[0], &byte, 1) != 1 || byte != 'K') {
        close(fds[0]);
        return fail("pipe-read");
    }
    close(fds[0]);
    if (wait_for_success(child, "pipe-child") != 0) {
        return 1;
    }
    child = fork();
    if (child < 0) {
        return fail("exec-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-exec-smoke",
              "thekernel-exec-smoke", (char *)NULL);
        fprintf(stderr, "THEKERNEL_SYSTEM_TEST_FAIL execve errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "exec-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_vfork(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/vfork-smoke",
        NULL,
        "vfork-child");
}

static int test_signal_mask_alias(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/signal-mask-alias",
        NULL,
        "signal-mask-alias-child");
}

static int test_io_uring(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-io-uring-smoke",
        NULL,
        "io-uring-child");
}

static int test_io_uring_trace(void) {
    /* The kernel mounts tracefs during pseudofs initialization. */
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-io-uring-smoke",
        "--trace",
        "io-uring-trace-child");
}

static int test_log_diagnostics(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-kernel-bench",
        "diagnostics",
        "log-diagnostics-child");
}

static int test_io_uring_buffers(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-io-uring-buffers-smoke",
        NULL,
        "io-uring-buffers-child");
}

static int test_signal_fp(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-signal-fp-smoke",
        NULL,
        "signal-fp-child");
}

/* Phase-0 contract probes for the guest toolchain and nested QEMU plan
 * (docs/design/guest-toolchain-and-nested-qemu.md).  Each one measures a
 * contract a compiler or an emulator depends on and classifies its own
 * findings as required or informational, so a non-zero result here means a
 * required contract failed rather than that a probe was unable to look. */
static int test_jit_mem(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-jit-mem-smoke",
        NULL,
        "jit-mem-child");
}

static int test_proc_shape(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-proc-shape-smoke",
        NULL,
        "proc-shape-child");
}

static int test_threads_futex(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-threads-futex-smoke",
        NULL,
        "threads-futex-child");
}

#if defined(THEKERNEL_TOOL_PAYLOAD_TCC)
/* The native C compilation case exists only in an image that carries the tcc
 * payload.  It is a compile-time selection, not a runtime probe: the case
 * table is the suite's plan, and a payload image must not be able to report a
 * different plan than the one it was built for.  An image built for the
 * payload that is missing the compiler therefore fails, which is what makes
 * the payload claim testable. */
static int test_compiler_smoke(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-compiler-smoke",
        NULL,
        "compiler-smoke-child");
}
#endif

#if defined(THEKERNEL_TOOL_PAYLOAD_GLIBC)
/* Phase 3, first milestone: a dynamically linked glibc program runs in the
 * guest.  This is a compile-time selection for the same reason the compiler and
 * nested cases are: an image built for this payload that cannot run a dynamic
 * program must fail, not quietly report a smaller plan.
 *
 * The helper checks two things separately, because either alone is worthless: a
 * static binary would exit 0 while proving nothing about a loader, and a loader
 * that runs while the program never starts would prove nothing about glibc. */
static int test_glibc_smoke(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-glibc-smoke",
        NULL,
        "glibc-smoke-child");
}
#endif

#if defined(THEKERNEL_TOOL_PAYLOAD_NESTED)
/* Phase 2a: a system emulator that lives in the guest boots a second kernel in
 * the guest's own userspace under TCG.  Like the compiler case, this is a
 * compile-time selection, so an image built for the nested payload that cannot
 * actually run the emulator fails instead of quietly reporting a smaller plan.
 *
 * The case's whole meaning is in the four conditions the helper checks
 * together: the emulator is static, the inner banner arrives, the inner
 * machine reached normal shutdown, and the whole thing finished inside its
 * deadline.  See tests/guest/tools/nested-tcg-hello.c. */
static int test_nested_tcg_hello(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-nested-tcg-hello",
        NULL,
        "nested-tcg-hello-child");
}

/* Phase 2b: a real Linux distribution -- Alpine, unmodified -- boots inside the
 * guest under that same emulator.  Its four conditions are the design's: the
 * inner workload reports INNER_ markers, the inner OS shuts down normally, the
 * emulator's exit status is checked, and the outer suite still completes.  The
 * first three belong to the helper; the fourth is this table. */
static int test_nested_linux_boot(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-nested-linux-boot",
        NULL,
        "nested-linux-boot-child");
}
#endif

static int test_ioprio(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-ioprio-smoke",
        "--linux-host",
        "ioprio-child");
}

static int test_membarrier(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-membarrier-smoke",
        "--thekernel",
        "membarrier-child");
}

static int test_userfaultfd(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-userfaultfd-smoke",
        NULL,
        "userfaultfd-child");
}

static int test_packet_socket(void) {
    static const char path[] =
        "/opt/thekernel-tests/portable/packet-socket-smoke";
    pid_t child = fork();
    if (child < 0) {
        return fail("packet-socket-fork");
    }
    if (child == 0) {
        /* Exercise the same strict Linux contract as the host differential;
         * no target-only option or capability skip is accepted. */
        execl(path, path, "--linux-host", "--require-options", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL packet-socket-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "packet-socket-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_seccomp(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/seccomp-smoke",
        "--thekernel",
        "seccomp-child");
}

static int test_signal_wait_boundary(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-signal-wait-boundary",
        NULL,
        "signal-wait-child");
}

static int test_pause(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-pause-smoke",
        NULL,
        "pause-child");
}

static int test_alarm(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-alarm-smoke",
        NULL,
        "alarm-child");
}

static int test_wait_boundary(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-wait-boundary",
        NULL,
        "wait-boundary-child");
}

static int test_rseq(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("rseq-fork");
    }
    if (child == 0) {
        if (setenv("GLIBC_TUNABLES", "glibc.pthread.rseq=0", 1) != 0) {
            fprintf(stderr,
                    "THEKERNEL_SYSTEM_TEST_FAIL rseq-tunable errno=%d (%s)\n",
                    errno, strerror(errno));
            _exit(127);
        }
        execl("/opt/thekernel-tests/bin/thekernel-rseq-smoke",
              "thekernel-rseq-smoke", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL rseq-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "rseq-child") != 0) {
        return 1;
    }
    return 0;
}

static int require_init_identity(const char *stage) {
    pid_t pid = getpid();
    pid_t tid = (pid_t)syscall(SYS_gettid);
    if (pid != 1 || tid != 1) {
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL %s expected-pid-tid=1 actual-pid=%ld actual-tid=%ld\n",
                stage, (long)pid, (long)tid);
        return 1;
    }
    return 0;
}

static int self_exec_init(const char *next_stage) {
    execl("/sbin/init", "init", next_stage, (char *)NULL);
    return fail(next_stage);
}

struct suite_case {
    const char *name;
    int (*run)(void);
    unsigned int timeout_seconds;
};

static int test_futex_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/futex-smoke", NULL,
        "futex-differential-child");
}

static int test_futex2_waitv_signal_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/futex2-waitv-signal-differential", NULL,
        "futex2-waitv-signal-differential-child");
}

static int test_epoll_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/epoll-smoke", "--thekernel",
        "epoll-differential-child");
}

static int test_eventfd_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/eventfd-differential", NULL,
        "eventfd-differential-child");
}

static int test_anon_fd_flags(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/anon-fd-flags", NULL,
        "anon-fd-flags-child");
}

static int test_select(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/select-smoke", NULL,
        "select-child");
}

static int test_exit_status(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/exit-status", NULL,
        "exit-status-child");
}

/* The kernel renders this ring on the stack of whichever process reads the
 * file, so the read is part of the contract: the file is world-readable, and
 * the kernel has to answer without touching memory it does not own.  The header
 * is also what makes a wrapped ring legible -- `first` is the oldest record the
 * ring still holds and `entries` is the width of the window -- so this checks
 * the header's arithmetic and then holds every record line to the window it
 * names, oldest first, with no holes. */
static int test_exit_status_trace_read(void) {
    static const char *const kinds[] = { "CLONE", "EXIT", "WAIT" };
    char line[4096];
    char kind[8];
    FILE *trace = fopen("/proc/sys/kernel/exit-status", "r");
    long sequence = -1;
    long first = -1;
    long entries = -1;
    long previous = -1;
    int records = 0;
    int terminated = 0;

    if (trace == NULL) {
        return fail("exit-status-trace-open");
    }
    if (fgets(line, sizeof(line), trace) == NULL ||
        sscanf(line, "EXITSTATUS_TRACE_BEGIN seq=%ld first=%ld entries=%ld",
               &sequence, &first, &entries) != 3) {
        (void)fclose(trace);
        errno = EPROTO;
        return fail("exit-status-trace-header");
    }
    /* 512 is `EXIT_STATUS_TRACE_LEN`, the length the ring is declared with. */
    if (first < 1 || sequence < first || entries != sequence - first + 1 ||
        entries > 512) {
        (void)fclose(trace);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL exit-status-trace-window seq=%ld "
                "first=%ld entries=%ld\n",
                sequence, first, entries);
        return 1;
    }
    while (fgets(line, sizeof(line), trace) != NULL) {
        long seq = -1;
        size_t index = 0;

        /* The dump closes with a terminator, so a reader can tell a complete
         * answer from one that stopped early. */
        if (strcmp(line, "EXITSTATUS_TRACE_END\n") == 0) {
            terminated = 1;
            break;
        }
        /* A line that does not end in a newline was longer than the buffer,
         * which would mean a rendered record had grown past every bound. */
        if (strchr(line, '\n') == NULL) {
            (void)fclose(trace);
            errno = EOVERFLOW;
            return fail("exit-status-trace-line");
        }
        if (sscanf(line, "%7s seq=%ld", kind, &seq) != 2) {
            (void)fclose(trace);
            fprintf(stderr,
                    "THEKERNEL_SYSTEM_TEST_FAIL exit-status-trace-record line=%s",
                    line);
            return 1;
        }
        while (index < sizeof(kinds) / sizeof(kinds[0]) &&
               strcmp(kind, kinds[index]) != 0) {
            index++;
        }
        if (index == sizeof(kinds) / sizeof(kinds[0]) || seq < first ||
            seq > sequence || seq <= previous) {
            (void)fclose(trace);
            fprintf(stderr,
                    "THEKERNEL_SYSTEM_TEST_FAIL exit-status-trace-record line=%s",
                    line);
            return 1;
        }
        previous = seq;
        records++;
    }
    if (!terminated || fgets(line, sizeof(line), trace) != NULL) {
        (void)fclose(trace);
        errno = EPROTO;
        return fail("exit-status-trace-terminator");
    }
    (void)fclose(trace);
    if (records != entries || (records > 0 && previous != sequence)) {
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL exit-status-trace-count records=%d "
                "entries=%ld last=%ld\n",
                records, entries, previous);
        return 1;
    }
    printf("exit-status-trace seq=%ld first=%ld entries=%ld records=%d\n",
           sequence, first, entries, records);
    return 0;
}

static int test_timer_create_validation(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/timer-create-validation", NULL,
        "timer-create-validation-child");
}

static int test_signal_order_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/signal-order-smoke", NULL,
        "signal-order-differential-child");
}

static int test_io_uring_directio_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/io-uring-directio-differential",
        NULL, "io-uring-directio-differential-child");
}

static int test_proc_zombie_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/proc-zombie-differential", NULL,
        "proc-zombie-differential-child");
}

static int test_native_ni_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/native-ni-differential", NULL,
        "native-ni-differential-child");
}

static int test_creat_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/creat-differential", NULL,
        "creat-differential-child");
}

static int test_time_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/time-differential", NULL,
        "time-differential-child");
}

static int test_umask_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/umask-differential", NULL,
        "umask-differential-child");
}

static int test_fs_boundary_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/fs-boundary-differential", NULL,
        "fs-boundary-differential-child");
}

static int test_signal_boundary_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/signal-boundary-differential", NULL,
        "signal-boundary-differential-child");
}

/* The only suite status protocol is the direct child status: 0 is pass, 1
 * is fail, and 4 is an explicit environmental skip.  Everything the child
 * writes is forwarded as KTAP diagnostics, never interpreted as a verdict. */
static int run_suite_case(const struct suite_case *test) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
        return 1;
    }
    pid_t child = fork();
    if (child < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return 1;
    }
    if (child == 0) {
        close(pipe_fds[0]);
        if (dup2(pipe_fds[1], STDOUT_FILENO) < 0 ||
            dup2(pipe_fds[1], STDERR_FILENO) < 0) {
            _exit(1);
        }
        if (pipe_fds[1] > STDERR_FILENO) {
            close(pipe_fds[1]);
        }
        int result = test->run();
        _exit(result == 0 ? 0 : result == 4 ? 4 : 1);
    }

    close(pipe_fds[1]);
    FILE *diagnostics = fdopen(pipe_fds[0], "r");
    if (diagnostics == NULL) {
        close(pipe_fds[0]);
        (void)waitpid(child, NULL, 0);
        return 1;
    }
    char line[512];
    while (fgets(line, sizeof(line), diagnostics) != NULL) {
        printf("# %s: %s", test->name, line);
        size_t length = strlen(line);
        if (length == 0 || line[length - 1] != '\n') {
            putchar('\n');
        }
    }
    int read_failed = ferror(diagnostics);
    fclose(diagnostics);

    int status = 0;
    if (waitpid(child, &status, 0) != child || read_failed || !WIFEXITED(status)) {
        return 1;
    }
    int exit_status = WEXITSTATUS(status);
    return exit_status == 0 || exit_status == 4 ? exit_status : 1;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (argc == 1) {
        if (require_init_identity("init-exec-stage-0") != 0) {
            return 1;
        }
        return self_exec_init("--thekernel-init-exec-stage-1");
    }
    if (argc == 2 && strcmp(argv[1], "--thekernel-init-exec-stage-1") == 0) {
        if (require_init_identity("init-exec-stage-1") != 0) {
            return 1;
        }
        return self_exec_init("--thekernel-init-exec-stage-2");
    }
    if (argc != 2 || strcmp(argv[1], "--thekernel-init-exec-stage-2") != 0) {
        errno = EINVAL;
        return fail("init-arguments");
    }
    if (require_init_identity("init-exec-stage-2") != 0) {
        return 1;
    }

    static const struct suite_case suite[] = {
        { "mounts", verify_core_filesystems, 60 },
        { "rootfs", test_rootfs, 60 },
        { "tmpfs", test_tmpfs, 60 },
        { "sysv-shm", test_sysv_shm, 60 },
        { "procfs", test_procfs, 60 },
        { "memory-pressure", test_memory_pressure_reclaim, 120 },
        { "process-exec", test_process_pipe_and_exec, 60 },
        { "vfork", test_vfork, 60 },
        { "signal-mask-alias", test_signal_mask_alias, 60 },
        { "signal-wait", test_signal_wait_boundary, 60 },
        { "pause", test_pause, 60 },
        { "alarm", test_alarm, 60 },
        { "wait-boundary", test_wait_boundary, 60 },
        { "rseq", test_rseq, 60 },
        { "futex", test_futex_differential, 60 },
        { "futex2-waitv-signal", test_futex2_waitv_signal_differential, 60 },
        { "epoll", test_epoll_differential, 60 },
        { "eventfd", test_eventfd_differential, 60 },
        { "anon-fd-flags", test_anon_fd_flags, 20 },
        { "select", test_select, 20 },
        { "exit-status", test_exit_status, 20 },
        { "exit-status-trace-read", test_exit_status_trace_read, 20 },
        { "timer-create-validation", test_timer_create_validation, 20 },
        { "signal-order", test_signal_order_differential, 60 },
        { "signal-boundary", test_signal_boundary_differential, 60 },
        { "fs-boundary", test_fs_boundary_differential, 60 },
        { "io-uring-directio", test_io_uring_directio_differential, 60 },
        { "proc-zombie", test_proc_zombie_differential, 60 },
        { "native-ni", test_native_ni_differential, 60 },
        { "creat", test_creat_differential, 60 },
        { "time", test_time_differential, 60 },
        { "umask", test_umask_differential, 60 },
        { "signal-fp", test_signal_fp, 60 },
        { "jit-mem", test_jit_mem, 30 },
        { "proc-shape", test_proc_shape, 30 },
        { "threads-futex", test_threads_futex, 60 },
#if defined(THEKERNEL_TOOL_PAYLOAD_TCC)
        { "compiler-smoke", test_compiler_smoke, 120 },
#endif
#if defined(THEKERNEL_TOOL_PAYLOAD_GLIBC)
        { "glibc-smoke", test_glibc_smoke, 60 },
#endif
#if defined(THEKERNEL_TOOL_PAYLOAD_NESTED)
        { "nested-tcg-hello", test_nested_tcg_hello, 300 },
        /* The helper's own inner deadline is 300 s plus a 5 s kill grace, so
         * this must exceed both; otherwise a slow inner boot would be reported
         * as a runner timeout rather than as the condition that broke. */
        { "nested-linux-boot", test_nested_linux_boot, 330 },
#endif
        { "io-uring", test_io_uring, 60 },
        { "io-uring-trace", test_io_uring_trace, 60 },
        { "log-diagnostics", test_log_diagnostics, 60 },
        { "io-uring-buffers", test_io_uring_buffers, 60 },
        { "ioprio", test_ioprio, 60 },
        { "membarrier", test_membarrier, 60 },
        { "userfaultfd", test_userfaultfd, 60 },
        { "packet", test_packet_socket, 60 },
        { "seccomp", test_seccomp, 60 },
    };

    puts("KTAP version 1");
    printf("1..%zu\n", sizeof(suite) / sizeof(suite[0]));
    unsigned int failures = 0;
    unsigned int skips = 0;
    for (size_t index = 0; index < sizeof(suite) / sizeof(suite[0]); ++index) {
        printf("# THEKERNEL_TEST_BEGIN %zu %s timeout_seconds=%u\n",
               index + 1, suite[index].name, suite[index].timeout_seconds);
        int result = run_suite_case(&suite[index]);
        printf("# THEKERNEL_TEST_END %zu %s result=%d\n",
               index + 1, suite[index].name, result);
        if (result == 0) {
            printf("ok %zu - %s\n", index + 1, suite[index].name);
        } else if (result == 4) {
            ++skips;
            printf("ok %zu - %s # SKIP unsupported by guest ABI\n",
                   index + 1, suite[index].name);
        } else {
            ++failures;
            printf("not ok %zu - %s\n", index + 1, suite[index].name);
        }
    }

    if (failures != 0) {
        printf("# KTAP suite failed failures=%u skips=%u\n", failures, skips);
        return 1;
    }
    sync();
    puts("# THEKERNEL_SYSTEM_TEST_COMPLETE");
    return 0;
}
