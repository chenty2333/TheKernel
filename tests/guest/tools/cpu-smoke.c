#define _GNU_SOURCE
#include <cpuid.h>
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* This program measures guest-visible and userspace-enabled state.  CPUID
 * alone is never evidence that privileged PCID/APIC/CET paths are enabled. */
static unsigned int leaf1c, leaf1d, leaf7c;
static int expected_cpus, require_kvm;
static const char *signal_fp_path = "/opt/thekernel-tests/bin/thekernel-signal-fp-smoke";

static int capabilities(void) {
    unsigned int a, b, c, d;
    if (!__get_cpuid(1, &a, &b, &c, &d)) return 1;
    leaf1c = c; leaf1d = d;
    if (__get_cpuid_count(7, 0, &a, &b, &c, &d)) leaf7c = c;
    char vendor[13] = {0};
    __cpuid(0x40000000, a, b, c, d);
    memcpy(vendor, &b, 4); memcpy(vendor + 4, &c, 4); memcpy(vendor + 8, &d, 4);
    printf("# guest-visible hypervisor=%u vendor=%s apic=%u pcid=%u xsave=%u pku=%u cet_ss=%u\n",
           leaf1c >> 31, vendor, (leaf1d >> 9) & 1, (leaf1c >> 17) & 1,
           (leaf1c >> 26) & 1, (leaf7c >> 3) & 1, (leaf7c >> 7) & 1);
    puts("# privileged PCID/APIC enable state is in THEKERNEL_CPU_ENABLED boot diagnostics");
    return require_kvm && (!(leaf1c & (1u << 31)) || memcmp(vendor, "KVMKVMKVM", 9));
}

static int cpu_scheduling(void) {
    cpu_set_t original, single;
    if (sched_getaffinity(0, sizeof(original), &original)) return 1;
    int count = CPU_COUNT(&original);
    printf("# usable-cpus=%d expected=%d\n", count, expected_cpus);
    if (expected_cpus && count != expected_cpus) return 1;
    int failed = 0;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &original)) continue;
        CPU_ZERO(&single); CPU_SET(cpu, &single);
        if (sched_setaffinity(0, sizeof(single), &single)) { failed = 1; break; }
        for (int i = 0; i < 32; ++i) {
            if (sched_yield() || sched_getcpu() != cpu || syscall(SYS_getpid) != getpid()) {
                failed = 1; break;
            }
        }
    }
    if (sched_setaffinity(0, sizeof(original), &original)) failed = 1;
    return failed;
}

static int timer_wakeup(void) {
    struct timespec start, end, delay = { .tv_nsec = 20000000 };
    if (clock_gettime(CLOCK_MONOTONIC, &start) || nanosleep(&delay, NULL) ||
        clock_gettime(CLOCK_MONOTONIC, &end)) return 1;
    int64_t elapsed = (end.tv_sec - start.tv_sec) * INT64_C(1000000000) + end.tv_nsec - start.tv_nsec;
    printf("# timer-wakeup-ns=%lld\n", (long long)elapsed);
    return elapsed < 20000000;
}

static int expect_fault(volatile unsigned char *page, const unsigned int *expected_pkru) {
    pid_t child = fork();
    if (child < 0) { perror("# protection fork"); return 1; }
    if (!child) {
        if (expected_pkru) {
            unsigned int actual, high;
            __asm__ volatile("rdpkru" : "=a"(actual), "=d"(high) : "c"(0));
            if (actual != *expected_pkru) {
                fprintf(stderr, "# child PKRU expected=0x%x actual=0x%x cpu=%d\n",
                        *expected_pkru, actual, sched_getcpu());
                _exit(2);
            }
        }
        *page = 7;
        _exit(1);
    }
    int status = 0;
    pid_t waited;
    do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != child) { perror("# protection waitpid"); return 1; }
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) {
        fprintf(stderr, "# protection child=%ld unexpected wait status=0x%x\n", (long)child, status);
        return 1;
    }
    return 0;
}

static int memory_protection(void) {
    size_t size = (size_t)sysconf(_SC_PAGESIZE);
    void *page = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) return 1;
    *(volatile unsigned char *)page = 1;
    int failed = mprotect(page, size, PROT_READ) || expect_fault(page, NULL);
    if (mprotect(page, size, PROT_READ | PROT_WRITE)) failed = 1;
    else *(volatile unsigned char *)page = 2;
    if (munmap(page, size)) failed = 1;
    return failed;
}

