/*
 * Differential coverage for the process/task-control ABI cells: prctl(2),
 * arch_prctl(2), iopl(2), setpgid(2), capset(2), clone3(2) and move_pages(2).
 *
 * Every assertion below must hold on both the reference Linux 7.2.3 guest and
 * TheKernel, so it sticks to errno precedence and byte-level results that no
 * configuration or hardware capability can change: no absolute PR_GET_AUXV
 * size (it depends on CONFIG_IA32_EMULATION), no speculation-control or TSC
 * state, and no XCOMP/CPUID mask values.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_prctl
#define SYS_prctl 157
#endif
#ifndef SYS_arch_prctl
#define SYS_arch_prctl 158
#endif
#ifndef SYS_iopl
#define SYS_iopl 172
#endif
#ifndef SYS_setpgid
#define SYS_setpgid 109
#endif
#ifndef SYS_capget
#define SYS_capget 125
#endif
#ifndef SYS_capset
#define SYS_capset 126
#endif
#ifndef SYS_move_pages
#define SYS_move_pages 279
#endif
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef SYS_modify_ldt
#define SYS_modify_ldt 154
#endif

/* prctl options that older libc headers do not carry yet. */
#ifndef PR_GET_TIMING
#define PR_GET_TIMING 13
#endif
#ifndef PR_SET_TIMING
#define PR_SET_TIMING 14
#endif
#ifndef PR_GET_NAME
#define PR_GET_NAME 16
#endif
#ifndef PR_SET_NAME
#define PR_SET_NAME 15
#endif
#ifndef PR_GET_AUXV
#define PR_GET_AUXV 0x41555856
#endif
#ifndef PR_GET_CFI
#define PR_GET_CFI 80
#endif
#ifndef PR_SET_CFI
#define PR_SET_CFI 81
#endif
#ifndef PR_CFI_BRANCH_LANDING_PADS
#define PR_CFI_BRANCH_LANDING_PADS 0
#endif
#ifndef PR_TIMING_STATISTICAL
#define PR_TIMING_STATISTICAL 0
#endif
#ifndef PR_TIMER_CREATE_RESTORE_IDS
#define PR_TIMER_CREATE_RESTORE_IDS 77
#define PR_TIMER_CREATE_RESTORE_IDS_OFF 0
#define PR_TIMER_CREATE_RESTORE_IDS_ON 1
#define PR_TIMER_CREATE_RESTORE_IDS_GET 2
#endif

/* arch_prctl subcodes (asm/prctl.h). */
#define ARCH_SET_GS 0x1001
#define ARCH_SET_FS 0x1002
#define ARCH_GET_FS 0x1003
#define ARCH_GET_GS 0x1004

/* clone3(2) values. CLONE_NEWTIME sits in the low byte, which clone(2)
 * spends on the exit signal, so only clone3 can express it. */
#define CLONE_NEWTIME 0x00000080UL
#define CLONE_THREAD 0x00010000UL
#define CLONE_SIGHAND 0x00000800UL
#define CLONE_VM 0x00000100UL
#define CLONE_PARENT 0x00008000UL
#define CLONE_AUTOREAP (1UL << 34)

#define CAP_VERSION_3 0x20080522U
#define NR_MOVE_PAGES 279
#define BAD ((void *)(uintptr_t)1)

/* Above TASK_SIZE_MAX for both 4-level and 5-level paging, so the kernel has
 * to refuse the base without needing a probe of the CPU's paging mode. */
#define NON_USER_BASE 0x8000000000000000UL

static const char *active;
static int failures;

static void check(int ok, const char *stage) {
    if (!ok) {
        fprintf(stderr, "THEKERNEL_TASK_CONTROL_FAIL %s %s errno=%d (%s)\n",
                active, stage, errno, strerror(errno));
        failures++;
    }
}

