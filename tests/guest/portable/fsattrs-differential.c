#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/vfs.h>
#include <unistd.h>

/* Native x86_64 UAPI, independent of the build host's libc headers. */
enum { NR_SET = 463, NR_GET, NR_LIST, NR_REMOVE, NR_TREE, NR_FGET, NR_FSET };
struct xargs { uint64_t value; uint32_t size, flags; };
struct fattr { uint64_t xflags; uint32_t extsize, nextents, projid, cowextsize; };
struct mattr { uint64_t set, clr, propagation, userns; };
_Static_assert(sizeof(struct xargs) == 16, "xattr_args");
_Static_assert(sizeof(struct fattr) == 24, "file_attr");
_Static_assert(sizeof(struct mattr) == 32, "mount_attr");
#define BAD ((void *)(uintptr_t)1)
#define BAD_FLAGS 0x80000000U
#define TREE_FLAGS (1U | O_CLOEXEC)
static const char *active;
static char dir[] = "/root/thekernel-fsattrs-XXXXXX";
static int dfd = -1, fd = -1;
static void cleanup(void) {
    if (fd >= 0) (void)close(fd);
    if (dfd >= 0) {
        (void)unlinkat(dfd, "file", 0);
        (void)unlinkat(dfd, "fifo", 0);
        (void)close(dfd);
    }
    (void)rmdir(dir);
}
static const char name[] = "user.thekernel.fsattrs";
static void begin(const char *id) {
    active = id;
    printf("THEKERNEL_ABI_CASE %s\n", active);
}
static void mark(const char *id) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, id);
}
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
static void check(int ok, const char *stage) {
    if (!ok) {
        fprintf(stderr, "THEKERNEL_FSATTRS_FAIL %s %s errno=%d (%s)\n",
                active, stage, errno, strerror(errno));
        exit(1);
    }
}
#define ERROR(call, expected, stage) do { errno = 0; long r_ = (call); \
    check(r_ == -1 && errno == (expected), (stage)); } while (0)
