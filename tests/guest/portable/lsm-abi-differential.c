/*
 * The LSM syscall family, differentially: lsm_list_modules(2),
 * lsm_get_self_attr(2) and lsm_set_self_attr(2).
 *
 * Every assertion here is one that a Linux v7.2.3 guest and TheKernel must
 * answer identically.  The reference is `security/lsm_syscalls.c` and
 * `security/security.c` of that release.
 *
 * Nothing here depends on the *set* of LSMs the reference guest booted with.
 * A get/set attribute case either fails before any module is consulted, or
 * names an LSM ID that no module in any kernel owns, so a stock distribution
 * kernel and TheKernel agree even though their active module lists differ.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_lsm_get_self_attr
#define SYS_lsm_get_self_attr 459
#endif
#ifndef SYS_lsm_set_self_attr
#define SYS_lsm_set_self_attr 460
#endif
#ifndef SYS_lsm_list_modules
#define SYS_lsm_list_modules 461
#endif

/* include/uapi/linux/lsm.h of v7.2.3. */
#define LSM_ATTR_CURRENT 100
#define LSM_ATTR_UNDEF 0
#define LSM_FLAG_SINGLE 1
#define LSM_ID_CAPABILITY 100
#define LSM_ID_LANDLOCK 110

/* A syntactically valid LSM ID that no module of any kernel owns. */
#define LSM_ID_UNOWNED 999

struct lsm_ctx {
    uint64_t id;
    uint64_t flags;
    uint64_t len;
    uint64_t ctx_len;
};

static long lsm_list_modules(uint64_t *ids, uint32_t *size, uint32_t flags) {
    return syscall(SYS_lsm_list_modules, ids, size, flags);
}

static long lsm_get_self_attr(unsigned int attr, void *ctx, uint32_t *size, uint32_t flags) {
    return syscall(SYS_lsm_get_self_attr, attr, ctx, size, flags);
}

static long lsm_set_self_attr(unsigned int attr, const void *ctx, uint32_t size, uint32_t flags) {
    return syscall(SYS_lsm_set_self_attr, attr, ctx, size, flags);
}

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_LSM_ABI_FAIL %s errno=%d (%s)\n", stage, errno, strerror(errno));
    return 1;
}

static int case_open(const char *name) {
    printf("THEKERNEL_ABI_CASE %s.raw-differential\n", name);
    return 0;
}

static int assert_ok(const char *name, const char *assertion) {
    printf("THEKERNEL_ABI_ASSERT %s.raw-differential %s pass\n", name, assertion);
    return 0;
}

static int case_close(const char *name) {
    printf("THEKERNEL_ABI_RESULT %s.raw-differential pass\n", name);
    return 0;
}

/* `errno == expected` for a syscall that failed; a successful call is a bug.
 * A mismatch keeps the observed errno so the caller's failure line reports it. */
static int expect_errno(long ret, int expected) {
    if (ret != -1) {
        errno = EPROTO;
        return 1;
    }
    return errno == expected ? 0 : 1;
}