#define ERROR(call, expected, stage) do { errno = 0; long r_ = (call); \
    check(r_ == -1 && errno == (expected), (stage)); } while (0)

static void begin(const char *name) {
    active = name;
    printf("THEKERNEL_ABI_CASE %s\n", name);
}

static void mark(const char *name) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
}

static void done(void) {
    fflush(stdout);
    printf("THEKERNEL_ABI_RESULT %s pass\n", active);
}

/* The bytes `/proc/<pid>/stat` prints between its parentheses, or -1 when the
 * file cannot be read.  Linux formats that field in `do_task_stat()`
 * (`fs/proc/array.c`):
 *
 *	seq_puts(m, " (");
 *	proc_task_name(m, task, false);
 *	seq_puts(m, ") ");
 *
 * and with `escape = false` `proc_task_name()` ends in
 * `seq_printf(m, "%.64s", tcomm)`, so the field is the raw `task_struct::comm`
 * image: bytes that do not form UTF-8 are neither replaced nor dropped. */
static long stat_comm(const char *path, unsigned char *out, size_t capacity) {
    char buffer[512];
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (count <= 0) {
        return -1;
    }
    buffer[count] = '\0';
    const char *open_paren = strchr(buffer, '(');
    const char *close_paren = strrchr(buffer, ')');
    if (open_paren == NULL || close_paren == NULL || close_paren <= open_paren) {
        return -1;
    }
    size_t length = (size_t)(close_paren - open_paren - 1);
    if (length > capacity) {
        return -1;
    }
    memcpy(out, open_paren + 1, length);
    return (long)length;
}

/* Linux's task comm is a raw 16-byte array, so PR_SET_NAME must truncate at
 * fifteen bytes, keep bytes that are not valid UTF-8, and never fail on a
 * long name; PR_GET_NAME copies all sixteen bytes. */
static void prctl_name_case(void) {
    begin("prctl-name.raw-differential");
    char name[32];
    unsigned char got[16];
    static const unsigned char expected[16] = {
        'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h',
        'i', 'j', 'k', 'l', 'm', 'n', 'o', 0,
    };
    static const unsigned char raw[16] = {
        0x80, 0xff, 0x41, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    };

    memset(name, 0, sizeof(name));
    memcpy(name, "abcdefghijklmnopqrst", 21);
    check(syscall(SYS_prctl, PR_SET_NAME, name, 0, 0, 0) == 0, "set-name");
    memset(got, 0xaa, sizeof(got));
    check(syscall(SYS_prctl, PR_GET_NAME, got, 0, 0, 0) == 0, "get-name");
    check(memcmp(got, expected, sizeof(got)) == 0, "name-truncated-at-fifteen-bytes");

    /* Bytes above 0x7f survive in both directions. */
    memset(name, 0, sizeof(name));
    name[0] = (char)0x80;
    name[1] = (char)0xff;
    name[2] = 'A';
    check(syscall(SYS_prctl, PR_SET_NAME, name, 0, 0, 0) == 0, "set-raw-name");
    memset(got, 0xaa, sizeof(got));
    check(syscall(SYS_prctl, PR_GET_NAME, got, 0, 0, 0) == 0, "get-raw-name");
    check(memcmp(got, raw, sizeof(got)) == 0, "name-keeps-non-utf8-bytes");

    ERROR(syscall(SYS_prctl, PR_GET_NAME, (void *)0, 0, 0, 0), EFAULT, "get-name-null");
    ERROR(syscall(SYS_prctl, PR_SET_NAME, (void *)0, 0, 0, 0), EFAULT, "set-name-null");
    mark("NAME_BYTES");

    /* A fifteen-byte name whose final byte starts a UTF-8 sequence is a legal
     * `comm`: PR_SET_NAME copies bytes, not characters.  `/proc/<pid>/stat`
     * does not escape the field, so it must come back as exactly those fifteen
     * bytes.  Rendering the name through a lossy UTF-8 conversion first would
     * instead produce fourteen 'a' bytes followed by U+FFFD, which is three
     * bytes long -- more than the fifteen that fit in the field. */
    static const unsigned char partial[16] = {
        'a', 'a', 'a', 'a', 'a', 'a', 'a', 'a',
        'a', 'a', 'a', 'a', 'a', 'a', 0xc3, 0,
    };
    unsigned char comm[64];
    memset(name, 0, sizeof(name));
    memcpy(name, partial, 15);
    check(syscall(SYS_prctl, PR_SET_NAME, name, 0, 0, 0) == 0, "set-partial-utf8-name");
    memset(comm, 0xaa, sizeof(comm));
    long comm_length = stat_comm("/proc/self/stat", comm, sizeof(comm));
    check(comm_length == 15, "stat-comm-length");
    check(comm_length == 15 && memcmp(comm, partial, 15) == 0,
          "stat-comm-keeps-partial-utf8-bytes");
    mark("STAT_COMM_RAW_BYTES");
    done();
}

