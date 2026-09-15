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
 *   - the `next_id` files under /proc/sys/kernel are gated on
 *     CONFIG_CHECKPOINT_RESTORE in Linux, which is not guaranteed in the
 *     oracle kernel, so the requested-identifier path is not exercised here;
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
#include <linux/capability.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/msg.h>
#include <sys/resource.h>
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

    begin("sysvipc-sem.undo-range");

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

    begin("sysvipc-shm.lock-memlock");

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

    begin("sysvipc-shm.hugetlb-existing-key");

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

    begin("sysvipc-shm.dest-stat");

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

    memcpy(msg.mtext, "payload", 8);
    errno = 0;
    ok_call(msgsnd(msqid, &msg, 8, 0), "msgsnd-ok");
    memset(&msg, 0, sizeof(msg));
    errno = 0;
    check(msgrcv(msqid, &msg, 8, 0, 0) == 8, "msgrcv-ok");
    check(msg.mtype == 1 && memcmp(msg.mtext, "payload", 8) == 0, "msgrcv-data");

    /* MSG_COPY prepares its scratch message from the caller's buffer before it
     * resolves the queue, so an unreadable buffer wins over the bad id. */
    errno = 0;
    errno_call(msgrcv(0x7fff, BAD, 8, 0, MSG_COPY | IPC_NOWAIT), EFAULT,
               "msgrcv-copy-efault-first");

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

    errno = 0;
    ok_call(msgctl(msqid, IPC_RMID, NULL), "msgctl-rmid");
    errno = 0;
    ok_call(semctl(semid, 0, IPC_RMID, NULL), "semctl-rmid");

    mark("SEMOP_EFBIG_BEFORE_EACCES");
    mark("MSGSND_FAULTS_BEFORE_VALIDATION");
    mark("MSGSND_SIZE_AND_TYPE_BEFORE_ID");
    mark("MSGRCV_COPY_FAULT_BEFORE_ID");
    mark("TABLE_COMMANDS_REJECT_NEGATIVE_ID");
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

int main(void) {
    case_identifier_progression();
    case_stat_index_resolution();
    case_info_max_index();
    case_sem_flags();
    case_sem_undo_range();
    case_ipc64_command();
    case_shm_lock_memlock();
    case_shm_hugetlb_existing_key();
    case_shm_dest_stat();
    case_errno_order();
    case_exclusive_create_order();
    puts("THEKERNEL_SYSVIPC_OK");
    return 0;
}