static long xa(int nr, int fd, const char *path, unsigned flags,
               const char *key, struct xargs *args, size_t size) {
    return syscall(nr, fd, path, flags, key, args, size);
}
static long fa(int nr, int fd, const char *path, void *attr, size_t size, unsigned flags) {
    return syscall(nr, fd, path, attr, size, flags);
}
static void same_attr(int fd, const struct fattr *expected) {
    struct fattr got;
    check(fa(NR_FGET, fd, "", &got, sizeof(got), AT_EMPTY_PATH) == 0,
          "state-get");
    if (memcmp(&got, expected, sizeof(got)) != 0) {
        fprintf(stderr, "THEKERNEL_FSATTRS_STATE got=%llx,%u,%u,%u,%u expected=%llx,%u,%u,%u,%u\n",
                (unsigned long long)got.xflags, got.extsize, got.nextents, got.projid, got.cowextsize,
                (unsigned long long)expected->xflags, expected->extsize, expected->nextents,
                expected->projid, expected->cowextsize);
        check(0, "state-preserved");
    }
}
int main(void) {
    active = "fsattrs.setup";
    check(mkdtemp(dir) != NULL, "mkdir");
    check(atexit(cleanup) == 0, "cleanup-register");
    dfd = open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check(dfd >= 0, "directory-open");
    struct statfs fixture_fs;
    check(fstatfs(dfd, &fixture_fs) == 0 && fixture_fs.f_type == 0xef53,
          "ext4-fixture-required");
    fd = openat(dfd, "file", O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
    check(fd >= 0, "file-open");
    check(mkfifoat(dfd, "fifo", 0600) == 0, "fifo-create");
    char value[] = "first", replacement[] = "second", output[64];
    struct xargs args = { (uintptr_t)value, sizeof(value), 1 };

    begin("setxattrat.raw-differential");
    check(xa(NR_SET, dfd, "file", 0, name, &args, sizeof(args)) == 0, "create");
    ERROR(xa(NR_SET, dfd, "file", 0, name, &args, sizeof(args)), EEXIST, "duplicate");
    args.value = (uintptr_t)replacement; args.size = sizeof(replacement); args.flags = 2;
    check(xa(NR_SET, fd, "", AT_EMPTY_PATH, name, &args, sizeof(args)) == 0, "replace-fd");
    mark("CREATE_REPLACE_EMPTY_PATH");
    ERROR(xa(NR_SET, -1, BAD, BAD_FLAGS, BAD, &args, sizeof(args)), EINVAL, "at-before-name");
    args.flags = 4;
    ERROR(xa(NR_SET, -1, BAD, 0, BAD, &args, sizeof(args)), EINVAL, "flags-before-name");
    ERROR(xa(NR_SET, -1, BAD, BAD_FLAGS, BAD, BAD, sizeof(args)), EFAULT, "copy-before-flags");
    ERROR(xa(NR_SET, -1, BAD, BAD_FLAGS, BAD, BAD, 15), EINVAL, "short-before-copy");
    args.flags = 0;
    ERROR(xa(NR_SET, dfd, "fifo", 0, name, &args, 16), EPERM, "fifo-user-set");
    mark("FIFO_USER_EPERM");
    mark("VALIDATION_ORDER"); done();

    begin("getxattrat.raw-differential");
    args = (struct xargs){ 0, 0, 0 };
    check(xa(NR_GET, fd, "", AT_EMPTY_PATH, name, &args, 16) == sizeof(replacement), "probe");
    args.value = (uintptr_t)output; args.size = sizeof(replacement) - 1;
    ERROR(xa(NR_GET, dfd, "file", 0, name, &args, 16), ERANGE, "small");
    args.size = sizeof(replacement);
    check(xa(NR_GET, dfd, "file", 0, name, &args, 16) == sizeof(replacement), "get");
    check(memcmp(output, replacement, sizeof(replacement)) == 0, "value");
    mark("PROBE_RANGE_VALUE");
    args.value = 1;
    ERROR(xa(NR_GET, fd, "", AT_EMPTY_PATH, name, &args, 16), EFAULT, "bad-output");
    args.flags = 1;
    ERROR(xa(NR_GET, -1, BAD, 0, BAD, &args, 16), EINVAL, "flags-before-name");
    args.flags = 0;
    ERROR(xa(NR_GET, -1, BAD, BAD_FLAGS, BAD, &args, 16), EINVAL, "at-before-name");
    ERROR(xa(NR_GET, dfd, "fifo", 0, name, &args, 16), ENODATA, "fifo-user-get");
    mark("FIFO_USER_ENODATA");
    mark("VALIDATION_ORDER"); done();

    begin("listxattrat.raw-differential");
    long length = syscall(NR_LIST, fd, "", AT_EMPTY_PATH, NULL, 0);
    check(length > 0 && length < 65536, "probe");
    char *list = malloc((size_t)length);
    check(list != NULL, "allocate");
    ERROR(syscall(NR_LIST, fd, "", AT_EMPTY_PATH, list, length - 1), ERANGE, "small");
    check(syscall(NR_LIST, dfd, "file", 0, list, length) == length, "list");
    int found = 0;
    for (size_t pos = 0; pos < (size_t)length;) {
        size_t n = strnlen(list + pos, (size_t)length - pos);
        check(n < (size_t)length - pos, "nul-terminated");
        if (strcmp(list + pos, name) == 0) found = 1;
        pos += n + 1;
    }
    check(found, "name-present"); free(list);
    mark("PROBE_RANGE_NAMES");
    ERROR(syscall(NR_LIST, fd, "", AT_EMPTY_PATH, BAD, length), EFAULT, "bad-output");
    ERROR(syscall(NR_LIST, -1, BAD, BAD_FLAGS, BAD, length), EINVAL, "flags-before-path");
    mark("VALIDATION_ORDER"); done();

    begin("removexattrat.raw-differential");
    check(syscall(NR_REMOVE, fd, "", AT_EMPTY_PATH, name) == 0, "remove");
    ERROR(syscall(NR_REMOVE, dfd, "file", 0, name), ENODATA, "absent");
    args = (struct xargs){0};
    ERROR(xa(NR_GET, fd, "", AT_EMPTY_PATH, name, &args, 16), ENODATA, "removed-state");
    mark("REMOVE_ABSENT_STATE");
    ERROR(syscall(NR_REMOVE, -1, BAD, BAD_FLAGS, BAD), EINVAL, "flags-before-name");
    ERROR(syscall(NR_REMOVE, dfd, "fifo", 0, name), EPERM, "fifo-user-remove");
    mark("FIFO_USER_EPERM");
    mark("VALIDATION_ORDER"); done();

    begin("file-getattr.raw-differential");
    char allocated[4096] = {1};
    check(write(fd, allocated, sizeof(allocated)) == sizeof(allocated) && fsync(fd) == 0,
          "allocate-extents");
    struct fattr initial;
    check(fa(NR_FGET, dfd, "file", &initial, 24, 0) == 0, "get");
    same_attr(fd, &initial);
    check(initial.nextents == 0 && initial.extsize == 0 && initial.cowextsize == 0,
          "ext4-fileattr-not-fiemap");
    mark("ALLOCATED_EXTENT_FIELDS_ZERO");
    struct { struct fattr attr; unsigned char tail[16]; } extended;
    memset(&extended, 0xa5, sizeof(extended));
    check(fa(NR_FGET, fd, "", &extended, sizeof(extended), AT_EMPTY_PATH) == 0, "extended");
    check(memcmp(&extended.attr, &initial, 24) == 0, "extended-value");
    for (size_t i = 0; i < sizeof(extended.tail); ++i) check(extended.tail[i] == 0, "zero-tail");
    mark("GET_EMPTY_PATH_ZERO_TAIL");
    ERROR(fa(NR_FGET, -1, BAD, BAD, 23, 0), EINVAL, "short-before-path");
    ERROR(fa(NR_FGET, -1, BAD, BAD, 4097, 0), E2BIG, "large-before-path");
    ERROR(fa(NR_FGET, -1, BAD, BAD, 24, BAD_FLAGS), EINVAL, "flags-before-path");
    ERROR(fa(NR_FGET, fd, "", BAD, 24, AT_EMPTY_PATH), EFAULT, "bad-output");
    mark("VALIDATION_ORDER"); done();

    begin("file-setattr.raw-differential");
    struct fattr changed = initial;
    changed.xflags ^= 0x80; /* FS_XFLAG_NODUMP: reversible, owner-settable. */
    struct fattr input = changed;
    input.nextents = 123;
    check(fa(NR_FSET, fd, "", &input, 24, AT_EMPTY_PATH) == 0, "toggle-nodump");
    same_attr(fd, &changed); mark("NODUMP_IGNORES_INPUT_NEXTENTS");
    input.extsize = 4096; input.cowextsize = 4096;
    check(fa(NR_FSET, fd, "", &input, 24, AT_EMPTY_PATH) == 0, "unflagged-hints");
    same_attr(fd, &changed); mark("EXT4_IGNORES_UNFLAGGED_HINTS");
    struct fattr invalid = changed; invalid.xflags |= UINT64_C(1) << 63;
    ERROR(fa(NR_FSET, fd, "", &invalid, 24, AT_EMPTY_PATH), EINVAL, "unknown-xflag"); same_attr(fd, &changed);
    ERROR(fa(NR_FSET, fd, "", BAD, 23, AT_EMPTY_PATH), EINVAL, "short"); same_attr(fd, &changed);
    ERROR(fa(NR_FSET, fd, "", BAD, 4097, AT_EMPTY_PATH), E2BIG, "large"); same_attr(fd, &changed);
    ERROR(fa(NR_FSET, fd, "", BAD, 24, AT_EMPTY_PATH), EFAULT, "bad-input"); same_attr(fd, &changed);
    ERROR(fa(NR_FSET, -1, BAD, BAD, 24, BAD_FLAGS), EINVAL, "flags-before-copy");
    mark("ERRORS_PRESERVE_STATE");
    check(fa(NR_FSET, fd, "", &initial, 24, AT_EMPTY_PATH) == 0, "restore");
    same_attr(fd, &initial); mark("RESTORE"); done();

    begin("open-tree-attr.raw-differential");
    struct mattr mount = { .set = 1 }; /* MOUNT_ATTR_RDONLY */
    int tree = syscall(NR_TREE, dfd, "", TREE_FLAGS | AT_EMPTY_PATH, &mount, 32);
    check(tree >= 0, "clone");
    check(fcntl(tree, F_GETFD) == FD_CLOEXEC, "cloexec");
    struct stat st;
    check(fstat(tree, &st) == 0 && S_ISDIR(st.st_mode), "directory-fd");
    struct stat original;
    check(fstat(dfd, &original) == 0 && st.st_ino == original.st_ino &&
          st.st_dev == original.st_dev, "retained-inode-identity");
    struct statvfs cloned_fs, source_fs;
    check(fstatvfs(tree, &cloned_fs) == 0 && fstatvfs(dfd, &source_fs) == 0,
          "mount-statfs");
    check((cloned_fs.f_flag & ST_RDONLY) != 0 &&
          (source_fs.f_flag & ST_RDONLY) == 0, "readonly-clone-isolated");
    check(pwrite(fd, "x", 1, 0) == 1, "source-still-writable");
    check(close(tree) == 0, "tree-close"); mark("CLONE_CLOEXEC_DIRECTORY");
    mark("READONLY_CLONE_SOURCE_UNCHANGED");
    ERROR(syscall(NR_TREE, dfd, "", TREE_FLAGS | AT_EMPTY_PATH, &mount, 31), EINVAL, "short");
    ERROR(syscall(NR_TREE, dfd, "", TREE_FLAGS | AT_EMPTY_PATH, &mount, 4097), E2BIG, "large");
    ERROR(syscall(NR_TREE, dfd, "", TREE_FLAGS | AT_EMPTY_PATH, BAD, 32), EFAULT, "bad-attr");
    ERROR(syscall(NR_TREE, -1, BAD, BAD_FLAGS, BAD, 32), EINVAL, "flags-before-copy");
    ERROR(syscall(NR_TREE, -1, BAD, BAD_FLAGS, NULL, 32), EINVAL, "null-size");
    mark("VALIDATION_ORDER"); done();

    check(close(fd) == 0, "close"); fd = -1;
    check(unlinkat(dfd, "file", 0) == 0 && unlinkat(dfd, "fifo", 0) == 0, "unlink");
    check(close(dfd) == 0, "directory-close"); dfd = -1;
    check(rmdir(dir) == 0, "rmdir");
    puts("THEKERNEL_FSATTRS_OK");
    return 0;
}
