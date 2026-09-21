#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static int test_rlimit_nofile_exhaustion(void) {
    struct rlimit orig;
    if (getrlimit(RLIMIT_NOFILE, &orig) != 0) {
        return fail("getrlimit-orig");
    }

    struct rlimit small_limit;
    small_limit.rlim_cur = 40;
    small_limit.rlim_max = orig.rlim_max;
    if (setrlimit(RLIMIT_NOFILE, &small_limit) != 0) {
        return fail("setrlimit-small");
    }

    int fds[128];
    int count = 0;
    int hit_emfile = 0;

    for (int i = 0; i < 128; ++i) {
        fds[i] = open("/dev/null", O_RDONLY | O_CLOEXEC);
        if (fds[i] < 0) {
            if (errno == EMFILE) {
                hit_emfile = 1;
                break;
            }
            // Unexpected error
            for (int j = 0; j < count; ++j) close(fds[j]);
            setrlimit(RLIMIT_NOFILE, &orig);
            return fail("open-dev-null-unexpected-error");
        }
        count++;
    }

    if (!hit_emfile) {
        for (int j = 0; j < count; ++j) close(fds[j]);
        setrlimit(RLIMIT_NOFILE, &orig);
        fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL did not hit EMFILE with limit 40\n");
        return 1;
    }

    // Now close one fd; immediately opening another fd must succeed
    if (count > 0) {
        close(fds[count - 1]);
        int retry_fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
        if (retry_fd < 0) {
            for (int j = 0; j < count - 1; ++j) close(fds[j]);
            setrlimit(RLIMIT_NOFILE, &orig);
            return fail("reopen-after-close-slot-failed");
        }
        close(retry_fd);
        count--;
    }

    for (int j = 0; j < count; ++j) {
        close(fds[j]);
    }

    if (setrlimit(RLIMIT_NOFILE, &orig) != 0) {
        return fail("restore-rlimit");
    }

    return 0;
}

static int test_pipe_capacity_saturation(void) {
    int fds[2];
    if (pipe2(fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("pipe2-nonblock");
    }

    char chunk[256];
    memset(chunk, 'X', sizeof(chunk));
    size_t total_written = 0;

    while (1) {
        ssize_t written = write(fds[1], chunk, sizeof(chunk));
        if (written < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                break; // Pipe buffer saturated
            }
            close(fds[0]);
            close(fds[1]);
            return fail("pipe-write-saturation");
        }
        total_written += (size_t)written;
        if (total_written > 16 * 1024 * 1024) {
            close(fds[0]);
            close(fds[1]);
            fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL pipe unbounded write\n");
            return 1;
        }
    }

    if (total_written == 0) {
        close(fds[0]);
        close(fds[1]);
        fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL pipe accepted zero bytes\n");
        return 1;
    }

    // Drain the pipe completely
    char drain[256];
    size_t total_read = 0;
    while (1) {
        ssize_t n = read(fds[0], drain, sizeof(drain));
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                break; // Pipe drained
            }
            close(fds[0]);
            close(fds[1]);
            return fail("pipe-drain");
        }
        if (n == 0) break;
        total_read += (size_t)n;
    }

    close(fds[0]);
    close(fds[1]);

    if (total_read != total_written) {
        fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL pipe written %zu != read %zu\n",
                total_written, total_read);
        return 1;
    }

    return 0;
}

static int test_path_length_limits(void) {
    char long_name[NAME_MAX + 16];
    memset(long_name, 'a', NAME_MAX + 1);
    long_name[NAME_MAX + 1] = '\0';

    int fd = open(long_name, O_RDONLY);
    if (fd >= 0) {
        close(fd);
        unlink(long_name);
        fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL opened file exceeding NAME_MAX\n");
        return 1;
    }
    if (errno != ENAMETOOLONG) {
        return fail("name-max-enametoolong");
    }

    return 0;
}

static int test_mmap_boundary_faults(void) {
    // Zero length mmap must fail with EINVAL
    void *ptr = mmap(NULL, 0, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (ptr != MAP_FAILED) {
        munmap(ptr, 0);
        fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL mmap zero length succeeded\n");
        return 1;
    }
    if (errno != EINVAL) {
        return fail("mmap-zero-len-einval");
    }

    // mprotect on unaligned address must fail with EINVAL
    if (mprotect((void *)0x1001, 4096, PROT_READ) == 0) {
        fprintf(stderr, "THEKERNEL_RESOURCE_LIMITS_FAIL mprotect unaligned succeeded\n");
        return 1;
    }
    if (errno != EINVAL) {
        return fail("mprotect-unaligned-einval");
    }

    return 0;
}

int main(void) {
    if (test_rlimit_nofile_exhaustion() != 0) return 1;
    if (test_pipe_capacity_saturation() != 0) return 1;
    if (test_path_length_limits() != 0) return 1;
    if (test_mmap_boundary_faults() != 0) return 1;

    puts("THEKERNEL_RESOURCE_LIMITS_OK");
    return 0;
}
