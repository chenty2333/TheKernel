#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
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
    return 0;
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
    pid_t child = fork();
    if (child < 0) {
        return fail("memory-pressure-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-memory-pressure-smoke",
              "thekernel-memory-pressure-smoke", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL memory-pressure-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "memory-pressure-child") != 0) {
        return 1;
    }
    return 0;
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
    pid_t child = fork();
    if (child < 0) {
        return fail("io-uring-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-io-uring-smoke",
              "thekernel-io-uring-smoke", (char *)NULL);
        fprintf(stderr, "THEKERNEL_SYSTEM_TEST_FAIL io-uring-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "io-uring-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_io_uring_buffers(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("io-uring-buffers-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-io-uring-buffers-smoke",
              "thekernel-io-uring-buffers-smoke", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL io-uring-buffers-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "io-uring-buffers-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_signal_fp(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-signal-fp-smoke",
        NULL,
        "signal-fp-child");
}

static int test_ioprio(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("ioprio-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-ioprio-smoke",
              "/opt/thekernel-tests/bin/thekernel-ioprio-smoke",
              "--linux-host", (char *)NULL);
        fprintf(stderr, "THEKERNEL_SYSTEM_TEST_FAIL ioprio-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "ioprio-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_membarrier(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("membarrier-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-membarrier-smoke",
              "/opt/thekernel-tests/bin/thekernel-membarrier-smoke",
              "--thekernel", (char *)NULL);
        fprintf(stderr, "THEKERNEL_SYSTEM_TEST_FAIL membarrier-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "membarrier-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_userfaultfd(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("userfaultfd-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-userfaultfd-smoke",
              "thekernel-userfaultfd-smoke", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL userfaultfd-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "userfaultfd-child") != 0) {
        return 1;
    }
    return 0;
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
    static const char path[] =
        "/opt/thekernel-tests/portable/seccomp-smoke";
    pid_t child = fork();
    if (child < 0) {
        return fail("seccomp-fork");
    }
    if (child == 0) {
        execl(path, path, "--thekernel", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL seccomp-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "seccomp-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_signal_wait_boundary(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("signal-wait-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-signal-wait-boundary",
              "thekernel-signal-wait-boundary", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL signal-wait-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "signal-wait-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_pause(void) {
    return run_guest_program(
        "/opt/thekernel-tests/bin/thekernel-pause-smoke",
        NULL,
        "pause-child");
}

static int test_alarm(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("alarm-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-alarm-smoke",
              "thekernel-alarm-smoke", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL alarm-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "alarm-child") != 0) {
        return 1;
    }
    return 0;
}

static int test_wait_boundary(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail("wait-boundary-fork");
    }
    if (child == 0) {
        execl("/opt/thekernel-tests/bin/thekernel-wait-boundary",
              "thekernel-wait-boundary", (char *)NULL);
        fprintf(stderr,
                "THEKERNEL_SYSTEM_TEST_FAIL wait-boundary-exec errno=%d (%s)\n",
                errno, strerror(errno));
        _exit(127);
    }
    if (wait_for_success(child, "wait-boundary-child") != 0) {
        return 1;
    }
    return 0;
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

static int test_umask_differential(void) {
    return run_guest_program(
        "/opt/thekernel-tests/portable/umask-differential", NULL,
        "umask-differential-child");
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
        { "mounts", verify_core_filesystems },
        { "rootfs", test_rootfs },
        { "tmpfs", test_tmpfs },
        { "procfs", test_procfs },
        { "memory-pressure", test_memory_pressure_reclaim },
        { "process-exec", test_process_pipe_and_exec },
        { "vfork", test_vfork },
        { "signal-mask-alias", test_signal_mask_alias },
        { "signal-wait", test_signal_wait_boundary },
        { "pause", test_pause },
        { "alarm", test_alarm },
        { "wait-boundary", test_wait_boundary },
        { "rseq", test_rseq },
        { "futex", test_futex_differential },
        { "futex2-waitv-signal", test_futex2_waitv_signal_differential },
        { "epoll", test_epoll_differential },
        { "eventfd", test_eventfd_differential },
        { "signal-order", test_signal_order_differential },
        { "io-uring-directio", test_io_uring_directio_differential },
        { "proc-zombie", test_proc_zombie_differential },
        { "native-ni", test_native_ni_differential },
        { "creat", test_creat_differential },
        { "umask", test_umask_differential },
        { "signal-fp", test_signal_fp },
        { "io-uring", test_io_uring },
        { "io-uring-buffers", test_io_uring_buffers },
        { "ioprio", test_ioprio },
        { "membarrier", test_membarrier },
        { "userfaultfd", test_userfaultfd },
        { "packet", test_packet_socket },
        { "seccomp", test_seccomp },
    };

    puts("KTAP version 1");
    printf("1..%zu\n", sizeof(suite) / sizeof(suite[0]));
    unsigned int failures = 0;
    unsigned int skips = 0;
    for (size_t index = 0; index < sizeof(suite) / sizeof(suite[0]); ++index) {
        int result = run_suite_case(&suite[index]);
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