static int xsave_state(void) {
    if (!(leaf1c & (1u << 26)) || !(leaf1c & (1u << 27))) {
        unsigned char legacy[512] __attribute__((aligned(16)));
        memset(legacy, 0, sizeof(legacy));
        puts("# enabled OSXSAVE=0: validating FXSAVE/FXRSTOR fallback and signal state");
        __asm__ volatile("fxsave64 %0\n\tfxrstor64 %0" : "+m"(legacy) :: "memory");
    } else {
        unsigned int lo, hi, a, b, c, d;
        __asm__ volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(0));
        __cpuid_count(0xd, 0, a, b, c, d);
        printf("# enabled xcr0=0x%08x%08x xsave_bytes=%u\n", hi, lo, b);
        if ((lo & 3) != 3 || b < 576 || b > 1024 * 1024) return 1;
        void *state = NULL;
        if (posix_memalign(&state, 64, b)) return 1;
        memset(state, 0, b);
        /* No calls or compiler FP work between saving and restoring. */
        __asm__ volatile("xsave64 (%0)\n\txrstor64 (%0)" :: "r"(state), "a"(lo), "d"(hi) : "memory");
        free(state);
    }
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) {
        execl(signal_fp_path, "thekernel-signal-fp-smoke", (char *)NULL);
        fprintf(stderr, "# missing signal FP helper %s: %s\n", signal_fp_path, strerror(errno));
        _exit(127);
    }
    int status;
    return waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status);
}

static int pkey_invalid_arguments(void) {
    const unsigned long invalid[][2] = {
        {1UL << 32, 0}, {1, 0}, {0, 1UL << 32}, {0, 4}, {0, ~0UL}
    };
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i) {
        errno = 0;
        long key = syscall(SYS_pkey_alloc, invalid[i][0], invalid[i][1]);
        int error = errno;
        if (key >= 0) syscall(SYS_pkey_free, key);
        if (key != -1 || error != EINVAL) {
            fprintf(stderr, "# invalid pkey_alloc flags=%#lx rights=%#lx result=%ld errno=%d\n",
                    invalid[i][0], invalid[i][1], key, error);
            return 1;
        }
    }
    return 0;
}

static int pkey_exhaustion(void) {
    long keys[15];
    size_t count = 0;
    int exhausted = 0, failed = 0;
    while (count < sizeof(keys) / sizeof(keys[0])) {
        errno = 0;
        long key = syscall(SYS_pkey_alloc, 0UL, 0UL);
        if (key == -1) {
            exhausted = errno == ENOSPC;
            if (!exhausted) perror("# pkey_alloc exhaustion");
            break;
        }
        if (key < 1 || key > 15) {
            if (key >= 0) syscall(SYS_pkey_free, key);
            failed = 1;
            break;
        }
        keys[count++] = key;
    }
    if (!exhausted || pkey_invalid_arguments()) failed = 1;
    if (exhausted) {
        errno = 0;
        long unexpected = syscall(SYS_pkey_alloc, 0UL, 0UL);
        if (unexpected != -1 || errno != ENOSPC) failed = 1;
        if (unexpected >= 0) syscall(SYS_pkey_free, unexpected);
    }
    while (count) {
        if (syscall(SYS_pkey_free, keys[--count])) failed = 1;
    }
    /* Invalid requests at exhaustion must neither allocate nor free keys. */
    long reused = syscall(SYS_pkey_alloc, 0UL, 0UL);
    if (reused < 1 || reused > 15) failed = 1;
    if (reused >= 0 && syscall(SYS_pkey_free, reused)) failed = 1;
    return failed;
}

static int pku_state(void) {
    if (pkey_invalid_arguments()) return 1;
    errno = 0;
    long key = syscall(SYS_pkey_alloc, 0, 0);
    int enabled = (leaf7c & (1u << 4)) != 0;
    printf("# enabled ospke=%d pkey_alloc=%ld errno=%d\n", enabled, key, errno);
    if (!enabled) return key != -1 || errno != ENOSPC;
    if (key < 1 || key > 15) return 1;
    if (pkey_exhaustion()) { syscall(SYS_pkey_free, key); return 1; }
    size_t size = (size_t)sysconf(_SC_PAGESIZE);
    void *page = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) { syscall(SYS_pkey_free, key); return 1; }
    int failed = syscall(SYS_pkey_mprotect, page, size, PROT_READ | PROT_WRITE, key) != 0;
    if (failed) perror("# pkey_mprotect");
    if (!failed) {
        unsigned int original, high;
        __asm__ volatile("rdpkru" : "=a"(original), "=d"(high) : "c"(0));
        unsigned int denied = original | (2u << (2 * key));
        __asm__ volatile("wrpkru" :: "a"(denied), "c"(0), "d"(0) : "memory");
        failed = expect_fault(page, &denied);
        unsigned int resumed;
        __asm__ volatile("rdpkru" : "=a"(resumed), "=d"(high) : "c"(0));
        if (resumed != denied) {
            fprintf(stderr, "# parent PKRU expected=0x%x actual=0x%x cpu=%d\n",
                    denied, resumed, sched_getcpu());
            failed = 1;
        }
        __asm__ volatile("wrpkru" :: "a"(original), "c"(0), "d"(0) : "memory");
        *(volatile unsigned char *)page = 3;
    }
    if (munmap(page, size)) { perror("# pku munmap"); failed = 1; }
    if (syscall(SYS_pkey_free, key)) { perror("# pkey_free"); failed = 1; }
    return failed;
}