/* PR_GET_TIMING takes no arguments and never validates them; PR_SET_TIMING
 * accepts only the statistical mode, which is the only mode Linux has. */
static void prctl_timing_case(void) {
    begin("prctl-timing.raw-differential");
    check(syscall(SYS_prctl, PR_GET_TIMING, 0xdeadUL, 0xbeefUL, 1UL, 2UL) ==
              PR_TIMING_STATISTICAL, "get-timing-ignores-arguments");
    ERROR(syscall(SYS_prctl, PR_SET_TIMING, 1, 0, 0, 0), EINVAL, "set-timing-nonzero");
    check(syscall(SYS_prctl, PR_SET_TIMING, PR_TIMING_STATISTICAL, 0, 0, 0) == 0,
          "set-timing-statistical");
    ERROR(syscall(SYS_prctl, 0x7fffffff, 0, 0, 0, 0), EINVAL, "unknown-option");
    mark("TIMING");
    done();
}

/* PR_GET_AUXV reports the size of the saved auxiliary vector rather than the
 * number of bytes it copied, so a zero-length request is a size probe and a
 * short buffer truncates the copy without failing. The absolute size depends
 * on CONFIG_IA32_EMULATION, so only these invariants are asserted. */
static void prctl_auxv_case(void) {
    unsigned char small[8];
    unsigned char large[256];
    long full;

    begin("prctl-auxv.raw-differential");
    memset(small, 0, sizeof(small));
    memset(large, 0, sizeof(large));
    full = syscall(SYS_prctl, PR_GET_AUXV, small, sizeof(small), 0, 0);
    check(full > 0 && full % 8 == 0, "reports-full-size");
    check(syscall(SYS_prctl, PR_GET_AUXV, (void *)0, 0, 0, 0) == full, "size-probe");
    check(syscall(SYS_prctl, PR_GET_AUXV, large, sizeof(large), 0, 0) == full,
          "same-count-for-large-buffer");
    check(memcmp(small, large, sizeof(small)) == 0, "short-copy-is-a-prefix");
    check(syscall(SYS_prctl, PR_GET_AUXV, large, 0, 0, 0) == full, "zero-length-still-reports");
    ERROR(syscall(SYS_prctl, PR_GET_AUXV, small, sizeof(small), 1, 0), EINVAL, "arg4-rejected");
    ERROR(syscall(SYS_prctl, PR_GET_AUXV, small, sizeof(small), 0, 1), EINVAL, "arg5-rejected");
    ERROR(syscall(SYS_prctl, PR_GET_AUXV, (void *)0, sizeof(small), 0, 0), EFAULT,
          "bad-pointer");
    mark("AUXV");
    done();
}

/* The CRIU timer-restore mode is a per-thread-group bit that only
 * timer_create(2) consumes: OFF and ON report success, GET reports the bit
 * itself rather than a copy of the request, and the tail arguments are
 * rejected before the command is examined. */
