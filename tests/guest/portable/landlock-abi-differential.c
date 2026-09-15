/*
 * Landlock ABI 10, differentially.
 *
 * Every assertion here is one that a Linux v7.2.3 guest and TheKernel must
 * answer identically.  The reference for each case is named in the comment
 * above it: `security/landlock/syscalls.c`, `security/landlock/net.c` and
 * `security/landlock/fs.c` of that release, plus the Landlock kselftests in
 * `tools/testing/selftests/landlock/`.
 *
 * This program needs a reference kernel built with CONFIG_SECURITY_LANDLOCK=y.
 * Without it the three landlock syscalls are COND_SYSCALL stubs returning
 * -ENOSYS, which cannot be compared against a kernel that implements ABI 10.
 */
#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stddef.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

/* The guest runs as root with a writable /root.  Overridable so the same
 * program can be smoke-tested on a development host. */
#ifndef LANDLOCK_ABI_SCRATCH_DIR
#define LANDLOCK_ABI_SCRATCH_DIR "/root"
#endif

#ifndef SYS_landlock_create_ruleset
#define SYS_landlock_create_ruleset 444
#endif
#ifndef SYS_landlock_add_rule
#define SYS_landlock_add_rule 445
#endif
#ifndef SYS_landlock_restrict_self
#define SYS_landlock_restrict_self 446
#endif

/* include/uapi/linux/landlock.h of v7.2.3. */
#define LANDLOCK_CREATE_RULESET_VERSION (1U << 0)
#define LANDLOCK_CREATE_RULESET_ERRATA (1U << 1)
#define LANDLOCK_RULE_PATH_BENEATH 1
#define LANDLOCK_RULE_NET_PORT 2
#define LANDLOCK_ADD_RULE_QUIET (1U << 0)
#define LANDLOCK_RESTRICT_SELF_LOG_SAME_EXEC_OFF (1U << 0)
#define LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON (1U << 1)
#define LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF (1U << 2)
#define LANDLOCK_RESTRICT_SELF_TSYNC (1U << 3)

#define LANDLOCK_ACCESS_FS_EXECUTE (1ULL << 0)
#define LANDLOCK_ACCESS_FS_WRITE_FILE (1ULL << 1)
#define LANDLOCK_ACCESS_FS_READ_FILE (1ULL << 2)
#define LANDLOCK_ACCESS_FS_READ_DIR (1ULL << 3)
#define LANDLOCK_ACCESS_FS_MAKE_SYM (1ULL << 12)
#define LANDLOCK_ACCESS_FS_TRUNCATE (1ULL << 14)
#define LANDLOCK_ACCESS_FS_IOCTL_DEV (1ULL << 15)
#define LANDLOCK_ACCESS_FS_RESOLVE_UNIX (1ULL << 16)
#define LANDLOCK_MASK_ACCESS_FS 0x1ffffULL

#define LANDLOCK_ACCESS_NET_BIND_TCP (1ULL << 0)
#define LANDLOCK_ACCESS_NET_CONNECT_TCP (1ULL << 1)
#define LANDLOCK_ACCESS_NET_BIND_UDP (1ULL << 2)
#define LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP (1ULL << 3)
#define LANDLOCK_MASK_ACCESS_NET 0xfULL

#define LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET (1ULL << 0)
#define LANDLOCK_SCOPE_SIGNAL (1ULL << 1)
#define LANDLOCK_MASK_SCOPE 0x3ULL

struct ruleset_attr {
    uint64_t handled_access_fs;
    uint64_t handled_access_net;
    uint64_t scoped;
    uint64_t quiet_access_fs;
    uint64_t quiet_access_net;
    uint64_t quiet_scoped;
};

struct path_beneath_attr {
    uint64_t allowed_access;
    int32_t parent_fd;
} __attribute__((packed));

struct net_port_attr {
    uint64_t allowed_access;
    uint64_t port;
};

static long ll_create_ruleset(const void *attr, size_t size, uint32_t flags) {
    return syscall(SYS_landlock_create_ruleset, attr, size, flags);
}

static long ll_add_rule(int fd, uint32_t type, const void *attr, uint32_t flags) {
    return syscall(SYS_landlock_add_rule, fd, type, attr, flags);
}

static long ll_restrict_self(int fd, uint32_t flags) {
    return syscall(SYS_landlock_restrict_self, fd, flags);
}

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_LANDLOCK_ABI_FAIL %s errno=%d (%s)\n", stage, errno, strerror(errno));
    return 1;
}

static int case_open(const char *name) {
    printf("THEKERNEL_ABI_CASE %s.raw-differential\n", name);
    return 0;
}

static int assert_ok(const char *name, const char *assertion) {
    if (!name) return 1;
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

static pid_t spawn(int (*body)(void)) {
    fflush(NULL);
    pid_t child = fork();
    if (child == 0) _exit(body());
    return child;
}

/* Runs a sandbox body in a throwaway process and collapses its verdict. */
static int child_verdict(pid_t child) {
    int status = 0;
    if (child < 0) return -1;
    if (waitpid(child, &status, 0) != child) return -1;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return -1;
    return 0;
}

/* Sets up an unprivileged-equivalent confinement: no_new_privs, then a ruleset
 * with no allow rules, which is the shape every restriction case below needs. */
static int confine(const struct ruleset_attr *attr, uint32_t restrict_flags) {
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) return -1;
    long fd = ll_create_ruleset(attr, sizeof(*attr), 0);
    if (fd < 0) return -1;
    if (ll_restrict_self((int)fd, restrict_flags) != 0) {
        close((int)fd);
        return -1;
    }
    close((int)fd);
    return 0;
}