static int cet_status(void) {
    unsigned long status = ~0ul;
    errno = 0;
    long result = syscall(SYS_arch_prctl, 0x5005, &status);
    printf("# enabled cet_status result=%ld bits=0x%lx errno=%d\n", result, status, errno);
    if (result == 0) {
        if (status != 0) return 1;
        /* No libc return is allowed while newly enabled SHSTK has no matching
         * entry for this C frame. Keep enable, CALL/RET, status and disable in
         * one asm block, and exercise an actual shadow-stack-protected return. */
        long values[4] = {-1, -1, -1, -1};
        __asm__ volatile(
            "mov $158, %%eax; mov $0x5001, %%edi; mov $1, %%esi; syscall\n\t"
            "mov %%rax, 0(%0); test %%rax, %%rax; jnz 1f\n\t"
            "call 2f; jmp 3f; 2: ret; 3:\n\t"
            "mov $158, %%eax; mov $0x5005, %%edi; lea 8(%0), %%rsi; syscall\n\t"
            "mov %%rax, 16(%0)\n\t"
            "mov $158, %%eax; mov $0x5002, %%edi; mov $1, %%esi; syscall\n\t"
            "mov %%rax, 24(%0); 1:"
            :: "r"(values) : "rax", "rdi", "rsi", "rcx", "r11", "memory", "cc");
        printf("# CET live enable=%ld status=%ld bits=%ld disable=%ld\n",
               values[0], values[2], values[1], values[3]);
        return values[0] || values[2] || values[1] != 1 || values[3];
    }
    /* A hardware feature can be visible without kernel support. In that case
     * verify a rejected request; do not call this native CET validation. */
    if (errno != EINVAL && errno != ENODEV && errno != EOPNOTSUPP) return 1;
    puts("# CET unavailable: validating enable rejection");
    errno = 0;
    result = syscall(SYS_arch_prctl, 0x5001, 1);
    return result != -1 || (errno != EINVAL && errno != ENODEV && errno != EOPNOTSUPP);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    const struct rlimit no_core = {0, 0};
    if (setrlimit(RLIMIT_CORE, &no_core)) return 1;
    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--require-kvm")) require_kvm = 1;
        else if (!strcmp(argv[i], "--signal-fp") && i + 1 < argc) signal_fp_path = argv[++i];
        else if (!strcmp(argv[i], "--expected-cpus") && i + 1 < argc) {
            char *end;
            long count = strtol(argv[++i], &end, 10);
            if (*end || count < 1 || count > CPU_SETSIZE) return 2;
            expected_cpus = (int)count;
        } else return 2;
    }
    const struct { const char *name; int (*run)(void); } cases[] = {
        {"cpu-capabilities", capabilities}, {"cpu-smp-syscall", cpu_scheduling},
        {"cpu-timer", timer_wakeup}, {"cpu-memory-protection", memory_protection},
        {"cpu-xsave-signal", xsave_state}, {"cpu-pku", pku_state}, {"cpu-cet-status", cet_status},
    };
    puts("KTAP version 1");
    printf("1..%zu\n", sizeof(cases) / sizeof(cases[0]));
    int failures = 0;
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); ++i) {
        printf("# THEKERNEL_TEST_BEGIN %zu %s timeout_seconds=60\n", i + 1, cases[i].name);
        int result = cases[i].run();
        printf("# THEKERNEL_TEST_END %zu %s result=%d\n", i + 1, cases[i].name, result);
        printf("%s %zu - %s\n", result ? "not ok" : "ok", i + 1, cases[i].name);
        failures += result != 0;
    }
    if (!failures) puts("# THEKERNEL_CPU_TEST_COMPLETE");
    return failures ? 1 : 0;
}
