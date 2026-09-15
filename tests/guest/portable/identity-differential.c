/*
 * Differential coverage for the identity, resource-limit, usage-reporting and
 * UTS-name ABI cells: getuid(102), geteuid(107), getgid(104), getegid(108),
 * getresuid(118), getresgid(120), setuid(105), setgid(106), getgroups(115),
 * setgroups(116), setfsuid(122), setfsgid(123), getrlimit(97), setrlimit(160),
 * prlimit64(302), getrusage(98), personality(135), sethostname(170) and
 * setdomainname(171).
 *
 * The suite boots one guest and runs every program as root with full
 * capabilities, so nothing below may leave state that a later program could
 * observe: credentials, supplementary groups and resource limits are only
 * changed inside a forked child whose exit status the parent checks, and the
 * hostname and domainname are written only after unshare(CLONE_NEWUTS).  The
 * parent re-reads its own identities, group list, limits and UTS names after
 * every such child and asserts they are untouched.
 *
 * Every assertion must also hold on the Linux reference guest, so the program
 * asserts relationships, errno precedence and copy order instead of the
 * absolute identity, limit or release values of the account it runs as.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_getrlimit
#define SYS_getrlimit 97
#endif
#ifndef SYS_setrlimit
#define SYS_setrlimit 160
#endif
#ifndef SYS_prlimit64
#define SYS_prlimit64 302
#endif
#ifndef SYS_getrusage
#define SYS_getrusage 98
#endif
#ifndef SYS_getuid
#define SYS_getuid 102
#endif
#ifndef SYS_getgid
#define SYS_getgid 104
#endif
#ifndef SYS_setuid
#define SYS_setuid 105
#endif
#ifndef SYS_setgid
#define SYS_setgid 106
#endif
#ifndef SYS_geteuid
#define SYS_geteuid 107
#endif
#ifndef SYS_getegid
#define SYS_getegid 108
#endif
#ifndef SYS_getgroups
#define SYS_getgroups 115
#endif
#ifndef SYS_setgroups
#define SYS_setgroups 116
#endif
#ifndef SYS_getresuid
#define SYS_getresuid 118
#endif
#ifndef SYS_getresgid
#define SYS_getresgid 120
#endif
#ifndef SYS_setfsuid
#define SYS_setfsuid 122
#endif
#ifndef SYS_setfsgid
#define SYS_setfsgid 123
#endif
#ifndef SYS_personality
#define SYS_personality 135
#endif
#ifndef SYS_sethostname
#define SYS_sethostname 170
#endif
#ifndef SYS_setdomainname
#define SYS_setdomainname 171
#endif

/* Linux personality flags (uapi/linux/personality.h). */
#define PERSONALITY_QUERY 0xffffffffU
#define PERSONALITY_UNAME26 0x0020000U
#define PERSONALITY_ADDR_NO_RANDOMIZE 0x0040000U
#define PERSONALITY_PER_LINUX32 0x0008U

/* An unmapped user id: Linux validates the mapped result before every branch,
 * and both UTS setters surface it as EINVAL rather than as a copy failure. */
#define INVALID_ID 0xffffffffU

/* The one identity the program assumes anything about. */
#define UNPRIVILEGED_ID 65534U

static const char *active;
static int failures;

static void check(int ok, const char *stage) {
    if (!ok) {
        fprintf(stderr, "THEKERNEL_IDENTITY_FAIL %s %s errno=%d (%s)\n",
                active, stage, errno, strerror(errno));
        failures++;
    }
}

#define ERROR(call, expected, stage) do { \
    errno = 0; \
    long result_ = (call); \
    check(result_ == -1 && errno == (expected), (stage)); \
} while (0)

static void begin(const char *name) {
    active = name;
    printf("THEKERNEL_ABI_CASE %s\n", name);
}

static void mark(const char *name) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
}

static void done(void) {
    printf("THEKERNEL_ABI_RESULT %s pass\n", active);
    fflush(stdout);
}

/* Every mutating probe runs in a forked child: the parent's credentials, group
 * list, limits, personality and UTS names must survive the whole program.  The
 * child reports through its exit status and its own FAIL lines; the parent
 * asserts only that it exited zero. */
static void in_child(const char *stage, void (*body)(void)) {
    pid_t child = fork();
    if (child < 0) {
        check(0, stage);
        return;
    }
    if (child == 0) {
        /* The child reports only its own failures, not the parent's. */
        failures = 0;
        body();
        /* _exit, not exit: a child must never flush the parent's stdio. */
        _exit(failures == 0 ? 0 : 1);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        check(0, stage);
        return;
    }
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, stage);
}

/* setuid(2) out of the privileged branch clears the capability sets, which is
 * what makes the denied-request probes below observe a real EPERM. */
static int drop_privileges(void) {
    return syscall(SYS_setuid, UNPRIVILEGED_ID) == 0;
}

static int timeval_lt(struct timeval a, struct timeval b) {
    return a.tv_sec < b.tv_sec || (a.tv_sec == b.tv_sec && a.tv_usec < b.tv_usec);
}

static int timeval_eq(struct timeval a, struct timeval b) {
    return a.tv_sec == b.tv_sec && a.tv_usec == b.tv_usec;
}

static long long timeval_us(struct timeval value) {
    return (long long)value.tv_sec * 1000000 + (long long)value.tv_usec;
}

static long long rusage_cpu_us(const struct rusage *usage) {
    return timeval_us(usage->ru_utime) + timeval_us(usage->ru_stime);
}

/* ------------------------------------------------------------------ *
 * identity-ids: getuid, geteuid, getgid, getegid, getresuid, getresgid
 * ------------------------------------------------------------------ */

/* The plain getters and the three-id calls must read the same fields, and the
 * real, effective and saved slots must be distinct: a value that satisfies
 * "everything is equal" proves nothing about which field each getter returns. */
static void distinct_uid_child(void) {
    if (syscall(SYS_setresuid, 1001, 1002, 1003) != 0) {
        check(0, "setresuid-distinct");
        return;
    }
    check(syscall(SYS_getuid) == 1001, "getuid-reports-real");
    check(syscall(SYS_geteuid) == 1002, "geteuid-reports-effective");
    uint32_t real = 0, effective = 0, saved = 0;
    check(syscall(SYS_getresuid, &real, &effective, &saved) == 0, "getresuid");
    check(real == 1001 && effective == 1002 && saved == 1003, "getresuid-fields");
    /* setresuid publishes euid into fsuid; -1 is Linux's read-only idiom. */
    check(syscall(SYS_setfsuid, INVALID_ID) == 1002, "fsuid-mirrors-euid");
}

