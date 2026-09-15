/* Differential coverage for the socket provider and address semantics that
 * Linux answers before any transport is reached: creation validation order,
 * the per-family type/protocol tables, and the generic SOL_SOCKET state every
 * `struct sock` carries plus the SOL_NETLINK option table.
 *
 * Every assertion here was read from the Linux 7.2.3 source named in the
 * comment above it and is expected to hold identically under TheKernel, so a
 * mismatch is a real provider-semantics divergence rather than a difference in
 * feature set.  Values that Linux leaves to configuration or to uninitialized
 * kernel memory (buffer minima, the `sin6_scope_id` tail of a 24-byte IPv6
 * address) are deliberately not asserted.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/capability.h>
#include <linux/netlink.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <unistd.h>

#ifndef AF_MAX
#define AF_MAX 46
#endif
#ifndef NETLINK_USERSOCK
#define NETLINK_USERSOCK 2
#endif

static const char *active;
static void begin(const char *name) { active = name; printf("THEKERNEL_ABI_CASE %s\n", name); }
static void mark(const char *name, int good) {
    printf("THEKERNEL_ABI_ASSERT %s %s %s\n", active, name, good ? "pass" : "fail");
    if (!good) { fprintf(stderr, "%s: errno=%d (%s)\n", name, errno, strerror(errno)); exit(1); }
}
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
static void check(const char *name, int good) { if (!good) mark(name, 0); }

static int effective_capability(int capability) {
    struct __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
    struct __user_cap_data_struct data[2] = {{0}, {0}};
    if (syscall(SYS_capget, &header, data) != 0) return 0;
    return (data[capability / 32].effective & (1U << (capability % 32))) != 0;
}

/* `__sys_socket_create` rejects a flag bit outside
 * SOCK_CLOEXEC|SOCK_NONBLOCK with EINVAL, `__sock_create` then tests the
 * family range (EAFNOSUPPORT) before the type range (EINVAL), and only an
 * in-range family reaches its own `create` hook. */
static void creation_order(void) {
    int fd;
    errno = 0;
    mark("FLAG_MASK_EINVAL", socket(AF_INET, SOCK_STREAM | 0x40, 0) == -1 && errno == EINVAL);
    errno = 0;
    mark("TYPE_AT_SOCK_MAX_EINVAL", socket(AF_INET, 11, 0) == -1 && errno == EINVAL);
    errno = 0;
    mark("TYPE_MASK_MAX_EINVAL", socket(AF_INET, 0xf, 0) == -1 && errno == EINVAL);
    /* Both arguments are out of range: the family range wins. */
    errno = 0;
    mark("FAMILY_BEFORE_TYPE", socket(AF_MAX, 11, 0) == -1 && errno == EAFNOSUPPORT);
    errno = 0;
    mark("FAMILY_RANGE_EAFNOSUPPORT", socket(AF_MAX, SOCK_STREAM, 0) == -1 && errno == EAFNOSUPPORT);
    /* Zero and SOCK_RDM are legal *types*; `inet_create` is what refuses them,
     * through the empty `inetsw[type]` list. */
    errno = 0;
    mark("INET_TYPE_ZERO_ESOCKTNOSUPPORT", socket(AF_INET, 0, 0) == -1 && errno == ESOCKTNOSUPPORT);
    errno = 0;
    mark("INET_RDM_ESOCKTNOSUPPORT", socket(AF_INET, SOCK_RDM, 0) == -1 && errno == ESOCKTNOSUPPORT);
    /* `if (protocol < 0 || protocol >= IPPROTO_MAX) return -EINVAL;` runs
     * before the protocol table is consulted, and the uapi enum makes
     * IPPROTO_MAX 263 (`IPPROTO_SMC = 256`, `IPPROTO_MPTCP = 262`), so 262 is
     * a lookup and 263 is the first rejected value. */
    errno = 0;
    mark("INET_PROTOCOL_MISS_EPROTONOSUPPORT",
         socket(AF_INET, SOCK_STREAM, IPPROTO_UDP) == -1 && errno == EPROTONOSUPPORT);
    errno = 0;
    mark("INET_PROTOCOL_BELOW_MAX_EPROTONOSUPPORT",
         socket(AF_INET, SOCK_STREAM, 250) == -1 && errno == EPROTONOSUPPORT);
    errno = 0;
    mark("INET_PROTOCOL_RANGE_EINVAL", socket(AF_INET, SOCK_STREAM, 263) == -1 && errno == EINVAL);
    errno = 0;
    mark("INET_PROTOCOL_FAR_RANGE_EINVAL", socket(AF_INET, SOCK_STREAM, 65536) == -1 && errno == EINVAL);
    /* The range test precedes the SOCK_RAW capability gate, so it is EINVAL
     * for every caller. */
    errno = 0;
    mark("INET_RAW_PROTOCOL_RANGE_EINVAL", socket(AF_INET, SOCK_RAW, 263) == -1 && errno == EINVAL);
    int net_raw = effective_capability(CAP_NET_RAW);
    errno = 0;
    int raw = socket(AF_INET, SOCK_RAW, 250);
    mark("INET_RAW_POLICY", net_raw ? raw >= 0 : (raw == -1 && errno == EPERM));
    if (raw >= 0) close(raw);
    fd = socket(AF_INET6, 0, 0);
    check("INET6_TYPE_ZERO_ESOCKTNOSUPPORT", fd == -1 && errno == ESOCKTNOSUPPORT);
    fd = socket(AF_INET6, SOCK_DGRAM, 263);
    check("INET6_PROTOCOL_RANGE_EINVAL", fd == -1 && errno == EINVAL);
}

/* `unix_create` checks the protocol before its type switch and rewrites the
 * BSD compatibility spelling SOCK_RAW to SOCK_DGRAM, which SO_TYPE then
 * reports because `sock_init_data` copies `sock->type` into `sk_type`. */
static void unix_creation(void) {
    int value = 0;
    socklen_t length = sizeof(value);
    int fd = socket(AF_UNIX, SOCK_RAW, 0);
    check("UNIX_RAW_CREATES", fd >= 0);
    check("UNIX_RAW_IS_DGRAM",
          getsockopt(fd, SOL_SOCKET, SO_TYPE, &value, &length) == 0 && value == SOCK_DGRAM);
    close(fd);
    errno = 0;
    mark("UNIX_PROTOCOL_EPROTONOSUPPORT", socket(AF_UNIX, SOCK_STREAM, IPPROTO_TCP) == -1
         && errno == EPROTONOSUPPORT);
    errno = 0;
    mark("UNIX_TYPE_RDM_ESOCKTNOSUPPORT", socket(AF_UNIX, SOCK_RDM, 0) == -1
         && errno == ESOCKTNOSUPPORT);
    /* `protocol` is either zero or the AF_UNIX family number itself. */
    fd = socket(AF_UNIX, SOCK_STREAM, AF_UNIX);
    check("UNIX_PROTOCOL_SELF_ADMITTED", fd >= 0);
    close(fd);
}

