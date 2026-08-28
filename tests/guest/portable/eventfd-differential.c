#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *self_path;

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_EVENTFD_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static int fail_value(const char *stage, uint64_t actual, uint64_t expected) {
    errno = EPROTO;
    fprintf(stderr,
            "THEKERNEL_EVENTFD_FAIL %s actual=%" PRIu64 " expected=%" PRIu64
            "\n",
            stage, actual, expected);
    return 1;
}

static int expect_errno(const char *stage, ssize_t result, int expected) {
    if (result != -1 || errno != expected) {
        fprintf(stderr,
                "THEKERNEL_EVENTFD_FAIL %s result=%zd errno=%d expected=%d\n",
                stage, result, errno, expected);
        return 1;
    }
    return 0;
}

static int sys_eventfd_legacy(unsigned int initval) {
    return (int)syscall(SYS_eventfd, initval);
}

static int sys_eventfd2(unsigned int initval, unsigned int flags) {
    return (int)syscall(SYS_eventfd2, initval, flags);
}

static int read_value(int fd, uint64_t *value) {
    return read(fd, value, sizeof(*value)) == (ssize_t)sizeof(*value) ? 0 : -1;
}

static int write_value(int fd, uint64_t value) {
    return write(fd, &value, sizeof(value)) == (ssize_t)sizeof(value) ? 0 : -1;
}

static int test_legacy_and_flags(void) {
    int fd = sys_eventfd_legacy(2);
    if (fd < 0)
        return fail("legacy-create");
    if (fcntl(fd, F_GETFD) != 0) {
        close(fd);
        return fail("legacy-cloexec-clear");
    }
    uint64_t value = 0;
    if (read_value(fd, &value) != 0 || value != 2) {
        close(fd);
        return fail_value("legacy-read", value, 2);
    }
    if (close(fd) != 0)
        return fail("legacy-close");

    errno = 0;
    fd = sys_eventfd2(0, EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE |
                             0x80000000U);
    if (expect_errno("eventfd2-invalid-flags", fd, EINVAL))
        return 1;
    return 0;
}

static int test_short_io_and_maximum(void) {
    int fd = sys_eventfd2(2, EFD_NONBLOCK);
    if (fd < 0)
        return fail("short-create");
    uint8_t short_buffer[sizeof(uint64_t) - 1] = {0};
    errno = 0;
    if (expect_errno("short-read", read(fd, short_buffer, sizeof(short_buffer)),
                     EINVAL)) {
        close(fd);
        return 1;
    }
    uint64_t value = 0;
    if (read_value(fd, &value) != 0 || value != 2) {
        close(fd);
        return fail_value("short-read-state", value, 2);
    }
    errno = 0;
    if (expect_errno("short-write", write(fd, short_buffer, sizeof(short_buffer)),
                     EINVAL)) {
        close(fd);
        return 1;
    }
    uint8_t long_buffer[sizeof(uint64_t) + 1] = {0};
    errno = 0;
    if (expect_errno("long-write", write(fd, long_buffer, sizeof(long_buffer)),
                     EINVAL)) {
        close(fd);
        return 1;
    }
    errno = 0;
    if (expect_errno("short-write-state", read(fd, &value, sizeof(value)),
                     EAGAIN)) {
        close(fd);
        return 1;
    }
    if (write_value(fd, 2) != 0) {
        close(fd);
        return fail("maximum-seed");
    }
    value = UINT64_MAX;
    errno = 0;
    if (expect_errno("maximum-write", write(fd, &value, sizeof(value)), EINVAL)) {
        close(fd);
        return 1;
    }
    if (read_value(fd, &value) != 0 || value != 2) {
        close(fd);
        return fail_value("maximum-state", value, 2);
    }
    if (close(fd) != 0)
        return fail("short-close");
    return 0;
}