static void distinct_gid_child(void) {
    if (syscall(SYS_setresgid, 2001, 2002, 2003) != 0) {
        check(0, "setresgid-distinct");
        return;
    }
    check(syscall(SYS_getgid) == 2001, "getgid-reports-real");
    check(syscall(SYS_getegid) == 2002, "getegid-reports-effective");
    uint32_t real = 0, effective = 0, saved = 0;
    check(syscall(SYS_getresgid, &real, &effective, &saved) == 0, "getresgid");
    check(real == 2001 && effective == 2002 && saved == 2003, "getresgid-fields");
    check(syscall(SYS_setfsgid, INVALID_ID) == 2002, "fsgid-mirrors-egid");
}

static void getter_agreement(void) {
    uint32_t real = 0, effective = 0, saved = 0;
    check(syscall(SYS_getresuid, &real, &effective, &saved) == 0, "main-getresuid");
    check(syscall(SYS_getuid) == (long)real, "getuid-agrees-with-getresuid");
    check(syscall(SYS_geteuid) == (long)effective, "geteuid-agrees-with-getresuid");
    check(syscall(SYS_getresgid, &real, &effective, &saved) == 0, "main-getresgid");
    check(syscall(SYS_getgid) == (long)real, "getgid-agrees-with-getresgid");
    check(syscall(SYS_getegid) == (long)effective, "getegid-agrees-with-getresgid");
}

/* Linux writes the three IDs in order and stops at the first fault, so an
 * earlier destination keeps the value that already reached it. */
static void resid_faults(uint32_t *slot, uint32_t *guard, long nr) {
    uint32_t real = 0, effective = 0, saved = 0;
    check(syscall(nr, &real, &effective, &saved) == 0, "resid-reference");

    slot[0] = slot[1] = slot[2] = 0xdeadbeefU;
    ERROR(syscall(nr, guard, slot, slot), EFAULT, "resid-first-fault");
    check(slot[0] == 0xdeadbeefU && slot[1] == 0xdeadbeefU && slot[2] == 0xdeadbeefU,
          "resid-first-fault-writes-nothing");

    slot[0] = slot[1] = slot[2] = 0xdeadbeefU;
    ERROR(syscall(nr, slot, guard, slot + 2), EFAULT, "resid-second-fault");
    check(slot[0] == real && slot[1] == 0xdeadbeefU && slot[2] == 0xdeadbeefU,
          "resid-second-fault-keeps-first");

    slot[0] = slot[1] = slot[2] = 0xdeadbeefU;
    ERROR(syscall(nr, slot, slot + 1, guard), EFAULT, "resid-third-fault");
    check(slot[0] == real && slot[1] == effective && slot[2] == 0xdeadbeefU,
          "resid-third-fault-keeps-prefix");

    slot[0] = slot[1] = slot[2] = 0xdeadbeefU;
    check(syscall(nr, slot, slot + 1, slot + 2) == 0, "resid-success");
    check(slot[0] == real && slot[1] == effective && slot[2] == saved, "resid-success-fields");
}

