#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "the JIT memory smoke is x86_64-only"
#endif

/* These are Linux x86_64 ABI values, not libc-private declarations.  glibc
 * exposes the memfd flags through <bits/mman-shared.h>, while older glibc and
 * musl releases may lack the newer ones entirely, so keep the ABI numbers
 * local and explicit. */
#ifndef SYS_memfd_create
#define SYS_memfd_create 319
#endif
#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif
#ifndef MFD_ALLOW_SEALING
#define MFD_ALLOW_SEALING 0x0002U
#endif
#ifndef MFD_NOEXEC_SEAL
#define MFD_NOEXEC_SEAL 0x0008U
#endif
#ifndef MFD_EXEC
#define MFD_EXEC 0x0010U
#endif

#if !defined(MAP_ANONYMOUS) && defined(MAP_ANON)
#define MAP_ANONYMOUS MAP_ANON
#endif

/* mov eax, imm32; ret -- the smallest function body a JIT can emit.  The
 * immediate is the entire observable result, so a stale page, a stale alias or
 * a page that silently kept its old bytes shows up as a wrong return value
 * rather than as a crash. */
#define JIT_CODE_BYTES 6U

/* Generation constants are distinct and non-trivial: a value that is cached,
 * reloaded from the wrong generation, or read from one mapping too early can
 * never accidentally match the expected constant. */
#define ANON_GEN1_VALUE UINT32_C(42)
#define ANON_GEN2_VALUE UINT32_C(0x5a5a1234)
#define ANON_GEN3_VALUE UINT32_C(0x0badf00d)
#define MEMFD_ALIAS_VALUE UINT32_C(0x1337c0de)
#define MEMFD_ALIAS_REWRITE_VALUE UINT32_C(0x7e57ab1e)

typedef uint32_t (*jit_code_fn)(void);

static void emit_mov_eax_ret(unsigned char *code, uint32_t value) {
    code[0] = 0xb8U; /* mov eax, imm32 */
    code[1] = (unsigned char)(value & 0xffU);
    code[2] = (unsigned char)((value >> 8) & 0xffU);
    code[3] = (unsigned char)((value >> 16) & 0xffU);
    code[4] = (unsigned char)((value >> 24) & 0xffU);
    code[5] = 0xc3U; /* ret */
}

/* Enter the page as code.  On x86_64 the instruction and data caches are
 * coherent, so an explicit instruction-cache flush is deliberately not part of
 * the contract this probe measures. */
static uint32_t call_jit_code(const unsigned char *code) {
    jit_code_fn function = (jit_code_fn)(const void *)code;
    return function();
}

/* Compare a page view against the intended instruction bytes and return the
 * index of the first mismatch, or -1 when the view matches.  Reading a mapping
 * back before entering it lets a broken alias be reported as data instead of
 * crashing on whatever bytes it really holds. */
static int first_mismatched_code_byte(const unsigned char *view,
                                      const unsigned char *expected) {
    for (size_t index = 0; index < JIT_CODE_BYTES; ++index) {
        if (view[index] != expected[index]) {
            return (int)index;
        }
    }
    return -1;
}

/* Failures always name the operation, the violated semantic condition and the
 * raw observation (errno plus its text), so a differential run can be grepped
 * without rerunning the probe. */
static int fail_errno(const char *operation, const char *condition,
                      int call_errno) {
    fprintf(stderr, "THEKERNEL_JIT_MEM_FAIL %s condition=%s errno=%d (%s)\n",
            operation, condition, call_errno, strerror(call_errno));
    return EXIT_FAILURE;
}

static int fail_value(const char *operation, const char *condition,
                      uint32_t expected, uint32_t actual, int call_errno) {
    fprintf(stderr,
            "THEKERNEL_JIT_MEM_FAIL %s condition=%s expected=%u got=%u errno=%d (%s)\n",
            operation, condition, expected, actual, call_errno,
            strerror(call_errno));
    return EXIT_FAILURE;
}