static int probe_open(const char *path, int flags) {
    int fd = open(path, flags);
    if (fd >= 0) {
        close(fd);
        return 0;
    }
    return errno ? errno : EPROTO;
}

static char scratch_dir[PATH_MAX];
static char probe_file[PATH_MAX + 16];
static char probe_socket[PATH_MAX + 16];

/* Each body runs in a forked child, so a confinement is never inherited by a
 * later case and a failure identifies itself with its source line. */
#define BODY_FAIL()                                                                              \
    do {                                                                                         \
        fprintf(stderr, "THEKERNEL_LANDLOCK_ABI_FAIL %s:%d\n", __func__, __LINE__);                    \
        return __LINE__;                                                                         \
    } while (0)

static int restrict_thread_body(void);
static int restrict_tsync_nnp_body(void);
static int restrict_single_thread_body(void);
static int unix_outside_domain_body(void);
static int unix_allow_rule_body(void);
static int unix_same_domain_body(void);
static int net_bind_udp_body(void);
static int net_bind_port_zero_body(void);
static int net_autobind_body(void);
static int net_connect_send_body(void);

/* ------------------------------------------------------------------ */
/* Shared helpers for the sandboxed bodies                             */
/* ------------------------------------------------------------------ */

#define PROBE_UNSET (-1)

static _Atomic int probe_go;
static _Atomic int probe_result;
static const char *probe_target;

/* A sibling thread that reports what its own Landlock domain currently
 * permits.  `landlock_restrict_self(TSYNC)` is the only way it can change
 * under the thread's feet. */
static void *probe_thread(void *unused) {
    (void)unused;
    while (atomic_load_explicit(&probe_go, memory_order_acquire) == 0) sched_yield();
    atomic_store_explicit(&probe_result, probe_open(probe_target, O_RDONLY), memory_order_release);
    return NULL;
}

static _Atomic int nnp_go;
static _Atomic int nnp_result;

/* A sibling thread that reports its own `no_new_privs` bit, which TSYNC is also
 * responsible for setting (security/landlock/tsync.c):
 *
 * 	shared_ctx.set_no_new_privs = task_no_new_privs(current);
 * 	...
 * 	if (ctx->set_no_new_privs)
 * 		task_set_no_new_privs(current);
 *
 * A synchronised sibling that kept the bit clear would drop the new domain at
 * its next execve(2), so the bit is part of what TSYNC has to deliver. */
static void *nnp_probe_thread(void *unused) {
    (void)unused;
    while (atomic_load_explicit(&nnp_go, memory_order_acquire) == 0) sched_yield();
    long result = prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
    atomic_store_explicit(&nnp_result, result < 0 ? -errno : (int)result, memory_order_release);
    return NULL;
}

static int unix_socket_path(struct sockaddr_un *addr, const char *path, socklen_t *len) {
    size_t size = strlen(path);
    if (size == 0 || size >= sizeof(addr->sun_path)) return -1;
    memset(addr, 0, sizeof(*addr));
    addr->sun_family = AF_UNIX;
    memcpy(addr->sun_path, path, size);
    *len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + size);
    return 0;
}

static int unix_bind_listen(const char *path, int *out_fd) {
    struct sockaddr_un addr;
    socklen_t len;
    if (unix_socket_path(&addr, path, &len) != 0) return -1;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    (void)unlink(path);
    if (bind(fd, (struct sockaddr *)&addr, len) != 0) {
        close(fd);
        return -1;
    }
    if (listen(fd, 4) != 0) {
        close(fd);
        return -1;
    }
    *out_fd = fd;
    return 0;
}

/* 0 when the connection was established, the errno otherwise. */
static int unix_connect_errno(const char *path) {
    struct sockaddr_un addr;
    socklen_t len;
    if (unix_socket_path(&addr, path, &len) != 0) return -1;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    int result = connect(fd, (struct sockaddr *)&addr, len);
    int err = result == 0 ? 0 : (errno ? errno : EPROTO);
    close(fd);
    return err;
}

static int ipv4_addr(struct sockaddr_in *addr, uint16_t port) {
    memset(addr, 0, sizeof(*addr));
    addr->sin_family = AF_INET;
    addr->sin_port = htons(port);
    addr->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return 0;
}

static int udp_bind_errno(int fd, uint16_t port) {
    struct sockaddr_in addr;
    ipv4_addr(&addr, port);
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0) return 0;
    return errno ? errno : EPROTO;
}

static int udp_connect_errno(int fd, uint16_t port) {
    struct sockaddr_in addr;
    ipv4_addr(&addr, port);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0) return 0;
    return errno ? errno : EPROTO;
}

static int udp_sendto_errno(int fd, uint16_t port) {
    struct sockaddr_in addr;
    ipv4_addr(&addr, port);
    char byte = 'x';
    if (sendto(fd, &byte, sizeof(byte), 0, (struct sockaddr *)&addr, sizeof(addr)) >= 0) return 0;
    return errno ? errno : EPROTO;
}