static void ids_case(void) {
    long page = sysconf(_SC_PAGESIZE);
    uint32_t real = 0, effective = 0, saved = 0;
    char *region;

    begin("identity-ids.raw-differential");
    getter_agreement();
    mark("PLAIN_GETTER_AGREEMENT");

    in_child("distinct-uid-child", distinct_uid_child);
    in_child("distinct-gid-child", distinct_gid_child);
    mark("DISTINCT_ID_FIELDS");

    region = mmap(NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (region == MAP_FAILED) {
        check(0, "resid-mmap");
    } else {
        check(mprotect(region + page, (size_t)page, PROT_NONE) == 0, "resid-mprotect");
        uint32_t *slot = (uint32_t *)(void *)(region + page - 3 * (long)sizeof(uint32_t));
        uint32_t *guard = (uint32_t *)(void *)(region + page);
        resid_faults(slot, guard, SYS_getresuid);
        resid_faults(slot, guard, SYS_getresgid);
        check(munmap(region, (size_t)page * 2) == 0, "resid-munmap");
    }
    mark("RESID_FAULT_PREFIX_ORDER");

    check(syscall(SYS_getresuid, &real, &effective, &saved) == 0
              && real == (uint32_t)syscall(SYS_getuid),
          "main-ids-survived-children");
    done();
}

/* ------------------------------------------------------------------ *
 * identity-switch: setuid, setgid
 * ------------------------------------------------------------------ */

/* As root, setuid(2) replaces real, effective and saved together and publishes
 * the new effective id as fsuid.  Once the process is no longer privileged the
 * same call admits only the real and saved ids and refuses everything else
 * with EPERM, while an unmapped id is EINVAL before any authority check. */
static void setuid_child(void) {
    uint32_t real = 0, effective = 0, saved = 0;

    if (syscall(SYS_setuid, 1000) != 0) {
        check(0, "setuid-root-branch");
        return;
    }
    check(syscall(SYS_getuid) == 1000 && syscall(SYS_geteuid) == 1000, "setuid-plain-getters");
    check(syscall(SYS_getresuid, &real, &effective, &saved) == 0 && real == 1000
              && effective == 1000 && saved == 1000,
          "setuid-saved-ids");
    check(syscall(SYS_setfsuid, INVALID_ID) == 1000, "setuid-publishes-fsuid");
    ERROR(syscall(SYS_setuid, 0), EPERM, "setuid-back-to-root-denied");
    check(syscall(SYS_getuid) == 1000 && syscall(SYS_geteuid) == 1000, "denied-setuid-keeps-ids");
    ERROR(syscall(SYS_setuid, 1001), EPERM, "setuid-foreign-id-denied");
    check(syscall(SYS_setuid, 1000) == 0, "setuid-real-id-allowed");
    ERROR(syscall(SYS_setuid, INVALID_ID), EINVAL, "setuid-unmapped-id");
}

/* setgid(2000) from root succeeds without clearing the capability sets: only a
 * uid transition runs the capability fixup, so the same child can still show
 * the privileged branch twice.  Dropping the uid afterwards is what makes the
 * unprivileged branch observable. */
static void setgid_child(void) {
    uint32_t real = 0, effective = 0, saved = 0;

    if (syscall(SYS_setgid, 2000) != 0) {
        check(0, "setgid-root-branch");
        return;
    }
    check(syscall(SYS_getgid) == 2000 && syscall(SYS_getegid) == 2000, "setgid-plain-getters");
    check(syscall(SYS_getresgid, &real, &effective, &saved) == 0 && real == 2000
              && effective == 2000 && saved == 2000,
          "setgid-saved-ids");
    check(syscall(SYS_setfsgid, INVALID_ID) == 2000, "setgid-publishes-fsgid");

    if (syscall(SYS_setuid, UNPRIVILEGED_ID) != 0) {
        check(0, "setgid-drop-privileges");
        return;
    }
    ERROR(syscall(SYS_setgid, 3000), EPERM, "setgid-foreign-id-denied");
    ERROR(syscall(SYS_setgid, 0), EPERM, "setgid-back-to-root-denied");
    check(syscall(SYS_getgid) == 2000 && syscall(SYS_getegid) == 2000, "denied-setgid-keeps-ids");
    check(syscall(SYS_setgid, 2000) == 0, "setgid-real-id-allowed");
    ERROR(syscall(SYS_setgid, INVALID_ID), EINVAL, "setgid-unmapped-id");
}

/* setuid() is not setresuid(): the unprivileged branch admits the real or the
 * saved id but publishes only the effective and filesystem ids, so a process
 * whose saved id differs from its real id can switch the effective id to the
 * saved one without adopting it as its real identity. */
static void setuid_saved_child(void) {
    uint32_t real = 0, effective = 0, saved = 0;

    if (syscall(SYS_setresuid, 1000, 1000, 2000) != 0) {
        check(0, "saved-id-setup");
        return;
    }
    check(syscall(SYS_getresuid, &real, &effective, &saved) == 0 && real == 1000
              && effective == 1000 && saved == 2000,
          "saved-id-setup-fields");

    ERROR(syscall(SYS_setuid, 0), EPERM, "saved-id-cannot-return-to-root");
    ERROR(syscall(SYS_setuid, 3000), EPERM, "saved-id-foreign-denied");
    check(syscall(SYS_getuid) == 1000 && syscall(SYS_geteuid) == 1000, "denied-setuid-keeps-real");

    check(syscall(SYS_setuid, 2000) == 0, "saved-id-admitted");
    check(syscall(SYS_geteuid) == 2000, "saved-id-publishes-effective");
    check(syscall(SYS_getuid) == 1000, "saved-id-keeps-real");
    check(syscall(SYS_getresuid, &real, &effective, &saved) == 0 && saved == 2000,
          "saved-id-keeps-saved");
    check(syscall(SYS_setfsuid, INVALID_ID) == 2000, "saved-id-publishes-fsuid");
    ERROR(syscall(SYS_setuid, 3001), EPERM, "saved-id-foreign-after-switch");
    check(syscall(SYS_setuid, 1000) == 0 && syscall(SYS_geteuid) == 1000,
          "real-id-still-admitted");
}

static void switch_case(void) {
    uint32_t real = 0, effective = 0, saved = 0;
    uint32_t gid_real = 0, gid_effective = 0, gid_saved = 0;

    begin("identity-switch.raw-differential");
    check(syscall(SYS_getresuid, &real, &effective, &saved) == 0, "switch-uid-before");
    check(syscall(SYS_getresgid, &gid_real, &gid_effective, &gid_saved) == 0, "switch-gid-before");
    in_child("setuid-child", setuid_child);
    mark("SETUID_CAPABILITY_BRANCH");
    in_child("setuid-saved-child", setuid_saved_child);
    mark("SETUID_SAVED_ID_ADMISSION");
    in_child("setgid-child", setgid_child);
    mark("SETGID_CAPABILITY_BRANCH");

    uint32_t now_real = 0, now_effective = 0, now_saved = 0;
    check(syscall(SYS_getresuid, &now_real, &now_effective, &now_saved) == 0
              && now_real == real && now_effective == effective && now_saved == saved,
          "uid-change-stayed-in-child");
    check(syscall(SYS_getresgid, &now_real, &now_effective, &now_saved) == 0
              && now_real == gid_real && now_effective == gid_effective && now_saved == gid_saved,
          "gid-change-stayed-in-child");
    mark("CREDENTIAL_CHANGES_STAY_IN_CHILD");
    done();
}

/* ------------------------------------------------------------------ *
 * identity-groups: getgroups, setgroups
 * ------------------------------------------------------------------ */

static int inherited_group_count;

/* getgroups is a length protocol: zero never touches the pointer, a buffer
 * that cannot hold every group is EINVAL before any copy, and only a
 * sufficient buffer reaches the copy that can fail with EFAULT.  A two-group
 * list is installed first so the short-buffer case is never the zero case. */
static void groups_length_child(void) {
    gid_t list[2] = { 3001, 3002 };
    gid_t readback[8];

    check(syscall(SYS_getgroups, 0, NULL) == inherited_group_count, "getgroups-inherited-count");
    check(syscall(SYS_getgroups, 0, (void *)1) == inherited_group_count,
          "getgroups-zero-size-ignores-pointer");
    check(syscall(SYS_setgroups, 2, list) == 0, "getgroups-install-list");
    check(syscall(SYS_getgroups, 0, NULL) == 2, "getgroups-known-count");
    ERROR(syscall(SYS_getgroups, 1, readback), EINVAL, "getgroups-short-buffer");
    ERROR(syscall(SYS_getgroups, 2, NULL), EFAULT, "getgroups-null-buffer");
    ERROR(syscall(SYS_getgroups, 2, (void *)1), EFAULT, "getgroups-bad-buffer");
    ERROR(syscall(SYS_getgroups, -1, NULL), EINVAL, "getgroups-negative-size");
    memset(readback, 0, sizeof(readback));
    check(syscall(SYS_getgroups, 2, readback) == 2 && readback[0] == 3001 && readback[1] == 3002,
          "getgroups-sufficient-buffer");
}

/* setgroups sorts its input, keeps duplicates, and replaces the whole list --
 * including with an empty one. */
static void groups_state_child(void) {
    gid_t list[4] = { 3003, 3001, 3002, 3003 };
    gid_t readback[4];
    gid_t invalid = INVALID_ID;

    check(syscall(SYS_setgroups, 4, list) == 0, "setgroups-unsorted");
    check(syscall(SYS_getgroups, 0, NULL) == 4, "getgroups-count-after-set");
    memset(readback, 0, sizeof(readback));
    check(syscall(SYS_getgroups, 4, readback) == 4, "getgroups-read-back");
    check(readback[0] == 3001 && readback[1] == 3002 && readback[2] == 3003
              && readback[3] == 3003,
          "getgroups-sorted-with-duplicates");

    ERROR(syscall(SYS_setgroups, 1, &invalid), EINVAL, "setgroups-unmapped-gid");
    ERROR(syscall(SYS_setgroups, NGROUPS_MAX + 1, readback), EINVAL, "setgroups-over-limit");
    ERROR(syscall(SYS_setgroups, 1, (void *)1), EFAULT, "setgroups-bad-pointer");
    check(syscall(SYS_getgroups, 0, NULL) == 4, "rejected-setgroups-keeps-list");

    check(syscall(SYS_setgroups, 0, NULL) == 0, "setgroups-empty");
    check(syscall(SYS_getgroups, 0, NULL) == 0, "getgroups-empty-after-clear");
}

static void groups_denied_child(void) {
    gid_t one = 4001;

    if (!drop_privileges()) {
        check(0, "groups-drop-privileges");
        return;
    }
    ERROR(syscall(SYS_setgroups, 1, &one), EPERM, "setgroups-unprivileged");
    ERROR(syscall(SYS_setgroups, NGROUPS_MAX + 1, &one), EPERM,
          "setgroups-capability-before-size");
    ERROR(syscall(SYS_setgroups, 1, (void *)1), EPERM, "setgroups-capability-before-usercopy");
    check(syscall(SYS_getgroups, 0, NULL) == inherited_group_count,
          "denied-setgroups-keeps-list");
}

static void groups_case(void) {
    gid_t before[8];
    gid_t after[8];
    int count = (int)syscall(SYS_getgroups, 0, NULL);

    begin("identity-groups.raw-differential");
    check(count >= 0 && count <= 8, "groups-main-count");
    inherited_group_count = count;
    if (count > 0) {
        check(syscall(SYS_getgroups, count, before) == count, "groups-main-read");
    }

    in_child("groups-length-child", groups_length_child);
    mark("GETGROUPS_LENGTH_PROTOCOL");

    in_child("groups-state-child", groups_state_child);
    mark("SETGROUPS_SORT_AND_STATE");

    in_child("groups-denied-child", groups_denied_child);
    mark("SETGROUPS_ADMISSION_BEFORE_VALIDATION");

    check(syscall(SYS_getgroups, 0, NULL) == count, "groups-count-survived-children");
    if (count > 0) {
        memset(after, 0, sizeof(after));
        check(syscall(SYS_getgroups, count, after) == count, "groups-main-reread");
        check(memcmp(before, after, (size_t)count * sizeof(gid_t)) == 0,
              "groups-content-survived-children");
    }
    mark("GROUP_LIST_CHANGES_STAY_IN_CHILD");
    done();
}

/* ------------------------------------------------------------------ *
 * identity-fsids: setfsuid, setfsgid
 * ------------------------------------------------------------------ */

/* Creates a file in the guest's world-writable /tmp and reports the owner and
 * group Linux stamped on it, then removes it again. */
static int create_as(const char *prefix, uid_t *uid, gid_t *gid) {
    char path[128];
    struct stat status;
    int fd;

    snprintf(path, sizeof(path), "/tmp/%s-%d", prefix, (int)getpid());
    (void)unlink(path);
    fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0) {
        return -1;
    }
    int ok = fstat(fd, &status) == 0;
    int saved_errno = errno;
    (void)close(fd);
    (void)unlink(path);
    errno = saved_errno;
    if (!ok) {
        return -1;
    }
    *uid = status.st_uid;
    *gid = status.st_gid;
    return 0;
}

