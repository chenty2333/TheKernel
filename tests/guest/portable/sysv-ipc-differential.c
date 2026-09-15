/*
 * Differential coverage for the SysV IPC identifier, errno-order and
 * SEM_UNDO contracts.
 *
 * The harness runs this program in a TheKernel guest and in a Linux v7.2.3
 * guest and requires the printed records to match, so every assertion below
 * must describe behaviour that is identical on both kernels.  Kernel-specific
 * behaviour is deliberately absent and named as such in the case that would
 * otherwise have covered it:
 *
 *   - a *new* key created with SHM_HUGETLB must succeed on Linux and fails on
 *     TheKernel, which has no huge-page backing, so only the existing-key path
 *     (which never reaches newseg() on either kernel) is asserted;
 *   - the `next_id` files under /proc/sys/kernel and the MSG_COPY flag are
 *     gated on CONFIG_CHECKPOINT_RESTORE in Linux, which the oracle kernel
 *     disables, so neither the requested-identifier path nor MSG_COPY's
 *     argument order is asserted here;
 *   - the number of cyclic wraps needed to observe a sequence bump depends on
 *     how many objects other guests processes hold, so the identifier cases
 *     assert reuse *progression* instead of a fixed sequence value.
 *
 * The identifier cases are the regression cover for the flat-identifier bug:
 * the published msqid/semid/shmid must carry a sequence, an RMID must advance
 * the cyclic cursor instead of freeing the identifier for immediate reuse, and
 * a retired identifier must stay invalid in the index space.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/capability.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/msg.h>
#include <sys/resource.h>
#include <sched.h>
#include <sys/sem.h>
#include <sys/shm.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

enum {
    PAGE_BYTES = 4096,
    /* Linux's `ipc_min_cycle` keeps the identifier window at least 64 wide, so
     * 200 create/remove rounds can never hand out one identifier twice. */
    ROUNDS = 200,
    /* Native x86_64 `semtimedop`, used directly so the probe does not depend
     * on the guest libc exporting the wrapper. */
    NR_SEMTIMEDOP = 220,
    /* An unknown `sem_flg` bit, plus the two defined ones. */
    SEM_FLAG_UNKNOWN = 0x2000,
};

/* A key used by only one case, so the probe can never observe another case's
 * object. */
#define KEY_SEMGET_BOUNDS 0x5eed0001

#define BAD ((void *)(uintptr_t)1)
/*
 * The first bit above Linux's default `IPCMNI_IDX_MASK` of 0x7fff.  The index
 * occupies bits 0-14 and a sequence occupies the bits above it, so this bit
 * must be ignored by every command that takes an index.  (A kernel booted with
 * `ipcmni_extend` widens the mask to 24 bits and would treat it as part of the
 * index; the oracle and TheKernel both run the default layout.)
 */
#define ABOVE_INDEX_MASK 0x8000

/*
 * glibc does not define `union semun`; the man page asks the caller for it.
 * `ptr` carries the IPC_INFO/SEM_INFO argument, which is neither a semid_ds
 * nor a value array.
 */
union semun {
    int val;
    struct semid_ds *buf;
    unsigned short *array;
    void *ptr;
};

static const char *active;

static void begin(const char *name) {
    active = name;
    printf("THEKERNEL_ABI_CASE %s\n", active);
}

static void mark(const char *name) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
}

static void done(void) {
    printf("THEKERNEL_ABI_RESULT %s pass\n", active);
}

static void fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_SYSVIPC_FAIL %s %s errno=%d (%s)\n", active,
            stage, errno, strerror(errno));
    exit(1);
}

static void check(int ok, const char *stage) {
    if (!ok) {
        fail(stage);
    }
}

/* Runs `call` and requires success. */
static long ok_call(long result, const char *stage) {
    if (result == -1) {
        fail(stage);
    }
    return result;
}

/* Runs `call` and requires exactly `expected` in errno. */
static void errno_call(long result, int expected, const char *stage) {
    if (result != -1 || errno != expected) {
        fail(stage);
    }
}

static void reap_child(pid_t child, const char *stage) {
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        fail(stage);
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "THEKERNEL_SYSVIPC_CHILD %s status=%d exit=%d\n", stage,
                status, WIFEXITED(status) ? WEXITSTATUS(status) : -1);
        fail(stage);
    }
}

/*
 * Reads a `/proc/sys` file into `buf`, which is NUL-terminated, and returns
 * its length or -1.  Every whitespace run in the result is collapsed to a
 * single space so a kernel that separates columns with tabs compares equal to
 * one that uses spaces.
 */
static long read_sysctl(const char *path, char *buf, size_t size) {
    int fd = open(path, O_RDONLY);
    long total = 0;
    if (fd < 0) {
        return -1;
    }
    while (total < (long)size - 1) {
        ssize_t got = read(fd, buf + total, (size_t)((long)size - 1 - total));
        if (got < 0) {
            int saved = errno;
            close(fd);
            errno = saved;
            return -1;
        }
        if (got == 0) {
            break;
        }
        total += got;
    }
    close(fd);
    buf[total] = '\0';
    char *read_ptr = buf;
    char *write_ptr = buf;
    int pending = 0;
    while (*read_ptr != '\0') {
        if (*read_ptr == ' ' || *read_ptr == '\t' || *read_ptr == '\n' ||
            *read_ptr == '\r') {
            pending = write_ptr != buf;
        } else {
            if (pending) {
                *write_ptr++ = ' ';
            }
            pending = 0;
            *write_ptr++ = *read_ptr;
        }
        read_ptr++;
    }
    *write_ptr = '\0';
    return (long)(write_ptr - buf);
}

/* Writes `value` to a `/proc/sys` file; -1 with errno on failure. */
static int write_sysctl(const char *path, const char *value) {
    int fd = open(path, O_WRONLY);
    ssize_t written;
    int saved;
    if (fd < 0) {
        return -1;
    }
    written = write(fd, value, strlen(value));
    saved = errno;
    close(fd);
    errno = saved;
    return written == (ssize_t)strlen(value) ? 0 : -1;
}

/* Requires a `/proc/sys` write to fail with exactly `expected`. */
static void errno_sysctl(const char *path, const char *value, int expected,
                         const char *stage) {
    errno = 0;
    if (write_sysctl(path, value) != -1 || errno != expected) {
        fail(stage);
    }
}

/*
 * Case: sysvipc-ids.reuse-progression
 *
 * Linux `ipc_idr_alloc()` advances the cyclic identifier cursor on every
 * allocation, so removing an object never makes its identifier available
 * again: the next allocation either takes the next index or wraps the window
 * and increments the sequence.  A flat identifier table that reuses the freed
 * identifier immediately fails both the "not the identifier just removed"
 * check and the "every identifier in the run is distinct" check.
 */
static void case_identifier_progression(void) {
    static int seen[ROUNDS];
    int removed = -1;

    begin("sysvipc-ids.reuse-progression");
    for (int round = 0; round < ROUNDS; round++) {
        errno = 0;
        int msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "msgget");
        check(msqid >= 0, "msgget-range");

        if (removed >= 0) {
            check(msqid != removed, "identifier-immediate-reuse");
            /* The retired identifier keeps naming nothing even though the
             * allocation above may have taken its index back with a new
             * sequence. */
            struct msqid_ds ds;
            memset(&ds, 0, sizeof(ds));
            errno = 0;
            errno_call(msgctl(removed, IPC_STAT, &ds), EINVAL,
                       "retired-identifier-invalid");
        }
        for (int back = 0; back < round; back++) {
            check(msqid != seen[back], "identifier-repeat");
        }

        errno = 0;
        ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");
        seen[round] = msqid;
        removed = msqid;
    }
    mark("IDENTIFIER_NEVER_REUSED_IMMEDIATELY");
    mark("RETIRED_IDENTIFIER_STAYS_INVALID");
    mark("NO_IDENTIFIER_REPEAT_WITHIN_CYCLE");
    done();
}