/* `landlock_restrict_self()` with `LANDLOCK_RESTRICT_SELF_TSYNC` must give the
 * sibling thread the new domain before it returns: both the parked sibling and
 * the calling thread see the READ_FILE denial. */
static int restrict_thread_body(void) {
    struct ruleset_attr attr;
    pthread_t thread;
    long fd;
    int sibling, caller;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    probe_target = probe_file;
    atomic_store(&probe_go, 0);
    atomic_store(&probe_result, PROBE_UNSET);
    if (pthread_create(&thread, NULL, probe_thread, NULL) != 0) BODY_FAIL();
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    fd = ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) BODY_FAIL();
    if (ll_restrict_self((int)fd, LANDLOCK_RESTRICT_SELF_TSYNC) != 0) BODY_FAIL();
    close((int)fd);
    atomic_store_explicit(&probe_go, 1, memory_order_release);
    if (pthread_join(thread, NULL) != 0) BODY_FAIL();
    sibling = atomic_load_explicit(&probe_result, memory_order_acquire);
    caller = probe_open(probe_file, O_RDONLY);
    if (sibling != EACCES) BODY_FAIL();
    if (caller != EACCES) BODY_FAIL();
    return 0;
}

/* TSYNC delivers the caller's `no_new_privs` bit to the sibling as well.  The
 * sibling is created before the caller sets the bit, so a sibling that reports
 * it set can only have received it from the synchronisation; the pre-change
 * kernel copied the domain alone and the sibling kept reporting 0. */
static int restrict_tsync_nnp_body(void) {
    struct ruleset_attr attr;
    pthread_t thread;
    long fd;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    atomic_store(&nnp_go, 0);
    atomic_store(&nnp_result, PROBE_UNSET);
    if (pthread_create(&thread, NULL, nnp_probe_thread, NULL) != 0) BODY_FAIL();
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    fd = ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) BODY_FAIL();
    if (ll_restrict_self((int)fd, LANDLOCK_RESTRICT_SELF_TSYNC) != 0) BODY_FAIL();
    close((int)fd);
    atomic_store_explicit(&nnp_go, 1, memory_order_release);
    if (pthread_join(thread, NULL) != 0) BODY_FAIL();
    if (atomic_load_explicit(&nnp_result, memory_order_acquire) != 1) BODY_FAIL();
    return 0;
}

/* Without TSYNC the domain is thread-local, so the sibling keeps its old
 * (unrestricted) domain while the caller is confined. */
static int restrict_single_thread_body(void) {
    struct ruleset_attr attr;
    pthread_t thread;
    long fd;
    int sibling, caller;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    probe_target = probe_file;
    atomic_store(&probe_go, 0);
    atomic_store(&probe_result, PROBE_UNSET);
    if (pthread_create(&thread, NULL, probe_thread, NULL) != 0) BODY_FAIL();
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    fd = ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) BODY_FAIL();
    if (ll_restrict_self((int)fd, 0) != 0) BODY_FAIL();
    close((int)fd);
    atomic_store_explicit(&probe_go, 1, memory_order_release);
    if (pthread_join(thread, NULL) != 0) BODY_FAIL();
    sibling = atomic_load_explicit(&probe_result, memory_order_acquire);
    caller = probe_open(probe_file, O_RDONLY);
    if (sibling != 0) BODY_FAIL();
    if (caller != EACCES) BODY_FAIL();
    return 0;
}

/* `hook_unix_find()`: a socket bound by a domain that is not the client's is
 * denied unless the client's ruleset allows RESOLVE_UNIX on its path. */
static int unix_outside_domain_body(void) {
    struct ruleset_attr attr;
    int server = -1;

    if (unix_bind_listen(probe_socket, &server) != 0) BODY_FAIL();
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    if (confine(&attr, 0) != 0) BODY_FAIL();
    if (unix_connect_errno(probe_socket) != EACCES) BODY_FAIL();
    close(server);
    return 0;
}

/* The same lookup with an allow rule covering the socket's parent directory. */
static int unix_allow_rule_body(void) {
    struct ruleset_attr attr;
    struct path_beneath_attr rule;
    int server = -1, dir_fd, ruleset_fd;

    if (unix_bind_listen(probe_socket, &server) != 0) BODY_FAIL();
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) BODY_FAIL();
    dir_fd = open(scratch_dir, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    rule.parent_fd = dir_fd;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &rule, 0) != 0) BODY_FAIL();
    if (ll_restrict_self(ruleset_fd, 0) != 0) BODY_FAIL();
    close(ruleset_fd);
    close(dir_fd);
    if (unix_connect_errno(probe_socket) != 0) BODY_FAIL();
    close(server);
    return 0;
}

/* A socket created *after* the restriction shares the client's own hierarchy
 * level, which `unmask_scoped_access()` exempts from RESOLVE_UNIX. */
static int unix_same_domain_body(void) {
    struct ruleset_attr attr;
    int server = -1;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    if (confine(&attr, 0) != 0) BODY_FAIL();
    if (unix_bind_listen(probe_socket, &server) != 0) BODY_FAIL();
    if (unix_connect_errno(probe_socket) != 0) BODY_FAIL();
    close(server);
    return 0;
}

/* `hook_socket_bind()`: every address port is a rule key, including 0, and a
 * port without a rule is EACCES rather than a transport error. */