/* Both setters return the previous filesystem id, never fail, and a request
 * that is not admitted leaves the id alone.  The id they hold is observable:
 * Linux stamps a newly created file with the caller's fsuid/fsgid.  The
 * inherited ids are read with the same "set -1" query at the start, so the
 * assertions stay true for any account the suite happens to run as. */
static void setfsuid_child(void) {
    uid_t uid = 0;
    gid_t gid = 0;
    uid_t inherited_fsuid = (uid_t)syscall(SYS_setfsuid, INVALID_ID);

    check(create_as("identity-fsuid-a", &uid, &gid) == 0, "create-before-setfsuid");
    check(uid == inherited_fsuid, "inherited-fsuid-owns-first-file");

    check(syscall(SYS_setfsuid, 1000) == (long)inherited_fsuid, "setfsuid-returns-previous");
    check(syscall(SYS_setfsuid, 1000) == 1000, "setfsuid-repeat-returns-itself");
    check(create_as("identity-fsuid-b", &uid, &gid) == 0, "create-with-fsuid");
    check(uid == 1000, "fsuid-owns-new-file");

    check(syscall(SYS_setfsuid, INVALID_ID) == 1000, "setfsuid-unmapped-returns-previous");
    check(create_as("identity-fsuid-c", &uid, &gid) == 0, "create-after-unmapped");
    check(uid == 1000, "unmapped-setfsuid-keeps-state");

    check(syscall(SYS_setfsuid, inherited_fsuid) == 1000, "setfsuid-restore-returns-previous");
    check(create_as("identity-fsuid-d", &uid, &gid) == 0, "create-after-restore");
    check(uid == inherited_fsuid, "restored-fsuid-owns-new-file");
}

static void setfsgid_child(void) {
    uid_t uid = 0;
    gid_t gid = 0;
    gid_t inherited_fsgid = (gid_t)syscall(SYS_setfsgid, INVALID_ID);

    check(create_as("identity-fsgid-a", &uid, &gid) == 0, "create-before-setfsgid");
    check(gid == inherited_fsgid, "inherited-fsgid-owns-first-file");

    check(syscall(SYS_setfsgid, 2000) == (long)inherited_fsgid, "setfsgid-returns-previous");
    check(syscall(SYS_setfsgid, 2000) == 2000, "setfsgid-repeat-returns-itself");
    check(create_as("identity-fsgid-b", &uid, &gid) == 0, "create-with-fsgid");
    check(gid == 2000, "fsgid-owns-new-file");

    check(syscall(SYS_setfsgid, INVALID_ID) == 2000, "setfsgid-unmapped-returns-previous");
    check(create_as("identity-fsgid-c", &uid, &gid) == 0, "create-after-unmapped");
    check(gid == 2000, "unmapped-setfsgid-keeps-state");

    check(syscall(SYS_setfsgid, inherited_fsgid) == 2000, "setfsgid-restore-returns-previous");
    check(create_as("identity-fsgid-d", &uid, &gid) == 0, "create-after-restore");
    check(gid == inherited_fsgid, "restored-fsgid-owns-new-file");
}

/* Without CAP_SETUID an id outside {ruid, euid, suid, fsuid} is refused, and
 * refusal is still reported as the unchanged old value with no error.  A uid
 * transition is what clears the capability sets; the group ids are untouched,
 * so the fsgid probe keeps the inherited group identity. */
static void setfsid_denied_child(void) {
    uid_t uid = 0;
    gid_t gid = 0;
    gid_t inherited_fsgid = (gid_t)syscall(SYS_setfsgid, INVALID_ID);
    gid_t foreign_gid = inherited_fsgid == 2000 ? 2001 : 2000;

    if (!drop_privileges()) {
        check(0, "fsid-drop-privileges");
        return;
    }
    check(syscall(SYS_setfsuid, 1000) == (long)UNPRIVILEGED_ID, "setfsuid-denied-returns-old");
    check(create_as("identity-fsuid-e", &uid, &gid) == 0, "create-after-denied-fsuid");
    check(uid == UNPRIVILEGED_ID, "denied-setfsuid-keeps-state");
    check(syscall(SYS_setfsgid, foreign_gid) == (long)inherited_fsgid,
          "setfsgid-denied-returns-old");
    check(create_as("identity-fsgid-e", &uid, &gid) == 0, "create-after-denied-fsgid");
    check(gid == inherited_fsgid, "denied-setfsgid-keeps-state");
}