/*
 * Case: sysvipc-stat.index-resolution
 *
 * MSG_STAT, SEM_STAT and SHM_STAT take an *index*: Linux `ipc_obtain_object_idr()`
 * masks with `IPCMNI_IDX_MASK`, resolves the object without comparing the
 * sequence, and returns the object's full identifier so a caller walking the
 * index space learns the sequence it must use next.  The IPC_STAT variants
 * take the full identifier instead and return zero.
 */
static void case_stat_index_resolution(void) {
    struct msqid_ds mds;
    struct semid_ds sds;
    struct shmid_ds hds;
    union semun arg;

    begin("sysvipc-stat.index-resolution");

    int msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "msgget");
    int msq_index = msqid & 0x7fff;
    memset(&mds, 0, sizeof(mds));
    errno = 0;
    check(msgctl(msq_index, MSG_STAT, &mds) == msqid, "msg-stat-full-id");
    errno = 0;
    check(msgctl(ABOVE_INDEX_MASK | msq_index, MSG_STAT, &mds) == msqid,
          "msg-stat-index-masked");
    errno = 0;
    check(msgctl(ABOVE_INDEX_MASK | msq_index, MSG_STAT_ANY, &mds) == msqid,
          "msg-stat-any-index-masked");
    errno = 0;
    check(msgctl(msqid, IPC_STAT, &mds) == 0, "msg-ipc-stat-zero");

    int semid = (int)ok_call(semget(IPC_PRIVATE, 2, IPC_CREAT | 0600), "semget");
    int sem_index = semid & 0x7fff;
    memset(&sds, 0, sizeof(sds));
    memset(&arg, 0, sizeof(arg));
    arg.buf = &sds;
    errno = 0;
    check(semctl(sem_index, 0, SEM_STAT, arg) == semid, "sem-stat-full-id");
    errno = 0;
    check(semctl(ABOVE_INDEX_MASK | sem_index, 0, SEM_STAT_ANY, arg) == semid,
          "sem-stat-any-index-masked");
    errno = 0;
    check(semctl(semid, 0, IPC_STAT, arg) == 0, "sem-ipc-stat-zero");

    int shmid = (int)ok_call(shmget(IPC_PRIVATE, PAGE_BYTES, IPC_CREAT | 0600),
                             "shmget");
    int shm_index = shmid & 0x7fff;
    memset(&hds, 0, sizeof(hds));
    errno = 0;
    check(shmctl(shm_index, SHM_STAT, &hds) == shmid, "shm-stat-full-id");
    errno = 0;
    check(shmctl(ABOVE_INDEX_MASK | shm_index, SHM_STAT_ANY, &hds) == shmid,
          "shm-stat-any-index-masked");
    errno = 0;
    check(shmctl(shmid, IPC_STAT, &hds) == 0, "shm-ipc-stat-zero");

    errno = 0;
    ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");
    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, arg), "semctl-rmid");
    errno = 0;
    ok_call(shmctl(shmid, IPC_RMID, NULL), "shmctl-rmid");

    mark("STAT_RETURNS_FULL_IDENTIFIER");
    mark("STAT_MASKS_INDEX_BITS");
    mark("IPC_STAT_RETURNS_ZERO");
    done();
}

/*
 * Case: sysvipc-info.max-index
 *
 * IPC_INFO and the per-family INFO command both answer with the highest index
 * in use (`ipc_get_maxidx()`, clamped to 0 for an empty table), so the two
 * return values are equal and neither can be lower than the index of an object
 * this program created.
 */
static void case_info_max_index(void) {
    struct msginfo minfo;
    struct seminfo sinfo;
    struct shminfo hinfo;

    begin("sysvipc-info.max-index");

    int msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "msgget");
    int semid = (int)ok_call(semget(IPC_PRIVATE, 1, IPC_CREAT | 0600), "semget");
    int shmid = (int)ok_call(shmget(IPC_PRIVATE, PAGE_BYTES, IPC_CREAT | 0600),
                             "shmget");

    memset(&minfo, 0, sizeof(minfo));
    errno = 0;
    long msg_ipc = ok_call(msgctl(0, IPC_INFO, (struct msqid_ds *)&minfo),
                           "msg-ipc-info");
    errno = 0;
    long msg_info = ok_call(msgctl(0, MSG_INFO, (struct msqid_ds *)&minfo),
                            "msg-info");
    check(msg_ipc == msg_info, "msg-info-max-index-equal");
    check(msg_ipc >= (msqid & 0x7fff), "msg-info-covers-live-index");

    union semun info_arg;
    memset(&sinfo, 0, sizeof(sinfo));
    memset(&info_arg, 0, sizeof(info_arg));
    info_arg.ptr = &sinfo;
    errno = 0;
    long sem_ipc = ok_call(semctl(0, 0, IPC_INFO, info_arg), "sem-ipc-info");
    errno = 0;
    long sem_info = ok_call(semctl(0, 0, SEM_INFO, info_arg), "sem-info");
    check(sem_ipc == sem_info, "sem-info-max-index-equal");
    check(sem_ipc >= (semid & 0x7fff), "sem-info-covers-live-index");

    memset(&hinfo, 0, sizeof(hinfo));
    errno = 0;
    long shm_ipc = ok_call(shmctl(0, IPC_INFO, (struct shmid_ds *)&hinfo),
                           "shm-ipc-info");
    errno = 0;
    long shm_info = ok_call(shmctl(0, SHM_INFO, (struct shmid_ds *)&hinfo),
                            "shm-info");
    check(shm_ipc == shm_info, "shm-info-max-index-equal");
    check(shm_ipc >= (shmid & 0x7fff), "shm-info-covers-live-index");

    errno = 0;
    ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");
    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");
    errno = 0;
    ok_call(shmctl(shmid, IPC_RMID, NULL), "shmctl-rmid");

    mark("INFO_AND_FAMILY_INFO_AGREE");
    mark("MAX_INDEX_COVERS_LIVE_OBJECT");
    done();
}

/*
 * Case: sysvipc-sem.flags
 *
 * `sem_flg` is a bit set, not an enumeration: Linux `perform_atomic_semop()`
 * tests SEM_UNDO and IPC_NOWAIT and ignores every other bit.  `sem_op == 0`
 * with SEM_UNDO is an ordinary wait-for-zero that also records an adjustment,
 * and when it cannot proceed it reports EAGAIN because the *blocking*
 * operation carries IPC_NOWAIT.
 */
static void case_sem_flags(void) {
    struct sembuf op;

    begin("sysvipc-sem.flags");

    int semid = (int)ok_call(semget(IPC_PRIVATE, 1, IPC_CREAT | 0600), "semget");
    errno = 0;
    ok_call(semctl(semid, 0, SETVAL, 5), "setval-five");

    op.sem_num = 0;
    op.sem_op = -1;
    op.sem_flg = (short)(SEM_UNDO | IPC_NOWAIT | SEM_FLAG_UNKNOWN);
    errno = 0;
    ok_call(semop(semid, &op, 1), "unknown-flag-accepted");
    errno = 0;
    check(semctl(semid, 0, GETVAL) == 4, "value-after-unknown-flag-op");

    op.sem_op = 0;
    op.sem_flg = (short)(SEM_UNDO | IPC_NOWAIT);
    errno = 0;
    errno_call(semop(semid, &op, 1), EAGAIN, "wait-zero-eagain");

    errno = 0;
    ok_call(semctl(semid, 0, SETVAL, 0), "setval-zero");
    errno = 0;
    ok_call(semop(semid, &op, 1), "wait-zero-with-undo");
    errno = 0;
    check(semctl(semid, 0, GETVAL) == 0, "value-still-zero");

    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");

    mark("UNKNOWN_FLAG_BITS_IGNORED");
    mark("WAIT_ZERO_WITH_UNDO_SUCCEEDS");
    mark("WAIT_ZERO_WITH_UNDO_EAGAIN");
    done();
}