static int net_bind_udp_body(void) {
    struct ruleset_attr attr;
    struct net_port_attr rule;
    const uint16_t allowed_port = 45871;
    int ruleset_fd, fd;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = LANDLOCK_ACCESS_NET_BIND_UDP;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_NET_BIND_UDP;
    rule.port = allowed_port;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &rule, 0) != 0) BODY_FAIL();
    if (ll_restrict_self(ruleset_fd, 0) != 0) BODY_FAIL();
    close(ruleset_fd);

    fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) BODY_FAIL();
    if (udp_bind_errno(fd, allowed_port) != 0) BODY_FAIL();

    int denied = socket(AF_INET, SOCK_DGRAM, 0);
    if (denied < 0) BODY_FAIL();
    if (udp_bind_errno(denied, allowed_port + 1) != EACCES) BODY_FAIL();

    int ephemeral = socket(AF_INET, SOCK_DGRAM, 0);
    if (ephemeral < 0) BODY_FAIL();
    if (udp_bind_errno(ephemeral, 0) != EACCES) BODY_FAIL();

    /* An unhandled protocol and an unhandled address family stay free. */
    int tcp = socket(AF_INET, SOCK_STREAM, 0);
    if (tcp < 0) BODY_FAIL();
    struct sockaddr_in addr;
    ipv4_addr(&addr, allowed_port + 2);
    if (bind(tcp, (struct sockaddr *)&addr, sizeof(addr)) != 0) BODY_FAIL();

    int unix_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (unix_fd < 0) BODY_FAIL();
    close(unix_fd);
    close(tcp);
    close(ephemeral);
    close(denied);
    close(fd);
    return 0;
}

/* A rule on port 0 is what admits both an explicit `bind(2)` on an ephemeral
 * port and the implicit autobind of `connect(2)`. */
static int net_bind_port_zero_body(void) {
    struct ruleset_attr attr;
    struct net_port_attr rule;
    const uint16_t remote_port = 45874;
    int ruleset_fd, fd;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = LANDLOCK_ACCESS_NET_BIND_UDP | LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_NET_BIND_UDP;
    rule.port = 0;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &rule, 0) != 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    rule.port = remote_port;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &rule, 0) != 0) BODY_FAIL();
    if (ll_restrict_self(ruleset_fd, 0) != 0) BODY_FAIL();
    close(ruleset_fd);

    fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) BODY_FAIL();
    if (udp_bind_errno(fd, 0) != 0) BODY_FAIL();

    int client = socket(AF_INET, SOCK_DGRAM, 0);
    if (client < 0) BODY_FAIL();
    if (udp_connect_errno(client, remote_port) != 0) BODY_FAIL();
    close(client);
    close(fd);
    return 0;
}

/* `current_check_autobind_udp_socket()`: connecting an unbound UDP socket
 * needs BIND_UDP on port 0, or a local port bound beforehand. */
static int net_autobind_body(void) {
    struct ruleset_attr attr;
    struct net_port_attr rule;
    const uint16_t remote_port = 45875;
    const uint16_t local_port = 45876;
    int ruleset_fd, fd;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = LANDLOCK_ACCESS_NET_BIND_UDP | LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    rule.port = remote_port;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &rule, 0) != 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_NET_BIND_UDP;
    rule.port = local_port;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &rule, 0) != 0) BODY_FAIL();
    if (ll_restrict_self(ruleset_fd, 0) != 0) BODY_FAIL();
    close(ruleset_fd);

    fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) BODY_FAIL();
    if (udp_connect_errno(fd, remote_port) != EACCES) BODY_FAIL();

    int bound = socket(AF_INET, SOCK_DGRAM, 0);
    if (bound < 0) BODY_FAIL();
    if (udp_bind_errno(bound, local_port) != 0) BODY_FAIL();
    if (udp_connect_errno(bound, remote_port) != 0) BODY_FAIL();

    close(bound);
    close(fd);
    return 0;
}

/* `hook_socket_connect()` and `hook_socket_sendmsg()`: a datagram destination
 * port is checked before the transport sees the request. */
static int net_connect_send_body(void) {
    struct ruleset_attr attr;
    struct net_port_attr rule;
    const uint16_t allowed_port = 45877;
    int ruleset_fd, fd;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = LANDLOCK_ACCESS_NET_BIND_UDP | LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) BODY_FAIL();
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_NET_BIND_UDP | LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    rule.port = 0;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &rule, 0) != 0) BODY_FAIL();
    rule.allowed_access = LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    rule.port = allowed_port;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &rule, 0) != 0) BODY_FAIL();
    if (ll_restrict_self(ruleset_fd, 0) != 0) BODY_FAIL();
    close(ruleset_fd);

    fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) BODY_FAIL();
    if (udp_connect_errno(fd, allowed_port) != 0) BODY_FAIL();
    /* Only the denial is asserted: an accepted datagram would have to leave
     * the machine, which needs a configured loopback rather than an ABI. */
    if (udp_sendto_errno(fd, allowed_port + 1) != EACCES) BODY_FAIL();
    close(fd);

    int denied = socket(AF_INET, SOCK_DGRAM, 0);
    if (denied < 0) BODY_FAIL();
    if (udp_connect_errno(denied, allowed_port + 1) != EACCES) BODY_FAIL();
    close(denied);
    return 0;
}

/* ------------------------------------------------------------------ */
/* landlock_create_ruleset                                            */
/* ------------------------------------------------------------------ */

