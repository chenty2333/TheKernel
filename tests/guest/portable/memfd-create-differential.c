#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_memfd_create
#define SYS_memfd_create 319
#endif
#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif
#ifndef MFD_ALLOW_SEALING
#define MFD_ALLOW_SEALING 0x0002U
#endif
#ifndef MFD_HUGETLB
#define MFD_HUGETLB 0x0004U
#endif
#ifndef MFD_NOEXEC_SEAL
#define MFD_NOEXEC_SEAL 0x0008U
#endif
#ifndef MFD_EXEC
#define MFD_EXEC 0x0010U
#endif
#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#endif
#ifndef F_GET_SEALS
#define F_GET_SEALS 1034
#endif
#ifndef F_SEAL_SEAL
#define F_SEAL_SEAL 0x0001
#endif
#ifndef F_SEAL_SHRINK
#define F_SEAL_SHRINK 0x0002
#endif
#ifndef F_SEAL_EXEC
#define F_SEAL_EXEC 0x0020
#endif

/*
 * Differential coverage for the memfd_create(2) entry point itself.
 *
 * Every assertion below is a rule Linux v7.2.3 states unconditionally, so it
 * holds on both TheKernel and the reference kernel:
 *
 *   mm/memfd.c:sanitize_flags() (:409)
 *                 - a flag bit outside MFD_ALL_FLAGS is -EINVAL, and the
 *                   MFD_HUGE_MASK << MFD_HUGE_SHIFT size field is admitted
 *                   only when MFD_HUGETLB is present (:412-419), so bit 25 is
 *                   still -EINVAL for an MFD_HUGETLB request
 *                 - MFD_EXEC together with MFD_NOEXEC_SEAL is -EINVAL (:422)
 *                 - both rules are decided before the name is read, because
 *                   SYSCALL_DEFINE2(memfd_create) sanitizes first (:513-517)
 *   mm/memfd.c:alloc_name() (:430)
 *                 - the name is read with a MFD_NAME_MAX_LEN + 1 budget
 *                   (:442), and MFD_NAME_MAX_LEN is NAME_MAX - sizeof("memfd:")
 *                   = 249 (:338-340), so 249 characters are accepted and 250
 *                   are -EINVAL
 *                 - an empty name is accepted (:430-455)
 *                 - an unreadable name is -EFAULT (:443-446)
 *   mm/memfd.c:memfd_alloc_file() (:458)
 *                 - the shmem inode keeps S_IRWXUGO because
 *                   shmem_file_setup()/inode_init_owner() apply no umask to a
 *                   directory-less inode, and MFD_NOEXEC_SEAL clears the
 *                   execute bits (:489-492), so the visible mode is 0777 or
 *                   0666
 *                 - the initial seal word is F_SEAL_SEAL (:3040 in
 *                   mm/shmem.c), cleared by MFD_ALLOW_SEALING and by
 *                   MFD_NOEXEC_SEAL, which then also sets F_SEAL_EXEC, so
 *                   F_GET_SEALS reports F_SEAL_SEAL, 0 or F_SEAL_EXEC (:489-499)
 *                 - MFD_EXEC names the implied default and leaves F_SEAL_SEAL
 *                   set, so it is not a way to make the file sealable
 *   mm/memfd.c:memfd_add_seals() (:158)
 *                 - the descriptor's write mode is judged first (-EPERM), then
 *                   bits outside F_ALL_SEALS (-EINVAL), then whether the inode
 *                   is sealable (-EINVAL), then F_SEAL_SEAL (-EPERM), so an
 *                   unknown bit is -EINVAL even on an unsealable memfd (:173-190)
 *   mm/shmem.c    a memfd is a regular, seekable, truncatable file of size 0
 *
 * MFD_HUGETLB is deliberately not asserted: the reference kernel is built
 * with CONFIG_HUGETLBFS=y and creates a hugetlbfs file, while this kernel has
 * no anonymous hugetlbfs inode and answers -EOPNOTSUPP.  Only the flag-mask
 * rule that rejects a reserved size bit is shared, and it belongs to
 * MFD_FLAG_VALIDATION above.
 */