/*
 * Case: sysvipc-sem.undo-range
 *
 * Linux `perform_atomic_semop()` rejects an operation whose resulting
 * `semadj` would leave `[-SEMAEM - 1, SEMAEM]` with ERANGE instead of clamping
 * the adjustment, and rejects it before the semaphore values are committed.
 * The sequence below keeps the semaphore inside `[0, SEMVMX]` at every step so
 * that the ERANGE can only come from the undo range, never from the value
 * range.
 */
static void case_sem_undo_range(void) {
    struct sembuf op;

    begin("sysvipc-sem-undo.undo-range");

    int semid = (int)ok_call(semget(IPC_PRIVATE, 1, IPC_CREAT | 0600), "semget");
    errno = 0;
    ok_call(semctl(semid, 0, SETVAL, 0), "setval-zero");

    /* semval 32767, semadj -32767: the lowest admissible adjustment. */
    op.sem_num = 0;
    op.sem_op = 32767;
    op.sem_flg = SEM_UNDO;
    errno = 0;
    ok_call(semop(semid, &op, 1), "undo-ceiling");
    /* semval 0, semadj unchanged: a plain operation moves the value without
     * touching the adjustment. */
    op.sem_op = -32767;
    op.sem_flg = 0;
    errno = 0;
    ok_call(semop(semid, &op, 1), "value-only-drain");
    /* semval 1, semadj -32768: the lowest value the range still admits. */
    op.sem_op = 1;
    op.sem_flg = SEM_UNDO;
    errno = 0;
    ok_call(semop(semid, &op, 1), "undo-floor");
    errno = 0;
    check(semctl(semid, 0, GETVAL) == 1, "value-before-erange");

    /* semval would be 2, which is admissible; semadj would be -32769, which is
     * not. */
    errno = 0;
    errno_call(semop(semid, &op, 1), ERANGE, "undo-range-erange");
    errno = 0;
    check(semctl(semid, 0, GETVAL) == 1, "value-unchanged-after-erange");

    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");

    mark("UNDO_RANGE_REPORTS_ERANGE");
    mark("ERANGE_LEAVES_VALUE_UNCHANGED");
    done();
}

/*
 * Case: sysvipc-control.ipc64
 *
 * Linux `ksys_semctl()`, `ksys_msgctl()` and `ksys_shmctl()` switch on the raw
 * command word.  IPC_64 is handled by the syscall entry (and only for the
 * 32-bit compat layouts), so an application that sets the bit itself - as
 * 32-bit binaries did - reaches the default arm and gets EINVAL.  Stripping
 * IPC_64 inside the dispatcher turns that into a silent success.
 */
static void case_ipc64_command(void) {
    struct msqid_ds mds;
    struct semid_ds sds;
    struct shmid_ds hds;
    union semun arg;

    begin("sysvipc-control.ipc64");

    int msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "msgget");
    int semid = (int)ok_call(semget(IPC_PRIVATE, 1, IPC_CREAT | 0600), "semget");
    int shmid = (int)ok_call(shmget(IPC_PRIVATE, PAGE_BYTES, IPC_CREAT | 0600),
                             "shmget");

    memset(&mds, 0, sizeof(mds));
    memset(&sds, 0, sizeof(sds));
    memset(&hds, 0, sizeof(hds));
    memset(&arg, 0, sizeof(arg));
    arg.buf = &sds;

    errno = 0;
    errno_call(msgctl(msqid, IPC_STAT | 0x100, &mds), EINVAL,
               "msgctl-ipc64-einval");
    errno = 0;
    errno_call(semctl(semid, 0, IPC_STAT | 0x100, arg), EINVAL,
               "semctl-ipc64-einval");
    errno = 0;
    errno_call(shmctl(shmid, IPC_STAT | 0x100, &hds), EINVAL,
               "shmctl-ipc64-einval");

    /* The unadorned commands must still work. */
    errno = 0;
    ok_call(msgctl(msqid, IPC_STAT, &mds), "msgctl-ipc-stat");
    errno = 0;
    ok_call(semctl(semid, 0, IPC_STAT, arg), "semctl-ipc-stat");
    errno = 0;
    ok_call(shmctl(shmid, IPC_STAT, &hds), "shmctl-ipc-stat");

    errno = 0;
    ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");
    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");
    errno = 0;
    ok_call(shmctl(shmid, IPC_RMID, NULL), "shmctl-rmid");

    mark("IPC64_COMMAND_EINVAL");
    mark("PLAIN_COMMAND_STILL_WORKS");
    done();
}

/*
 * Case: sysvipc-shm.lock-memlock
 *
 * Linux `shmctl_do_lock()` refuses SHM_LOCK with EPERM for a caller that is
 * neither privileged nor the segment's owner/creator once its RLIMIT_MEMLOCK
 * is zero.  The child drops CAP_IPC_LOCK and lowers the limit, so the run is
 * identical whether the harness starts this program as root or as a plain
 * user; the parent's own limit is left alone.
 */
static void case_shm_lock_memlock(void) {
    struct shmid_ds ds;

    begin("sysvipc-shm-lock.lock-memlock");

    int shmid = (int)ok_call(shmget(IPC_PRIVATE, PAGE_BYTES, IPC_CREAT | 0600),
                             "shmget");

    pid_t child = fork();
    if (child < 0) {
        fail("fork");
    }
    if (child == 0) {
        struct rlimit zero = {0, 0};
        struct __user_cap_header_struct header;
        struct __user_cap_data_struct data[2];

        /* Lowering a limit is always allowed; a zero limit is the state the
         * kernel refuses to lock under. */
        if (setrlimit(RLIMIT_MEMLOCK, &zero) != 0) {
            _exit(2);
        }
        /* Root starts with CAP_IPC_LOCK, which bypasses the limit entirely.
         * Dropping every capability is allowed for a process that holds them
         * and fails harmlessly for one that does not. */
        memset(&header, 0, sizeof(header));
        memset(data, 0, sizeof(data));
        header.version = _LINUX_CAPABILITY_VERSION_3;
        header.pid = 0;
        (void)syscall(SYS_capset, &header, data);

        memset(&ds, 0, sizeof(ds));
        errno = 0;
        if (shmctl(shmid, SHM_LOCK, &ds) != -1) {
            _exit(3);
        }
        if (errno != EPERM) {
            _exit(4);
        }
        _exit(0);
    }
    reap_child(child, "shm-lock-memlock-eperm");

    errno = 0;
    ok_call(shmctl(shmid, IPC_RMID, NULL), "shmctl-rmid");

    mark("SHM_LOCK_ZERO_MEMLOCK_EPERM");
    done();
}

/*
 * Case: sysvipc-shm.hugetlb-existing-key
 *
 * The SHM_HUGETLB validation lives in Linux `newseg()`, which only runs when
 * the key is new.  Repeating a create request for a key that already names a
 * segment therefore resolves the existing segment and never looks at the huge
 * page hint.  (Creating a *new* huge segment is not asserted: Linux backs it,
 * TheKernel cannot, and that difference is a declared limitation.)
 */
static void case_shm_hugetlb_existing_key(void) {
    key_t key = 0x5a5a;

    begin("sysvipc-shm-hugetlb.hugetlb-existing-key");

    int shmid = (int)ok_call(shmget(key, PAGE_BYTES, IPC_CREAT | 0600), "shmget");
    errno = 0;
    long again = shmget(key, PAGE_BYTES, IPC_CREAT | SHM_HUGETLB | 0600);
    check(again == shmid, "existing-key-ignores-hugetlb");

    errno = 0;
    ok_call(shmctl(shmid, IPC_RMID, NULL), "shmctl-rmid");

    mark("EXISTING_KEY_IGNORES_HUGETLB");
    done();
}