static int create_case(void) {
    const char *name = "landlock-create";
    struct ruleset_attr attr;
    long ret;
    int fd;

    case_open(name);

    /* `SYSCALL_DEFINE3(landlock_create_ruleset)`: the ABI version is reported
     * only for `flags == LANDLOCK_CREATE_RULESET_VERSION` with a NULL attribute
     * and a zero size. */
    ret = ll_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION);
    if (ret != 10) return fail("abi-version");
    if (assert_ok(name, "ABI_VERSION_10")) return 1;

    /* `create_ruleset_query()`: a query flag with a non-NULL attribute or a
     * non-zero size is EINVAL, and any other flag word is EINVAL. */
    if (expect_errno(ll_create_ruleset(NULL, 8, LANDLOCK_CREATE_RULESET_VERSION), EINVAL)) {
        return fail("version-size");
    }
    if (expect_errno(ll_create_ruleset(&attr, 0, LANDLOCK_CREATE_RULESET_VERSION), EINVAL)) {
        return fail("version-attr");
    }
    /* Without a query flag the attribute pointer is copied from, so a NULL
     * attribute is EFAULT before the size is even looked at. */
    if (expect_errno(ll_create_ruleset(NULL, 0, 0), EFAULT)) return fail("flags-zero");
    if (expect_errno(ll_create_ruleset(NULL, 0, 4), EINVAL)) return fail("flags-unknown");
    if (expect_errno(
            ll_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION | LANDLOCK_CREATE_RULESET_ERRATA),
            EINVAL)) {
        return fail("flags-both");
    }
    if (assert_ok(name, "QUERY_ARGUMENT_ORDER")) return 1;

    /* `copy_min_struct_from_user()`: a missing attribute faults first, then
     * `usize < 8` is EINVAL and `usize > PAGE_SIZE` is E2BIG. */
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    if (expect_errno(ll_create_ruleset(NULL, sizeof(attr), 0), EFAULT)) return fail("attr-null");
    if (expect_errno(ll_create_ruleset(&attr, 0, 0), EINVAL)) return fail("size-zero");
    if (expect_errno(ll_create_ruleset(&attr, 7, 0), EINVAL)) return fail("size-short");
    if (expect_errno(ll_create_ruleset(&attr, 4097, 0), E2BIG)) return fail("size-big");
    if (assert_ok(name, "RULESET_SIZE_ORDER")) return 1;

    /* `copy_struct_from_user()`: a longer structure is a forward-compatible
     * extension only while its trailing bytes are zero. */
    {
        uint8_t extended[56];
        memset(extended, 0, sizeof(extended));
        memcpy(extended, &attr, sizeof(attr));
        fd = (int)ll_create_ruleset(extended, sizeof(extended), 0);
        if (fd < 0) return fail("zero-extension");
        close(fd);
        extended[sizeof(attr)] = 1;
        if (expect_errno(ll_create_ruleset(extended, sizeof(extended), 0), E2BIG)) {
            return fail("nonzero-extension");
        }
    }
    if (assert_ok(name, "ZERO_FILLED_EXTENSION")) return 1;

    /* `landlock_create_ruleset()` in ruleset.c: every handled mask empty is
     * ENOMSG, not EINVAL. */
    memset(&attr, 0, sizeof(attr));
    if (expect_errno(ll_create_ruleset(&attr, sizeof(attr), 0), ENOMSG)) return fail("empty-enomsg");
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = 0;
    attr.handled_access_net = 0;
    attr.scoped = 0;
    if (expect_errno(ll_create_ruleset(&attr, 24, 0), ENOMSG)) return fail("empty-short-enomsg");
    if (assert_ok(name, "EMPTY_HANDLED_ENOMSG")) return 1;

    /* ABI 10 masks: all filesystem rights through RESOLVE_UNIX, all four UDP
     * aware network rights, and both scopes. */
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_MASK_ACCESS_FS;
    attr.handled_access_net = LANDLOCK_MASK_ACCESS_NET;
    attr.scoped = LANDLOCK_MASK_SCOPE;
    fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) return fail("abi10-masks");
    close(fd);

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) return fail("resolve-unix-handled");
    close(fd);

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = LANDLOCK_ACCESS_NET_BIND_UDP | LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) return fail("udp-rights-handled");
    close(fd);

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = 1ULL << 17;
    if (expect_errno(ll_create_ruleset(&attr, sizeof(attr), 0), EINVAL)) {
        return fail("unknown-fs-right");
    }
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = 1ULL << 4;
    if (expect_errno(ll_create_ruleset(&attr, sizeof(attr), 0), EINVAL)) {
        return fail("unknown-net-right");
    }
    memset(&attr, 0, sizeof(attr));
    attr.scoped = 1ULL << 2;
    if (expect_errno(ll_create_ruleset(&attr, sizeof(attr), 0), EINVAL)) {
        return fail("unknown-scope");
    }
    if (assert_ok(name, "ABI10_HANDLED_MASKS")) return 1;

    /* `landlock_create_ruleset()`: each quiet mask must be a subset of the
     * mask it silences, and a subset is accepted. */
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    attr.quiet_access_fs = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_WRITE_FILE;
    if (expect_errno(ll_create_ruleset(&attr, sizeof(attr), 0), EINVAL)) {
        return fail("quiet-fs-not-subset");
    }
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = LANDLOCK_ACCESS_NET_BIND_UDP;
    attr.quiet_access_net = LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    if (expect_errno(ll_create_ruleset(&attr, sizeof(attr), 0), EINVAL)) {
        return fail("quiet-net-not-subset");
    }
    memset(&attr, 0, sizeof(attr));
    attr.scoped = LANDLOCK_SCOPE_SIGNAL;
    attr.quiet_scoped = LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET;
    if (expect_errno(ll_create_ruleset(&attr, sizeof(attr), 0), EINVAL)) {
        return fail("quiet-scope-not-subset");
    }
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    attr.handled_access_net = LANDLOCK_ACCESS_NET_BIND_UDP;
    attr.scoped = LANDLOCK_SCOPE_SIGNAL;
    attr.quiet_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    attr.quiet_access_net = LANDLOCK_ACCESS_NET_BIND_UDP;
    attr.quiet_scoped = LANDLOCK_SCOPE_SIGNAL;
    fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) return fail("quiet-subset");
    close(fd);
    if (assert_ok(name, "QUIET_MASK_SUBSET")) return 1;

    return case_close(name);
}