/* Publishing a filesystem identity moves the filesystem half of the effective
 * capability set with it.  Linux's cap_task_fix_setuid() takes the LSM_SETID_FS
 * arm for setfsuid only: crossing away from the namespace root drops
 * CAP_FS_SET (the historical fsuid==0 privileges, which include CAP_MKNOD and
 * CAP_LINUX_IMMUTABLE) from the effective set, crossing back raises exactly
 * those bits from permitted again, and unrelated capabilities are untouched.
 * setfsgid has no such arm because group transitions never alter capabilities. */
#define CAP_CHOWN_BIT 0U
#define CAP_KILL_BIT 5U

static int capability_bit(unsigned int bit, int permitted) {
    struct {
        uint32_t version;
        uint32_t pid;
    } header = { 0x20080522U, 0 };
    struct {
        uint32_t effective;
        uint32_t permitted;
        uint32_t inheritable;
    } data[2] = { { 0, 0, 0 }, { 0, 0, 0 } };

    if (syscall(SYS_capget, &header, data) != 0) {
        return -1;
    }
    uint32_t mask = data[bit / 32U].effective;
    if (permitted) {
        mask = data[bit / 32U].permitted;
    }
    return (int)((mask >> (bit % 32U)) & 1U);
}

static void fsid_caps_child(void) {
    /* The coupling is defined against the namespace root, so the probe states
     * the identity it needs instead of assuming it. */
    check(syscall(SYS_setfsuid, INVALID_ID) == 0, "fsid-caps-root-fsuid");
    check(capability_bit(CAP_CHOWN_BIT, 0) == 1, "filesystem-capability-effective-before");
    check(capability_bit(CAP_KILL_BIT, 0) == 1, "unrelated-capability-effective-before");

    check(syscall(SYS_setfsuid, 1000) == 0, "fsid-caps-leave-root");
    check(capability_bit(CAP_CHOWN_BIT, 0) == 0, "fsuid-drops-filesystem-capability");
    check(capability_bit(CAP_CHOWN_BIT, 1) == 1, "fsuid-keeps-filesystem-capability-permitted");
    check(capability_bit(CAP_KILL_BIT, 0) == 1, "fsuid-keeps-unrelated-capability");

    check(syscall(SYS_setfsgid, 2000) == 0, "fsid-caps-setfsgid");
    check(capability_bit(CAP_CHOWN_BIT, 0) == 0, "fsgid-does-not-touch-capabilities");

    check(syscall(SYS_setfsuid, 0) == 1000, "fsid-caps-return-to-root");
    check(capability_bit(CAP_CHOWN_BIT, 0) == 1, "root-fsuid-raises-filesystem-capability");
    check(capability_bit(CAP_KILL_BIT, 0) == 1, "root-fsuid-keeps-unrelated-capability");
}

static void fsids_case(void) {
    begin("identity-fsids.raw-differential");
    in_child("setfsuid-child", setfsuid_child);
    mark("SETFSUID_RETURNS_PRIOR_AND_OWNS_FILE");
    in_child("setfsgid-child", setfsgid_child);
    mark("SETFSGID_RETURNS_PRIOR_AND_OWNS_FILE");
    in_child("setfsid-denied-child", setfsid_denied_child);
    mark("UNPRIVILEGED_FSID_REQUEST_IS_A_QUERY");
    in_child("fsid-caps-child", fsid_caps_child);
    mark("FSUID_PUBLICATION_UPDATES_FILESYSTEM_CAPS");
    done();
}

/* ------------------------------------------------------------------ *
 * identity-limits: getrlimit, setrlimit, prlimit64
 * ------------------------------------------------------------------ */

/* Raising a hard limit is what CAP_SYS_RESOURCE buys, so the probes need a
 * resource with a finite hard limit.  RLIMIT_NICE is {0, 0} on both guests. */
static int finite_raise_target(struct rlimit *current) {
    static const int candidates[] = { RLIMIT_NICE, RLIMIT_NOFILE, RLIMIT_SIGPENDING };

    for (size_t index = 0; index < sizeof(candidates) / sizeof(candidates[0]); index++) {
        if (syscall(SYS_getrlimit, candidates[index], current) != 0) {
            continue;
        }
        if (current->rlim_max == RLIM_INFINITY) {
            continue;
        }
        return candidates[index];
    }
    return -1;
}

static void limits_readback_child(void) {
    struct rlimit current = { 0, 0 };
    struct rlimit wanted = { 0, 0 };
    struct rlimit back = { 0, 0 };
    struct rlimit mirror = { 0, 0 };
    int resource = finite_raise_target(&current);

    if (resource < 0) {
        check(0, "finite-raise-target");
        return;
    }
    /* Root carries CAP_SYS_RESOURCE, so raising the hard limit succeeds and
     * both readers report the new pair. */
    wanted.rlim_cur = current.rlim_max + 1;
    wanted.rlim_max = current.rlim_max + 1;
    check(syscall(SYS_setrlimit, resource, &wanted) == 0, "root-raises-hard-limit");
    check(syscall(SYS_getrlimit, resource, &back) == 0 && back.rlim_cur == wanted.rlim_cur
              && back.rlim_max == wanted.rlim_max,
          "root-raise-readback");
    check(syscall(SYS_prlimit64, 0, (unsigned int)resource, NULL, &mirror) == 0
              && mirror.rlim_cur == wanted.rlim_cur && mirror.rlim_max == wanted.rlim_max,
          "prlimit64-reads-setrlimit-value");

    /* A lowered pair is observable through both readers as well. */
    struct rlimit lowered = { 1, 2 };
    check(syscall(SYS_setrlimit, RLIMIT_NOFILE, &lowered) == 0, "setrlimit-lower-nofile");
    check(syscall(SYS_getrlimit, RLIMIT_NOFILE, &back) == 0 && back.rlim_cur == 1
              && back.rlim_max == 2,
          "setrlimit-lower-readback");
    check(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &mirror) == 0 && mirror.rlim_cur == 1
              && mirror.rlim_max == 2,
          "prlimit64-mirrors-getrlimit");
}

/* Linux refuses an RLIMIT_NOFILE hard limit above the system-wide ceiling even
 * for a privileged caller, and it refuses every hard-limit raise without
 * CAP_SYS_RESOURCE. */
