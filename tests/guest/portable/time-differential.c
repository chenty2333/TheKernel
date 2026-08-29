#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

_Static_assert(sizeof(time_t) == 8, "native x86_64 time_t must be eight bytes");

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_TIME_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static time_t raw_time(void *tloc) {
    return (time_t)syscall(SYS_time, tloc);
}

static int realtime(time_t *seconds) {
    struct timespec value;
    if (syscall(SYS_clock_gettime, CLOCK_REALTIME, &value) != 0) {
        return -1;
    }
    *seconds = value.tv_sec;
    return 0;
}

int main(void) {
    puts("THEKERNEL_ABI_CASE time.raw-differential");

    errno = E2BIG;
    time_t value = raw_time(NULL);
    if (value < 1704067200 || errno != E2BIG) {
        errno = EPROTO;
        return fail("null-epoch-errno");
    }
    puts("THEKERNEL_ABI_ASSERT time.raw-differential NULL_EPOCH_ERRNO pass");

    unsigned char unaligned[sizeof(time_t) + 2];
    memset(unaligned, 0xa5, sizeof(unaligned));
    unsigned char *slot = unaligned + 1;
    value = raw_time(slot);
    time_t stored;
    memcpy(&stored, slot, sizeof(stored));
    if (value < 1704067200 || stored != value || unaligned[0] != 0xa5 ||
        unaligned[sizeof(unaligned) - 1] != 0xa5 ||
        memcmp(unaligned + 1, &value, sizeof(value)) != 0) {
        errno = EPROTO;
        return fail("unaligned-store");
    }
    puts("THEKERNEL_ABI_ASSERT time.raw-differential UNALIGNED_EIGHT_BYTES pass");

    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        errno = EINVAL;
        return fail("page-size");
    }
    unsigned char *pages = mmap(NULL, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED) {
        return fail("mmap");
    }
    slot = pages + page_size - sizeof(time_t) / 2;
    value = raw_time(slot);
    memcpy(&stored, slot, sizeof(stored));
    if (value < 1704067200 || stored != value) {
        (void)munmap(pages, (size_t)page_size * 2);
        errno = EPROTO;
        return fail("cross-writable-page");
    }
    puts("THEKERNEL_ABI_ASSERT time.raw-differential CROSS_WRITABLE_PAGE pass");

    errno = 0;
    if (raw_time((void *)(uintptr_t)1) != (time_t)-1 || errno != EFAULT) {
        (void)munmap(pages, (size_t)page_size * 2);
        errno = EPROTO;
        return fail("bad-pointer-efault");
    }
    if (mprotect(pages, (size_t)page_size, PROT_READ) != 0) {
        (void)munmap(pages, (size_t)page_size * 2);
        return fail("mprotect-readonly");
    }
    errno = 0;
    if (raw_time(pages) != (time_t)-1 || errno != EFAULT) {
        (void)munmap(pages, (size_t)page_size * 2);
        errno = EPROTO;
        return fail("readonly-efault");
    }
    if (mprotect(pages, (size_t)page_size, PROT_READ | PROT_WRITE) != 0 ||
        mprotect(pages + page_size, (size_t)page_size, PROT_NONE) != 0) {
        (void)munmap(pages, (size_t)page_size * 2);
        return fail("mprotect-none");
    }
    errno = 0;
    if (raw_time(pages + page_size - sizeof(time_t) / 2) !=
            (time_t)-1 ||
        errno != EFAULT) {
        (void)munmap(pages, (size_t)page_size * 2);
        errno = EPROTO;
        return fail("cross-prot-none-efault");
    }
    (void)munmap(pages, (size_t)page_size * 2);
    puts("THEKERNEL_ABI_ASSERT time.raw-differential EFAULT_COPYOUT pass");

    time_t before, after;
    if (realtime(&before) != 0 || (value = raw_time(NULL)) < 0 ||
        realtime(&after) != 0 || before > value || value > after) {
        errno = EPROTO;
        return fail("realtime-bracket");
    }
    puts("THEKERNEL_ABI_ASSERT time.raw-differential REALTIME_BRACKET pass");
    puts("THEKERNEL_TIME_OK");
    puts("THEKERNEL_ABI_RESULT time.raw-differential pass");
    return 0;
}