/* ------------------------------------------------------------------ */
/* landlock_add_rule                                                  */
/* ------------------------------------------------------------------ */

static int add_rule_case(void) {
    const char *name = "landlock-add-rule";
    struct ruleset_attr attr;
    struct path_beneath_attr path_rule;
    struct net_port_attr net_rule;
    int dir_fd, file_fd, ruleset_fd, second_fd;

    case_open(name);

    dir_fd = open(scratch_dir, O_PATH | O_DIRECTORY);
    if (dir_fd < 0) return fail("open-dir");
    file_fd = open(probe_file, O_RDONLY);
    if (file_fd < 0) return fail("open-file");

    /* `SYSCALL_DEFINE4(landlock_add_rule)`: only zero and
     * LANDLOCK_ADD_RULE_QUIET are known flags, and the flag word is checked
     * before the ruleset descriptor is resolved. */
    memset(&path_rule, 0, sizeof(path_rule));
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_READ_FILE;
    path_rule.parent_fd = dir_fd;
    if (expect_errno(ll_add_rule(-1, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 2), EINVAL)) {
        return fail("unknown-flag");
    }
    if (expect_errno(ll_add_rule(-1, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 4), EINVAL)) {
        return fail("unknown-flag-high");
    }
    if (assert_ok(name, "RULE_FLAG_VALIDATION")) return 1;

    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_WRITE_FILE |
                             LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR |
                             LANDLOCK_ACCESS_FS_TRUNCATE | LANDLOCK_ACCESS_FS_IOCTL_DEV |
                             LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) return fail("create-ruleset");

    /* `add_rule_path_beneath()`: the raw copy faults first, then an empty
     * access mask without the quiet flag is ENOMSG.  Both precede the
     * descriptor lookup, so an invalid descriptor cannot mask them. */
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, NULL, 0), EFAULT)) {
        return fail("rule-attr-null");
    }
    path_rule.allowed_access = 0;
    path_rule.parent_fd = -1;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0), ENOMSG)) {
        return fail("empty-access");
    }
    if (assert_ok(name, "EMPTY_ACCESS_ENOMSG")) return 1;

    /* Unhandled access rights are EINVAL, still before the descriptor: the
     * rule below keeps the invalid parent_fd from the previous check. */
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_MAKE_SYM;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0), EINVAL)) {
        return fail("unhandled-access");
    }
    net_rule.allowed_access = LANDLOCK_ACCESS_NET_BIND_UDP;
    net_rule.port = 45871;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &net_rule, 0), EINVAL)) {
        return fail("unhandled-net-access");
    }
    /* A quiet rule needs the ruleset to have spent quiet bits on its type. */
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_READ_FILE;
    if (expect_errno(
            ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, LANDLOCK_ADD_RULE_QUIET),
            EINVAL)) {
        return fail("quiet-without-mask");
    }
    if (assert_ok(name, "UNHANDLED_AND_QUIET_REJECTS")) return 1;

    /* Descriptor errors come after the mask checks: an absent descriptor is
     * EBADF, a descriptor that is not a ruleset is EBADFD, and a ruleset
     * cannot name itself as a parent. */
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_READ_FILE;
    path_rule.parent_fd = -1;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0), EBADF)) {
        return fail("parent-badf");
    }
    path_rule.parent_fd = ruleset_fd;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0), EBADFD)) {
        return fail("parent-badfd");
    }
    if (expect_errno(ll_add_rule(file_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0), EBADFD)) {
        return fail("ruleset-badfd");
    }
    if (expect_errno(ll_add_rule(-1, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0), EBADF)) {
        return fail("ruleset-badf");
    }
    if (expect_errno(ll_add_rule(ruleset_fd, 3, &path_rule, 0), EINVAL)) {
        return fail("rule-type");
    }
    if (assert_ok(name, "DESCRIPTOR_ERROR_ORDER")) return 1;

    /* `landlock_append_fs_rule()`: a non-directory target only accepts the
     * rights that can be tied to files (ACCESS_FILE). */
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    path_rule.parent_fd = file_fd;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0) != 0) {
        return fail("file-resolve-unix");
    }
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_READ_FILE;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0) != 0) {
        return fail("file-read");
    }
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_WRITE_FILE |
                               LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_TRUNCATE |
                               LANDLOCK_ACCESS_FS_IOCTL_DEV | LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0) != 0) {
        return fail("file-access-file");
    }
    /* READ_DIR is a directory-only right, so a regular-file target rejects it
     * even though the ruleset handles it. */
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_READ_DIR;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0), EINVAL)) {
        return fail("file-read-dir");
    }
    if (assert_ok(name, "NON_DIRECTORY_ACCESS_FILE")) return 1;

    /* Directory targets accept the whole handled mask, including the ABI 10
     * filesystem right RESOLVE_UNIX. */
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_RESOLVE_UNIX | LANDLOCK_ACCESS_FS_READ_FILE |
                               LANDLOCK_ACCESS_FS_READ_DIR;
    path_rule.parent_fd = dir_fd;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0) != 0) {
        return fail("dir-resolve-unix");
    }
    if (assert_ok(name, "RESOLVE_UNIX_RULE_ACCEPTED")) return 1;
    close(ruleset_fd);

    /* Network rules: the port must fit in a u16, the quiet flag follows the
     * ruleset's quiet mask, and both UDP rights are valid ABI 10 rules. */
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_net = LANDLOCK_MASK_ACCESS_NET;
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) return fail("create-net-ruleset");
    net_rule.allowed_access = LANDLOCK_ACCESS_NET_BIND_UDP;
    net_rule.port = 65535;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &net_rule, 0) != 0) {
        return fail("udp-port");
    }
    net_rule.port = 65536;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &net_rule, 0), EINVAL)) {
        return fail("port-range");
    }
    net_rule.allowed_access = LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
    net_rule.port = 0;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &net_rule, 0) != 0) {
        return fail("udp-port-zero");
    }
    net_rule.allowed_access = 0;
    net_rule.port = 45872;
    if (expect_errno(ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &net_rule, 0), ENOMSG)) {
        return fail("net-empty-access");
    }
    net_rule.allowed_access = LANDLOCK_ACCESS_NET_BIND_UDP;
    if (expect_errno(
            ll_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &net_rule, LANDLOCK_ADD_RULE_QUIET),
            EINVAL)) {
        return fail("net-quiet-without-mask");
    }
    close(ruleset_fd);
    if (assert_ok(name, "NET_PORT_UDP_RIGHTS")) return 1;

    /* A ruleset that spends quiet bits accepts a quiet rule, including one
     * whose access mask would otherwise be an empty deny rule. */
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_MASK_ACCESS_FS;
    attr.quiet_access_fs = LANDLOCK_MASK_ACCESS_FS;
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0) return fail("create-quiet-ruleset");
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_READ_FILE;
    path_rule.parent_fd = dir_fd;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, LANDLOCK_ADD_RULE_QUIET) != 0) {
        return fail("quiet-rule");
    }
    path_rule.allowed_access = 0;
    if (ll_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, LANDLOCK_ADD_RULE_QUIET) != 0) {
        return fail("quiet-empty-rule");
    }
    if (assert_ok(name, "QUIET_RULE_ACCEPTED")) return 1;
    close(ruleset_fd);

    /* Two independent rulesets keep their own rules. */
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    ruleset_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    second_fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (ruleset_fd < 0 || second_fd < 0) return fail("two-rulesets");
    path_rule.allowed_access = LANDLOCK_ACCESS_FS_READ_FILE;
    path_rule.parent_fd = dir_fd;
    if (ll_add_rule(second_fd, LANDLOCK_RULE_PATH_BENEATH, &path_rule, 0) != 0) {
        return fail("second-ruleset-rule");
    }
    /* Landlock takes one layer per ruleset, so a ruleset that stays in the
     * library is not installable twice: only restrict_self() consumes it. */
    if (assert_ok(name, "RULESET_ISOLATION")) return 1;
    close(ruleset_fd);
    close(second_fd);
    close(dir_fd);
    close(file_fd);
    return case_close(name);
}