/*
 * Case: sysvipc-shm.dest-stat
 *
 * `shmctl(IPC_RMID)` on a segment that still has attachments only marks it
 * SHM_DEST: Linux `do_shm_rmid()` keeps the object in the identifier table
 * until the last detach, so an index walk still finds it and still learns its
 * full identifier.  Once the last attachment goes away the index is empty
 * again and the same command reports EINVAL.
 */
static void case_shm_dest_stat(void) {
    struct shmid_ds ds;

    begin("sysvipc-shm-dest.dest-stat");

    int shmid = (int)ok_call(shmget(IPC_PRIVATE, PAGE_BYTES, IPC_CREAT | 0600),
                             "shmget");
    void *addr = shmat(shmid, NULL, 0);
    check(addr != (void *)-1, "shmat");

    errno = 0;
    ok_call(shmctl(shmid, IPC_RMID, NULL), "shmctl-rmid");

    memset(&ds, 0, sizeof(ds));
    errno = 0;
    check(shmctl(shmid & 0x7fff, SHM_STAT, &ds) == shmid, "dest-stat-full-id");

    errno = 0;
    ok_call(shmdt(addr), "shmdt");
    errno = 0;
    errno_call(shmctl(shmid & 0x7fff, SHM_STAT, &ds), EINVAL,
               "destroyed-index-invalid");

    mark("DEST_SEGMENT_STILL_STATABLE");
    mark("DEST_SEGMENT_RETURNS_FULL_ID");
    mark("DESTROYED_SEGMENT_EINVAL");
    done();
}

/*
 * The out-of-range bound and the permission check, in Linux's order: EFBIG
 * for a `sem_num` outside the set, EACCES for the same operation inside it.
 * Returns 0 on success and a distinct status per failing probe.
 */
static int semop_bound_precedes_permission(int semid) {
    struct sembuf op;

    memset(&op, 0, sizeof(op));
    op.sem_num = 5;
    op.sem_op = -1;
    errno = 0;
    if (semop(semid, &op, 1) != -1 || errno != EFBIG) {
        return 3;
    }
    op.sem_num = 0;
    errno = 0;
    if (semop(semid, &op, 1) != -1 || errno != EACCES) {
        return 4;
    }
    return 0;
}

/*
 * Case: sysvipc-errno.order
 *
 * Argument validation order is the ABI.  Each probe below pairs an invalid
 * argument with a second, *differently* invalid argument and requires the
 * errno Linux produces for the earlier check, so a dispatcher that validates
 * in another order reports the other errno.
 */
static void case_errno_order(void) {
    struct msgbuf {
        long mtype;
        char mtext[16];
    } msg;
    struct msqid_ds mds;
    struct shmid_ds hds;
    struct sembuf op;
    struct timespec timeout;
    union semun arg;

    begin("sysvipc-errno.order");

    int msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "msgget");
    /* Read-only for its owner, so the permission check would report EACCES if
     * it ran before the semaphore-number bounds check. */
    int semid = (int)ok_call(semget(IPC_PRIVATE, 1, IPC_CREAT | 0444), "semget");

    memset(&msg, 0, sizeof(msg));
    msg.mtype = 1;

    /* do_msgsnd() reads mtype before it looks at the size or the identifier. */
    errno = 0;
    errno_call(msgsnd(0x7fff, BAD, 8, 0), EFAULT, "msgsnd-msgp-efault-first");
    /* A negative size is EINVAL before the identifier is resolved. */
    errno = 0;
    errno_call(msgsnd(0x7fff, &msg, (size_t)-1, 0), EINVAL, "msgsnd-size-einval");
    /* A non-positive type is EINVAL before the text is copied. */
    msg.mtype = 0;
    errno = 0;
    errno_call(msgsnd(0x7fff, &msg, 8, 0), EINVAL, "msgsnd-mtype-einval");
    /* With every argument otherwise valid the identifier is what fails. */
    msg.mtype = 1;
    errno = 0;
    errno_call(msgsnd(0x7fff, &msg, 8, 0), EINVAL, "msgsnd-id-einval");
    /* A live queue with an unreadable type is the text copy's EFAULT. */
    errno = 0;
    errno_call(msgsnd(msqid, BAD, 8, 0), EFAULT, "msgsnd-text-efault");

    /* do_msgrcv() reads the buffer size as a *signed* word:
     * `if (msqid < 0 || (long) bufsz < 0) return -EINVAL;`.  A size with the
     * sign bit set is therefore EINVAL, and the message must survive the
     * refusal. */
    memcpy(msg.mtext, "survive!", 8);
    errno = 0;
    ok_call(msgsnd(msqid, &msg, 8, 0), "msgsnd-negative-size-setup");
    errno = 0;
    errno_call(msgrcv(msqid, &msg, (size_t)-1, 0, IPC_NOWAIT), EINVAL,
               "msgrcv-negative-size");
    memset(&msg, 0, sizeof(msg));
    errno = 0;
    check(msgrcv(msqid, &msg, 8, 0, 0) == 8, "msgrcv-negative-size-kept");
    check(memcmp(msg.mtext, "survive!", 8) == 0, "msgrcv-negative-size-data");

    memcpy(msg.mtext, "payload", 8);
    errno = 0;
    ok_call(msgsnd(msqid, &msg, 8, 0), "msgsnd-ok");
    memset(&msg, 0, sizeof(msg));
    errno = 0;
    check(msgrcv(msqid, &msg, 8, 0, 0) == 8, "msgrcv-ok");
    check(msg.mtype == 1 && memcmp(msg.mtext, "payload", 8) == 0, "msgrcv-data");

    /* MSG_COPY is not asserted: with CONFIG_CHECKPOINT_RESTORE disabled Linux
     * answers it with ENOSYS before it looks at the buffer at all, and the
     * oracle kernel has that configuration off. TheKernel's ordering (usercopy
     * before the queue lookup) was checked against a CHECKPOINT_RESTORE=y
     * Linux instead. */

    /* The table-wide commands still reject a negative identifier. */
    memset(&mds, 0, sizeof(mds));
    errno = 0;
    errno_call(msgctl(-1, IPC_INFO, &mds), EINVAL, "msgctl-negative-id-einval");
    errno = 0;
    errno_call(msgctl(-1, MSG_STAT, &mds), EINVAL, "msg-stat-negative-id-einval");

    /* semctl_setval() range-checks the value before it resolves the array. */
    errno = 0;
    errno_call(semctl(0x7fff, 0, SETVAL, 99999), ERANGE,
               "semctl-setval-erange-first");
    /* ... and the semaphore number before the permission check: the array is
     * owned by this process but its mode is read-only. */
    errno = 0;
    errno_call(semctl(semid, 5, SETVAL, 1), EINVAL,
               "semctl-setval-semnum-first");

    /* __do_semtimedop() bounds the vector against the array with EFBIG before
     * ipcperms(), so an out-of-range sem_num wins over the read-only mode.
     * The order is only observable for a caller the mode excludes, so the
     * probe drops to uid 1 when the suite runs as root (CAP_IPC_OWNER would
     * otherwise bypass the mode check and the first probe would succeed). */
    if (geteuid() == 0) {
        pid_t child = fork();
        if (child < 0) {
            fail("fork");
        }
        if (child == 0) {
            if (setresuid(1, 1, 1) != 0) {
                _exit(2);
            }
            _exit(semop_bound_precedes_permission(semid));
        }
        reap_child(child, "semop-efbig-before-eacces");
    } else {
        check(semop_bound_precedes_permission(semid) == 0,
              "semop-efbig-before-eacces");
    }

    /* __do_semtimedop() validates the count, then the timeout, then the
     * operations, and only then resolves the array. */
    memset(&op, 0, sizeof(op));
    timeout.tv_sec = -1;
    timeout.tv_nsec = 0;
    errno = 0;
    errno_call(syscall(NR_SEMTIMEDOP, 0x7fff, &op, 1, &timeout), EINVAL,
               "semtimedop-timeout-einval");
    errno = 0;
    errno_call(syscall(NR_SEMTIMEDOP, semid, &op, 0, NULL), EINVAL,
               "semtimedop-count-einval");

    /* shmctl() rejects a negative identifier before the command. */
    memset(&hds, 0, sizeof(hds));
    errno = 0;
    errno_call(shmctl(-1, IPC_INFO, &hds), EINVAL, "shmctl-negative-id-einval");

    /* The three *ctl families copy the IPC_SET record out of userspace before
     * they resolve the identifier (`ksys_msgctl()`, `ksys_semctl()` and
     * `ksys_shmctl()` all perform the copy ahead of the `*ctl_down()` call), so
     * a faulting buffer is EFAULT even for an identifier that names nothing. */
    errno = 0;
    errno_call(msgctl(0x7fff, IPC_SET, (struct msqid_ds *)BAD), EFAULT,
               "msgctl-set-copy-before-id");
    memset(&arg, 0, sizeof(arg));
    arg.ptr = BAD;
    errno = 0;
    errno_call(semctl(0x7fff, 0, IPC_SET, arg), EFAULT,
               "semctl-set-copy-before-id");
    errno = 0;
    errno_call(shmctl(0x7fff, IPC_SET, (struct shmid_ds *)BAD), EFAULT,
               "shmctl-set-copy-before-id");

    /* __do_semtimedop() rejects `semid < 0` and an invalid timespec only after
     * do_semtimedop() has copied the operation vector in, so a faulting vector
     * is EFAULT in both places. */
    errno = 0;
    errno_call(semop(-1, (struct sembuf *)BAD, 1), EFAULT,
               "semop-vector-before-semid");
    errno = 0;
    errno_call(syscall(NR_SEMTIMEDOP, semid, (struct sembuf *)BAD, 1, &timeout),
               EFAULT, "semtimedop-vector-before-timeout");

    errno = 0;
    ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");
    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");

    mark("SEMOP_EFBIG_BEFORE_EACCES");
    mark("MSGSND_FAULTS_BEFORE_VALIDATION");
    mark("MSGSND_SIZE_AND_TYPE_BEFORE_ID");
    mark("MSGRCV_NEGATIVE_SIZE_EINVAL");
    mark("TABLE_COMMANDS_REJECT_NEGATIVE_ID");
    mark("IPC_SET_COPY_BEFORE_IDENTIFIER");
    mark("SEMOP_VECTOR_COPY_BEFORE_VALIDATION");
    mark("SEMCTL_VALUE_AND_SEMNUM_ORDER");
    mark("SEMTIMEDOP_COUNT_AND_TIMEOUT_ORDER");
    done();
}