static void prctl_timer_restore_ids_case(void) {
    begin("prctl-timer-restore-ids.raw-differential");
    check(syscall(SYS_prctl, PR_TIMER_CREATE_RESTORE_IDS, PR_TIMER_CREATE_RESTORE_IDS_GET, 0,
                  0, 0) == 0, "get-default-off");
    ERROR(syscall(SYS_prctl, PR_TIMER_CREATE_RESTORE_IDS, PR_TIMER_CREATE_RESTORE_IDS_GET, 1, 0,
                  0), EINVAL, "get-rejects-arg3");
    check(syscall(SYS_prctl, PR_TIMER_CREATE_RESTORE_IDS, PR_TIMER_CREATE_RESTORE_IDS_ON, 0, 0,
                  0) == 0, "set-on");
    check(syscall(SYS_prctl, PR_TIMER_CREATE_RESTORE_IDS, PR_TIMER_CREATE_RESTORE_IDS_GET, 0, 0,
                  0) == 1, "get-reports-bit");
    check(syscall(SYS_prctl, PR_TIMER_CREATE_RESTORE_IDS, PR_TIMER_CREATE_RESTORE_IDS_OFF, 0, 0,
                  0) == 0, "set-off");
    check(syscall(SYS_prctl, PR_TIMER_CREATE_RESTORE_IDS, PR_TIMER_CREATE_RESTORE_IDS_GET, 0, 0,
                  0) == 0, "get-after-off");
    ERROR(syscall(SYS_prctl, PR_TIMER_CREATE_RESTORE_IDS, 3, 0, 0, 0), EINVAL, "unknown-command");
    mark("RESTORE_IDS");
    done();
}

/* The x86 control-flow-integrity hooks are weak and return -EINVAL, so every
 * PR_GET_CFI/PR_SET_CFI request fails the same way: an unknown state selector
 * is rejected before the arch hook, and a known one reaches a hook that
 * cannot report a state. */
static void prctl_cfi_case(void) {
    begin("prctl-cfi.raw-differential");
    unsigned long state = 0;
    ERROR(syscall(SYS_prctl, PR_GET_CFI, PR_CFI_BRANCH_LANDING_PADS, &state, 0, 0),
          EINVAL, "get-cfi-weak-hook");
    ERROR(syscall(SYS_prctl, PR_GET_CFI, PR_CFI_BRANCH_LANDING_PADS + 1, &state, 0, 0),
          EINVAL, "get-cfi-unknown-selector");
    ERROR(syscall(SYS_prctl, PR_GET_CFI, PR_CFI_BRANCH_LANDING_PADS, &state, 1, 0),
          EINVAL, "get-cfi-nonzero-arg4");
    ERROR(syscall(SYS_prctl, PR_SET_CFI, PR_CFI_BRANCH_LANDING_PADS, 0, 0, 0),
          EINVAL, "set-cfi-weak-hook");
    ERROR(syscall(SYS_prctl, PR_SET_CFI, PR_CFI_BRANCH_LANDING_PADS, 0, 0, 1),
          EINVAL, "set-cfi-nonzero-arg5");
    mark("CFI");
    done();
}

/* arch_prctl rejects a segment base at or above TASK_SIZE_MAX before it
 * touches the descriptor, so the loaded base is unchanged afterwards. */
static void arch_prctl_case(void) {
    begin("arch_prctl.raw-differential");
    unsigned long before = 0;
    unsigned long after = 0;
    check(syscall(SYS_arch_prctl, ARCH_GET_FS, &before) == 0, "get-fs");
    ERROR(syscall(SYS_arch_prctl, ARCH_SET_FS, NON_USER_BASE), EPERM, "set-fs-high");
    ERROR(syscall(SYS_arch_prctl, ARCH_SET_GS, NON_USER_BASE), EPERM, "set-gs-high");
    check(syscall(SYS_arch_prctl, ARCH_GET_FS, &after) == 0 && after == before,
          "rejected-base-not-applied");
    check(syscall(SYS_arch_prctl, ARCH_GET_GS, &after) == 0, "get-gs");
    ERROR(syscall(SYS_arch_prctl, ARCH_GET_FS, (void *)0), EFAULT, "get-fs-null");
    ERROR(syscall(SYS_arch_prctl, 0x7fff, 0), EINVAL, "unknown-arch-code");
    mark("SEGMENT_BASE_EPERM");
    done();
}