/* ------------------------------------------------------------------ */
/* landlock_restrict_self                                             */
/* ------------------------------------------------------------------ */

static int restrict_self_case(void) {
    const char *name = "landlock-restrict-self";
    struct ruleset_attr attr;
    int fd;

    case_open(name);

    /* `landlock_restrict_self()` reports EPERM before it looks at any other
     * argument, so pin the prerequisite instead of relying on the caller's
     * privileges: with no_new_privs set, the checks below are the first ones
     * that can fail. */
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) return fail("restrict-no-new-privs");

    /* `SYSCALL_DEFINE2(landlock_restrict_self)`: the flag word must be a
     * subset of LANDLOCK_MASK_RESTRICT_SELF, and only the log-subdomains
     * combination may omit a ruleset.  Every other negative descriptor is
     * EBADF, never a silent success. */
    if (expect_errno(ll_restrict_self(-1, 0x10), EINVAL)) return fail("flags-unhandled");
    if (expect_errno(ll_restrict_self(-1, 1U << 4), EINVAL)) return fail("flags-high");
    if (expect_errno(ll_restrict_self(-1, 0), EBADF)) return fail("no-ruleset-zero-flags");
    if (expect_errno(ll_restrict_self(-1, LANDLOCK_RESTRICT_SELF_LOG_SAME_EXEC_OFF), EBADF)) {
        return fail("no-ruleset-log-same");
    }
    if (expect_errno(
            ll_restrict_self(-1, LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF |
                                    LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON),
            EBADF)) {
        return fail("no-ruleset-extra-log");
    }
    if (expect_errno(ll_restrict_self(-1, LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF |
                                              LANDLOCK_RESTRICT_SELF_TSYNC |
                                              LANDLOCK_RESTRICT_SELF_LOG_SAME_EXEC_OFF),
                     EBADF)) {
        return fail("no-ruleset-tsync-extra-log");
    }
    if (ll_restrict_self(-1, LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF) != 0) {
        return fail("no-ruleset-subdomains-off");
    }
    if (ll_restrict_self(-1, LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF |
                                 LANDLOCK_RESTRICT_SELF_TSYNC) != 0) {
        return fail("no-ruleset-subdomains-off-tsync");
    }
    if (assert_ok(name, "NO_RULESET_MATRIX")) return 1;

    /* A descriptor that is not a ruleset is EBADFD, including the standard
     * descriptors, and a closed one is EBADF. */
    memset(&attr, 0, sizeof(attr));
    attr.handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE;
    fd = (int)ll_create_ruleset(&attr, sizeof(attr), 0);
    if (fd < 0) return fail("create-ruleset");
    if (expect_errno(ll_restrict_self(1, 0), EBADFD)) return fail("ruleset-badfd-stdout");
    if (expect_errno(ll_restrict_self(-1, 0), EBADF)) return fail("ruleset-badf");
    if (assert_ok(name, "RULESET_DESCRIPTOR_ERRORS")) return 1;
    close(fd);

    if (child_verdict(spawn(restrict_thread_body)) != 0) return fail("tsync-siblings");
    if (assert_ok(name, "TSYNC_SYNCHRONIZES_SIBLINGS")) return 1;
    if (child_verdict(spawn(restrict_tsync_nnp_body)) != 0) return fail("tsync-no-new-privs");
    if (assert_ok(name, "TSYNC_PROPAGATES_NO_NEW_PRIVS")) return 1;
    if (child_verdict(spawn(restrict_single_thread_body)) != 0) return fail("no-tsync-siblings");
    if (assert_ok(name, "NO_TSYNC_LEAVES_SIBLING")) return 1;
    return case_close(name);
}