/* `netlink_create` tests the type first (`sock->type != SOCK_RAW &&
 * sock->type != SOCK_DGRAM` is ESOCKTNOSUPPORT) and only then the protocol
 * range and registration (EPROTONOSUPPORT).  NETLINK_USERSOCK is registered by
 * `netlink_add_usersock_entry` with NL_CFG_F_NONROOT_SEND. */
static void netlink_creation(void) {
    int value = 0;
    socklen_t length = sizeof(value);
    int fd = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_USERSOCK);
    check("USERSOCK_CREATES", fd >= 0);
    check("USERSOCK_TYPE",
          getsockopt(fd, SOL_SOCKET, SO_TYPE, &value, &length) == 0 && value == SOCK_DGRAM);
    value = -1; length = sizeof(value);
    check("USERSOCK_PROTOCOL",
          getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, &value, &length) == 0
          && value == NETLINK_USERSOCK);
    value = -1; length = sizeof(value);
    check("USERSOCK_DOMAIN",
          getsockopt(fd, SOL_SOCKET, SO_DOMAIN, &value, &length) == 0 && value == AF_NETLINK);
    close(fd);
    errno = 0;
    mark("NETLINK_TYPE_ESOCKTNOSUPPORT",
         socket(AF_NETLINK, SOCK_SEQPACKET, NETLINK_ROUTE) == -1 && errno == ESOCKTNOSUPPORT);
    /* The type is checked first, so a bad type outranks a bad protocol. */
    errno = 0;
    mark("NETLINK_TYPE_BEFORE_PROTOCOL",
         socket(AF_NETLINK, SOCK_SEQPACKET, 32) == -1 && errno == ESOCKTNOSUPPORT);
    /* MAX_LINKS is 32, and the range test needs no registration lookup.  An
     * in-range protocol that this kernel has not registered is deliberately
     * not asserted: Linux tries `request_module` first, so a modular protocol
     * legitimately succeeds there. */
    errno = 0;
    mark("NETLINK_PROTOCOL_RANGE_EPROTONOSUPPORT",
         socket(AF_NETLINK, SOCK_RAW, 32) == -1 && errno == EPROTONOSUPPORT);
}

/* `bind` reaches the socket only after `move_addr_to_kernel` bounded the copy
 * by `sizeof(struct sockaddr_storage)`, and each family owns the rest:
 * `inet_bind` needs 16 bytes, IPv6 needs SIN6_LEN_RFC2133 (24) rather than a
 * whole sockaddr_in6 (28), and `netlink_bind` needs 12 bytes and answers a
 * family mismatch with EINVAL. */
static void address_lengths(void) {
    unsigned char storage[129] = {0};
    struct sockaddr_in *v4 = (void *)storage;
    struct sockaddr_in6 *v6 = (void *)storage;
    struct sockaddr_nl *nl = (void *)storage;
    int fd;

    v4->sin_family = AF_INET;
    v4->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    fd = socket(AF_INET, SOCK_DGRAM, 0);
    check("IPV4_SOCKET", fd >= 0);
    errno = 0;
    mark("IPV4_BIND_OVERLONG_EINVAL", syscall(SYS_bind, fd, storage, 129) == -1 && errno == EINVAL);
    errno = 0;
    mark("IPV4_BIND_SHORT_EINVAL", syscall(SYS_bind, fd, storage, 12) == -1 && errno == EINVAL);
    mark("IPV4_BIND_STORAGE_BOUNDARY", syscall(SYS_bind, fd, storage, 128) == 0);
    close(fd);

    memset(storage, 0, sizeof(storage));
    v6->sin6_family = AF_INET6;
    v6->sin6_addr = in6addr_loopback;
    fd = socket(AF_INET6, SOCK_DGRAM, 0);
    check("IPV6_SOCKET", fd >= 0);
    /* 24 is SIN6_LEN_RFC2133: accepted even though struct sockaddr_in6 is 28. */
    mark("IPV6_BIND_RFC2133", syscall(SYS_bind, fd, storage, 24) == 0);
    close(fd);
    fd = socket(AF_INET6, SOCK_DGRAM, 0);
    check("IPV6_SOCKET_AGAIN", fd >= 0);
    errno = 0;
    mark("IPV6_BIND_SHORT_EINVAL", syscall(SYS_bind, fd, storage, 23) == -1 && errno == EINVAL);
    errno = 0;
    mark("IPV6_BIND_OVERLONG_EINVAL", syscall(SYS_bind, fd, storage, 129) == -1 && errno == EINVAL);
    close(fd);

    memset(storage, 0, sizeof(storage));
    nl->nl_family = AF_NETLINK;
    fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("NETLINK_SOCKET", fd >= 0);
    errno = 0;
    mark("NETLINK_BIND_SHORT_EINVAL", bind(fd, (struct sockaddr *)nl, 11) == -1 && errno == EINVAL);
    mark("NETLINK_BIND_EXACT", bind(fd, (struct sockaddr *)nl, 12) == 0);
    close(fd);
    /* A longer address is a legal prefix, not an error. */
    fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("NETLINK_SOCKET_LONGER", fd >= 0);
    mark("NETLINK_BIND_LONGER", bind(fd, (struct sockaddr *)nl, 24) == 0);
    close(fd);
    fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("NETLINK_SOCKET_AGAIN", fd >= 0);
    nl->nl_family = AF_INET;
    errno = 0;
    mark("NETLINK_BIND_FAMILY_EINVAL", bind(fd, (struct sockaddr *)nl, 12) == -1 && errno == EINVAL);
    close(fd);

    /* `unix_bind` treats a two-byte AF_UNIX address as an autobind request,
     * while `unix_validate_addr` rejects it on every other path. */
    memset(storage, 0, sizeof(storage));
    struct sockaddr_un *un = (void *)storage;
    un->sun_family = AF_UNIX;
    fd = socket(AF_UNIX, SOCK_STREAM, 0);
    check("UNIX_SOCKET", fd >= 0);
    mark("UNIX_BIND_TWO_BYTE_AUTOBINDS", bind(fd, (struct sockaddr *)un, 2) == 0);
    errno = 0;
    mark("UNIX_CONNECT_TWO_BYTE_EINVAL",
         connect(fd, (struct sockaddr *)un, 2) == -1 && errno == EINVAL);
    close(fd);
}