/* Only levels above 3 are invalid, and lowering the level needs no
 * capability: level 0 is always already in effect for a fresh process. */
static void iopl_case(void) {
    begin("iopl.raw-differential");
    ERROR(syscall(SYS_iopl, 4), EINVAL, "level-above-three");
    check(syscall(SYS_iopl, 0) == 0, "lower-to-zero-without-capability");
    errno = 0;
    long raised = syscall(SYS_iopl, 3);
    check(raised == 0 || (raised == -1 && errno == EPERM), "raise-or-eperm");
    check(syscall(SYS_iopl, 0) == 0, "lower-after-raise");
    check(syscall(SYS_iopl, 3) == raised, "raise-is-stable");
    (void)syscall(SYS_iopl, 0);
    mark("LEVEL_AND_LOWERING");
    done();
}

struct cap_header_wire {
    uint32_t version;
    int32_t pid;
};

struct cap_data_wire {
    uint32_t effective;
    uint32_t permitted;
    uint32_t inheritable;
};

/* capset(2) may only affect the caller, and Linux decides that from the
 * header pid before it copies the data array: a foreign pid reports EPERM
 * even with an unreadable data pointer, while an accepted pid copies and
 * reports EFAULT. An unknown version is rewritten to the current one. */
static void capset_case(void) {
    begin("capset.raw-differential");
    struct cap_header_wire header;
    struct cap_data_wire data[2];

    memset(data, 0, sizeof(data));
    header.version = CAP_VERSION_3;
    header.pid = 0;
    check(syscall(SYS_capget, &header, data) == 0, "capget");
    check(syscall(SYS_capset, &header, data) == 0, "capset-own-pid-zero");
    header.pid = (int32_t)getpid();
    check(syscall(SYS_capset, &header, data) == 0, "capset-visible-own-pid");

    header.pid = (int32_t)getpid() + 4242;
    ERROR(syscall(SYS_capset, &header, (void *)0), EPERM, "foreign-pid-before-copy");
    header.pid = 0;
    ERROR(syscall(SYS_capset, &header, (void *)0), EFAULT, "copy-for-own-pid");

    header.version = 0xdeadbeefU;
    ERROR(syscall(SYS_capset, &header, data), EINVAL, "unknown-version");
    check(header.version == CAP_VERSION_3, "version-rewritten");
    mark("PID_BEFORE_COPY");
    done();
}

/* Linux resolves and authorises the target before it looks at nr_pages, then
 * accepts an empty request without touching the page arrays. */
static void move_pages_case(void) {
    begin("move_pages.raw-differential");
    check(syscall(NR_MOVE_PAGES, getpid(), 0, (void *)0, (void *)0, (void *)0, 0) == 0,
          "empty-request");
    check(syscall(NR_MOVE_PAGES, getpid(), 0, BAD, BAD, BAD, 0) == 0,
          "empty-request-ignores-arrays");
    ERROR(syscall(NR_MOVE_PAGES, getpid(), 0, (void *)0, (void *)0, (void *)0, ~0UL),
          EINVAL, "invalid-flags");
    mark("EMPTY_REQUEST_AND_FLAGS");
    done();
}

/* setpgid(2) is complete here: a child may join its own group, its parent may
 * move it into a group before exec, and after exec the parent's request is
 * refused with EACCES and must leave the group untouched. */