/*
 * Case: sysvipc-msg.exclusive-create-order
 *
 * Linux `ipcget_public()` answers IPC_CREAT|IPC_EXCL on an existing key with
 * EEXIST before it consults permissions.  The probe runs as a different uid
 * from the queue's creator, which is the only way to tell the two orders
 * apart: for a caller that already passes the permission check both orders
 * return EEXIST.
 */
static void case_exclusive_create_order(void) {
    key_t key = 0x5b5b;

    begin("sysvipc-msg.exclusive-create-order");

    int msqid = (int)ok_call(msgget(key, IPC_CREAT | 0000), "msgget");

    if (geteuid() == 0) {
        pid_t child = fork();
        if (child < 0) {
            fail("fork");
        }
        if (child == 0) {
            if (setresuid(1, 1, 1) != 0) {
                _exit(2);
            }
            errno = 0;
            if (msgget(key, IPC_CREAT | IPC_EXCL | 0600) != -1) {
                _exit(3);
            }
            _exit(errno == EEXIST ? 0 : 4);
        }
        reap_child(child, "exclusive-create-eexist");
    } else {
        errno = 0;
        errno_call(msgget(key, IPC_CREAT | IPC_EXCL | 0600), EEXIST,
                   "exclusive-create-eexist");
    }

    errno = 0;
    ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");

    mark("EXCLUSIVE_CREATE_BEFORE_PERMISSION");
    done();
}

/*
 * Hands each SysV object this process owns to uid 0 and gid 0 while running as
 * uid 1, then reads the ownership back.  Every step returns a distinct status
 * so a failure names the family and the operation that diverged.
 */
static int owner_change_unheld_id(void) {
    struct msqid_ds mds;
    struct semid_ds sds;
    struct shmid_ds hds;
    union semun arg;

    int msqid = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
    if (msqid < 0) {
        return 1;
    }
    memset(&mds, 0, sizeof(mds));
    if (msgctl(msqid, IPC_STAT, &mds) != 0) {
        return 2;
    }
    mds.msg_perm.uid = 0;
    mds.msg_perm.gid = 0;
    if (msgctl(msqid, IPC_SET, &mds) != 0) {
        return 3;
    }
    memset(&mds, 0, sizeof(mds));
    if (msgctl(msqid, IPC_STAT, &mds) != 0) {
        return 4;
    }
    if (mds.msg_perm.uid != 0 || mds.msg_perm.gid != 0) {
        return 5;
    }
    msgctl(msqid, IPC_RMID, NULL);

    int semid = semget(IPC_PRIVATE, 1, IPC_CREAT | 0600);
    if (semid < 0) {
        return 6;
    }
    memset(&arg, 0, sizeof(arg));
    memset(&sds, 0, sizeof(sds));
    arg.buf = &sds;
    if (semctl(semid, 0, IPC_STAT, arg) != 0) {
        return 7;
    }
    sds.sem_perm.uid = 0;
    sds.sem_perm.gid = 0;
    if (semctl(semid, 0, IPC_SET, arg) != 0) {
        return 8;
    }
    memset(&sds, 0, sizeof(sds));
    if (semctl(semid, 0, IPC_STAT, arg) != 0) {
        return 9;
    }
    if (sds.sem_perm.uid != 0 || sds.sem_perm.gid != 0) {
        return 10;
    }
    semctl(semid, 0, IPC_RMID, arg);

    int shmid = shmget(IPC_PRIVATE, PAGE_BYTES, IPC_CREAT | 0600);
    if (shmid < 0) {
        return 11;
    }
    memset(&hds, 0, sizeof(hds));
    if (shmctl(shmid, IPC_STAT, &hds) != 0) {
        return 12;
    }
    hds.shm_perm.uid = 0;
    hds.shm_perm.gid = 0;
    if (shmctl(shmid, IPC_SET, &hds) != 0) {
        return 13;
    }
    memset(&hds, 0, sizeof(hds));
    if (shmctl(shmid, IPC_STAT, &hds) != 0) {
        return 14;
    }
    if (hds.shm_perm.uid != 0 || hds.shm_perm.gid != 0) {
        return 15;
    }
    shmctl(shmid, IPC_RMID, NULL);
    return 0;
}

/*
 * Case: sysvipc-owner.set
 *
 * `ipc_update_perm()` (ipc/util.c) refuses an IPC_SET only when the requested
 * owner id has no mapping in the caller's user namespace:
 *
 *   kuid_t uid = make_kuid(current_user_ns(), in->uid);
 *   kgid_t gid = make_kgid(current_user_ns(), in->gid);
 *   if (!uid_valid(uid) || !gid_valid(gid))
 *           return -EINVAL;
 *   out->uid = uid;
 *   out->gid = gid;
 *
 * The right to *change* the object is decided earlier, by
 * `ipcctl_obtain_check()`'s owner-or-CAP_SYS_ADMIN test.  A plain owner may
 * therefore hand the object to any representable uid and gid - it needs
 * neither CAP_CHOWN nor the ids it is handing the object to.  The probe must
 * run as an unprivileged uid, because for root every candidate rule agrees.
 */