/* `netlink_allowed` reads the connecting protocol's own registration flags:
 * rtnetlink registers NL_CFG_F_NONROOT_RECV, so an unprivileged peer needs
 * CAP_NET_ADMIN while an unprivileged group bind does not; usersock registers
 * NL_CFG_F_NONROOT_SEND, which is exactly the reverse. */
static void netlink_policy(void) {
    int net_admin = effective_capability(CAP_NET_ADMIN);
    struct sockaddr_nl peer = {.nl_family = AF_NETLINK, .nl_pid = 12345};
    struct sockaddr_nl groups = {.nl_family = AF_NETLINK, .nl_groups = 1};
    int fd, result;

    fd = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_USERSOCK);
    check("USERSOCK_SOCKET", fd >= 0);
    errno = 0;
    result = connect(fd, (struct sockaddr *)&peer, sizeof(peer));
    mark("USERSOCK_PEER_ALLOWED", result == 0);
    close(fd);

    fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("ROUTE_SOCKET", fd >= 0);
    errno = 0;
    result = connect(fd, (struct sockaddr *)&peer, sizeof(peer));
    mark("ROUTE_PEER_POLICY", net_admin ? result == 0 : (result == -1 && errno == EPERM));
    close(fd);

    /* `netlink_getsockbyportid()` refuses a destination that is connected to
     * another port ID, and accepts one whose connected peer is the sender:
     * `if (READ_ONCE(sock->sk_state) == NETLINK_CONNECTED &&
     * READ_ONCE(nlk->dst_portid) != nlk_sk(ssk)->portid) return
     * ERR_PTR(-ECONNREFUSED);` (`net/netlink/af_netlink.c:1147-1153`).  A
     * usersock socket may connect without CAP_NET_ADMIN because NL_CFG_F_NONROOT_SEND
     * is registered for it. */
    int target = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_USERSOCK);
    check("REFUSAL_TARGET_SOCKET", target >= 0);
    struct sockaddr_nl target_name = {.nl_family = AF_NETLINK};
    socklen_t target_length = sizeof(target_name);
    mark("REFUSAL_TARGET_BIND",
         bind(target, (struct sockaddr *)&target_name, sizeof(target_name)) == 0 &&
             getsockname(target, (struct sockaddr *)&target_name, &target_length) == 0 &&
             target_name.nl_pid != 0);

    struct sockaddr_nl connected_peer = {.nl_family = AF_NETLINK, .nl_pid = 12345};
    errno = 0;
    mark("REFUSAL_TARGET_CONNECT",
         connect(target, (struct sockaddr *)&connected_peer, sizeof(connected_peer)) == 0);

    struct sockaddr_nl destination = {.nl_family = AF_NETLINK, .nl_pid = target_name.nl_pid};
    struct sockaddr_nl foreign = {.nl_family = AF_NETLINK, .nl_pid = 54321};
    int sender = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_USERSOCK);
    check("REFUSAL_SENDER_SOCKET", sender >= 0);
    mark("REFUSAL_SENDER_BIND", bind(sender, (struct sockaddr *)&foreign, sizeof(foreign)) == 0);
    errno = 0;
    mark("NETLINK_CONNECT_REFUSES_OTHER_SENDER",
         sendto(sender, "x", 1, 0, (struct sockaddr *)&destination, sizeof(destination)) == -1
             && errno == ECONNREFUSED);
    close(sender);

    struct sockaddr_nl accepted_peer = {.nl_family = AF_NETLINK, .nl_pid = 12345};
    int connected = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_USERSOCK);
    check("REFUSAL_PEER_SOCKET", connected >= 0);
    mark("REFUSAL_PEER_BIND",
         bind(connected, (struct sockaddr *)&accepted_peer, sizeof(accepted_peer)) == 0);
    errno = 0;
    mark("NETLINK_CONNECT_ACCEPTS_PEER",
         sendto(connected, "y", 1, 0, (struct sockaddr *)&destination, sizeof(destination)) == 1);
    char refusal_record[4] = {0};
    mark("NETLINK_CONNECT_PEER_DELIVERED",
         recv(target, refusal_record, sizeof(refusal_record), 0) == 1 && refusal_record[0] == 'y');
    close(connected);
    close(target);

    fd = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_USERSOCK);
    check("USERSOCK_GROUP_SOCKET", fd >= 0);
    errno = 0;
    result = bind(fd, (struct sockaddr *)&groups, sizeof(groups));
    mark("USERSOCK_GROUP_POLICY", net_admin ? result == 0 : (result == -1 && errno == EPERM));
    close(fd);

    fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("ROUTE_GROUP_SOCKET", fd >= 0);
    errno = 0;
    mark("ROUTE_GROUP_ALLOWED", bind(fd, (struct sockaddr *)&groups, sizeof(groups)) == 0);
    close(fd);
}

/* `sk_getsockopt` reports the `struct sock` state `sock_init_data_uid` seeds,
 * and `sk_setsockopt` stores what those getters return.  Only relations that
 * hold on every configuration are asserted: the buffer values themselves come
 * from sysctl_, and the minima from CONFIG_ tunables. */
