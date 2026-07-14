#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYNC_FILE_RANGE_WRITE
#define SYNC_FILE_RANGE_WRITE 2
#endif

#ifndef SYNC_FILE_RANGE_WAIT_AFTER
#define SYNC_FILE_RANGE_WAIT_AFTER 4
#endif

static int write_full(int fd, const void *buf, size_t len)
{
    const uint8_t *p = (const uint8_t *)buf;
    size_t done = 0;

    while (done < len) {
        ssize_t n = write(fd, p + done, len - done);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            errno = EIO;
            return -1;
        }
        done += (size_t)n;
    }
    return 0;
}

static void fill_pattern(uint8_t *buf, size_t len, uint8_t seed)
{
    for (size_t i = 0; i < len; i++) {
        buf[i] = (uint8_t)(seed + (uint8_t)i);
    }
}

static int call_sync_file_range(int fd, off_t offset, off_t len)
{
#ifdef __NR_sync_file_range
    long rc = syscall(__NR_sync_file_range, fd, offset, len,
                      SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER);
    if (rc == 0) {
        return 0;
    }
    return -1;
#else
    errno = ENOSYS;
    (void)fd;
    (void)offset;
    (void)len;
    return -1;
#endif
}

int main(int argc, char **argv)
{
    const char *path = argc > 1 ? argv[1] : "/async_flush_fence";
    uint8_t buf[4096];
    int fd;

    unlink(path);
    fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (fd < 0) {
        perror("open");
        return 1;
    }

    fill_pattern(buf, sizeof(buf), 0x11);
    for (int i = 0; i < 32; i++) {
        if (write_full(fd, buf, sizeof(buf)) != 0) {
            perror("write initial");
            close(fd);
            return 1;
        }
    }
    if (fdatasync(fd) != 0) {
        perror("fdatasync");
        close(fd);
        return 1;
    }
    puts("SUPPORT_FDATASYNC_OK");

    if (lseek(fd, 0, SEEK_SET) < 0) {
        perror("lseek fsync");
        close(fd);
        return 1;
    }
    fill_pattern(buf, sizeof(buf), 0x55);
    for (int i = 0; i < 16; i++) {
        if (write_full(fd, buf, sizeof(buf)) != 0) {
            perror("write fsync");
            close(fd);
            return 1;
        }
    }
    if (fsync(fd) != 0) {
        perror("fsync");
        close(fd);
        return 1;
    }
    puts("SUPPORT_FSYNC_OK");

    if (lseek(fd, 64 * 1024, SEEK_SET) < 0) {
        perror("lseek sync_file_range");
        close(fd);
        return 1;
    }
    fill_pattern(buf, sizeof(buf), 0x99);
    for (int i = 0; i < 16; i++) {
        if (write_full(fd, buf, sizeof(buf)) != 0) {
            perror("write sync_file_range");
            close(fd);
            return 1;
        }
    }
    if (call_sync_file_range(fd, 64 * 1024, 64 * 1024) != 0) {
        perror("sync_file_range");
        close(fd);
        return 1;
    }
    puts("SUPPORT_SYNC_FILE_RANGE_OK");

    sync();
    puts("SUPPORT_SYNC_OK");

    close(fd);
    return 0;
}
