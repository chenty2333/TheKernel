#define _GNU_SOURCE

#if !defined(__x86_64__)
#error "the rseq differential requires the x86_64 Linux ABI"
#endif

/*
 * Differential coverage for the rseq(2) registration contract.
 *
 * Every assertion below is a rule the Linux v7.2.3 sources state
 * unconditionally, so it must hold on both TheKernel and the reference
 * kernel:
 *
 *   kernel/rseq.c:547-562  SYSCALL_DEFINE4(rseq, ...)
 *                 - RSEQ_FLAG_UNREGISTER takes the unregister path first
 *                   (:549-550), any other bit outside RSEQ_FLAGS_SUPPORTED
 *                   (`RSEQ_FLAG_SLICE_EXT_DEFAULT_ON`, :542) is -EINVAL
 *                   (:552-553), an already-registered thread goes to
 *                   rseq_reregister() (:555-556), and only then is the
 *                   length/alignment contract checked (:558-559)
 *   kernel/rseq.c:520-540  rseq_length_valid()
 *                 - 32 bytes keeps the original 32-byte alignment, anything
 *                   longer needs rseq_alloc_align() (64 here,
 *                   include/linux/rseq.h:170-173) and at least
 *                   offsetof(struct rseq, end) == 33
 *                   (include/uapi/linux/rseq.h:207)
 *   kernel/rseq.c:415-481  rseq_register()
 *                 - registration resets every kernel-owned field, and a
 *                   stale rseq_cs is cleared without ever being read
 *                   (:441-461)
 *   kernel/rseq.c:490-504  rseq_unregister()
 *                 - the pointer is compared before the length and the
 *                   signature, and a thread with no registration is -EINVAL
 *   kernel/rseq.c:506-517  rseq_reregister()
 *                 - same area is -EBUSY, same area with a different
 *                   signature is -EPERM, anything else is -EINVAL
 *   include/linux/rseq.h:145-160 rseq_fork()
 *                 - a non-CLONE_VM child inherits the registration, a
 *                   CLONE_VM child starts unregistered
 *
 * What is deliberately NOT asserted here: actually entering and restarting
 * an rseq critical section.  That needs a vDSO/ABI negotiation and an
 * abort-on-preemption observation that a busybox guest cannot make
 * deterministic, so this program stops at the registration contract.
 */

#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* glibc >= 2.35 registers an rseq area for every thread unless the
 * `glibc.pthread.rseq` tunable is cleared, so `main()` would otherwise start
 * with the thread already registered and every observation below would
 * describe libc's area instead of the syscall.  tests/guest/system-init.c:
 * 651-674 already re-executes the repository's rseq guest test with the
 * tunable disabled; do the same, then verify the re-execution really observed
 * an unregistered thread (the first registration in `main` must succeed). */
static void disable_glibc_rseq(const char *program) {
    if (getenv("THEKERNEL_RSEQ_DIFFERENTIAL_REEXEC") != NULL) {
        return;
    }
    setenv("GLIBC_TUNABLES", "glibc.pthread.rseq=0", 1);
    setenv("THEKERNEL_RSEQ_DIFFERENTIAL_REEXEC", "1", 1);
    if (program != NULL && program[0] == '/') {
        execl(program, program, (char *)NULL);
    }
    execl("/proc/self/exe", "rseq-differential", (char *)NULL);
    fprintf(stderr, "THEKERNEL_RSEQ_DIFFERENTIAL_FAIL rseq-reexec errno=%d\n",
            errno);
    _exit(1);
}

#ifndef SYS_rseq
#define SYS_rseq 334
#endif

/* Original `struct rseq` size and alignment (include/uapi/linux/rseq.h:102-207,
 * ORIG_RSEQ_SIZE in kernel/rseq.c:414). */
#define RSEQ_AREA_SIZE 32U
#define RSEQ_AREA_ALIGN 32U
/* Linux 7.2.3 extended registration: offsetof(struct rseq, end) and
 * rseq_alloc_align() = 1 << get_count_order(33) = 64. */