static void limits_denied_child(void) {
    struct rlimit infinite = { RLIM_INFINITY, RLIM_INFINITY };
    struct rlimit current = { 0, 0 };
    struct rlimit wanted = { 0, 0 };
    struct rlimit back = { 0, 0 };
    int resource = finite_raise_target(&current);

    ERROR(syscall(SYS_setrlimit, RLIMIT_NOFILE, &infinite), EPERM, "setrlimit-above-nr-open");
    ERROR(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &infinite, NULL), EPERM,
          "prlimit64-above-nr-open");
    if (resource < 0) {
        check(0, "finite-raise-target-unprivileged");
        return;
    }
    if (!drop_privileges()) {
        check(0, "limits-drop-privileges");
        return;
    }
    wanted.rlim_cur = current.rlim_max + 1;
    wanted.rlim_max = current.rlim_max + 1;
    ERROR(syscall(SYS_setrlimit, resource, &wanted), EPERM, "setrlimit-raise-denied");
    ERROR(syscall(SYS_prlimit64, 0, (unsigned int)resource, &wanted, NULL), EPERM,
          "prlimit64-raise-denied");
    check(syscall(SYS_getrlimit, resource, &back) == 0
              && back.rlim_cur == current.rlim_cur && back.rlim_max == current.rlim_max,
          "denied-raise-keeps-limit");
    /* Reading another process is denied on identity mismatch without
     * CAP_SYS_RESOURCE, while the caller may always read its own. */
    ERROR(syscall(SYS_prlimit64, getppid(), RLIMIT_NOFILE, NULL, &back), EPERM,
          "prlimit64-foreign-process-denied");
    ERROR(syscall(SYS_prlimit64, getppid(), RLIMIT_NOFILE, &infinite, NULL), EPERM,
          "prlimit64-foreign-write-denied");
    check(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &back) == 0,
          "prlimit64-own-process-allowed");
}

/* prlimit64 can address another process, and the admission for that is the
 * matching real uid/gid or CAP_SYS_RESOURCE over the target: a helper that
 * shares this root identity is fair game, while the caller's own limits are
 * untouched.  The helper blocks on a pipe so the change lands on a live,
 * unrelated task rather than on a zombie. */
static void limits_cross_process_child(void) {
    struct rlimit lowered = { 64, 64 };
    struct rlimit observed = { 0, 0 };
    struct rlimit own = { 0, 0 };
    int fds[2];
    int status = 0;
    char byte = 0;

    check(pipe(fds) == 0, "cross-process-pipe");
    pid_t helper = fork();
    if (helper == 0) {
        close(fds[1]);
        /* Bounded so a lost wakeup cannot hang the guest. */
        alarm(10);
        ssize_t read_bytes = read(fds[0], &byte, 1);
        (void)read_bytes;
        _exit(0);
    }
    if (helper <= 0) {
        check(0, "cross-process-helper-fork");
        return;
    }
    close(fds[0]);

    check(syscall(SYS_prlimit64, helper, RLIMIT_NOFILE, &lowered, &observed) == 0,
          "prlimit64-other-process-set");
    check(observed.rlim_cur != lowered.rlim_cur || observed.rlim_max != lowered.rlim_max,
          "prlimit64-other-process-returns-previous");
    check(syscall(SYS_prlimit64, helper, RLIMIT_NOFILE, NULL, &observed) == 0,
          "prlimit64-other-process-read");
    check(observed.rlim_cur == 64 && observed.rlim_max == 64, "prlimit64-other-process-effect");
    check(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &own) == 0, "prlimit64-self-read");
    check(own.rlim_cur != 64, "prlimit64-self-unaffected");

    check(close(fds[1]) == 0, "cross-process-release-helper");
    check(waitpid(helper, &status, 0) == helper, "cross-process-helper-reaped");
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "cross-process-helper-status");
}

static void limits_case(void) {
    struct rlimit nofile = { 0, 0 };
    struct rlimit stack = { 0, 0 };
    struct rlimit mirror = { 0, 0 };
    struct rlimit invalid = { 1, 1 };

    begin("identity-limits.raw-differential");
    check(syscall(SYS_getrlimit, RLIMIT_NOFILE, &nofile) == 0, "getrlimit-nofile");
    check(nofile.rlim_cur <= nofile.rlim_max, "getrlimit-soft-within-hard");
    check(syscall(SYS_getrlimit, RLIMIT_STACK, &stack) == 0, "getrlimit-stack");
    check(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &mirror) == 0, "prlimit64-read-nofile");
    check(mirror.rlim_cur == nofile.rlim_cur && mirror.rlim_max == nofile.rlim_max,
          "prlimit64-agrees-with-getrlimit");
    check(syscall(SYS_prlimit64, 0, RLIMIT_STACK, NULL, &mirror) == 0, "prlimit64-read-stack");
    check(mirror.rlim_cur == stack.rlim_cur && mirror.rlim_max == stack.rlim_max,
          "prlimit64-agrees-on-stack");

    /* The resource number is validated before the result is copied out ... */
    ERROR(syscall(SYS_getrlimit, RLIM_NLIMITS, &nofile), EINVAL, "getrlimit-invalid-resource");
    ERROR(syscall(SYS_getrlimit, RLIM_NLIMITS, NULL), EINVAL, "getrlimit-resource-before-copy");
    ERROR(syscall(SYS_getrlimit, RLIM_NLIMITS, (void *)1), EINVAL,
          "getrlimit-resource-before-bad-pointer");
    /* ... and the caller's buffer is faulted only afterwards. */
    ERROR(syscall(SYS_getrlimit, RLIMIT_NOFILE, NULL), EFAULT, "getrlimit-null-buffer");
    ERROR(syscall(SYS_getrlimit, RLIMIT_NOFILE, (void *)1), EFAULT, "getrlimit-bad-buffer");
    mark("GETRLIMIT_SNAPSHOT_AND_ERRORS");

    /* setrlimit and prlimit64 copy the replacement first, so a bad pointer
     * outranks the resource number, and only then validate the pair. */
    ERROR(syscall(SYS_setrlimit, RLIMIT_NOFILE, NULL), EFAULT, "setrlimit-null-buffer");
    ERROR(syscall(SYS_setrlimit, RLIMIT_NLIMITS, NULL), EFAULT, "setrlimit-copy-before-resource");
    ERROR(syscall(SYS_setrlimit, RLIMIT_NLIMITS, &invalid), EINVAL, "setrlimit-invalid-resource");
    ERROR(syscall(SYS_prlimit64, 0, RLIMIT_NLIMITS, (void *)1, NULL), EFAULT,
          "prlimit64-copy-before-resource");
    ERROR(syscall(SYS_prlimit64, 0, RLIMIT_NLIMITS, NULL, NULL), EINVAL,
          "prlimit64-invalid-resource");
    ERROR(syscall(SYS_prlimit64, 0, RLIMIT_NLIMITS, &invalid, &mirror), EINVAL,
          "prlimit64-invalid-resource-with-both-pointers");
    struct rlimit inverted = { 2, 1 };
    ERROR(syscall(SYS_setrlimit, RLIMIT_NOFILE, &inverted), EINVAL, "setrlimit-soft-above-hard");
    ERROR(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &inverted, NULL), EINVAL,
          "prlimit64-soft-above-hard");
    /* A pid that cannot exist is resolved before the resource number. */
    ERROR(syscall(SYS_prlimit64, 0x7ffffff0, RLIMIT_NOFILE, NULL, &mirror), ESRCH,
          "prlimit64-no-such-process");
    ERROR(syscall(SYS_prlimit64, 0x7ffffff0, RLIMIT_NLIMITS, NULL, NULL), ESRCH,
          "prlimit64-pid-before-resource");
    mark("SETRLIMIT_AND_PRLIMIT64_VALIDATION_ORDER");

    in_child("limits-readback-child", limits_readback_child);
    in_child("limits-denied-child", limits_denied_child);
    mark("LIMIT_MUTATION_AND_CAPABILITY_BRANCH");

    in_child("limits-cross-process-child", limits_cross_process_child);
    mark("PRLIMIT64_CROSS_PROCESS_ADMISSION");

    /* The parent's own limits are exactly what they were before the children. */
    check(syscall(SYS_getrlimit, RLIMIT_NOFILE, &mirror) == 0 && mirror.rlim_cur == nofile.rlim_cur
              && mirror.rlim_max == nofile.rlim_max,
          "limit-changes-stayed-in-children");
    mark("LIMIT_CHANGES_STAY_IN_CHILD");
    done();
}