static void case_owner_change(void) {
    begin("sysvipc-owner.set");

    if (geteuid() == 0) {
        pid_t child = fork();
        if (child < 0) {
            fail("fork");
        }
        if (child == 0) {
            if (setresuid(1, 1, 1) != 0) {
                _exit(20);
            }
            _exit(owner_change_unheld_id());
        }
        reap_child(child, "owner-change-unheld-id");
    } else {
        check(owner_change_unheld_id() == 0, "owner-change-unheld-id");
    }

    mark("OWNER_HANDS_OBJECT_TO_UNHELD_ID");
    done();
}

/*
 * Case: sysvipc-record.layout
 *
 * The control records are the native x86_64 `msqid64_ds`, `semid64_ds` and
 * `shmid64_ds`.  Their trailing `__unused` words are part of the layout, not
 * decoration:
 *
 *   - `include/uapi/asm-generic/msgbuf.h` leaves "2 miscellaneous 32-bit
 *     values" after `msg_lrpid`, so `msqid64_ds` is 120 bytes;
 *   - `arch/x86/include/uapi/asm/sembuf.h` pads after `sem_otime` *and* after
 *     `sem_ctime` on x86_64 only ("x86_64 and x32 incorrectly added padding
 *     here, so the structures are still incompatible with the padding on
 *     x86"), so `semid64_ds` is 104 bytes with `sem_ctime` at 64 and
 *     `sem_nsems` at 80;
 *   - `include/uapi/asm-generic/shmbuf.h` leaves two words after `shm_nattch`,
 *     so `shmid64_ds` is 112 bytes.
 *
 * A kernel that packs a record shifts every later field, which is invisible to
 * a test that only looks at the return value.  Each probe below therefore
 * pre-fills the caller's record with a non-zero pattern and then checks both
 * the field values (read through the caller's own header layout) and that the
 * kernel zeroed the placeholder words it owns.
 */
static void case_record_layout(void) {
    struct msqid_ds mds;
    struct semid_ds sds;
    struct shmid_ds hds;
    union semun arg;
    static const unsigned char zero[16];
    int msqid, semid, shmid;

    begin("sysvipc-record.layout");

    /* The struct sizes the checks below rely on, measured in the guest
     * itself: `sem_nsems` only lands where Linux puts it if the record is 104
     * bytes. */
    check(sizeof(struct msqid_ds) == 120, "sizeof-msqid_ds");
    check(sizeof(struct semid_ds) == 104, "sizeof-semid_ds");
    check(sizeof(struct shmid_ds) == 112, "sizeof-shmid_ds");

    msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "record-msgget");
    memset(&mds, 0xa5, sizeof(mds));
    errno = 0;
    ok_call(msgctl(msqid, IPC_STAT, &mds), "record-msgctl-stat");
    check(mds.msg_perm.mode == 0600, "msqid-ds-mode");
    check(mds.msg_qbytes == 16384, "msqid-ds-qbytes");
    check(mds.msg_qnum == 0 && mds.msg_cbytes == 0, "msqid-ds-empty");
    check(mds.msg_stime == 0 && mds.msg_rtime == 0, "msqid-ds-times");
    check(mds.msg_lspid == 0 && mds.msg_lrpid == 0, "msqid-ds-pids");
    check(memcmp((char *)&mds + 104, zero, sizeof(mds) - 104) == 0,
          "msqid-ds-unused");
    ok_call(msgctl(msqid, IPC_RMID, NULL), "record-msgctl-rmid");

    semid = (int)ok_call(semget(IPC_PRIVATE, 3, IPC_CREAT | 0600),
                         "record-semget");
    memset(&sds, 0xa5, sizeof(sds));
    memset(&arg, 0, sizeof(arg));
    arg.buf = &sds;
    errno = 0;
    ok_call(semctl(semid, 0, IPC_STAT, arg), "record-semctl-stat");
    check(sds.sem_perm.mode == 0600, "semid-ds-mode");
    check(sds.sem_nsems == 3, "semid-ds-nsems");
    check(sds.sem_otime == 0, "semid-ds-otime");
    check(memcmp((char *)&sds + 88, zero, sizeof(sds) - 88) == 0,
          "semid-ds-unused");
    ok_call(semctl(semid, 0, IPC_RMID, arg), "record-semctl-rmid");

    shmid = (int)ok_call(shmget(IPC_PRIVATE, PAGE_BYTES + 1, IPC_CREAT | 0600),
                         "record-shmget");
    memset(&hds, 0xa5, sizeof(hds));
    errno = 0;
    ok_call(shmctl(shmid, IPC_STAT, &hds), "record-shmctl-stat");
    check(hds.shm_perm.mode == 0600, "shmid-ds-mode");
    check(hds.shm_segsz == PAGE_BYTES + 1, "shmid-ds-segsz");
    check(hds.shm_nattch == 0, "shmid-ds-nattch");
    check(hds.shm_atime == 0 && hds.shm_dtime == 0, "shmid-ds-times");
    check(memcmp((char *)&hds + 96, zero, sizeof(hds) - 96) == 0,
          "shmid-ds-unused");
    ok_call(shmctl(shmid, IPC_RMID, NULL), "record-shmctl-rmid");

    mark("MSQID_DS_LAYOUT_MATCHES_LINUX");
    mark("SEMID_DS_LAYOUT_MATCHES_LINUX");
    mark("SHMID_DS_LAYOUT_MATCHES_LINUX");
    done();
}

/*
 * Case: sysvipc-sem-undo.unshare-detaches
 *
 * `unshare(CLONE_SYSVSEM)` is equivalent to `sys_exit()` for the undo list
 * (kernel/fork.c:3280-3284): `exit_sem()` detaches the caller, applies the
 * pending adjustments only when the caller was the list's last owner
 * (ipc/sem.c:2333-2345, where a shared list is merely unreferenced), and
 * leaves the caller with *no* list, so the next `semop()` starts an empty one
 * and nothing is applied a second time at exit.
 *
 * The verdict therefore has to outlive the task that unshares: a child makes
 * the adjustment, observes the value return when it unshares, and exits; the
 * parent then reads the value once more.  A kernel that kept a copy of the
 * adjustments would restore them again here.
 */
static void case_sem_undo_unshare(void) {
    struct sembuf op;
    int status = 0;

    begin("sysvipc-sem-undo-unshare.unshare-detaches");

    int semid = (int)ok_call(semget(IPC_PRIVATE, 1, IPC_CREAT | 0600), "semget");
    errno = 0;
    ok_call(semctl(semid, 0, SETVAL, 5), "setval-five");

    pid_t child = fork();
    check(child >= 0, "fork");
    if (child == 0) {
        op.sem_num = 0;
        op.sem_op = -1;
        op.sem_flg = SEM_UNDO;
        if (semop(semid, &op, 1) != 0)
            _exit(1);
        if (semctl(semid, 0, GETVAL) != 4)
            _exit(2);
        /* exit_sem() applies the pending adjustment here. */
        if (unshare(CLONE_SYSVSEM) != 0)
            _exit(3);
        if (semctl(semid, 0, GETVAL) != 5)
            _exit(4);
        _exit(0);
    }
    check(waitpid(child, &status, 0) == child, "wait-child");
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child-verdict");
    errno = 0;
    check(semctl(semid, 0, GETVAL) == 5, "value-after-child-exit");
    mark("UNSHARE_APPLIES_UNDO_ONCE");

    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");
    done();
}