static int wait_child(pid_t child, int *status, const char *stage) {
    pid_t got = waitpid(child, status, 0);
    check(got == child, stage);
    return got == child ? 0 : 1;
}

static void setpgid_case(void) {
    begin("setpgid.raw-differential");
    int ready[2];
    int status = 0;
    pid_t child;

    check(pipe(ready) == 0, "pipe");
    child = fork();
    if (child == 0) {
        (void)close(ready[0]);
        if (setpgid(0, 0) != 0) _exit(1);
        if (getpgid(0) != getpid()) _exit(2);
        ssize_t wrote = write(ready[1], "x", 1);
        _exit(wrote == 1 ? 0 : 3);
    }
    check(child > 0, "fork");
    (void)close(ready[1]);
    char byte = 0;
    check(read(ready[0], &byte, 1) == 1, "child-group-ready");
    (void)close(ready[0]);
    check(getpgid((pid_t)child) == (pid_t)child, "child-joined-own-group");
    if (wait_child((pid_t)child, &status, "wait-child-group") != 0) {
        return;
    }
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child-group-status");
    mark("CHILD_OWN_GROUP");

    check(pipe(ready) == 0, "exec-pipe");
    child = fork();
    if (child == 0) {
        char fd[16];
        (void)close(ready[0]);
        snprintf(fd, sizeof(fd), "%d", ready[1]);
        execl("/proc/self/exe", "task-control-differential", "--setpgid-stage", fd,
              (char *)NULL);
        _exit(1);
    }
    check(child > 0, "exec-fork");
    (void)close(ready[1]);
    check(read(ready[0], &byte, 1) == 1, "exec-committed");
    (void)close(ready[0]);
    ERROR(syscall(SYS_setpgid, (pid_t)child, (pid_t)child), EACCES, "after-exec-eacces");
    check(getpgid((pid_t)child) == getpgrp(), "group-unchanged-after-eacces");
    check(kill((pid_t)child, SIGKILL) == 0, "kill-exec-child");
    if (wait_child((pid_t)child, &status, "wait-exec-child") != 0) {
        return;
    }
    check(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL, "exec-child-killed");
    mark("EXEC_EACCES");
    done();
}

struct user_desc_wire {
    uint32_t entry_number;
    uint32_t base_addr;
    uint32_t limit;
    uint32_t flags;
};

/* modify_ldt(2) is the one x86-64 system call whose wrapper ends in
 * `return (unsigned int)ret;`, so a failure reaches user space zero-extended
 * in the low 32 bits of %rax. libc reads that word as a successful positive
 * result: it leaves errno untouched and returns the value, and a caller that
 * tests `ret == -1` never notices the failure. */
#define MLDT_ERROR(call, expected, stage) do { errno = 0; long r_ = (call); \
    check(r_ == (long)(unsigned int)(-(expected)) && errno == 0, (stage)); } while (0)

static long modify_ldt_desc(int func, uint32_t entry, uint32_t base, uint32_t limit,
                            uint32_t flags) {
    struct user_desc_wire desc;
    memset(&desc, 0, sizeof(desc));
    desc.entry_number = entry;
    desc.base_addr = base;
    desc.limit = limit;
    desc.flags = flags;
    errno = 0;
    return syscall(SYS_modify_ldt, func, &desc, sizeof(desc));
}

/* A fresh process has no LDT, so reading one reports an empty table without
 * touching the destination; the default LDT is 128 zero bytes and the count is
 * clamped; a stored descriptor is the fill_ldt() encoding of the request, with
 * CONFIG_X86_16BIT deciding that seg_32bit clear is stored rather than
 * rejected. */