static void sol_socket_table(void) {
    int value = 0, expected = 0;
    socklen_t length = sizeof(value);
    struct linger linger = {0};
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("ROUTE_SOCKET", fd >= 0);

    value = -1; length = sizeof(value);
    check("TYPE", getsockopt(fd, SOL_SOCKET, SO_TYPE, &value, &length) == 0 && value == SOCK_RAW);
    value = -1; length = sizeof(value);
    check("DOMAIN", getsockopt(fd, SOL_SOCKET, SO_DOMAIN, &value, &length) == 0 && value == AF_NETLINK);
    value = -1; length = sizeof(value);
    check("PROTOCOL", getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, &value, &length) == 0 && value == NETLINK_ROUTE);
    value = -1; length = sizeof(value);
    check("ERROR_CLEAR", getsockopt(fd, SOL_SOCKET, SO_ERROR, &value, &length) == 0 && value == 0);
    value = -1; length = sizeof(value);
    check("ACCEPTCONN", getsockopt(fd, SOL_SOCKET, SO_ACCEPTCONN, &value, &length) == 0 && value == 0);
    value = -1; length = sizeof(value);
    check("SNDLOWAT_IS_ONE", getsockopt(fd, SOL_SOCKET, SO_SNDLOWAT, &value, &length) == 0 && value == 1);
    value = -1; length = sizeof(value);
    check("RCVLOWAT_STARTS_AT_ONE", getsockopt(fd, SOL_SOCKET, SO_RCVLOWAT, &value, &length) == 0 && value == 1);
    length = sizeof(linger);
    check("LINGER_FRESH", getsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, &length) == 0
          && length == sizeof(linger) && linger.l_onoff == 0 && linger.l_linger == 0);
    value = -1; length = sizeof(value);
    check("BUFFERS_POSITIVE", getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &value, &length) == 0 && value > 0
          && getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &value, &length) == 0 && value > 0);

    /* `sk->sk_rcvbuf = max_t(int, val * 2, SOCK_MIN_RCVBUF)` and the same
     * doubling for sk_sndbuf, which is what the getter reports back. */
    value = 4096;
    check("SET_RCVBUF_DOUBLES", setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &value, sizeof(value)) == 0);
    expected = 0; length = sizeof(expected);
    check("RCVBUF_DOUBLED", getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &expected, &length) == 0
          && expected == 8192);
    value = 4096;
    check("SET_SNDBUF_DOUBLES", setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &value, sizeof(value)) == 0);
    expected = 0; length = sizeof(expected);
    check("SNDBUF_DOUBLED", getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &expected, &length) == 0
          && expected == 8192);

    /* Boolean SOL_SOCKET names round-trip through their own getters. */
    for (int option = 0; option < 5; ++option) {
        static const int names[] = {SO_REUSEADDR, SO_DONTROUTE, SO_BROADCAST, SO_KEEPALIVE, SO_OOBINLINE};
        value = 1;
        check("BOOL_SET", setsockopt(fd, SOL_SOCKET, names[option], &value, sizeof(value)) == 0);
        expected = 0; length = sizeof(expected);
        check("BOOL_ROUND_TRIP", getsockopt(fd, SOL_SOCKET, names[option], &expected, &length) == 0
              && expected == 1);
        value = 0;
        check("BOOL_CLEAR", setsockopt(fd, SOL_SOCKET, names[option], &value, sizeof(value)) == 0);
        expected = -1; length = sizeof(expected);
        check("BOOL_CLEARED", getsockopt(fd, SOL_SOCKET, names[option], &expected, &length) == 0
              && expected == 0);
    }

    /* SO_LINGER stores both fields, but clearing l_onoff only resets the
     * SOCK_LINGER flag: `sk_lingertime` keeps the seconds of the last enabled
     * request and the getter reports them regardless of the flag. */
    linger.l_onoff = 1; linger.l_linger = 2;
    check("LINGER_SET", setsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, sizeof(linger)) == 0);
    memset(&linger, 0, sizeof(linger)); length = sizeof(linger);
    check("LINGER_ROUND_TRIP", getsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, &length) == 0
          && linger.l_onoff == 1 && linger.l_linger == 2);
    linger.l_onoff = 0; linger.l_linger = 9;
    check("LINGER_CLEAR", setsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, sizeof(linger)) == 0);
    memset(&linger, 0xff, sizeof(linger)); length = sizeof(linger);
    check("LINGER_DISABLED_KEEPS_SECONDS", getsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, &length) == 0
          && linger.l_onoff == 0 && linger.l_linger == 2);
    linger.l_onoff = 1; linger.l_linger = 0;
    check("LINGER_ENABLED_ZERO_SECONDS", setsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, sizeof(linger)) == 0);
    memset(&linger, 0xff, sizeof(linger)); length = sizeof(linger);
    check("LINGER_ZERO_ROUND_TRIP", getsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, &length) == 0
          && linger.l_onoff == 1 && linger.l_linger == 0);

    /* `sk_getsockopt` clamps its copy to `min(user_len, lv)` and reports that
     * same clamped length, so a short buffer truncates instead of failing:
     * `SO_TYPE` has `lv == sizeof(int)` and still answers a two-byte request
     * with two. */
    value = -1; length = 2;
    memset(&value, 0, sizeof(value));
    mark("GET_SHORT_REPORTS_COPY", getsockopt(fd, SOL_SOCKET, SO_TYPE, &value, &length) == 0
         && length == 2 && (value & 0xff) == SOCK_RAW);
    /* `lv` is a ceiling as well: a larger buffer is clamped down to it. */
    linger.l_onoff = 1; linger.l_linger = 5;
    check("LINGER_SET_FOR_CLAMP", setsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, sizeof(linger)) == 0);
    value = -1; length = sizeof(value);
    mark("GET_LINGER_CLAMPED", getsockopt(fd, SOL_SOCKET, SO_LINGER, &value, &length) == 0
         && length == sizeof(value) && value == 1);
    value = -1; length = sizeof(linger);
    check("GET_LINGER_FULL", getsockopt(fd, SOL_SOCKET, SO_LINGER, &linger, &length) == 0
          && length == sizeof(linger) && linger.l_onoff == 1 && linger.l_linger == 5);
    /* A zero-length buffer copies nothing and reports nothing. */
    value = -1; length = 0;
    mark("GET_ZERO_LENGTH", getsockopt(fd, SOL_SOCKET, SO_TYPE, &value, &length) == 0
         && length == 0 && value == -1);

    /* SO_PRIORITY is stored verbatim once the capability gate admits it. */
    value = 6;
    check("PRIORITY_SET", setsockopt(fd, SOL_SOCKET, SO_PRIORITY, &value, sizeof(value)) == 0);
    expected = -1; length = sizeof(expected);
    check("PRIORITY_ROUND_TRIP", getsockopt(fd, SOL_SOCKET, SO_PRIORITY, &expected, &length) == 0
          && expected == 6);

    /* SO_PASSCRED is a real per-description flag for AF_NETLINK because
     * `sk_may_scm_recv` admits it. */
    value = 1;
    check("PASSCRED_SET", setsockopt(fd, SOL_SOCKET, SO_PASSCRED, &value, sizeof(value)) == 0);
    expected = 0; length = sizeof(expected);
    check("PASSCRED_ROUND_TRIP", getsockopt(fd, SOL_SOCKET, SO_PASSCRED, &expected, &length) == 0
          && expected == 1);

    /* Read-only names and unsettable ones answer ENOPROTOOPT. */
    value = 1;
    errno = 0;
    mark("SET_TYPE_ENOPROTOOPT", setsockopt(fd, SOL_SOCKET, SO_TYPE, &value, sizeof(value)) == -1
         && errno == ENOPROTOOPT);
    errno = 0;
    mark("SET_SNDLOWAT_ENOPROTOOPT", setsockopt(fd, SOL_SOCKET, SO_SNDLOWAT, &value, sizeof(value)) == -1
         && errno == ENOPROTOOPT);
    errno = 0;
    mark("SET_PROTOCOL_ENOPROTOOPT", setsockopt(fd, SOL_SOCKET, SO_PROTOCOL, &value, sizeof(value)) == -1
         && errno == ENOPROTOOPT);
    /* Every remaining SOL_SOCKET name needs a whole int, and SO_LINGER needs a
     * whole struct linger. */
    errno = 0;
    mark("SET_SHORT_OPTLEN_EINVAL",
         setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &value, 2) == -1 && errno == EINVAL);
    errno = 0;
    mark("SET_LINGER_SHORT_EINVAL",
         setsockopt(fd, SOL_SOCKET, SO_LINGER, &value, sizeof(value)) == -1 && errno == EINVAL);
    /* A level that is neither SOL_SOCKET nor SOL_NETLINK is ENOPROTOOPT on
     * both the set and the get path. */
    errno = 0;
    mark("SET_BAD_LEVEL_ENOPROTOOPT",
         setsockopt(fd, SOL_IP, SO_REUSEADDR, &value, sizeof(value)) == -1 && errno == ENOPROTOOPT);
    errno = 0;
    mark("GET_BAD_LEVEL_ENOPROTOOPT",
         getsockopt(fd, SOL_IP, SO_REUSEADDR, &value, &length) == -1 && errno == ENOPROTOOPT);

    /* `do_sock_setsockopt()` rejects a negative length before any protocol
     * sees the option (`net/socket.c:2342-2343`). */
    value = 4096;
    errno = 0;
    mark("SET_NEGATIVE_OPTLEN_EINVAL",
         setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &value, (socklen_t)-1) == -1 && errno == EINVAL);

    /* `SO_SNDBUF`/`SO_RCVBUF` never fail on a negative request: the unsigned
     * `min_t(u32, val, sysctl_*mem_max)` clamp turns it into the sysctl
     * maximum, which `sk_sndbuf`/`sk_rcvbuf` then double
     * (`net/core/sock.c:1342-1352`, `:1374`).  The getter reports that stored
     * value, so both options read back as 8388608. */
    value = -1;
    errno = 0;
    mark("NEGATIVE_SNDBUF_CLAMPS",
         setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &value, sizeof(value)) == 0);
    expected = 0;
    length = sizeof(expected);
    mark("NEGATIVE_SNDBUF_READS_MAX",
         getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &expected, &length) == 0 && expected == 8388608);
    value = -1;
    errno = 0;
    mark("NEGATIVE_RCVBUF_CLAMPS",
         setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &value, sizeof(value)) == 0);
    expected = 0;
    length = sizeof(expected);
    mark("NEGATIVE_RCVBUF_READS_MAX",
         getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &expected, &length) == 0 && expected == 8388608);
    close(fd);

    /* The same arithmetic for an internet socket, whose transports own the
     * buffer and report it back through their own capacity. */
    fd = socket(AF_INET, SOCK_DGRAM, 0);
    check("SOL_SOCKET_INET", fd >= 0);
    value = -1;
    errno = 0;
    mark("INET_NEGATIVE_SNDBUF_CLAMPS",
         setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &value, sizeof(value)) == 0);
    expected = 0;
    length = sizeof(expected);
    mark("INET_NEGATIVE_SNDBUF_READS_MAX",
         getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &expected, &length) == 0 && expected == 8388608);
    value = -1;
    errno = 0;
    mark("INET_NEGATIVE_RCVBUF_CLAMPS",
         setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &value, sizeof(value)) == 0);
    expected = 0;
    length = sizeof(expected);
    mark("INET_NEGATIVE_RCVBUF_READS_MAX",
         getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &expected, &length) == 0 && expected == 8388608);
    close(fd);
}