/* Step 1: anonymous RW -> RX transition and execution.
 *
 * Contract: a page that was ordinary writable memory can be filled with
 * instructions as data, switched to PROT_READ|PROT_EXEC, and then entered
 * through an ordinary indirect call that returns the emitted constant.  W^X
 * still holds -- the page is never writable and executable at once -- but the
 * transition itself must be permitted.  Without this no JIT can run at all. */
static int probe_anon_rx(unsigned char **region_out, size_t page_size) {
    errno = 0;
    unsigned char *region =
        mmap(NULL, page_size, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (region == MAP_FAILED) {
        return fail_errno("anon-rw-map",
                          "mmap(PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS)-"
                          "not-MAP_FAILED",
                          errno);
    }
    emit_mov_eax_ret(region, ANON_GEN1_VALUE);

    errno = 0;
    if (mprotect(region, page_size, PROT_READ | PROT_EXEC) != 0) {
        const int saved_errno = errno;
        return fail_errno("anon-rx-mprotect",
                          "mprotect-rw-to-rx-returns-0", saved_errno);
    }

    errno = 0;
    const uint32_t actual = call_jit_code(region);
    const int call_errno = errno;
    if (actual != ANON_GEN1_VALUE) {
        return fail_value("anon-rx-exec",
                          "executed-anonymous-page-returns-emitted-constant",
                          ANON_GEN1_VALUE, actual, call_errno);
    }

    *region_out = region;
    puts("THEKERNEL_JIT_MEM_ANON_RX_OK");
    return EXIT_SUCCESS;
}

/* Step 2: the RW -> RX transition must not be one-shot.
 *
 * Contract: the same virtual region accepts RX -> RW -> RX, and the second RX
 * view executes the bytes written while the region was writable again.  A JIT
 * that patches or re-emits into a page it has already executed depends on the
 * permission transitions being repeatable and on the rewrite being honoured. */
static int probe_repeat_transition(unsigned char *region, size_t page_size) {
    errno = 0;
    if (mprotect(region, page_size, PROT_READ | PROT_WRITE) != 0) {
        const int saved_errno = errno;
        return fail_errno("anon-rw-remprotect",
                          "mprotect-rx-to-rw-returns-0", saved_errno);
    }
    emit_mov_eax_ret(region, ANON_GEN2_VALUE);

    errno = 0;
    if (mprotect(region, page_size, PROT_READ | PROT_EXEC) != 0) {
        const int saved_errno = errno;
        return fail_errno("anon-rx-remprotect",
                          "second-mprotect-rw-to-rx-returns-0", saved_errno);
    }

    errno = 0;
    const uint32_t actual = call_jit_code(region);
    const int call_errno = errno;
    if (actual != ANON_GEN2_VALUE) {
        return fail_value("anon-rx-reexec",
                          "rewritten-page-returns-second-emitted-constant",
                          ANON_GEN2_VALUE, actual, call_errno);
    }

    puts("THEKERNEL_JIT_MEM_ANON_REWRITE_OK");
    return EXIT_SUCCESS;
}

/* Step 3: a new code generation must not invalidate live code.
 *
 * Contract: while the first executable region stays mapped and callable, a
 * fresh anonymous region can be filled and made executable, and afterwards the
 * first region still executes and still returns its own constant.  This is the
 * JIT's steady state: old translations remain live while new code is emitted. */
static int probe_second_generation(unsigned char **second_out,
                                   const unsigned char *first_region,
                                   size_t page_size) {
    errno = 0;
    unsigned char *second =
        mmap(NULL, page_size, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (second == MAP_FAILED) {
        return fail_errno("anon-gen2-map-rw",
                          "second-mmap(PROT_READ|PROT_WRITE,anonymous)-not-MAP_"
                          "FAILED",
                          errno);
    }
    emit_mov_eax_ret(second, ANON_GEN3_VALUE);

    errno = 0;
    if (mprotect(second, page_size, PROT_READ | PROT_EXEC) != 0) {
        const int saved_errno = errno;
        return fail_errno("anon-gen2-rx-mprotect",
                          "second-region-mprotect-rw-to-rx-returns-0",
                          saved_errno);
    }

    errno = 0;
    const uint32_t second_actual = call_jit_code(second);
    const int second_errno = errno;
    if (second_actual != ANON_GEN3_VALUE) {
        return fail_value("anon-gen2-exec",
                          "second-region-returns-its-emitted-constant",
                          ANON_GEN3_VALUE, second_actual, second_errno);
    }

    errno = 0;
    const uint32_t first_actual = call_jit_code(first_region);
    const int first_errno = errno;
    if (first_actual != ANON_GEN2_VALUE) {
        return fail_value("anon-gen1-still-live",
                          "first-region-still-returns-its-old-constant",
                          ANON_GEN2_VALUE, first_actual, first_errno);
    }

    *second_out = second;
    puts("THEKERNEL_JIT_MEM_SECOND_GEN_OK");
    return EXIT_SUCCESS;
}

/* Step 4: teardown must not damage the address space.
 *
 * Contract: munmap of both executable regions succeeds, and a fresh anonymous
 * RW page mapped afterwards can still be written and read back at both ends.
 * This catches stale executable PTEs, a leaked executable VMA, or corrupted
 * neighbouring mappings left behind by the earlier mprotect transitions. */
static int probe_teardown(unsigned char *first_region,
                          unsigned char *second_region, size_t page_size) {
    errno = 0;
    if (munmap(first_region, page_size) != 0) {
        const int saved_errno = errno;
        return fail_errno("teardown-unmap-gen1",
                          "munmap-first-executable-region-returns-0",
                          saved_errno);
    }
    errno = 0;
    if (munmap(second_region, page_size) != 0) {
        const int saved_errno = errno;
        return fail_errno("teardown-unmap-gen2",
                          "munmap-second-executable-region-returns-0",
                          saved_errno);
    }
    puts("THEKERNEL_JIT_MEM_UNMAP_OK");

    errno = 0;
    unsigned char *fresh =
        mmap(NULL, page_size, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (fresh == MAP_FAILED) {
        return fail_errno("teardown-remap-rw",
                          "post-teardown-mmap(PROT_READ|PROT_WRITE)-not-MAP_"
                          "FAILED",
                          errno);
    }
    /* volatile keeps the store/load a real memory round trip at -O2. */
    volatile unsigned char *probe = fresh;
    probe[0] = 0xa5U;
    probe[page_size - 1U] = 0x5aU;
    const uint32_t read_first = probe[0];
    const uint32_t read_last = probe[page_size - 1U];
    if (read_first != UINT32_C(0xa5)) {
        return fail_value("teardown-readback-first",
                          "fresh-page-first-byte-round-trips",
                          UINT32_C(0xa5), read_first, errno);
    }
    if (read_last != UINT32_C(0x5a)) {
        return fail_value("teardown-readback-last",
                          "fresh-page-last-byte-round-trips",
                          UINT32_C(0x5a), read_last, errno);
    }

    errno = 0;
    if (munmap(fresh, page_size) != 0) {
        const int saved_errno = errno;
        return fail_errno("teardown-unmap-fresh",
                          "munmap-fresh-rw-page-returns-0", saved_errno);
    }

    puts("THEKERNEL_JIT_MEM_TEARDOWN_OK");
    return EXIT_SUCCESS;
}

/* Step 5: split-WX aliases built from memfd_create plus mmap.
 *
 * Contract: one memfd has two MAP_SHARED mappings with different protections --
 * a writable hole and an executable view -- that share the same page cache, so
 * the executable alias executes what was written through the writable alias,
 * and a later rewrite through the writable alias is observed by the next
 * execution without touching the executable mapping.  This is precisely the
 * QEMU TCG memory contract: translate into the RW alias, run from the RX one. */
static int probe_memfd_split_wx(size_t page_size) {
    errno = 0;
    const int fd = (int)syscall(SYS_memfd_create, "thekernel-jit-mem",
                                MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0) {
        return fail_errno("memfd-create",
                          "memfd_create(MFD_CLOEXEC|MFD_ALLOW_SEALING)-returns-"
                          "fd",
                          errno);
    }

    errno = 0;
    if (ftruncate(fd, (off_t)page_size) != 0) {
        const int saved_errno = errno;
        return fail_errno("memfd-truncate",
                          "ftruncate(memfd,page_size)-returns-0", saved_errno);
    }

    errno = 0;
    unsigned char *rw_alias = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                                   MAP_SHARED, fd, 0);
    if (rw_alias == MAP_FAILED) {
        return fail_errno("memfd-rw-alias-map",
                          "mmap(PROT_READ|PROT_WRITE,MAP_SHARED,memfd)-not-MAP_"
                          "FAILED",
                          errno);
    }

    errno = 0;
    unsigned char *rx_alias = mmap(NULL, page_size, PROT_READ | PROT_EXEC,
                                   MAP_SHARED, fd, 0);
    if (rx_alias == MAP_FAILED) {
        return fail_errno("memfd-rx-alias-map",
                          "mmap(PROT_READ|PROT_EXEC,MAP_SHARED,memfd)-not-MAP_"
                          "FAILED",
                          errno);
    }
    if (rw_alias == rx_alias) {
        return fail_value("memfd-alias-addresses",
                          "rw-and-rx-aliases-are-distinct-virtual-addresses", 0U,
                          1U, errno);
    }

    emit_mov_eax_ret(rw_alias, MEMFD_ALIAS_VALUE);

    /* Both aliases must expose the freshly written instructions: the writable
     * one to prove the store landed, the executable one to prove the two
     * MAP_SHARED VMAs really share the memfd page cache.  Verifying the bytes
     * before entering the page keeps a non-shared alias a FAIL line rather than
     * a crash on garbage instructions. */
    unsigned char expected_code[JIT_CODE_BYTES];
    emit_mov_eax_ret(expected_code, MEMFD_ALIAS_VALUE);
    int mismatch = first_mismatched_code_byte(rw_alias, expected_code);
    if (mismatch >= 0) {
        return fail_value("memfd-rw-alias-visible-bytes",
                          "rw-alias-observes-bytes-it-just-wrote",
                          expected_code[mismatch], rw_alias[mismatch], errno);
    }
    mismatch = first_mismatched_code_byte(rx_alias, expected_code);
    if (mismatch >= 0) {
        return fail_value("memfd-rx-alias-visible-bytes",
                          "rx-alias-observes-bytes-written-through-rw-alias",
                          expected_code[mismatch], rx_alias[mismatch], errno);
    }

    errno = 0;
    const uint32_t first_actual = call_jit_code(rx_alias);
    const int first_errno = errno;
    if (first_actual != MEMFD_ALIAS_VALUE) {
        return fail_value("memfd-alias-exec",
                          "rx-alias-executes-constant-written-through-rw-alias",
                          MEMFD_ALIAS_VALUE, first_actual, first_errno);
    }

    /* Rewrite through the writable alias only: the executable alias must see
     * the new bytes without any remap or mprotect on its side, which is what
     * QEMU relies on for every translated block it emits after the first. */
    emit_mov_eax_ret(rw_alias, MEMFD_ALIAS_REWRITE_VALUE);
    emit_mov_eax_ret(expected_code, MEMFD_ALIAS_REWRITE_VALUE);
    mismatch = first_mismatched_code_byte(rx_alias, expected_code);
    if (mismatch >= 0) {
        return fail_value(
            "memfd-rx-alias-rewrite-visible-bytes",
            "rx-alias-observes-later-write-through-rw-alias",
            expected_code[mismatch], rx_alias[mismatch], errno);
    }

    errno = 0;
    const uint32_t second_actual = call_jit_code(rx_alias);
    const int second_errno = errno;
    if (second_actual != MEMFD_ALIAS_REWRITE_VALUE) {
        return fail_value(
            "memfd-alias-rewrite-exec",
            "rx-alias-observes-later-write-through-rw-alias",
            MEMFD_ALIAS_REWRITE_VALUE, second_actual, second_errno);
    }

    puts("THEKERNEL_JIT_MEM_MEMFD_WX_ALIAS_OK");

    errno = 0;
    if (munmap(rw_alias, page_size) != 0) {
        const int saved_errno = errno;
        return fail_errno("memfd-unmap-rw-alias",
                          "munmap-writable-alias-returns-0", saved_errno);
    }
    errno = 0;
    if (munmap(rx_alias, page_size) != 0) {
        const int saved_errno = errno;
        return fail_errno("memfd-unmap-rx-alias",
                          "munmap-executable-alias-returns-0", saved_errno);
    }
    errno = 0;
    if (close(fd) != 0) {
        const int saved_errno = errno;
        return fail_errno("memfd-close", "close(memfd)-returns-0", saved_errno);
    }

    return EXIT_SUCCESS;
}

/* Step 6: informational memfd flag surface -- never fatal.
 *
 * Records what this kernel does with MFD_EXEC and MFD_NOEXEC_SEAL, including
 * the errno when it rejects them.  The reviewed QEMU 10.2.2 helper does not
 * request MFD_EXEC, so a rejection here must not fail the probe; the
 * differential run only needs the observation to know whether an executable
 * memfd is the default or needs an explicit flag. */
static void report_memfd_flag_surface(void) {
    errno = 0;
    const int exec_fd =
        (int)syscall(SYS_memfd_create, "thekernel-jit-mem-exec",
                     MFD_CLOEXEC | MFD_EXEC);
    const int exec_errno = errno;
    printf("THEKERNEL_JIT_MEM_MEMFD_EXEC result=%d errno=%d\n", exec_fd,
           exec_errno);

    errno = 0;
    const int noexec_seal_fd =
        (int)syscall(SYS_memfd_create, "thekernel-jit-mem-noexec-seal",
                     MFD_CLOEXEC | MFD_NOEXEC_SEAL);
    const int noexec_seal_errno = errno;
    printf("THEKERNEL_JIT_MEM_MEMFD_NOEXEC_SEAL result=%d errno=%d\n",
           noexec_seal_fd, noexec_seal_errno);

    printf("THEKERNEL_JIT_MEM_MEMFD_FLAGS exec_result=%d exec_errno=%d "
           "noexec_seal_result=%d noexec_seal_errno=%d\n",
           exec_fd, exec_errno, noexec_seal_fd, noexec_seal_errno);

    /* The fds are only a by-product of the flag observation; the probe exits
     * immediately afterwards, so their teardown is not part of the contract. */
    if (exec_fd >= 0) {
        (void)close(exec_fd);
    }
    if (noexec_seal_fd >= 0) {
        (void)close(noexec_seal_fd);
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    const long configured_page_size = sysconf(_SC_PAGESIZE);
    if (configured_page_size <= 0) {
        return fail_errno("page-size", "sysconf(_SC_PAGESIZE)-greater-than-0",
                          errno);
    }
    const size_t page_size = (size_t)configured_page_size;

    /* Steps 1-5 are REQUIRED: each one is a semantic precondition for running
     * a JIT (and QEMU) in this address space. */
    unsigned char *first_region = NULL;
    unsigned char *second_region = NULL;
    if (probe_anon_rx(&first_region, page_size) != EXIT_SUCCESS ||
        probe_repeat_transition(first_region, page_size) != EXIT_SUCCESS ||
        probe_second_generation(&second_region, first_region, page_size) !=
            EXIT_SUCCESS ||
        probe_teardown(first_region, second_region, page_size) !=
            EXIT_SUCCESS ||
        probe_memfd_split_wx(page_size) != EXIT_SUCCESS) {
        return EXIT_FAILURE;
    }

    /* Step 6 is informational only and cannot fail the probe. */
    report_memfd_flag_surface();

    puts("THEKERNEL_JIT_MEM_OK");
    return EXIT_SUCCESS;
}