static int test_nonblock_accumulation_and_poll(void) {
    int fd = sys_eventfd2(0, EFD_NONBLOCK);
    if (fd < 0)
        return fail("nonblock-create");
    uint64_t value = 0;
    errno = 0;
    if (expect_errno("nonblock-empty-read", read(fd, &value, sizeof(value)),
                     EAGAIN)) {
        close(fd);
        return 1;
    }
    struct pollfd poll_fd = {.fd = fd, .events = POLLIN | POLLOUT};
    int ready = poll(&poll_fd, 1, 0);
    if (ready != 1 || (poll_fd.revents & POLLIN) != 0 ||
        (poll_fd.revents & POLLOUT) == 0) {
        errno = EPROTO;
        close(fd);
        return fail("poll-empty");
    }
    if (write_value(fd, 2) != 0 || write_value(fd, 3) != 0) {
        close(fd);
        return fail("accumulate-write");
    }
    poll_fd.revents = 0;
    ready = poll(&poll_fd, 1, 0);
    if (ready != 1 || (poll_fd.revents & (POLLIN | POLLOUT)) !=
                           (POLLIN | POLLOUT)) {
        errno = EPROTO;
        close(fd);
        return fail("poll-populated");
    }
    if (read_value(fd, &value) != 0 || value != 5) {
        close(fd);
        return fail_value("accumulate-read", value, 5);
    }
    if (close(fd) != 0)
        return fail("nonblock-close");
    return 0;
}

static int test_semaphore(void) {
    int fd = sys_eventfd2(2, EFD_NONBLOCK | EFD_SEMAPHORE);
    if (fd < 0)
        return fail("semaphore-create");
    if (write_value(fd, 3) != 0) {
        close(fd);
        return fail("semaphore-write");
    }
    for (unsigned int index = 0; index < 5; ++index) {
        uint64_t value = 0;
        if (read_value(fd, &value) != 0 || value != 1) {
            close(fd);
            return fail_value("semaphore-read", value, 1);
        }
    }
    uint64_t value = 0;
    errno = 0;
    if (expect_errno("semaphore-empty", read(fd, &value, sizeof(value)), EAGAIN)) {
        close(fd);
        return 1;
    }
    if (close(fd) != 0)
        return fail("semaphore-close");
    return 0;
}

static int cloexec_probe(int fd) {
    uint64_t value = 0;
    errno = 0;
    return expect_errno("cloexec-after-exec", read(fd, &value, sizeof(value)),
                        EBADF);
}

static int test_cloexec_and_close(void) {
    int fd = sys_eventfd2(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (fd < 0)
        return fail("cloexec-create");
    int descriptor_flags = fcntl(fd, F_GETFD);
    if (descriptor_flags < 0 || (descriptor_flags & FD_CLOEXEC) == 0) {
        close(fd);
        errno = EPROTO;
        return fail("cloexec-eventfd2-flag");
    }
    int probe_fd = fcntl(fd, F_DUPFD_CLOEXEC, 100);
    if (probe_fd < 0) {
        close(fd);
        return fail("cloexec-duplicate");
    }
    if (close(fd) != 0)
        return fail("cloexec-source-close");
    descriptor_flags = fcntl(probe_fd, F_GETFD);
    if (descriptor_flags < 0 || (descriptor_flags & FD_CLOEXEC) == 0) {
        close(probe_fd);
        errno = EPROTO;
        return fail("cloexec-flag");
    }
    char fd_argument[32];
    if (snprintf(fd_argument, sizeof(fd_argument), "%d", probe_fd) <= 0) {
        close(probe_fd);
        return fail("cloexec-format");
    }
    pid_t child = fork();
    if (child < 0) {
        close(probe_fd);
        return fail("cloexec-fork");
    }
    if (child == 0) {
        execl(self_path, self_path, "--cloexec-probe", fd_argument, (char *)NULL);
        _exit(127);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        close(probe_fd);
        errno = EPROTO;
        return fail("cloexec-exec");
    }
    if (close(probe_fd) != 0)
        return fail("cloexec-close");
    uint64_t value = 1;
    errno = 0;
    if (expect_errno("close-read-ebadf", read(probe_fd, &value, sizeof(value)),
                     EBADF))
        return 1;
    errno = 0;
    if (expect_errno("close-write-ebadf", write(probe_fd, &value, sizeof(value)),
                     EBADF))
        return 1;
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    if (argc == 3 && strcmp(argv[1], "--cloexec-probe") == 0) {
        char *end = NULL;
        long fd = strtol(argv[2], &end, 10);
        if (end == argv[2] || *end != '\0' || fd < 0 || fd > INT_MAX) {
            errno = EINVAL;
            return fail("cloexec-probe-argument");
        }
        return cloexec_probe((int)fd);
    }
    if (argc != 1 || argv[0] == NULL || argv[0][0] != '/') {
        errno = EINVAL;
        return fail("absolute-self-path-required");
    }
    self_path = argv[0];

    if (test_legacy_and_flags() || test_short_io_and_maximum() ||
        test_nonblock_accumulation_and_poll() || test_semaphore() ||
        test_cloexec_and_close())
        return 1;

    puts("THEKERNEL_EVENTFD_OK");
    return 0;
}