#define RSEQ_EXTENDED_MIN 33U
#define RSEQ_EXTENDED_ALIGN 64U
#define RSEQ_FLAG_UNREGISTER 1U
#define RSEQ_FLAG_SLICE_EXT_DEFAULT_ON 2U
#define RSEQ_CPU_ID_UNINITIALIZED UINT32_MAX
#define RSEQ_CPU_ID_REGISTRATION_FAILED (UINT32_MAX - 1U)
#define RSEQ_POISON UINT32_C(0xa5a5a5a5)
#define RSEQ_SIG UINT32_C(0x53053053)
#define RSEQ_OVERSIZED_LENGTH (1U << 20)

struct rseq_area {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t node_id;
    uint32_t mm_cid;
    uint32_t pad;
};

_Static_assert(sizeof(struct rseq_area) == RSEQ_AREA_SIZE,
               "rseq area must keep the original 32-byte size");

static const char *active;

static void begin(const char *name) {
    active = name;
    printf("THEKERNEL_ABI_CASE %s\n", name);
}
static void check(int ok, const char *name) {
    if (!ok) {
        fprintf(stderr, "THEKERNEL_RSEQ_DIFFERENTIAL_FAIL %s %s errno=%d\n",
                active, name, errno);
        exit(1);
    }
}
static void mark(const char *name) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
}
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
#define ERROR(call, err, name)                                                 \
    do {                                                                       \
        errno = 0;                                                             \
        long rc = (call);                                                      \
        check(rc == -1 && errno == (err), name);                               \
    } while (0)

static long rseq_call(void *area, uint32_t length, uint32_t flags,
                      uint32_t signature) {
    return syscall(SYS_rseq, area, length, flags, signature);
}

/* The 64-byte aligned and the 32-byte aligned (but not 64-byte aligned)
 * windows inside one mapping, plus the shared registration fingerprint. */
static unsigned char *aligned64;
static unsigned char *aligned32;