/* ------------------------------------------------------------------ *
 * identity-usage: getrusage
 * ------------------------------------------------------------------ */

/* Volatile so the -O2 build cannot fold the busy loop away: the CPU time it
 * burns has to show up in the ledger the assertions below read. */
static volatile unsigned long cpu_sink;

static void burn_cpu_ms(long milliseconds) {
    struct timespec start, now;

    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        for (long index = 0; index < milliseconds * 200000; index++) {
            cpu_sink += (unsigned long)index;
        }
        return;
    }
    for (;;) {
        for (int index = 0; index < 20000; index++) {
            cpu_sink += (unsigned long)index;
        }
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
            break;
        }
        long elapsed = (now.tv_sec - start.tv_sec) * 1000
                       + (now.tv_nsec - start.tv_nsec) / 1000000;
        if (elapsed >= milliseconds) {
            break;
        }
    }
}

static void usage_case(void) {
    struct rusage self_before, self_after, thread_before, thread_after;
    struct rusage children_before, children_after, children_waited;
    int status = 0;

    begin("identity-usage.raw-differential");
    /* The selector is validated before the result is copied, so an unknown
     * selector wins over a null buffer and the copy failure is EFAULT.
     * RUSAGE_SELF is 0, RUSAGE_CHILDREN is -1 and RUSAGE_THREAD is 1; every
     * other value, including the historical "both" selector, is EINVAL. */
    ERROR(syscall(SYS_getrusage, 2, &self_before), EINVAL, "getrusage-unknown-selector");
    ERROR(syscall(SYS_getrusage, -2, &self_before), EINVAL, "getrusage-both-selector");
    ERROR(syscall(SYS_getrusage, INT_MIN, &self_before), EINVAL, "getrusage-low-selector");
    ERROR(syscall(SYS_getrusage, -2, NULL), EINVAL, "getrusage-selector-before-copy");
    ERROR(syscall(SYS_getrusage, 2, NULL), EINVAL, "getrusage-selector-before-null-copy");
    ERROR(syscall(SYS_getrusage, RUSAGE_SELF, NULL), EFAULT, "getrusage-self-null");
    ERROR(syscall(SYS_getrusage, RUSAGE_THREAD, (void *)1), EFAULT, "getrusage-thread-bad-pointer");
    ERROR(syscall(SYS_getrusage, RUSAGE_CHILDREN, (void *)1), EFAULT,
          "getrusage-children-bad-pointer");
    mark("SELECTOR_VALIDATION_ORDER");

    /* Both ledgers must charge this process's own CPU time.  RUSAGE_THREAD and
     * RUSAGE_SELF are produced by different accounting paths, so the two
     * readings are compared with a tolerance far above their rounding
     * difference and far below the CPU time the loop below burns. */
    check(syscall(SYS_getrusage, RUSAGE_THREAD, &thread_before) == 0, "getrusage-thread");
    check(syscall(SYS_getrusage, RUSAGE_SELF, &self_before) == 0, "getrusage-self");
    check(self_before.ru_maxrss > 0, "self-maxrss-recorded");

    burn_cpu_ms(300);
    check(syscall(SYS_getrusage, RUSAGE_THREAD, &thread_after) == 0, "getrusage-thread-after-cpu");
    check(syscall(SYS_getrusage, RUSAGE_SELF, &self_after) == 0, "getrusage-self-after-cpu");
    check(timeval_lt(thread_before.ru_utime, thread_after.ru_utime), "thread-utime-advances");
    check(timeval_lt(self_before.ru_utime, self_after.ru_utime), "self-utime-advances");
    check(rusage_cpu_us(&thread_after) - rusage_cpu_us(&self_after) <= 100000
              && rusage_cpu_us(&self_after) - rusage_cpu_us(&thread_after) <= 100000,
          "thread-and-self-agree");
    mark("SELF_AND_THREAD_ACCOUNTING");

    /* CHILDREN is a durable ledger of reaped children: the parent's own CPU
     * time must not land in it. */
    check(syscall(SYS_getrusage, RUSAGE_CHILDREN, &children_before) == 0, "getrusage-children");
    burn_cpu_ms(200);
    check(syscall(SYS_getrusage, RUSAGE_CHILDREN, &children_after) == 0,
          "getrusage-children-after-self-cpu");
    check(timeval_eq(children_before.ru_utime, children_after.ru_utime)
              && timeval_eq(children_before.ru_stime, children_after.ru_stime),
          "children-excludes-own-cpu");
    mark("CHILDREN_LEDGER_EXCLUDES_SELF");

    pid_t child = fork();
    if (child == 0) {
        burn_cpu_ms(300);
        _exit(0);
    }
    check(child > 0, "usage-fork");
    if (child > 0) {
        check(waitpid(child, &status, 0) == child, "usage-wait");
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "usage-child-status");
        check(syscall(SYS_getrusage, RUSAGE_CHILDREN, &children_waited) == 0,
              "getrusage-children-after-wait");
        check(timeval_lt(children_after.ru_utime, children_waited.ru_utime)
                  || timeval_lt(children_after.ru_stime, children_waited.ru_stime),
              "children-cpu-charged-on-wait");
    }
    mark("CHILDREN_LEDGER_AFTER_WAIT");
    done();
}

/* ------------------------------------------------------------------ *
 * identity-personality: personality
 * ------------------------------------------------------------------ */

