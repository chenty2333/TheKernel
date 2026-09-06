#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Linux's kernel-visible bit; glibc defines O_LARGEFILE as zero on x86_64. */
#define LINUX_O_LARGEFILE 00100000

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_CREAT_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static int raw_creat(const char *path, mode_t mode) {
    return (int)syscall(SYS_creat, path, mode);
}

int main(void) {
    puts("THEKERNEL_ABI_CASE creat.raw-differential");
    char path[128];
    if (snprintf(path, sizeof(path), "/root/thekernel-creat-%ld", (long)getpid())
        >= (int)sizeof(path)) {
        errno = ENAMETOOLONG;
        return fail("path");
    }
    (void)unlink(path);

    mode_t old_umask = umask(0027);
    int fd = raw_creat(path, 0667);
    umask(old_umask);
    if (fd < 0) {
        return fail("create");
    }
    struct statfs filesystem;
    if (fstatfs(fd, &filesystem) != 0 || filesystem.f_type != 0xef53) {
        (void)close(fd);
        (void)unlink(path);
        errno = EPROTO;
        return fail("ext4-fixture-required");
    }
    puts("THEKERNEL_ABI_ASSERT creat.raw-differential PROVIDER_EXT4 pass");

    struct stat statbuf;
    if (fstat(fd, &statbuf) != 0 || (statbuf.st_mode & 0777) != 0640) {
        (void)close(fd);
        errno = EPROTO;
        return fail("umask-mode");
    }
    int status = fcntl(fd, F_GETFL);
    int descriptor = fcntl(fd, F_GETFD);
    if ((status & O_ACCMODE) != O_WRONLY ||
        (status & LINUX_O_LARGEFILE) == 0 || descriptor != 0) {
        (void)close(fd);
        errno = EPROTO;
        return fail("write-only-no-cloexec");
    }
    if (close(fd) != 0) {
        return fail("create-close");
    }
    puts("THEKERNEL_ABI_ASSERT creat.raw-differential CREATE_UMASK_STATUS pass");

    fd = open(path, O_WRONLY);
    if (fd < 0) {
        return fail("prepare-open");
    }
    if (write(fd, "x", 1) != 1) {
        (void)close(fd);
        return fail("prepare-write");
    }
    if (close(fd) != 0) {
        return fail("prepare-close");
    }
    if (chmod(path, 0601) != 0) {
        return fail("prepare-existing");
    }
    fd = raw_creat(path, 0777);
    if (fd < 0) {
        return fail("truncate-existing");
    }
    if (fstat(fd, &statbuf) != 0 || statbuf.st_size != 0 ||
        (statbuf.st_mode & 0777) != 0601) {
        (void)close(fd);
        errno = EPROTO;
        return fail("existing-truncate-mode");
    }
    if (close(fd) != 0) {
        return fail("truncate-close");
    }
    puts("THEKERNEL_ABI_ASSERT creat.raw-differential TRUNCATE_EXISTING pass");

    errno = 0;
    if (raw_creat((const char *)1, 0644) != -1 || errno != EFAULT) {
        errno = EPROTO;
        return fail("bad-path-efault");
    }
    puts("THEKERNEL_ABI_ASSERT creat.raw-differential BAD_PATH_EFAULT pass");
    if (unlink(path) != 0) {
        return fail("unlink");
    }
    puts("THEKERNEL_ABI_ASSERT creat.raw-differential TEARDOWN pass");

    puts("THEKERNEL_CREAT_OK");
    puts("THEKERNEL_ABI_RESULT creat.raw-differential pass");
    return 0;
}