/*
 * Case: sysvipc-limits.namespace-isolation
 *
 * Every SysV ceiling is a member of `struct ipc_namespace` published through
 * that namespace's own sysctl table:
 *
 * 	struct ipc_namespace {
 * 		int		sem_ctls[4];
 * 		int		msg_ctlmax, msg_ctlmnb, msg_ctlmni;
 * 		unsigned long	shm_ctlmax, shm_ctlall;
 * 		int		shm_ctlmni;
 * 	};
 *
 * (`include/linux/ipc_namespace.h`), copied into the new namespace by
 * `copy_ipcs()` and rewritten to point at it by `setup_ipc_sysctls()`
 * (`ipc/ipc_sysctl.c:236-282`).  A write in one namespace therefore cannot be
 * observed in another, and each namespace starts from the compiled defaults:
 * `SEMMSL 32000`, `SEMMNS (SEMMNI * SEMMSL)`, `SEMOPM 500`, `SEMMNI 32000`
 * (`include/uapi/linux/sem.h:80-85`, `ipc/sem.c:249-256`), `MSGMNI 32000`
 * (`include/uapi/linux/msg.h:64`, `ipc/msg.c:86`), `SHMMNI 4096`
 * (`include/uapi/linux/shm.h:22`, `ipc/shm.c:114`) and `SHMMAX`/`SHMALL` at
 * `ULONG_MAX - (1UL << 24)` (`include/uapi/linux/shm.h:19-21`,
 * `ipc/shm.c:112-113`).
 *
 * The child asks for each namespace-local effect and exits with a distinct
 * status per step: a zero ceiling refuses new objects with ENOSPC
 * (`ipc/util.c:287-288`, `ipc/sem.c:542`), and a value above `ipc_mni` is
 * refused by the sysctl handler itself - EINVAL for the count ceiling, ERANGE
 * for the semaphore tuple (`ipc/ipc_sysctl.c:117-124`, `:262-279`,
 * `ipc/util.h:248-255`).  The parent then proves that none of it leaked.
 *
 * The child also walks the size arithmetic that the default ceiling normally
 * keeps out of reach: `newseg()` computes `(size + PAGE_SIZE - 1) >> PAGE_SHIFT`
 * in `size_t`, so a size within one page of `ULONG_MAX` wraps, and the
 * `numpages << PAGE_SHIFT < size` comparison that follows answers ENOSPC
 * (`ipc/shm.c:712-721`).  Raising `shmmax` to `ULONG_MAX` is what admits the
 * size, so the write happens in the child's own namespace.
 */
static void case_limit_namespaces(void) {
    char buf[128];
    pid_t child;
    int msqid;

    begin("sysvipc-limits.namespace-isolation");

    check(read_sysctl("/proc/sys/kernel/sem", buf, sizeof buf) > 0, "read-sem");
    check(strcmp(buf, "32000 1024000000 500 32000") == 0, "sem-ceilings");
    check(read_sysctl("/proc/sys/kernel/msgmni", buf, sizeof buf) > 0, "read-msgmni");
    check(strcmp(buf, "32000") == 0, "msgmni-ceiling");
    check(read_sysctl("/proc/sys/kernel/shmmni", buf, sizeof buf) > 0, "read-shmmni");
    check(strcmp(buf, "4096") == 0, "shmmni-ceiling");
    check(read_sysctl("/proc/sys/kernel/shmmax", buf, sizeof buf) > 0, "read-shmmax");
    check(strtoull(buf, NULL, 10) == (unsigned long long)ULONG_MAX - (1ULL << 24),
          "shmmax-ceiling");
    check(read_sysctl("/proc/sys/kernel/shmall", buf, sizeof buf) > 0, "read-shmall");
    check(strtoull(buf, NULL, 10) == (unsigned long long)ULONG_MAX - (1ULL << 24),
          "shmall-ceiling");
    mark("CEILING_DEFAULTS_MATCH_UAPI");

    child = fork();
    check(child >= 0, "fork");
    if (child == 0) {
        /* A private IPC namespace carries its own copy of every ceiling. */
        if (unshare(CLONE_NEWIPC) != 0)
            _exit(11);
        if (write_sysctl("/proc/sys/kernel/msgmni", "0") != 0)
            _exit(12);
        errno = 0;
        if (msgget(IPC_PRIVATE, IPC_CREAT | 0600) != -1 || errno != ENOSPC)
            _exit(13);
        if (write_sysctl("/proc/sys/kernel/sem", "32000 1024000000 500 0") != 0)
            _exit(14);
        errno = 0;
        if (semget(IPC_PRIVATE, 1, IPC_CREAT | 0600) != -1 || errno != ENOSPC)
            _exit(15);
        /* `msgmni` is clamped to `ipc_mni` (32768) by the sysctl handler. */
        errno_sysctl("/proc/sys/kernel/msgmni", "32769", EINVAL, "msgmni-above-ipc-mni");
        /* `semmni` is range-checked separately from the parsed tuple. */
        errno_sysctl("/proc/sys/kernel/sem", "32000 1024000000 500 32769", ERANGE,
                     "semmni-above-ipc-mni");
        /* The written ceiling is the one `ipc_addid()` enforces. */
        if (write_sysctl("/proc/sys/kernel/msgmni", "1") != 0)
            _exit(18);
        if (msgget(IPC_PRIVATE, IPC_CREAT | 0600) < 0)
            _exit(19);
        errno = 0;
        if (msgget(IPC_PRIVATE, IPC_CREAT | 0600) != -1 || errno != ENOSPC)
            _exit(20);
        /* `size + PAGE_SIZE - 1` is computed in `size_t`, so a size within a
         * page of ULONG_MAX wraps to a small page count; `newseg()` compares
         * the shift back and answers ENOSPC.  The default ceiling is what
         * normally keeps the region unreachable, so the probe raises it
         * first and restores it before leaving. */
        if (write_sysctl("/proc/sys/kernel/shmmax", "18446744073709551615") != 0)
            _exit(21);
        errno = 0;
        if (shmget(IPC_PRIVATE, (size_t)-1, IPC_CREAT | 0600) != -1 || errno != ENOSPC)
            _exit(22);
        if (write_sysctl("/proc/sys/kernel/shmmax", "18446744073709534399") != 0)
            _exit(23);
        _exit(0);
    }
    reap_child(child, "namespace-child");

    /* Nothing the child did is visible here. */
    check(read_sysctl("/proc/sys/kernel/msgmni", buf, sizeof buf) > 0, "recheck-msgmni");
    check(strcmp(buf, "32000") == 0, "parent-msgmni-unchanged");
    check(read_sysctl("/proc/sys/kernel/sem", buf, sizeof buf) > 0, "recheck-sem");
    check(strcmp(buf, "32000 1024000000 500 32000") == 0, "parent-sem-unchanged");
    errno = 0;
    msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "parent-msgget");
    ok_call(msgctl(msqid, IPC_RMID, NULL), "parent-msgctl-rmid");
    mark("CEILINGS_ARE_IPC_NAMESPACE_LOCAL");
    mark("ZERO_CEILING_REFUSES_CREATION");
    mark("CEILING_WRITE_RANGE_ERRORS");
    mark("WRAPPED_SIZE_IS_ENOSPC");

    done();
}

/*
 * Case: sysvipc-semget-bounds.nsems-before-key
 *
 * `ksys_semget()` range-checks `nsems` against the namespace's `semmsl`
 * *before* `ipcget()` looks at the key, so an over-large request is EINVAL
 * even when the key names nothing at all:
 *
 * 	if (nsems < 0 || nsems > ns->sem_ctls[0])
 * 		return -EINVAL;
 * 	...
 * 	err = ipcget(ns, NULL, &sem_ids(ns), &sem_ops, &sem_params);
 *
 * (`ipc/sem.c:614-621`).  The check also runs ahead of the `IPC_EXCL` answer,
 * because `ipcget_public()` is only reached afterwards - an over-large
 * `nsems` with `IPC_CREAT | IPC_EXCL` on a live key is EINVAL, not EEXIST
 * (`ipc/util.c:397-433`).  Zero is the "existing set" spelling that
 * `ipcget_public()` accepts for a live key, while `newary()` rejects it for a
 * new one (`ipc/sem.c:484-487`).
 */