/* `netlink_setsockopt`/`netlink_getsockopt` own SOL_NETLINK.  The boolean
 * flags are `assign_bit`/`test_bit` pairs, a request shorter than an int
 * clears the flag instead of failing, and NETLINK_LIST_MEMBERSHIPS reports the
 * bitmap bounds `netlink_realloc_groups` installed, which only a group
 * transition ever does. */
static void netlink_option_table(void) {
    int value = 0;
    socklen_t length = sizeof(value);
    unsigned int words[2] = {0, 0};
    struct sockaddr_nl address = {.nl_family = AF_NETLINK, .nl_pid = 0, .nl_groups = 0};
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("ROUTE_SOCKET", fd >= 0);

    /* Before any group transition `nlk->groups` is NULL and `nlk->ngroups` is
     * zero, so the getter copies nothing and reports a zero length. */
    length = sizeof(words);
    check("MEMBERSHIPS_UNSIZED", getsockopt(fd, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, words, &length) == 0
          && length == 0);

    /* NETLINK_ROUTE registers RTNLGRP_MAX (39) groups, so the bitmap becomes
     * two four-byte chunks. */
    address.nl_groups = 1;
    check("BIND_GROUP", bind(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
    memset(words, 0, sizeof(words));
    length = sizeof(words);
    check("MEMBERSHIPS_SIZED", getsockopt(fd, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, words, &length) == 0
          && length == 8 && words[0] == 1 && words[1] == 0);
    /* A buffer that cannot hold a whole chunk copies none of it but still
     * reports the bitmap size. */
    memset(words, 0, sizeof(words));
    length = sizeof(words[0]);
    check("MEMBERSHIPS_TRUNCATED", getsockopt(fd, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, words, &length) == 0
          && length == 8 && words[0] == 1);

    /* Membership transitions are bounded by the protocol's own group count. */
    value = 40;
    errno = 0;
    mark("ADD_ABOVE_NGROUPS_EINVAL", setsockopt(fd, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &value, sizeof(value)) == -1
         && errno == EINVAL);
    value = 0;
    errno = 0;
    mark("ADD_ZERO_EINVAL", setsockopt(fd, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &value, sizeof(value)) == -1
         && errno == EINVAL);
    value = 39;
    check("ADD_LAST_GROUP", setsockopt(fd, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &value, sizeof(value)) == 0);
    memset(words, 0, sizeof(words));
    length = sizeof(words);
    check("MEMBERSHIPS_BOTH_WORDS", getsockopt(fd, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, words, &length) == 0
          && length == 8 && words[0] == 1 && words[1] == (1U << 6));
    /* Dropping a held group is unchecked and always succeeds. */
    check("DROP_LAST_GROUP", setsockopt(fd, SOL_NETLINK, NETLINK_DROP_MEMBERSHIP, &value, sizeof(value)) == 0);
    memset(words, 0, sizeof(words));
    length = sizeof(words);
    check("MEMBERSHIPS_AFTER_DROP", getsockopt(fd, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, words, &length) == 0
          && length == 8 && words[0] == 1 && words[1] == 0);

    /* The remaining SOL_NETLINK names are int-sized boolean flags. */
    for (int option = 0; option < 5; ++option) {
        static const int names[] = {NETLINK_PKTINFO, NETLINK_BROADCAST_ERROR, NETLINK_NO_ENOBUFS,
                                    NETLINK_CAP_ACK, NETLINK_EXT_ACK};
        value = 1;
        check("FLAG_SET", setsockopt(fd, SOL_NETLINK, names[option], &value, sizeof(value)) == 0);
        value = 0; length = sizeof(value);
        check("FLAG_ROUND_TRIP", getsockopt(fd, SOL_NETLINK, names[option], &value, &length) == 0
              && value == 1 && length == sizeof(value));
        value = 0;
        check("FLAG_CLEAR", setsockopt(fd, SOL_NETLINK, names[option], &value, sizeof(value)) == 0);
        value = -1; length = sizeof(value);
        check("FLAG_CLEARED", getsockopt(fd, SOL_NETLINK, names[option], &value, &length) == 0
              && value == 0);
    }
    /* NETLINK_LISTEN_ALL_NSID is the one boolean flag whose setter checks a
     * capability, and only the setter checks it. */
    int net_broadcast = effective_capability(CAP_NET_BROADCAST);
    value = 1;
    errno = 0;
    int enabled = setsockopt(fd, SOL_NETLINK, NETLINK_LISTEN_ALL_NSID, &value, sizeof(value));
    mark("LISTEN_ALL_NSID_POLICY", net_broadcast ? enabled == 0 : (enabled == -1 && errno == EPERM));
    value = -1; length = sizeof(value);
    check("LISTEN_ALL_NSID_GET", getsockopt(fd, SOL_NETLINK, NETLINK_LISTEN_ALL_NSID, &value, &length) == 0
          && value == (net_broadcast ? 1 : 0));

    /* A request shorter than an int is a legal clear, not EINVAL. */
    value = 1;
    check("FLAG_SET_AGAIN", setsockopt(fd, SOL_NETLINK, NETLINK_PKTINFO, &value, sizeof(value)) == 0);
    check("FLAG_CLEAR_SHORT", setsockopt(fd, SOL_NETLINK, NETLINK_PKTINFO, &value, 1) == 0);
    value = -1; length = sizeof(value);
    check("FLAG_SHORT_CLEARED", getsockopt(fd, SOL_NETLINK, NETLINK_PKTINFO, &value, &length) == 0
          && value == 0);
    /* An unknown SOL_NETLINK name is ENOPROTOOPT on both paths. */
    errno = 0;
    mark("SET_UNKNOWN_ENOPROTOOPT", setsockopt(fd, SOL_NETLINK, 13, &value, sizeof(value)) == -1
         && errno == ENOPROTOOPT);
    errno = 0;
    mark("GET_UNKNOWN_ENOPROTOOPT", getsockopt(fd, SOL_NETLINK, 13, &value, &length) == -1
         && errno == ENOPROTOOPT);
    close(fd);
}

/* A provider without accept/listen/shutdown installs the `sock_no_*` stubs,
 * which answer EOPNOTSUPP rather than the ENOTSOCK a non-socket descriptor
 * reports.  `shutdown` reaches that stub before it validates the direction. */
static void null_operations(void) {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("NETLINK_SOCKET", fd >= 0);
    errno = 0;
    mark("NETLINK_LISTEN_EOPNOTSUPP", listen(fd, 1) == -1 && errno == EOPNOTSUPP);
    errno = 0;
    mark("NETLINK_ACCEPT_EOPNOTSUPP", accept(fd, NULL, NULL) == -1 && errno == EOPNOTSUPP);
    errno = 0;
    mark("NETLINK_ACCEPT4_EOPNOTSUPP", accept4(fd, NULL, NULL, SOCK_CLOEXEC) == -1 && errno == EOPNOTSUPP);
    errno = 0;
    mark("NETLINK_SHUTDOWN_EOPNOTSUPP", shutdown(fd, SHUT_RDWR) == -1 && errno == EOPNOTSUPP);
    errno = 0;
    mark("NETLINK_SHUTDOWN_INVALID_HOW_EOPNOTSUPP", shutdown(fd, 7) == -1 && errno == EOPNOTSUPP);
    close(fd);

    int pair[2];
    errno = 0;
    mark("NETLINK_SOCKETPAIR_EOPNOTSUPP", socketpair(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE, pair) == -1
         && errno == EOPNOTSUPP);
    errno = 0;
    mark("INET_SOCKETPAIR_EOPNOTSUPP", socketpair(AF_INET, SOCK_STREAM, 0, pair) == -1
         && errno == EOPNOTSUPP);
    errno = 0;
    mark("INET_SOCKETPAIR_PROTOCOL_EPROTONOSUPPORT", socketpair(AF_INET, SOCK_STREAM, IPPROTO_UDP, pair) == -1
         && errno == EPROTONOSUPPORT);
    errno = 0;
    mark("INET_SOCKETPAIR_TYPE_ESOCKTNOSUPPORT", socketpair(AF_INET, SOCK_RDM, 0, pair) == -1
         && errno == ESOCKTNOSUPPORT);
    errno = 0;
    mark("SOCKETPAIR_FLAG_MASK_EINVAL", socketpair(AF_UNIX, SOCK_STREAM | 0x40, 0, pair) == -1
         && errno == EINVAL);

    /* `__sys_accept4` resolves the descriptor before it validates the flag
     * mask (`net/socket.c:1917-1930`), so a bad descriptor outranks a bad
     * flag.  `__sys_socketpair` reserves both descriptors with
     * `get_unused_fd_flags` and writes them into `usockvec` before it creates
     * either socket, and the failure path releases them again
     * (`:1817-1899`), so a pair request that cannot be satisfied still leaves
     * two numbers in the caller's array. */
    errno = 0;
    mark("ACCEPT4_BADFD_BADFLAG", accept4(-1, NULL, NULL, 0x40) == -1 && errno == EBADF);
    pair[0] = -1;
    pair[1] = -1;
    errno = 0;
    mark("SOCKETPAIR_RESERVES_FDS",
         socketpair(AF_INET, SOCK_STREAM, 0, pair) == -1 && errno == EOPNOTSUPP &&
             pair[0] >= 0 && pair[1] >= 0 && pair[0] != pair[1]);
}

/* Reports whether every byte of `buffer` still holds `filler`.  A provider that
 * never writes a source address must leave the caller's bytes exactly as they
 * were, so a filled sentinel is the only way to observe it. */
static int untouched(const void *buffer, size_t length, unsigned char filler) {
    const unsigned char *bytes = buffer;
    for (size_t index = 0; index < length; index++) {
        if (bytes[index] != filler) {
            return 0;
        }
    }
    return 1;
}

/* Binds a loopback listener, connects `*client` to it and accepts the peer as
 * `*server`, closing the listener again.  A failure leaves both endpoints at
 * -1.  The two ends are returned separately because the source address of a
 * receive is only observable on the end that did not send. */
static void loopback_stream_pair(int *server, int *client) {
    struct sockaddr_in address;
    memset(&address, 0, sizeof(address));
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t length = sizeof(address);
    *server = -1;
    *client = -1;
    int bound = socket(AF_INET, SOCK_STREAM, 0);
    if (bound < 0 || bind(bound, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        listen(bound, 1) != 0 ||
        getsockname(bound, (struct sockaddr *)&address, &length) != 0) {
        if (bound >= 0) {
            close(bound);
        }
        return;
    }
    int connected = socket(AF_INET, SOCK_STREAM, 0);
    if (connected < 0 || connect(connected, (struct sockaddr *)&address, length) != 0) {
        if (connected >= 0) {
            close(connected);
        }
        close(bound);
        return;
    }
    int accepted = accept(bound, NULL, NULL);
    close(bound);
    if (accepted < 0) {
        close(connected);
        return;
    }
    *server = accepted;
    *client = connected;
}

/* `__sys_getsockname`/`__sys_getpeername` run the family's `getname` before
 * `move_addr_to_user` reads `*addrlen` (`net/socket.c:1849-1876`), so a
 * provider error survives an unusable length pointer while a successful
 * provider call still reports the copy-out fault.  The inet record is
 * `struct sockaddr_in` with the bound port and address (`inet_getname`), and a
 * two-byte AF_UNIX bind autobinds through `unix_autobind()`, whose abstract
 * `%05x` name `unix_getname()` reports with an eight-byte length
 * (`net/unix/af_unix.c:1463-1471`, `:1236-1244`). */
static void name_record(void) {
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    struct sockaddr_in record;
    socklen_t length = sizeof(record);
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    check("NAME_SOCKET", fd >= 0);
    memset(&record, 0xa5, sizeof(record));
    errno = 0;
    mark("GETSOCKNAME_UNBOUND_ZERO",
         getsockname(fd, (struct sockaddr *)&record, &length) == 0 &&
             length == sizeof(record) && record.sin_family == AF_INET && record.sin_port == 0 &&
             record.sin_addr.s_addr == 0);
    mark("NAME_BIND", bind(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
    memset(&record, 0, sizeof(record));
    length = sizeof(record);
    errno = 0;
    mark("GETSOCKNAME_BOUND_PORT",
         getsockname(fd, (struct sockaddr *)&record, &length) == 0 &&
             length == sizeof(record) && record.sin_family == AF_INET && record.sin_port != 0 &&
             record.sin_addr.s_addr == htonl(INADDR_LOOPBACK));
    errno = 0;
    mark("GETPEERNAME_UNCONNECTED_ENOTCONN", getpeername(fd, NULL, (socklen_t *)0x1) == -1
         && errno == ENOTCONN);
    errno = 0;
    mark("GETSOCKNAME_BADLEN_EFAULT",
         getsockname(fd, NULL, (socklen_t *)0x1) == -1 && errno == EFAULT);
    /* The provider's own length is reported even when the copy faults, because
     * `move_addr_to_user()` writes `klen` back before `copy_to_user()`
     * (`net/socket.c:288-303`); the four-byte request is only a destination
     * capacity, so a truncating request still reports the whole record. */
    length = 4;
    errno = 0;
    mark("GETSOCKNAME_LENGTH_BEFORE_FAULT",
         getsockname(fd, (struct sockaddr *)0x1, &length) == -1 && errno == EFAULT &&
             length == sizeof(struct sockaddr_in));
    close(fd);

    /* `move_addr_to_user()` is also the tail that exports a receive source
     * address (`__sys_recvfrom()` -> `sock_recvmsg()` -> `move_addr_to_user()`,
     * `net/socket.c:2280-2300`), so a faulting source-address copy reports the
     * provider's length in the same way. */
    int receiver = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in loopback;
    memset(&loopback, 0, sizeof(loopback));
    loopback.sin_family = AF_INET;
    loopback.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t loopback_length = sizeof(loopback);
    int receiver_ready = receiver >= 0 &&
                         bind(receiver, (struct sockaddr *)&loopback, sizeof(loopback)) == 0 &&
                         getsockname(receiver, (struct sockaddr *)&loopback, &loopback_length) == 0 &&
                         loopback_length == sizeof(loopback);
    mark("RECVFROM_NAME_SOCKET", receiver_ready);
    if (receiver_ready) {
        char payload = 'q';
        ssize_t sent = sendto(receiver, &payload, 1, 0, (struct sockaddr *)&loopback,
                              sizeof(loopback));
        char buffer[8];
        socklen_t source_length = 4;
        errno = 0;
        ssize_t received = recvfrom(receiver, buffer, sizeof(buffer), 0,
                                    (struct sockaddr *)0x1, &source_length);
        mark("RECVFROM_LENGTH_BEFORE_FAULT",
             sent == 1 && received == -1 && errno == EFAULT &&
                 source_length == sizeof(struct sockaddr_in));
    }
    close(receiver);

    /* `__sys_recvfrom()` passes the protocol a zeroed `struct msghdr` and only
     * `.msg_name` is filled in (`net/socket.c:2277-2302`):
     *
     * ```c
     * 	struct msghdr msg;
     * 	struct sockaddr_storage address;
     * 	...
     * 	memset(&msg, 0, sizeof(msg));
     * 	if (addr) {
     * 		msg.msg_name = &address;
     * 		msg.msg_namelen = sizeof(address);
     * 	}
     * ```
     *
     * The kernel buffer it imports is then re-exported by `move_addr_to_user()`
     * with whatever length the protocol stored in `msg_namelen`, so a transport
     * that never writes a peer address reports a zero length and leaves the
     * caller's bytes alone.  `tcp_recvmsg_locked()` is that transport —
     * "According to UNIX98, msg_name/msg_namelen are ignored on connected
     * socket." (`net/ipv4/tcp.c:2910-2912`) — while `udp_recvmsg()` always
     * stores the source it dequeued (`net/ipv4/udp.c:2094-2101`). */
    int server = -1;
    int client = -1;
    loopback_stream_pair(&server, &client);
    check("CONNECTED_STREAM_READY", server >= 0 && client >= 0);
    if (server >= 0 && client >= 0) {
        struct sockaddr_in source;
        socklen_t source_length;
        char octet = 'r';
        ssize_t received;

        memset(&source, 0xa5, sizeof(source));
        check("CONNECTED_STREAM_SENT", send(server, &octet, 1, 0) == 1);
        source_length = sizeof(source);
        errno = 0;
        received = recvfrom(client, &octet, 1, 0, (struct sockaddr *)&source, &source_length);
        mark("RECVFROM_CONNECTED_STREAM_NAME_ABSENT",
             received == 1 && source_length == 0 &&
                 untouched(&source, sizeof(source), 0xa5));

        check("CONNECTED_STREAM_SENT_AGAIN", send(server, &octet, 1, 0) == 1);
        struct iovec vector = {.iov_base = &octet, .iov_len = 1};
        struct msghdr header = {.msg_iov = &vector, .msg_iovlen = 1};
        memset(&source, 0xa5, sizeof(source));
        header.msg_name = &source;
        header.msg_namelen = sizeof(source);
        errno = 0;
        received = recvmsg(client, &header, 0);
        mark("RECVMSG_CONNECTED_STREAM_NAME_ABSENT",
             received == 1 && header.msg_namelen == 0 &&
                 untouched(&source, sizeof(source), 0xa5));

        /* A negative request is the one arm where `move_addr_to_user()`
         * neither copies nor writes the length back (`net/socket.c:288-303`);
         * the zero-length provider reaches it the same way. */
        check("CONNECTED_STREAM_SENT_THIRD", send(server, &octet, 1, 0) == 1);
        source_length = (socklen_t)-1;
        memset(&source, 0xa5, sizeof(source));
        errno = 0;
        received = recvfrom(client, &octet, 1, 0, (struct sockaddr *)&source, &source_length);
        mark("RECVFROM_CONNECTED_STREAM_NEGATIVE_LENGTH_EINVAL",
             received == -1 && errno == EINVAL && source_length == (socklen_t)-1 &&
                 untouched(&source, sizeof(source), 0xa5));
    }
    if (client >= 0) {
        close(client);
    }
    if (server >= 0) {
        close(server);
    }

    /* The datagram half of the same rule: the reported source is the one the
     * transport stored, with its own address and port.  The sender is bound
     * explicitly because an implicit bind leaves `inet->inet_saddr` at the
     * wildcard, so `inet_getname()` reports `0.0.0.0` even though the datagram
     * the peer dequeued carries the route's source address — comparing the two
     * would compare two different things. */
    int datagram_receiver = socket(AF_INET, SOCK_DGRAM, 0);
    int datagram_sender = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in datagram_address;
    memset(&datagram_address, 0, sizeof(datagram_address));
    datagram_address.sin_family = AF_INET;
    datagram_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t datagram_length = sizeof(datagram_address);
    int datagram_ready = datagram_receiver >= 0 && datagram_sender >= 0 &&
                         bind(datagram_receiver, (struct sockaddr *)&datagram_address,
                              sizeof(datagram_address)) == 0 &&
                         getsockname(datagram_receiver, (struct sockaddr *)&datagram_address,
                                     &datagram_length) == 0;
    check("DATAGRAM_READY", datagram_ready);
    if (datagram_ready) {
        struct sockaddr_in sender_address;
        memset(&sender_address, 0, sizeof(sender_address));
        sender_address.sin_family = AF_INET;
        sender_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        socklen_t sender_length = sizeof(sender_address);
        char octet = 's';
        check("DATAGRAM_SENT",
              bind(datagram_sender, (struct sockaddr *)&sender_address,
                   sizeof(sender_address)) == 0 &&
                  getsockname(datagram_sender, (struct sockaddr *)&sender_address,
                              &sender_length) == 0 &&
                  sendto(datagram_sender, &octet, 1, 0, (struct sockaddr *)&datagram_address,
                         sizeof(datagram_address)) == 1);
        struct sockaddr_in source;
        socklen_t source_length = sizeof(source);
        memset(&source, 0xa5, sizeof(source));
        errno = 0;
        ssize_t received =
            recvfrom(datagram_receiver, &octet, 1, 0, (struct sockaddr *)&source, &source_length);
        mark("RECVFROM_DATAGRAM_NAME_PRESENT",
             received == 1 && source_length == sizeof(source) && source.sin_family == AF_INET &&
                 source.sin_port == sender_address.sin_port &&
                 source.sin_addr.s_addr == sender_address.sin_addr.s_addr);
    }
    if (datagram_sender >= 0) {
        close(datagram_sender);
    }
    if (datagram_receiver >= 0) {
        close(datagram_receiver);
    }

    int unix_socket = socket(AF_UNIX, SOCK_STREAM, 0);
    check("AUTOBIND_NAME_SOCKET", unix_socket >= 0);
    struct sockaddr_un request;
    memset(&request, 0, sizeof(request));
    request.sun_family = AF_UNIX;
    mark("AUTOBIND_NAME_BIND", bind(unix_socket, (struct sockaddr *)&request, 2) == 0);
    struct sockaddr_un name;
    memset(&name, 0xa5, sizeof(name));
    length = sizeof(name);
    errno = 0;
    int shape = getsockname(unix_socket, (struct sockaddr *)&name, &length) == 0 && length == 8 &&
                name.sun_family == AF_UNIX && name.sun_path[0] == '\0';
    for (int index = 1; shape && index <= 5; index++) {
        char digit = name.sun_path[index];
        if (!((digit >= '0' && digit <= '9') || (digit >= 'a' && digit <= 'f'))) {
            shape = 0;
        }
    }
    mark("UNIX_AUTOBIND_NAME_RECORD", shape);
    close(unix_socket);
}

static const char *only_case;

/* Development affordance: an optional first argument names one case (the
 * `PROGRAM_CASES` spelling from the differential runner), so a single
 * divergence can be inspected without the fail-fast `mark()` aborting an
 * earlier case of the same program.  The registered differential run passes no
 * argument and therefore still executes and reports every case exactly once. */
static int case_selected(const char *name) {
    return only_case == NULL || strcmp(only_case, name) == 0;
}

int main(int argc, char **argv) {
    if (argc > 1) {
        only_case = argv[1];
    }
    alarm(30);
    if (case_selected("socket_creation_order")) {
        begin("socket_creation_order.portable-differential");
        creation_order();
        done();
    }

    if (case_selected("socket_unix_creation")) {
        begin("socket_unix_creation.portable-differential");
        unix_creation();
        done();
    }

    if (case_selected("socket_netlink_creation")) {
        begin("socket_netlink_creation.portable-differential");
        netlink_creation();
        done();
    }

    if (case_selected("socket_netlink_policy")) {
        begin("socket_netlink_policy.portable-differential");
        netlink_policy();
        done();
    }

    if (case_selected("socket_address_lengths")) {
        begin("socket_address_lengths.portable-differential");
        address_lengths();
        done();
    }

    if (case_selected("socket_name_record")) {
        begin("socket_name_record.portable-differential");
        name_record();
        done();
    }

    if (case_selected("socket_sol_socket_table")) {
        begin("socket_sol_socket_table.portable-differential");
        sol_socket_table();
        done();
    }

    if (case_selected("socket_netlink_option_table")) {
        begin("socket_netlink_option_table.portable-differential");
        netlink_option_table();
        done();
    }

    if (case_selected("socket_null_operations")) {
        begin("socket_null_operations.portable-differential");
        null_operations();
        done();
    }

    puts("THEKERNEL_SOCKET_PROVIDER_PASS");
    return 0;
}