static void *clone_vm_probe(void *unused) {
    (void)unused;
    /* rseq_fork() resets the state for a CLONE_VM clone
     * (include/linux/rseq.h:145-160), so this thread owns no registration and
     * every unregister form is -EINVAL (kernel/rseq.c:490-504). */
    long rc = rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER,
                        RSEQ_SIG);
    return rc == -1 && errno == EINVAL ? (void *)1 : (void *)0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    disable_glibc_rseq(argc > 0 ? argv[0] : NULL);
    begin("rseq.raw-differential");

    void *mapping = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(mapping != MAP_FAILED, "rseq-mmap");
    /* mmap() returns a page-aligned address, so +64 is 64-byte aligned and
     * +32 is 32-byte aligned only. */
    aligned64 = (unsigned char *)mapping + RSEQ_EXTENDED_ALIGN;
    aligned32 = (unsigned char *)mapping + RSEQ_AREA_ALIGN;
    memset(mapping, 0, 2 * 4096);

    /* Flag admission (kernel/rseq.c:542-556). */
    ERROR(rseq_call(aligned64, RSEQ_AREA_SIZE, 4U, RSEQ_SIG), EINVAL,
          "rseq-flag-unknown");
    ERROR(rseq_call(aligned64, RSEQ_AREA_SIZE, 0x80000000U, RSEQ_SIG), EINVAL,
          "rseq-flag-high");
    /* The unregister path rejects every bit but its own (:490-492). */
    ERROR(rseq_call(aligned64, RSEQ_AREA_SIZE,
                    RSEQ_FLAG_UNREGISTER | RSEQ_FLAG_SLICE_EXT_DEFAULT_ON,
                    RSEQ_SIG),
          EINVAL, "rseq-flag-unregister-mixed");
    /* `RSEQ_FLAG_SLICE_EXT_DEFAULT_ON` is admitted even though this kernel
     * configures no slice extension (CONFIG_RSEQ_SLICE_EXTENSION is off), and
     * the request is then simply not advertised back through rseq.flags. */
    check(rseq_call(aligned64, RSEQ_AREA_SIZE, RSEQ_FLAG_SLICE_EXT_DEFAULT_ON,
                    RSEQ_SIG) == 0,
          "rseq-flag-slice-admitted");
    check(((volatile struct rseq_area *)aligned64)->flags == 0,
          "rseq-flag-slice-not-advertised");
    check(rseq_call(aligned64, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG) == 0,
          "rseq-flag-slice-unregister");
    mark("FLAG_ADMISSION");

    /* Length and alignment (kernel/rseq.c:520-540). */
    ERROR(rseq_call(aligned64, 0, 0, RSEQ_SIG), EINVAL, "rseq-length-zero");
    ERROR(rseq_call(aligned64, RSEQ_AREA_SIZE - 1U, 0, RSEQ_SIG), EINVAL,
          "rseq-length-short");
    ERROR(rseq_call(aligned32, RSEQ_EXTENDED_MIN, 0, RSEQ_SIG), EINVAL,
          "rseq-length-33-half-aligned");
    ERROR(rseq_call(aligned32, RSEQ_EXTENDED_ALIGN, 0, RSEQ_SIG), EINVAL,
          "rseq-length-64-half-aligned");
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, 0, RSEQ_SIG) == 0,
          "rseq-length-32-aligned32");
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG) == 0,
          "rseq-length-32-unregister");
    check(rseq_call(aligned64, RSEQ_EXTENDED_MIN, 0, RSEQ_SIG) == 0,
          "rseq-length-33-aligned64");
    check(rseq_call(aligned64, RSEQ_EXTENDED_MIN, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG) == 0,
          "rseq-length-33-unregister");
    check(rseq_call(aligned64, RSEQ_EXTENDED_ALIGN, 0, RSEQ_SIG) == 0,
          "rseq-length-64-aligned64");
    check(rseq_call(aligned64, RSEQ_EXTENDED_ALIGN, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG) == 0,
          "rseq-length-64-unregister");
    /* Only size and alignment are validated.  The declared length may be far
     * larger than the mapped area because registration writes the first 33
     * bytes and `access_ok()` (kernel/rseq.c:421) is a task-size check, not a
     * mapping check. */
    check(rseq_call(aligned64, RSEQ_OVERSIZED_LENGTH, 0, RSEQ_SIG) == 0,
          "rseq-length-oversized");
    check(rseq_call(aligned64, RSEQ_OVERSIZED_LENGTH, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG) == 0,
          "rseq-length-oversized-unregister");
    mark("LENGTH_AND_ALIGNMENT");

    /* Registration resets the kernel-owned fields, including a stale rseq_cs
     * it never reads (kernel/rseq.c:441-461). */
    memset(aligned32, 0xa5, RSEQ_AREA_SIZE);
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, 0, RSEQ_SIG) == 0,
          "rseq-initialize-register");
    volatile struct rseq_area *published =
        (volatile struct rseq_area *)aligned32;
    check(published->rseq_cs == 0, "rseq-initialize-cs-cleared");
    check(published->flags == 0, "rseq-initialize-flags");
    check(published->cpu_id_start == published->cpu_id,
          "rseq-initialize-cpu-ids-agree");
    check(published->cpu_id_start != RSEQ_POISON, "rseq-initialize-cpu-written");
    check(published->cpu_id != RSEQ_CPU_ID_UNINITIALIZED,
          "rseq-initialize-cpu-initialized");
    check(published->cpu_id != RSEQ_CPU_ID_REGISTRATION_FAILED,
          "rseq-initialize-cpu-not-failed");
    check(published->node_id != RSEQ_POISON, "rseq-initialize-node-written");
    check(published->mm_cid != RSEQ_POISON, "rseq-initialize-cid-written");
    mark("REGISTRATION_INITIALIZES_AREA");

    /* Re-registration verdicts (kernel/rseq.c:506-517, reached from :555). */
    ERROR(rseq_call(aligned32, RSEQ_AREA_SIZE, 0, RSEQ_SIG), EBUSY,
          "rseq-reregister-same");
    ERROR(rseq_call(aligned32, RSEQ_AREA_SIZE, 0, RSEQ_SIG ^ 1U), EPERM,
          "rseq-reregister-signature");
    ERROR(rseq_call(aligned64, RSEQ_AREA_SIZE, 0, RSEQ_SIG), EINVAL,
          "rseq-reregister-address");
    ERROR(rseq_call(aligned32, RSEQ_EXTENDED_ALIGN, 0, RSEQ_SIG), EINVAL,
          "rseq-reregister-length");
    mark("REREGISTER_VERDICTS");

    /* Unregister matching (kernel/rseq.c:490-504): the pointer is compared
     * before the length, and the signature last. */
    ERROR(rseq_call(aligned64, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG),
          EINVAL, "rseq-unregister-address");
    ERROR(rseq_call(aligned32, RSEQ_EXTENDED_ALIGN, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG),
          EINVAL, "rseq-unregister-length");
    ERROR(rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG ^ 1U),
          EPERM, "rseq-unregister-signature");
    mark("UNREGISTER_MATCHES_REGISTRATION");

    /* A successful unregister writes exactly the four reset fields
     * (kernel/rseq.c:390-405). */
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG) ==
              0,
          "rseq-unregister-success");
    check(published->cpu_id_start == 0, "rseq-reset-cpu-id-start");
    check(published->cpu_id == RSEQ_CPU_ID_UNINITIALIZED, "rseq-reset-cpu-id");
    check(published->node_id == 0, "rseq-reset-node-id");
    check(published->mm_cid == 0, "rseq-reset-mm-cid");
    mark("UNREGISTER_RESETS_FIELDS");

    /* Every unregister form on a thread that owns no registration is -EINVAL
     * (`!current->rseq.usrptr`, kernel/rseq.c:494-495). */
    ERROR(rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG),
          EINVAL, "rseq-unregistered-same-area");
    ERROR(rseq_call(aligned64, RSEQ_EXTENDED_ALIGN, RSEQ_FLAG_UNREGISTER,
                    RSEQ_SIG),
          EINVAL, "rseq-unregistered-other-area");
    ERROR(rseq_call(NULL, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG),
          EINVAL, "rseq-unregistered-null-area");
    mark("UNREGISTER_NOT_REGISTERED");

    /* fork() inherits the registration; the child can unregister with the
     * parent's exact fingerprint (include/linux/rseq.h:145-160). */
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, 0, RSEQ_SIG) == 0,
          "rseq-fork-register");
    pid_t child = fork();
    if (child == 0) {
        long rc = rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER,
                            RSEQ_SIG);
        _exit(rc == 0 ? 0 : 1);
    }
    check(child > 0, "rseq-fork");
    int status = -1;
    check(waitpid(child, &status, 0) == child, "rseq-fork-wait");
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "rseq-fork-child");
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG) ==
              0,
          "rseq-fork-parent-unregister");
    mark("FORK_INHERITS_REGISTRATION");

    /* A CLONE_VM clone starts with no registration. */
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, 0, RSEQ_SIG) == 0,
          "rseq-clone-register");
    pthread_t thread;
    check(pthread_create(&thread, NULL, clone_vm_probe, NULL) == 0,
          "rseq-clone-create");
    void *probe = NULL;
    check(pthread_join(thread, &probe) == 0, "rseq-clone-join");
    check(probe == (void *)1, "rseq-clone-unregistered");
    check(rseq_call(aligned32, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG) ==
              0,
          "rseq-clone-parent-unregister");
    mark("CLONE_VM_CLEARS_REGISTRATION");

    check(munmap(mapping, 2 * 4096) == 0, "rseq-munmap");
    done();
    puts("THEKERNEL_RSEQ_DIFFERENTIAL_OK");
    return 0;
}