/* A 250-character name plus its terminator: the first length the scan
 * budget of `strncpy_from_user(..., MFD_NAME_MAX_LEN + 1)` cannot accept. */
#define NAME_STORAGE 251

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_MEMFD_CREATE_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr, "THEKERNEL_MEMFD_CREATE_FAIL %s actual=%ld expected=%ld\n",
            stage, actual, expected);
    return 1;
}

static int expect_errno(const char *stage, long result, int expected) {
    if (result != -1 || errno != expected) {
        fprintf(stderr,
                "THEKERNEL_MEMFD_CREATE_FAIL %s result=%ld errno=%d "
                "expected=%d\n",
                stage, result, errno, expected);
        return 1;
    }
    return 0;
}

static long do_memfd_create(const char *name, unsigned int flags) {
    return syscall(SYS_memfd_create, name, (long)flags);
}

static int test_flag_validation(void) {
    /* sanitize_flags() runs before alloc_name(), so each of these is decided
     * without ever reading the NULL name pointer. */
    errno = 0;
    if (expect_errno("memfd-unknown-flag", do_memfd_create(NULL, 0x100U),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("memfd-both-exec-bits",
                     do_memfd_create(NULL, MFD_EXEC | MFD_NOEXEC_SEAL), EINVAL))
        return 1;
    /* The huge-page size field is only part of the admitted mask when
     * MFD_HUGETLB is present, so bit 25 is a reserved bit either way. */
    errno = 0;
    if (expect_errno("memfd-huge-reserved-bit",
                     do_memfd_create(NULL, MFD_HUGETLB | (1U << 25)), EINVAL))
        return 1;
    /* With the flags admitted, the name read is the next rule, and a NULL
     * name is the only -EFAULT in this family. */
    errno = 0;
    if (expect_errno("memfd-null-name", do_memfd_create(NULL, 0), EFAULT))
        return 1;
    return 0;
}

static int test_name_rules(void) {
    static char name[NAME_STORAGE];
    int fd;

    fd = (int)do_memfd_create("", 0);
    if (fd < 0)
        return fail("memfd-empty-name");
    if (close(fd) != 0)
        return fail("memfd-empty-name-close");

    memset(name, 'n', sizeof(name));
    name[249] = '\0';
    fd = (int)do_memfd_create(name, 0);
    if (fd < 0)
        return fail("memfd-max-name");
    if (close(fd) != 0)
        return fail("memfd-max-name-close");

    /* One more character moves the terminator outside the scan budget. */
    name[249] = 'n';
    name[250] = '\0';
    errno = 0;
    if (expect_errno("memfd-over-long-name", do_memfd_create(name, 0), EINVAL))
        return 1;

    /* The name is a label, not an identifier: two memfds may share one. */
    fd = (int)do_memfd_create("memfd-shared-name", 0);
    int second = (int)do_memfd_create("memfd-shared-name", 0);
    if (fd < 0 || second < 0)
        return fail("memfd-shared-name");
    if (fd == second)
        return fail_value("memfd-shared-name-distinct", second, fd);
    if (close(second) != 0 || close(fd) != 0)
        return fail("memfd-shared-name-close");
    return 0;
}

static int test_descriptor_state(void) {
    int fd = (int)do_memfd_create("memfd-cloexec", MFD_CLOEXEC);
    if (fd < 0)
        return fail("memfd-cloexec-create");
    int flags = fcntl(fd, F_GETFD);
    if (flags < 0 || (flags & FD_CLOEXEC) == 0)
        return fail_value("memfd-cloexec-set", flags, FD_CLOEXEC);
    if (close(fd) != 0)
        return fail("memfd-cloexec-close");

    fd = (int)do_memfd_create("memfd-no-cloexec", 0);
    if (fd < 0)
        return fail("memfd-no-cloexec-create");
    flags = fcntl(fd, F_GETFD);
    if (flags < 0 || (flags & FD_CLOEXEC) != 0)
        return fail_value("memfd-cloexec-absent", flags, 0);
    if (close(fd) != 0)
        return fail("memfd-no-cloexec-close");
    return 0;
}

static int test_inode_shape(void) {
    struct stat status;
    int fd = (int)do_memfd_create("memfd-mode-default", 0);
    if (fd < 0)
        return fail("memfd-default-create");
    if (fstat(fd, &status) != 0)
        return fail("memfd-default-stat");
    if (!S_ISREG(status.st_mode))
        return fail_value("memfd-default-regular", status.st_mode, S_IFREG);
    if ((status.st_mode & 07777) != 0777)
        return fail_value("memfd-default-mode", status.st_mode & 07777, 0777);
    if (status.st_size != 0)
        return fail_value("memfd-default-size", status.st_size, 0);
    if (close(fd) != 0)
        return fail("memfd-default-close");

    /* MFD_EXEC names the same executable mode explicitly and is not a
     * separate inode mode. */
    fd = (int)do_memfd_create("memfd-mode-exec", MFD_EXEC);
    if (fd < 0)
        return fail("memfd-exec-create");
    if (fstat(fd, &status) != 0)
        return fail("memfd-exec-stat");
    if ((status.st_mode & 07777) != 0777)
        return fail_value("memfd-exec-mode", status.st_mode & 07777, 0777);
    if (close(fd) != 0)
        return fail("memfd-exec-close");

    fd = (int)do_memfd_create("memfd-mode-noexec", MFD_NOEXEC_SEAL);
    if (fd < 0)
        return fail("memfd-noexec-create");
    if (fstat(fd, &status) != 0)
        return fail("memfd-noexec-stat");
    if (!S_ISREG(status.st_mode))
        return fail_value("memfd-noexec-regular", status.st_mode, S_IFREG);
    if ((status.st_mode & 07777) != 0666)
        return fail_value("memfd-noexec-mode", status.st_mode & 07777, 0666);
    if (close(fd) != 0)
        return fail("memfd-noexec-close");
    return 0;
}

static int test_seal_admission(void) {
    /* No MFD_ALLOW_SEALING: the inode keeps the F_SEAL_SEAL that
     * shmem_get_inode() installs, so the seal word reports it and no seal can
     * be added. */
    int fixed = (int)do_memfd_create("memfd-unsealable", 0);
    if (fixed < 0)
        return fail("memfd-unsealable-create");
    if (fcntl(fixed, F_GET_SEALS) != F_SEAL_SEAL)
        return fail_value("memfd-unsealable-seals",
                          fcntl(fixed, F_GET_SEALS), F_SEAL_SEAL);
    errno = 0;
    if (expect_errno("memfd-unsealable-add",
                     fcntl(fixed, F_ADD_SEALS, F_SEAL_SEAL), EPERM))
        return 1;
    /* The seal bits are validated before the F_SEAL_SEAL test, so an unknown
     * bit is -EINVAL even here. */
    errno = 0;
    if (expect_errno("memfd-unknown-seal-bit",
                     fcntl(fixed, F_ADD_SEALS, 0x40), EINVAL))
        return 1;
    if (close(fixed) != 0)
        return fail("memfd-unsealable-close");

    /* MFD_ALLOW_SEALING clears F_SEAL_SEAL and adds nothing else. */
    int sealable = (int)do_memfd_create("memfd-sealable", MFD_ALLOW_SEALING);
    if (sealable < 0)
        return fail("memfd-sealable-create");
    if (fcntl(sealable, F_GET_SEALS) != 0)
        return fail_value("memfd-sealable-seals", fcntl(sealable, F_GET_SEALS),
                          0);
    if (fcntl(sealable, F_ADD_SEALS, F_SEAL_SHRINK) != 0)
        return fail("memfd-sealable-add");
    if (fcntl(sealable, F_GET_SEALS) != F_SEAL_SHRINK)
        return fail_value("memfd-sealable-seals-after",
                          fcntl(sealable, F_GET_SEALS), F_SEAL_SHRINK);
    if (close(sealable) != 0)
        return fail("memfd-sealable-close");

    /* MFD_NOEXEC_SEAL is the other way to a sealable file: it clears
     * F_SEAL_SEAL and installs F_SEAL_EXEC in the same step. */
    int noexec = (int)do_memfd_create("memfd-noexec-seal", MFD_NOEXEC_SEAL);
    if (noexec < 0)
        return fail("memfd-noexec-seal-create");
    if (fcntl(noexec, F_GET_SEALS) != F_SEAL_EXEC)
        return fail_value("memfd-noexec-seals", fcntl(noexec, F_GET_SEALS),
                          F_SEAL_EXEC);
    if (fcntl(noexec, F_ADD_SEALS, 0) != 0)
        return fail("memfd-noexec-add-empty");
    if (close(noexec) != 0)
        return fail("memfd-noexec-seal-close");

    /* MFD_EXEC is the explicit form of the default, so the file stays
     * unsealable. */
    int exec = (int)do_memfd_create("memfd-exec-unsealable", MFD_EXEC);
    if (exec < 0)
        return fail("memfd-exec-unsealable-create");
    if (fcntl(exec, F_GET_SEALS) != F_SEAL_SEAL)
        return fail_value("memfd-exec-unsealable-seals",
                          fcntl(exec, F_GET_SEALS), F_SEAL_SEAL);
    errno = 0;
    if (expect_errno("memfd-exec-unsealable-add",
                     fcntl(exec, F_ADD_SEALS, F_SEAL_SEAL), EPERM))
        return 1;
    if (close(exec) != 0)
        return fail("memfd-exec-unsealable-close");
    return 0;
}

static int test_file_shape(void) {
    struct stat status;
    char buffer[4] = {0};
    int fd = (int)do_memfd_create("memfd-io", MFD_ALLOW_SEALING);
    if (fd < 0)
        return fail("memfd-io-create");
    if (write(fd, "mem", 3) != 3)
        return fail("memfd-io-write");
    if (fstat(fd, &status) != 0 || status.st_size != 3)
        return fail_value("memfd-io-size", status.st_size, 3);
    if (lseek(fd, 0, SEEK_END) != 3)
        return fail_value("memfd-io-seek-end", lseek(fd, 0, SEEK_END), 3);
    if (pread(fd, buffer, 3, 0) != 3 || memcmp(buffer, "mem", 3) != 0)
        return fail("memfd-io-pread");

    /* A memfd is an ordinary truncatable file until F_SEAL_GROW or
     * F_SEAL_SHRINK is added. */
    if (ftruncate(fd, 1) != 0)
        return fail("memfd-io-truncate-shrink");
    if (pread(fd, buffer, sizeof(buffer), 0) != 1 || buffer[0] != 'm')
        return fail("memfd-io-after-shrink");
    if (ftruncate(fd, 8) != 0)
        return fail("memfd-io-truncate-grow");
    if (fstat(fd, &status) != 0 || status.st_size != 8)
        return fail_value("memfd-io-grown-size", status.st_size, 8);
    if (close(fd) != 0)
        return fail("memfd-io-close");
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    puts("THEKERNEL_ABI_CASE memfd-create.portable-differential");
    if (test_flag_validation())
        return 1;
    puts("THEKERNEL_ABI_ASSERT memfd-create.portable-differential "
         "MFD_FLAG_VALIDATION pass");
    if (test_name_rules())
        return 1;
    puts("THEKERNEL_ABI_ASSERT memfd-create.portable-differential "
         "MFD_NAME_RULES pass");
    if (test_descriptor_state())
        return 1;
    puts("THEKERNEL_ABI_ASSERT memfd-create.portable-differential "
         "MFD_CLOEXEC_STATE pass");
    if (test_inode_shape())
        return 1;
    puts("THEKERNEL_ABI_ASSERT memfd-create.portable-differential "
         "MFD_INODE_MODE pass");
    if (test_seal_admission())
        return 1;
    puts("THEKERNEL_ABI_ASSERT memfd-create.portable-differential "
         "MFD_SEAL_ADMISSION pass");
    if (test_file_shape())
        return 1;
    puts("THEKERNEL_ABI_ASSERT memfd-create.portable-differential "
         "MFD_FILE_SHAPE pass");

    puts("THEKERNEL_MEMFD_CREATE_OK");
    puts("THEKERNEL_ABI_RESULT memfd-create.portable-differential pass");
    return 0;
}
