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
    puts("THEKERNEL_SYSTEM_TEST_MOUNTS_OK");
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
    puts("THEKERNEL_SYSTEM_TEST_ROOTFS_OK");
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
    puts("THEKERNEL_SYSTEM_TEST_TMPFS_OK");
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
    puts("THEKERNEL_SYSTEM_TEST_PROCFS_OK");

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
    puts("THEKERNEL_SYSTEM_TEST_MM_PRESSURE_OK");
    return 0;
}

static int wait_for_success(pid_t child, const char *stage) {
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = ECHILD;
        return fail(stage);
    }
    return 0;
}

static int test_portable_differential(const char *path, const char *argument,
                                      const char *stage,
                                      const char *success_marker) {
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
    if (wait_for_success(child, stage) != 0) {
        return 1;
    }
    puts(success_marker);
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
    puts("THEKERNEL_SYSTEM_TEST_MM_PRESSURE_RECLAIM_OK");
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
    puts("THEKERNEL_SYSTEM_TEST_PROCESS_OK");

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
    puts("THEKERNEL_SYSTEM_TEST_EXEC_OK");
    return 0;
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
    puts("THEKERNEL_SYSTEM_TEST_IO_URING_OK");
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
    puts("THEKERNEL_SYSTEM_TEST_USERFAULTFD_OK");
    return 0;
}

static int test_packet_socket(void) {
    static const char path[] =
        "/opt/thekernel-tests/bin/thekernel-packet-socket-smoke";
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
    puts("THEKERNEL_SYSTEM_TEST_PACKET_OK");
    return 0;
}

static int test_seccomp(void) {
    static const char path[] =
        "/opt/thekernel-tests/bin/thekernel-seccomp-smoke";
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
    puts("THEKERNEL_SYSTEM_TEST_SECCOMP_OK");
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
    puts("THEKERNEL_SYSTEM_TEST_SIGNAL_WAIT_OK");
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
    puts("THEKERNEL_SYSTEM_TEST_WAIT_BOUNDARY_OK");
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
        puts("THEKERNEL_SYSTEM_TEST_INIT_EXEC_1_OK");
        return self_exec_init("--thekernel-init-exec-stage-2");
    }
    if (argc != 2 || strcmp(argv[1], "--thekernel-init-exec-stage-2") != 0) {
        errno = EINVAL;
        return fail("init-arguments");
    }
    if (require_init_identity("init-exec-stage-2") != 0) {
        return 1;
    }
    puts("THEKERNEL_SYSTEM_TEST_INIT_EXEC_2_OK");
    puts("THEKERNEL_SYSTEM_TEST_START");

    if (verify_core_filesystems() || test_rootfs() || test_tmpfs() || test_procfs() ||
        test_memory_pressure_reclaim() || test_process_pipe_and_exec() || test_signal_wait_boundary() ||
        test_wait_boundary() ||
        test_portable_differential(
            "/opt/thekernel-tests/bin/thekernel-futex-smoke",
            NULL,
            "futex-differential-child",
            "THEKERNEL_SYSTEM_TEST_FUTEX_DIFFERENTIAL_OK") ||
        test_portable_differential(
            "/opt/thekernel-tests/bin/thekernel-epoll-smoke",
            "--thekernel",
            "epoll-differential-child",
            "THEKERNEL_SYSTEM_TEST_EPOLL_DIFFERENTIAL_OK") ||
        test_portable_differential(
            "/opt/thekernel-tests/bin/thekernel-signal-order-smoke",
            NULL,
            "signal-order-differential-child",
            "THEKERNEL_SYSTEM_TEST_SIGNAL_ORDER_DIFFERENTIAL_OK") ||
        test_io_uring() || test_userfaultfd() || test_packet_socket() ||
        test_seccomp()) {
        return 1;
    }

    sync();
    puts("THEKERNEL_SYSTEM_TEST_PASS");
    return 0;
}
