#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int failed;
static int case_failed;
static const char *active;
static void begin(const char *name)
{
    active = name;
    case_failed = 0;
    printf("THEKERNEL_ABI_CASE %s\n", active);
}
static void done(void)
{
    if (!case_failed) printf("THEKERNEL_ABI_RESULT %s pass\n", active);
}
static void check(const char *name, int good)
{
    if (!good) {
        fprintf(stderr, "THEKERNEL_SCHEDULER_BASIC_FAIL %s errno=%d\n", name, errno);
        failed = case_failed = 1;
    } else {
        printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
    }
}
#define EXPECT_ERR(name, call, error) do { errno = 0; long rc = (call); check(name, rc == -1 && errno == (error)); } while (0)
int main(void)
{
    unsigned long mask[128] = {0}, copy[128] = {0};
    begin("sched_getaffinity.raw-differential");
    long bytes = syscall(SYS_sched_getaffinity, 0, sizeof(mask), mask);
    check("get-affinity", bytes > 0 && bytes <= (long)sizeof(mask));
    if (failed) return 1;
    EXPECT_ERR("get-unaligned-length", syscall(SYS_sched_getaffinity, 0, sizeof(mask) - 1, copy), EINVAL);
    EXPECT_ERR("get-low32-zero", syscall(SYS_sched_getaffinity, 0, 1UL << 32, copy), EINVAL);
    long high_bytes = syscall(SYS_sched_getaffinity, 0, (1UL << 32) | sizeof(mask), copy);
    check("get-low32-length", high_bytes == bytes && memcmp(mask, copy, (size_t)bytes) == 0);
    done();
    begin("sched_setaffinity.raw-differential");
    check("set-low32-length", syscall(SYS_sched_setaffinity, 0, (1UL << 32) | (unsigned long)bytes, mask) == 0);
    EXPECT_ERR("set-low32-zero", syscall(SYS_sched_setaffinity, 0, 1UL << 32, mask), EINVAL);
    /* A short set mask is zero-extended, unlike getaffinity's aligned size. */
    size_t last = sizeof(mask);
    while (last && ((unsigned char *)mask)[last - 1] == 0) last--;
    check("set-short-mask", last > 0 && syscall(SYS_sched_setaffinity, 0, last, mask) == 0);
    done();
    begin("getcpu.raw-differential");
    unsigned node = UINT32_MAX;
    EXPECT_ERR("getcpu-first-copy-fault", syscall(SYS_getcpu, (void *)1, &node, 0), EFAULT);
    check("getcpu-node-written-after-cpu-fault", node != UINT32_MAX);
    done();
    begin("sched_setparam.raw-differential");
    EXPECT_ERR("setparam-negative-pid-before-copy", syscall(SYS_sched_setparam, -1, (void *)1), EINVAL);
    EXPECT_ERR("setparam-null", syscall(SYS_sched_setparam, 0, NULL), EINVAL);
    EXPECT_ERR("setparam-bad-pointer", syscall(SYS_sched_setparam, 0, (void *)1), EFAULT);
    done();
    begin("sched_setscheduler.raw-differential");
    EXPECT_ERR("setscheduler-negative-pid-before-copy", syscall(SYS_sched_setscheduler, -1, SCHED_OTHER, (void *)1), EINVAL);
    EXPECT_ERR("setscheduler-negative-policy-before-copy", syscall(SYS_sched_setscheduler, 0, -1, (void *)1), EINVAL);
    EXPECT_ERR("setscheduler-positive-invalid-policy-after-copy", syscall(SYS_sched_setscheduler, 0, 12345, (void *)1), EFAULT);
    EXPECT_ERR("setscheduler-null", syscall(SYS_sched_setscheduler, 0, SCHED_OTHER, NULL), EINVAL);
    done();
    begin("sched_get_priority_max.raw-differential");
    check("ext-priority-max", syscall(SYS_sched_get_priority_max, 7) == 0);
    done();
    begin("sched_get_priority_min.raw-differential");
    check("ext-priority-min", syscall(SYS_sched_get_priority_min, 7) == 0);
    done();
    if (failed) return 1;
    puts("THEKERNEL_SCHEDULER_BASIC_DIFFERENTIAL_OK");
    return 0;
}