int main(void) {
    const char *name = "lsm-self-attr";
    uint64_t ids[64];
    struct lsm_ctx ctx;
    uint32_t size;
    size_t index;
    long ret;
    int capability_seen;

    case_open(name);

    /* `SYSCALL_DEFINE3(lsm_list_modules)`: flags are validated first, then the
     * size pointer, then the write-back, then the caller's capacity. */
    size = 64;
    if (expect_errno(lsm_list_modules(ids, &size, 1), EINVAL)) return fail("list-flags");
    if (expect_errno(lsm_list_modules(ids, NULL, 0), EFAULT)) return fail("list-null-size");
    size = 0;
    if (expect_errno(lsm_list_modules(ids, &size, 0), E2BIG)) return fail("list-e2big");
    if (size == 0 || size % sizeof(uint64_t) != 0) return fail("list-required-size");
    if (size > sizeof(ids)) return fail("list-too-many");
    memset(ids, 0, sizeof(ids));
    ret = lsm_list_modules(ids, &size, 0);
    if (ret <= 0) return fail("list-count");
    if ((uint64_t)ret * sizeof(uint64_t) != size) return fail("list-count-size");
    capability_seen = 0;
    for (index = 0; index < (size_t)ret; ++index) {
        if (ids[index] == LSM_ID_CAPABILITY) capability_seen = 1;
    }
    if (!capability_seen) return fail("list-capability");
    if (assert_ok(name, "LIST_MODULES_ARGUMENTS")) return 1;

    /* `security_getselfattr()`: the attribute and size validation precede every
     * module lookup, so these errnos are independent of the booted LSM set. */
    size = 64;
    if (expect_errno(lsm_get_self_attr(LSM_ATTR_UNDEF, &ctx, &size, 0), EINVAL)) {
        return fail("get-attr-undef");
    }
    if (expect_errno(lsm_get_self_attr(LSM_ATTR_CURRENT, &ctx, NULL, 0), EINVAL)) {
        return fail("get-null-size");
    }
    if (expect_errno(lsm_get_self_attr(LSM_ATTR_CURRENT, &ctx, &size, 2), EINVAL)) {
        return fail("get-unknown-flag");
    }
    if (expect_errno(lsm_get_self_attr(LSM_ATTR_CURRENT, NULL, &size, LSM_FLAG_SINGLE), EINVAL)) {
        return fail("get-single-null-ctx");
    }
    memset(&ctx, 0, sizeof(ctx));
    if (expect_errno(lsm_get_self_attr(LSM_ATTR_CURRENT, &ctx, &size, LSM_FLAG_SINGLE), EINVAL)) {
        return fail("get-single-zero-id");
    }
    if (assert_ok(name, "GET_SELF_ATTR_ARGUMENT_ORDER")) return 1;

    /* An LSM ID that no module owns reaches no provider, so the core reports
     * EOPNOTSUPP after writing the produced size back.  A structurally valid
     * but unowned ID is exactly the input Linux must not reject with EINVAL. */
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = LSM_ID_UNOWNED;
    ctx.len = sizeof(ctx);
    size = 64;
    if (expect_errno(lsm_get_self_attr(LSM_ATTR_CURRENT, &ctx, &size, LSM_FLAG_SINGLE), EOPNOTSUPP)) {
        return fail("get-unowned-id");
    }
    if (size != 0) return fail("get-unowned-size");
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = LSM_ID_CAPABILITY;
    ctx.len = sizeof(ctx);
    size = 64;
    if (expect_errno(lsm_get_self_attr(LSM_ATTR_CURRENT, &ctx, &size, LSM_FLAG_SINGLE), EOPNOTSUPP)) {
        return fail("get-capability-id");
    }
    if (assert_ok(name, "GET_SELF_ATTR_UNOWNED_ID_EOPNOTSUPP")) return 1;

    /* `security_setselfattr()`: flags, then the minimum and maximum sizes,
     * then the copy, then the structure's own consistency. */
    memset(&ctx, 0, sizeof(ctx));
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 32, 1), EINVAL)) {
        return fail("set-flags");
    }
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 31, 0), EINVAL)) {
        return fail("set-size-short");
    }
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 4097, 0), E2BIG)) {
        return fail("set-size-big");
    }
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, NULL, 32, 0), EFAULT)) {
        return fail("set-null-ctx");
    }
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = LSM_ID_LANDLOCK;
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 32, 0), EINVAL)) {
        return fail("set-zero-len");
    }
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = LSM_ID_LANDLOCK;
    ctx.len = 40;
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 32, 0), EINVAL)) {
        return fail("set-len-over-size");
    }
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = LSM_ID_LANDLOCK;
    ctx.len = 32;
    ctx.ctx_len = 8;
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 32, 0), EINVAL)) {
        return fail("set-ctx-over-len");
    }
    if (assert_ok(name, "SET_SELF_ATTR_STRUCTURE")) return 1;

    /* No module owns these IDs, so the core default applies regardless of which
     * LSMs are active; `attr` is never inspected by the core. */
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = LSM_ID_UNOWNED;
    ctx.len = sizeof(ctx);
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 32, 0), EOPNOTSUPP)) {
        return fail("set-unowned-id");
    }
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = 0;
    ctx.len = sizeof(ctx);
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 32, 0), EOPNOTSUPP)) {
        return fail("set-zero-id");
    }
    memset(&ctx, 0, sizeof(ctx));
    ctx.id = LSM_ID_CAPABILITY;
    ctx.len = sizeof(ctx);
    if (expect_errno(lsm_set_self_attr(LSM_ATTR_CURRENT, &ctx, 32, 0), EOPNOTSUPP)) {
        return fail("set-capability-id");
    }
    if (assert_ok(name, "SET_SELF_ATTR_UNOWNED_ID_EOPNOTSUPP")) return 1;

    if (case_close(name) != 0) return 1;
    puts("THEKERNEL_LSM_ABI_OK");
    return 0;
}