static void case_semget_nsems_bounds(void) {
    int semid;

    begin("sysvipc-semget-bounds.nsems-before-key");

    /* The key names nothing. */
    errno = 0;
    errno_call(semget(KEY_SEMGET_BOUNDS, 40000, 0), EINVAL, "oversize-absent");
    errno = 0;
    errno_call(semget(KEY_SEMGET_BOUNDS, 40000, IPC_CREAT), EINVAL, "oversize-create");
    semid = (int)ok_call(semget(KEY_SEMGET_BOUNDS, 1, IPC_CREAT | 0600), "create-one");
    /* The key names a live one-semaphore set. */
    errno = 0;
    errno_call(semget(KEY_SEMGET_BOUNDS, 40000, IPC_CREAT | IPC_EXCL), EINVAL,
               "oversize-exclusive");
    errno = 0;
    errno_call(semget(KEY_SEMGET_BOUNDS, 40000, 0), EINVAL, "oversize-existing");
    errno = 0;
    check(semget(KEY_SEMGET_BOUNDS, 0, 0) == semid, "zero-existing");
    mark("OVERSIZE_NSEMS_EINVAL_BEFORE_KEY_LOOKUP");
    mark("ZERO_NSEMS_ACCEPTED_FOR_LIVE_KEY");
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");

    /* A brand-new set has no `sem_nsems` to inherit, so zero is invalid. */
    errno = 0;
    errno_call(semget(IPC_PRIVATE, 0, IPC_CREAT | 0600), EINVAL, "zero-new");
    mark("ZERO_NSEMS_EINVAL_FOR_NEW_SET");

    done();
}

/*
 * Case: sysvipc-task-pid.nested-pid-namespace
 *
 * `msg_lspid`, `msg_lrpid`, `shm_cpid`, `shm_lpid` and the per-semaphore
 * `sempid` store a `task_tgid()` and are rendered with `pid_vnr()` on the way
 * out, so a reader in a nested PID namespace sees its own numbering:
 *
 * 	ipc_update_pid(&msq->q_lspid, task_tgid(current));      - ipc/msg.c:574
 * 	ipc_update_pid(&msq->q_lrpid, task_tgid(current));      - ipc/msg.c:1103
 * 	ipc_update_pid(&sma->sems[semnum].sempid, task_tgid(current));
 * 	...
 * 	err = pid_vnr(...);                                     - ipc/sem.c:1548
 *
 * (`ipc/msg.c:574-575`, `:1100-1104`, `ipc/sem.c:1526-1530`, `:1548-1550`;
 * `pid_vnr()` is `task_pid_nr_ns(pid, task_active_pid_ns(current))`,
 * `include/linux/pid.h`).  The grandchild below is PID 1 of the namespace it
 * is born into, so every one of those fields has a single correct rendering.
 */
static void case_task_pid_rendering(void) {
    int msqid;
    int semid;
    pid_t child;

    begin("sysvipc-task-pid.nested-pid-namespace");

    msqid = (int)ok_call(msgget(IPC_PRIVATE, IPC_CREAT | 0600), "msgget");
    semid = (int)ok_call(semget(IPC_PRIVATE, 1, IPC_CREAT | 0600), "semget");
    ok_call(semctl(semid, 0, SETVAL, 7), "setval");

    child = fork();
    check(child >= 0, "fork");
    if (child == 0) {
        pid_t grandchild;
        int grand_status = 0;
        /* Only children born after the unshare join the new namespace. */
        if (unshare(CLONE_NEWPID) != 0)
            _exit(21);
        grandchild = fork();
        if (grandchild < 0)
            _exit(22);
        if (grandchild == 0) {
            struct {
                long mtype;
                char mtext[8];
            } msg;
            struct msqid_ds mds;
            struct sembuf op;
            if (getpid() != 1)
                _exit(23);
            msg.mtype = 1;
            memcpy(msg.mtext, "pid", 4);
            if (msgsnd(msqid, &msg, 4, 0) != 0)
                _exit(24);
            memset(&mds, 0, sizeof mds);
            if (msgctl(msqid, IPC_STAT, &mds) != 0)
                _exit(25);
            if (mds.msg_lspid != getpid())
                _exit(26);
            if (msgrcv(msqid, &msg, sizeof msg.mtext, 0, 0) != 4)
                _exit(27);
            memset(&mds, 0, sizeof mds);
            if (msgctl(msqid, IPC_STAT, &mds) != 0)
                _exit(28);
            if (mds.msg_lrpid != getpid())
                _exit(29);
            op.sem_num = 0;
            op.sem_op = -1;
            op.sem_flg = 0;
            if (semop(semid, &op, 1) != 0)
                _exit(30);
            if (semctl(semid, 0, GETPID) != getpid())
                _exit(31);
            _exit(0);
        }
        if (waitpid(grandchild, &grand_status, 0) != grandchild)
            _exit(32);
        _exit(WIFEXITED(grand_status) ? WEXITSTATUS(grand_status) : 33);
    }
    reap_child(child, "nested-pid-child");
    mark("MSG_LSPID_RENDERS_PID_VNR");
    mark("MSG_LRPID_RENDERS_PID_VNR");
    mark("SEM_GETPID_RENDERS_PID_VNR");

    ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");
    done();
}

/*
 * Case: sysvipc-shmat-range.address-limit
 *
 * `do_shmat()` applies only the alignment rule itself; the requested range is
 * handed to `do_mmap()`, whose `mmap_region()` bound check reports ENOMEM:
 *
 * 	if (addr & (shmlba - 1)) { ... goto out; }             - EINVAL
 * 	addr = do_mmap(file, addr, size, prot, flags, ...);
 * 	...
 * 	if (addr > TASK_SIZE - len)
 * 		return -ENOMEM;
 * 	if (offset_in_page(addr))
 * 		return -EINVAL;
 *
 * (`ipc/shm.c:1541-1560`, `:1655`, `mm/mmap.c:858-860`).  An address above
 * `TASK_SIZE` is therefore ENOMEM once it is aligned - a distinct errno from
 * the misaligned case, which never reaches the mapping layer.
 */
static void case_shmat_address_range(void) {
    int shmid;

    begin("sysvipc-shmat-range.address-limit");

    shmid = (int)ok_call(shmget(IPC_PRIVATE, PAGE_BYTES, IPC_CREAT | 0600), "shmget");

    /* TASK_SIZE is 0x00007fffffffffff on x86_64, so 2^47 is past it. */
    errno = 0;
    errno_call((long)shmat(shmid, (void *)(uintptr_t)0x800000000001ULL, 0), EINVAL,
               "misaligned");
    errno = 0;
    errno_call((long)shmat(shmid, (void *)(uintptr_t)0x800000000000ULL, 0), ENOMEM,
               "above-task-size");
    mark("MISALIGNED_ADDRESS_EINVAL");
    mark("ALIGNED_ADDRESS_ABOVE_TASK_SIZE_ENOMEM");

    ok_call(shmctl(shmid, IPC_RMID, NULL), "shmctl-rmid");
    done();
}

int main(void) {
    case_limit_namespaces();
    case_semget_nsems_bounds();
    case_task_pid_rendering();
    case_shmat_address_range();
    case_identifier_progression();
    case_stat_index_resolution();
    case_info_max_index();
    case_sem_flags();
    case_sem_undo_range();
    case_sem_undo_unshare();
    case_ipc64_command();
    case_shm_lock_memlock();
    case_shm_hugetlb_existing_key();
    case_shm_dest_stat();
    case_errno_order();
    case_exclusive_create_order();
    case_owner_change();
    case_record_layout();
    puts("THEKERNEL_SYSVIPC_OK");
    return 0;
}