static void modify_ldt_case(void) {
    static const unsigned char sixteen_bit[8] = {
        0xff, 0xff, 0x00, 0x10, 0x00, 0xf1, 0x00, 0x00,
    };
    static const unsigned char absent_code[8] = {
        0x21, 0x43, 0x00, 0x30, 0x00, 0x7f, 0x00, 0x00,
    };
    unsigned char data[256];
    int nonzero = 0;
    unsigned i;

    begin("modify_ldt.raw-differential");
    check(syscall(SYS_modify_ldt, 0, (void *)0, 16) == 0, "read-without-ldt");

    memset(data, 0xaa, sizeof(data));
    check(syscall(SYS_modify_ldt, 2, data, sizeof(data)) == 128, "default-ldt-count");
    for (i = 0; i < 128; i++) {
        if (data[i] != 0) nonzero = 1;
    }
    check(nonzero == 0, "default-ldt-zero-filled");
    check(data[128] == 0xaa, "default-ldt-clamped-to-128");
    MLDT_ERROR(syscall(SYS_modify_ldt, 2, (void *)0, 16), EFAULT, "default-ldt-null");
    mark("DEFAULT_LDT");

    MLDT_ERROR(syscall(SYS_modify_ldt, 3, data, sizeof(data)), ENOSYS, "unknown-function");
    MLDT_ERROR(syscall(SYS_modify_ldt, 0x12, data, sizeof(data)), ENOSYS, "no-function-0x12");
    MLDT_ERROR(syscall(SYS_modify_ldt, 1, data, 8), EINVAL, "short-descriptor");

    /* contents = 0, read_exec_only = 1: a present 16-bit data segment.
     * seg_32bit is clear, which the old mode stores with DB clear. */
    check(modify_ldt_desc(1, 1, 0x1000, 0xffff, 1u << 3) == 0, "old-mode-16bit-data");
    memset(data, 0xaa, sizeof(data));
    check(syscall(SYS_modify_ldt, 0, data, sizeof(data)) == 256, "read-back-count");
    check(memcmp(data + 8, sixteen_bit, sizeof(sixteen_bit)) == 0,
          "sixteen-bit-entry-encoding");

    /* contents = 3 is a code segment: the old mode rejects it outright, and
     * the new mode needs it marked not-present. */
    check(modify_ldt_desc(0x11, 2, 0x2000, 0x1234, 3u << 1) ==
              (long)(unsigned int)-EINVAL, "contents-three-needs-absent");
    check(modify_ldt_desc(0x11, 3, 0x3000, 0x4321, (3u << 1) | (1u << 5)) == 0,
          "new-mode-absent-code");
    memset(data, 0xaa, sizeof(data));
    check(syscall(SYS_modify_ldt, 0, data, 64) == 64, "read-back-short-count");
    check(memcmp(data + 24, absent_code, sizeof(absent_code)) == 0,
          "absent-code-entry-encoding");
    MLDT_ERROR(modify_ldt_desc(1, 8192, 0x1000, 0xffff, 1u << 3), EINVAL,
               "entry-out-of-range");
    mark("WRITE_RULES");
    done();
}

struct clone_args_wire {
    uint64_t flags;
    uint64_t pidfd;
    uint64_t child_tid;
    uint64_t parent_tid;
    uint64_t exit_signal;
    uint64_t stack;
    uint64_t stack_size;
    uint64_t tls;
    uint64_t set_tid;
    uint64_t set_tid_size;
    uint64_t cgroup;
};

static long clone3_call(struct clone_args_wire *args, unsigned long size) {
    errno = 0;
    return syscall(SYS_clone3, args, size);
}

/* clone3 refuses sizes below the known prefix, an untruncated exit signal in
 * the flag word, deprecated CLONE_DETACHED, and the autoreap combinations
 * that copy_process() rejects. CLONE_NEWTIME is a flag clone3 alone can
 * express, and a child created with it is an ordinary waitable child. */