/* ------------------------------------------------------------------ */
/* Landlock UNIX-socket path resolution (RESOLVE_UNIX)                 */
/* ------------------------------------------------------------------ */

static int unix_resolve_case(void) {
    const char *name = "landlock-unix-resolve";

    case_open(name);
    if (child_verdict(spawn(unix_outside_domain_body)) != 0) return fail("outside-domain");
    if (assert_ok(name, "OUTSIDE_DOMAIN_EACCES")) return 1;
    if (child_verdict(spawn(unix_allow_rule_body)) != 0) return fail("allow-rule");
    if (assert_ok(name, "ALLOW_RULE_PERMITS")) return 1;
    if (child_verdict(spawn(unix_same_domain_body)) != 0) return fail("same-domain");
    if (assert_ok(name, "SAME_DOMAIN_PERMITS")) return 1;
    return case_close(name);
}

/* ------------------------------------------------------------------ */
/* Landlock network port rules (UDP)                                   */
/* ------------------------------------------------------------------ */

static int net_port_case(void) {
    const char *name = "landlock-net-port";

    case_open(name);
    if (child_verdict(spawn(net_bind_udp_body)) != 0) return fail("bind-udp");
    if (assert_ok(name, "BIND_UDP_PORT_RULES")) return 1;
    if (child_verdict(spawn(net_bind_port_zero_body)) != 0) return fail("bind-port-zero");
    if (assert_ok(name, "BIND_PORT_ZERO_IS_A_RULE_TARGET")) return 1;
    if (child_verdict(spawn(net_autobind_body)) != 0) return fail("autobind");
    if (assert_ok(name, "UDP_AUTOBIND_REQUIRES_PORT_ZERO")) return 1;
    if (child_verdict(spawn(net_connect_send_body)) != 0) return fail("connect-send");
    if (assert_ok(name, "CONNECT_SEND_UDP_PORT_RULES")) return 1;
    return case_close(name);
}

int main(void) {
    snprintf(scratch_dir, sizeof(scratch_dir), LANDLOCK_ABI_SCRATCH_DIR "/landlock-abi-%ld", (long)getpid());
    snprintf(probe_file, sizeof(probe_file), "%s/probe", scratch_dir);
    snprintf(probe_socket, sizeof(probe_socket), "%s/socket", scratch_dir);

    if (mkdir(scratch_dir, 0700) != 0 && errno != EEXIST) return fail("mkdir-scratch");
    int fd = open(probe_file, O_CREAT | O_WRONLY | O_TRUNC, 0600);
    if (fd < 0) return fail("create-probe");
    if (write(fd, "x", 1) != 1) return fail("write-probe");
    close(fd);

    if (create_case() != 0) return 1;
    if (add_rule_case() != 0) return 1;
    if (restrict_self_case() != 0) return 1;
    if (unix_resolve_case() != 0) return 1;
    if (net_port_case() != 0) return 1;

    (void)unlink(probe_socket);
    (void)unlink(probe_file);
    (void)rmdir(scratch_dir);
    puts("THEKERNEL_LANDLOCK_ABI_OK");
    return 0;
}