static void personality_case(void) {
    struct utsname base, compat, uname26;
    long raw = syscall(SYS_personality, PERSONALITY_QUERY);
    unsigned int original;

    begin("identity-personality.raw-differential");
    check(raw >= 0 && raw != -1, "personality-query");
    original = (unsigned int)raw;
    check(syscall(SYS_personality, original) == (long)original, "personality-idempotent-return");

    unsigned int flagged = original | PERSONALITY_ADDR_NO_RANDOMIZE;
    check(syscall(SYS_personality, flagged) == (long)original, "personality-returns-prior");
    check(syscall(SYS_personality, PERSONALITY_QUERY) == (long)flagged, "personality-stores-bits");
    check(syscall(SYS_personality, original) == (long)flagged, "personality-restore-returns-prior");
    check(syscall(SYS_personality, PERSONALITY_QUERY) == (long)original, "personality-restored");
    mark("QUERY_AND_REPLACE_RETURNS_PRIOR");

    /* The value a query returns is the state uname(2) reads. */
    check(uname(&base) == 0, "uname-native");
    check(syscall(SYS_personality, PERSONALITY_PER_LINUX32) == (long)original, "personality-linux32");
    check(uname(&compat) == 0, "uname-linux32");
    check(strcmp(compat.machine, "i686") == 0, "linux32-overrides-machine");
    check(strcmp(compat.sysname, base.sysname) == 0 && strcmp(compat.release, base.release) == 0
              && strcmp(compat.nodename, base.nodename) == 0,
          "linux32-keeps-other-fields");
    check(syscall(SYS_personality, original) == (long)PERSONALITY_PER_LINUX32,
          "personality-linux32-restore");

    check(syscall(SYS_personality, original | PERSONALITY_UNAME26) == (long)original,
          "personality-uname26");
    check(uname(&uname26) == 0, "uname-uname26");
    check(strncmp(uname26.release, "2.6.", 4) == 0, "uname26-maps-release-to-2.6");
    check(strcmp(uname26.release, base.release) != 0, "uname26-replaces-release");
    check(strcmp(uname26.machine, base.machine) == 0 && strcmp(uname26.sysname, base.sysname) == 0,
          "uname26-keeps-machine");
    check(syscall(SYS_personality, original) == (long)(original | PERSONALITY_UNAME26),
          "personality-uname26-restore");
    check(syscall(SYS_personality, PERSONALITY_QUERY) == (long)original, "personality-final-state");
    mark("UNAME_OVERRIDES_FOLLOW_PERSONALITY");
    done();
}

/* ------------------------------------------------------------------ *
 * identity-uts: sethostname, setdomainname
 * ------------------------------------------------------------------ */

/* Both setters check CAP_SYS_ADMIN in the user namespace that owns the UTS
 * namespace before they look at the length or touch the caller's buffer. */
static void uts_denied_child(void) {
    char name[8] = "abc";

    if (!drop_privileges()) {
        check(0, "uts-drop-privileges");
        return;
    }
    ERROR(syscall(SYS_sethostname, name, 3), EPERM, "sethostname-unprivileged");
    ERROR(syscall(SYS_sethostname, NULL, 65), EPERM, "sethostname-capability-before-length");
    ERROR(syscall(SYS_sethostname, NULL, 1), EPERM, "sethostname-capability-before-usercopy");
    ERROR(syscall(SYS_setdomainname, name, 3), EPERM, "setdomainname-unprivileged");
    ERROR(syscall(SYS_setdomainname, NULL, 65), EPERM, "setdomainname-capability-before-length");
    ERROR(syscall(SYS_setdomainname, NULL, 1), EPERM, "setdomainname-capability-before-usercopy");
}

/* A private UTS namespace is the only place a successful change may happen:
 * the copy is a raw byte array of at most 64 bytes, NUL padded, and the parent
 * namespace keeps its names. */
static void uts_private_child(void) {
    struct utsname before, after;
    unsigned char hostname[64];
    unsigned char domainname[64];

    check(uname(&before) == 0, "private-uname-before");
    if (syscall(SYS_unshare, CLONE_NEWUTS) != 0) {
        check(0, "unshare-newuts");
        return;
    }
    check(uname(&after) == 0, "private-uname-after-unshare");
    check(strcmp(after.nodename, before.nodename) == 0, "new-uts-copies-nodename");
    check(strcmp(after.domainname, before.domainname) == 0, "new-uts-copies-domainname");

    for (int index = 0; index < 64; index++) {
        hostname[index] = (unsigned char)(index == 63 ? 0xff : 'a' + index % 26);
    }
    check(syscall(SYS_sethostname, hostname, 64) == 0, "sethostname-64-bytes");
    check(uname(&after) == 0, "private-uname-after-hostname");
    check(memcmp(after.nodename, hostname, 64) == 0 && after.nodename[64] == '\0',
          "nodename-keeps-64-raw-bytes");
    check(syscall(SYS_sethostname, "k42", 3) == 0, "sethostname-shorter");
    check(uname(&after) == 0, "private-uname-after-shorter");
    check(strcmp(after.nodename, "k42") == 0, "shorter-name-clears-tail");

    for (int index = 0; index < 64; index++) {
        domainname[index] = (unsigned char)(0x80 + index);
    }
    check(syscall(SYS_setdomainname, domainname, 64) == 0, "setdomainname-64-bytes");
    check(uname(&after) == 0, "private-uname-after-domainname");
    check(memcmp(after.domainname, domainname, 64) == 0 && after.domainname[64] == '\0',
          "domainname-keeps-64-raw-bytes");
    check(strcmp(after.nodename, "k42") == 0, "domainname-keeps-nodename");
}

static void uts_case(void) {
    struct utsname before, after;
    char long_name[80];

    begin("identity-uts.raw-differential");
    memset(long_name, 'a', sizeof(long_name));
    check(uname(&before) == 0, "uts-uname-before");

    /* Length is validated before the copy, and the copy failure is EFAULT. */
    ERROR(syscall(SYS_sethostname, long_name, 65), EINVAL, "sethostname-over-limit");
    ERROR(syscall(SYS_sethostname, long_name, -1), EINVAL, "sethostname-negative-length");
    ERROR(syscall(SYS_sethostname, NULL, 1), EFAULT, "sethostname-null-name");
    ERROR(syscall(SYS_sethostname, (void *)1, 4), EFAULT, "sethostname-bad-name");
    mark("HOSTNAME_ERROR_ORDER");

    ERROR(syscall(SYS_setdomainname, long_name, 65), EINVAL, "setdomainname-over-limit");
    ERROR(syscall(SYS_setdomainname, long_name, -1), EINVAL, "setdomainname-negative-length");
    ERROR(syscall(SYS_setdomainname, NULL, 1), EFAULT, "setdomainname-null-name");
    mark("DOMAINNAME_ERROR_ORDER");

    in_child("uts-denied-child", uts_denied_child);
    in_child("uts-private-child", uts_private_child);
    mark("PRIVATE_UTS_MUTATION");

    check(uname(&after) == 0, "uts-uname-after");
    check(strcmp(after.nodename, before.nodename) == 0, "global-nodename-unchanged");
    check(strcmp(after.domainname, before.domainname) == 0, "global-domainname-unchanged");
    mark("GLOBAL_UTS_NAMES_UNCHANGED");
    done();
}

int main(void) {
    ids_case();
    switch_case();
    groups_case();
    fsids_case();
    limits_case();
    usage_case();
    personality_case();
    uts_case();

    fflush(stdout);
    if (failures != 0) {
        fprintf(stderr, "THEKERNEL_IDENTITY_FAILURES %d\n", failures);
        return 1;
    }
    puts("THEKERNEL_IDENTITY_OK");
    return 0;
}