static void clone3_case(void) {
    begin("clone3.raw-differential");
    struct clone_args_wire args;
    int status = 0;

    memset(&args, 0, sizeof(args));
    args.flags = CLONE_THREAD;
    check(clone3_call(&args, 0) == -1 && errno == EINVAL, "zero-size");
    check(clone3_call(&args, 63) == -1 && errno == EINVAL, "undersized");
    check(clone3_call(&args, sizeof(args)) == -1 && errno == EINVAL, "thread-needs-vm-and-sighand");

    args.flags = 1;
    check(clone3_call(&args, sizeof(args)) == -1 && errno == EINVAL, "exit-signal-in-flags");
    args.flags = 0x00400000UL;
    check(clone3_call(&args, sizeof(args)) == -1 && errno == EINVAL, "clone-detached");

    args.flags = CLONE_AUTOREAP | CLONE_THREAD | CLONE_VM | CLONE_SIGHAND;
    check(clone3_call(&args, sizeof(args)) == -1 && errno == EINVAL, "autoreap-needs-no-thread");
    args.flags = CLONE_AUTOREAP;
    args.exit_signal = 17;
    check(clone3_call(&args, sizeof(args)) == -1 && errno == EINVAL, "autoreap-needs-no-signal");
    args.flags = CLONE_AUTOREAP | CLONE_PARENT;
    args.exit_signal = 0;
    check(clone3_call(&args, sizeof(args)) == -1 && errno == EINVAL, "autoreap-needs-no-parent");
    mark("FLAG_ADMISSION");

    args.flags = CLONE_NEWTIME;
    args.exit_signal = 17;
    fflush(stdout);
    long child = clone3_call(&args, sizeof(args));
    if (child == 0) {
        _exit(0);
    }
    if (child < 0) {
        /* A guest without CAP_SYS_ADMIN cannot create a time namespace. Both
         * kernels must then refuse the request as EPERM, not as EINVAL: the
         * flag itself is admitted either way. */
        check(errno == EPERM, "newtime-without-capability");
    } else {
        check(waitpid((pid_t)child, &status, 0) == (pid_t)child && WIFEXITED(status) &&
                  WEXITSTATUS(status) == 0, "newtime-child-exits");
    }
    mark("NEWTIME");

    /* CLONE_AUTOREAP children are reaped as they exit: nothing is left for
     * wait(2), so the parent sees ECHILD rather than a status. */
    args.flags = CLONE_AUTOREAP;
    args.exit_signal = 0;
    fflush(stdout);
    child = clone3_call(&args, sizeof(args));
    if (child == 0) {
        _exit(0);
    }
    check(child > 0, "autoreap-child");
    if (child > 0) {
        errno = 0;
        check(waitpid((pid_t)child, &status, 0) == -1 && errno == ECHILD,
              "autoreap-child-not-waitable");
    }
    mark("AUTOREAP");
    done();
}

static int setpgid_stage(int fd) {
    if (fd < 0) return 1;
    ssize_t wrote = write(fd, "x", 1);
    if (wrote != 1) return 1;
    if (close(fd) != 0) return 1;
    /* Exec has now committed, so any later setpgid(2) from the parent has to
     * report EACCES; stay alive until the parent decides to kill us. */
    for (;;) {
        (void)pause();
    }
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "--setpgid-stage") == 0) {
        char *end = NULL;
        long fd = strtol(argv[2], &end, 10);
        if (end == NULL || *end != '\0' || fd < 0 || fd > INT32_MAX) return 1;
        return setpgid_stage((int)fd);
    }

    prctl_name_case();
    prctl_timing_case();
    prctl_auxv_case();
    prctl_timer_restore_ids_case();
    prctl_cfi_case();
    arch_prctl_case();
    iopl_case();
    capset_case();
    move_pages_case();
    modify_ldt_case();
    setpgid_case();
    clone3_case();

    if (failures != 0) {
        fprintf(stderr, "THEKERNEL_TASK_CONTROL_FAILURES %d\n", failures);
        return 1;
    }
    puts("THEKERNEL_TASK_CONTROL_OK");
    return 0;
}
