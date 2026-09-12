#define _GNU_SOURCE
#include <errno.h>
#include <linux/capability.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <netinet/in.h>
#include <net/if.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <linux/errqueue.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *active;
static void begin(const char *name) { active = name; printf("THEKERNEL_ABI_CASE %s\n", name); }
static void mark(const char *name, int good) {
    printf("THEKERNEL_ABI_ASSERT %s %s %s\n", active, name, good ? "pass" : "fail");
    if (!good) { fprintf(stderr, "%s: errno=%d (%s)\n", name, errno, strerror(errno)); exit(1); }
}
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
static int udp(int family) { int fd = socket(family, SOCK_DGRAM, 0); if (fd < 0) exit(1); return fd; }
static int peer(int fd, unsigned pid, unsigned groups) {
    struct sockaddr_nl value = {0}; socklen_t len = sizeof(value);
    return getpeername(fd, (struct sockaddr *)&value, &len) == 0 && len == sizeof(value)
        && value.nl_family == AF_NETLINK && value.nl_pad == 0
        && value.nl_pid == pid && value.nl_groups == groups;
}

static void check(const char *name, int good) { if (!good) mark(name, 0); }

static void udp_error_queue(int family) {
    struct sockaddr_storage target = {0};
    socklen_t target_len;
    int level, option;
    if (family == AF_INET) {
        struct sockaddr_in *address = (void *)&target;
        address->sin_family = AF_INET;
        address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        target_len = sizeof(*address); level = IPPROTO_IP; option = IP_RECVERR;
    } else {
        struct sockaddr_in6 *address = (void *)&target;
        address->sin6_family = AF_INET6;
        address->sin6_addr = in6addr_loopback;
        target_len = sizeof(*address); level = IPPROTO_IPV6; option = IPV6_RECVERR;
    }
    int closed = udp(family), fd = udp(family);
    check("ERROR_DESTINATION_BIND", bind(closed, (void *)&target, target_len) == 0);
    check("ERROR_DESTINATION_PORT", getsockname(closed, (void *)&target, &target_len) == 0);
    check("ERROR_CONNECT", connect(fd, (void *)&target, target_len) == 0);
    close(closed);
    int enabled = 1, value = 0;
    socklen_t value_len = sizeof(value);
    check("RECVERR_ENABLE", setsockopt(fd, level, option, &enabled, sizeof(enabled)) == 0);
    check("RECVERR_GET", getsockopt(fd, level, option, &value, &value_len) == 0 &&
         value == 1 && value_len == sizeof(value));
    const char payload[] = "udp-error-payload";
    struct pollfd pending = {.fd = fd};
    for (int pass = 0; pass < 6; ++pass) {
        check("ERROR_SEND", send(fd, payload, sizeof(payload), 0) == sizeof(payload));
        pending.revents = 0;
        check("ERROR_POLL", poll(&pending, 1, 3000) == 1 && (pending.revents & POLLERR));
        value = 0; value_len = sizeof(value);
        check("ERROR_SO_ERROR", getsockopt(fd, SOL_SOCKET, SO_ERROR, &value, &value_len) == 0 &&
             value == ECONNREFUSED);
        pending.revents = 0;
        check("ERROR_QUEUE_REMAINS_READY", poll(&pending, 1, 0) == 1 && (pending.revents & POLLERR));
        char data[64] = {0};
        union { struct cmsghdr align; unsigned char bytes[128]; } control;
        struct sockaddr_storage destination = {0};
        struct iovec iov = {.iov_base = data, .iov_len = pass == 1 ? 1 : sizeof(data)};
        struct msghdr message = {.msg_name = &destination, .msg_namelen = sizeof(destination),
            .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control.bytes,
            .msg_controllen = pass == 2 ? 1 : pass == 3 ? 20 : pass == 4 ? 32 : pass == 5 ? 47 : sizeof(control)};
        int flags = MSG_ERRQUEUE | MSG_DONTWAIT | (pass == 1 ? MSG_PEEK | MSG_TRUNC | MSG_OOB : 0);
        ssize_t got = recvmsg(fd, &message, flags);
        check("ERROR_PAYLOAD", got == (pass == 1 ? 1 : (ssize_t)sizeof(payload)) &&
             memcmp(data, payload, got) == 0);
        check("ERROR_FLAGS", (message.msg_flags & MSG_ERRQUEUE) &&
             !!(message.msg_flags & MSG_TRUNC) == (pass == 1) &&
             !!(message.msg_flags & MSG_CTRUNC) == (pass >= 2));
        check("ERROR_DESTINATION", message.msg_namelen == target_len &&
             memcmp(&destination, &target, target_len) == 0);
        if (pass != 2) {
            struct cmsghdr *cmsg = CMSG_FIRSTHDR(&message);
            check("ERROR_CMSG", cmsg && cmsg->cmsg_level == level && cmsg->cmsg_type == option &&
                 cmsg->cmsg_len == (pass < 2 ? CMSG_LEN(sizeof(struct sock_extended_err) + target_len) : message.msg_controllen));
            struct sock_extended_err *error = (void *)CMSG_DATA(cmsg);
            check("ERROR_ERRNO", error->ee_errno == ECONNREFUSED);
            if (pass != 3)
                check("ERROR_ICMP", error->ee_errno == ECONNREFUSED &&
                 error->ee_origin == (family == AF_INET ? SO_EE_ORIGIN_ICMP : SO_EE_ORIGIN_ICMP6) &&
                 error->ee_type == (family == AF_INET ? 3 : 1) &&
                 error->ee_code == (family == AF_INET ? 3 : 4) && !error->ee_info && !error->ee_data);
            if (pass < 2) {
                struct sockaddr *offender = SO_EE_OFFENDER(error);
                check("ERROR_OFFENDER", offender->sa_family == family &&
                     (family == AF_INET ? ((struct sockaddr_in *)offender)->sin_addr.s_addr == htonl(INADDR_LOOPBACK) :
                      memcmp(&((struct sockaddr_in6 *)offender)->sin6_addr, &in6addr_loopback, 16) == 0));
            }
        }
        errno = 0;
        check("ERROR_DEQUEUED", recvmsg(fd, &message, MSG_ERRQUEUE | MSG_DONTWAIT) == -1 && errno == EAGAIN);
        value = -1; value_len = sizeof(value);
        check("ERROR_CLEARED", getsockopt(fd, SOL_SOCKET, SO_ERROR, &value, &value_len) == 0 && value == 0);
    }
    enabled = 0;
    check("RECVERR_DISABLE", setsockopt(fd, level, option, &enabled, sizeof(enabled)) == 0);
    close(fd);
    mark(family == AF_INET ? "UDP4_ERROR_QUEUE" : "UDP6_ERROR_QUEUE", 1);
}
/* A successful send followed by close must outlive the descriptor when the
 * receiver has not yet drained its window. Synchronize after close, rather
 * than relying on a scheduler delay to reproduce queued-data loss. */
static void tcp_close_queued(int family, int lifecycle) {
    enum { LENGTH = 192 * 1024 };
    int listener = socket(family, SOCK_STREAM, 0), ready[2];
    struct sockaddr_storage address = {0};
    socklen_t length;
    if (family == AF_INET) {
        struct sockaddr_in *a = (void *)&address;
        a->sin_family = AF_INET; a->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        length = sizeof(*a);
    } else {
        struct sockaddr_in6 *a = (void *)&address;
        a->sin6_family = AF_INET6; a->sin6_addr = in6addr_loopback;
        length = sizeof(*a);
    }
    check("TCP_LISTENER", listener >= 0 && bind(listener, (void *)&address, length) == 0 &&
          listen(listener, 1) == 0 && getsockname(listener, (void *)&address, &length) == 0);
    check("TCP_CLOSE_PIPE", pipe(ready) == 0);
    pid_t child = fork();
    check("TCP_CLOSE_FORK", child >= 0);
    if (!child) {
        close(ready[0]);
        int sender = accept(listener, NULL, NULL);
        close(listener);
        if (sender < 0) _exit(2);
        unsigned char block[4096];
        for (unsigned i = 0; i < sizeof(block); i++) block[i] = (unsigned char)(i * 17 + 3);
        size_t sent = 0;
        while (sent < LENGTH) {
            ssize_t n = send(sender, block, sizeof(block), MSG_NOSIGNAL);
            if (n <= 0) _exit(3);
            /* Each full block repeats, but partial sends retain their offset. */
            sent += n;
            if (n != sizeof(block)) {
                size_t offset = n;
                while (offset < sizeof(block)) {
                    n = send(sender, block + offset, sizeof(block) - offset, MSG_NOSIGNAL);
                    if (n <= 0) _exit(4);
                    offset += n; sent += n;
                }
            }
        }
        close(sender);
        if (write(ready[1], "c", 1) != 1) _exit(5);
        close(ready[1]);
        _exit(0);
    }
    close(ready[1]);
    int receiver = socket(family, SOCK_STREAM, 0);
    check("TCP_CLOSE_CONNECT", receiver >= 0 && connect(receiver, (void *)&address, length) == 0);
    close(listener);
    char closed;
    check("TCP_CLOSED_BEFORE_READ", read(ready[0], &closed, 1) == 1 && closed == 'c');
    close(ready[0]);
    if (lifecycle == 2) {
        /* No socket syscall or peer window update while the orphan timeout
         * runs. A later bind observes whether the service reclaimed it. */
        sleep(65);
        int rebound = socket(family, SOCK_STREAM, 0);
        check("TCP_ORPHAN_TIMEOUT_RECLAIM", rebound >= 0 && bind(rebound, (void *)&address, length) == 0);
        close(rebound);
    }
    size_t received = 0;
    unsigned char block[8192];
    while (lifecycle != 2) {
        ssize_t n = read(receiver, block, sizeof(block));
        check("TCP_CLOSE_READ", n >= 0);
        if (!n) break;
        for (ssize_t i = 0; i < n; i++)
            check("TCP_CLOSE_PAYLOAD", block[i] == (unsigned char)((received + i) * 17 + 3));
        received += n;
    }
    if (lifecycle != 2) check("TCP_CLOSE_LENGTH", received == LENGTH);
    close(receiver);
    int status;
    check("TCP_CLOSE_CHILD", waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
    if (lifecycle == 1) {
        /* The kernel's smoltcp TIME-WAIT is 10 s. All connection fds have
         * gone: only the namespace service timer can retire the handles. */
        sleep(12);
        int rebound = socket(family, SOCK_STREAM, 0);
        check("TCP_LAST_FD_TIMER_RECLAIM", rebound >= 0 && bind(rebound, (void *)&address, length) == 0);
        close(rebound);
    }
}

static void lifecycle_enable_loopback(void) {
    int query = udp(AF_INET);
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "lo");
    check("TCP_LIFECYCLE_LO_QUERY_DOWN", ioctl(query, SIOCGIFFLAGS, &ifr) == 0);
    mark("TCP_LIFECYCLE_LO_IS_DOWN", !(ifr.ifr_flags & IFF_UP));
    check("TCP_LIFECYCLE_LO_INDEX", ioctl(query, SIOCGIFINDEX, &ifr) == 0);
    int index = ifr.ifr_ifindex;
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    check("TCP_LIFECYCLE_ROUTE_SOCKET", fd >= 0);
    struct sockaddr_nl kernel = {.nl_family = AF_NETLINK};
    struct { struct nlmsghdr header; struct ifinfomsg link; } request = {
        .header = {.nlmsg_len = NLMSG_LENGTH(sizeof(struct ifinfomsg)),
                   .nlmsg_type = RTM_NEWLINK, .nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK,
                   .nlmsg_seq = 1},
        .link = {.ifi_family = AF_UNSPEC, .ifi_index = index,
                 .ifi_flags = IFF_UP, .ifi_change = IFF_UP},
    };
    check("TCP_LIFECYCLE_LO_UP_SEND", sendto(fd, &request, request.header.nlmsg_len, 0,
          (struct sockaddr *)&kernel, sizeof(kernel)) == (ssize_t)request.header.nlmsg_len);
    unsigned char response[256];
    ssize_t count = recv(fd, response, sizeof(response), 0);
    struct nlmsghdr *header = (void *)response;
    check("TCP_LIFECYCLE_LO_UP_ACK", count >= (ssize_t)NLMSG_LENGTH(sizeof(struct nlmsgerr))
          && header->nlmsg_type == NLMSG_ERROR && header->nlmsg_seq == 1
          && ((struct nlmsgerr *)NLMSG_DATA(header))->error == 0);
    close(fd);
    check("TCP_LIFECYCLE_LO_QUERY_UP", ioctl(query, SIOCGIFFLAGS, &ifr) == 0);
    mark("TCP_LIFECYCLE_LO_IS_UP", (ifr.ifr_flags & IFF_UP) != 0);
    close(query);
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--close-lifecycle")) {
        alarm(100);
        begin("network_tcp_close.kernel-lifecycle");
        check("TCP_LIFECYCLE_NETNS", unshare(CLONE_NEWNET) == 0);
        // Let the namespace worker observe administratively down lo. Bringing
        // it up must resume service rather than inherit a terminal RX state.
        sleep(1);
        check("TCP_LIFECYCLE_LO_ADDRESS", system("/sbin/ip address add 127.0.0.1/8 dev lo") == 0);
        lifecycle_enable_loopback();
        tcp_close_queued(AF_INET, 1);
        mark("LAST_FD_TIMER_RECLAIM", 1);
        tcp_close_queued(AF_INET, 2);
        mark("ZERO_WINDOW_TIMEOUT_RECLAIM", 1);
        done();
        return 0;
    }
    alarm(30);
    unsigned char storage[129] = {0};
    struct sockaddr_in *v4 = (void *)storage;
    v4->sin_family = AF_INET; v4->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    int fd = udp(AF_INET);
    begin("network_bind.raw-differential");
    errno = 0; mark("IPV4_OVERLONG_EINVAL", syscall(SYS_bind, fd, storage, 129) == -1 && errno == EINVAL);
    mark("IPV4_STORAGE_BOUNDARY", syscall(SYS_bind, fd, storage, 128) == 0);
    close(fd);
    memset(storage, 0, sizeof(storage));
    struct sockaddr_in6 *v6 = (void *)storage; v6->sin6_family = AF_INET6; v6->sin6_addr = in6addr_loopback;
    fd = udp(AF_INET6);
    errno = 0; mark("IPV6_OVERLONG_EINVAL", syscall(SYS_bind, fd, storage, 129) == -1 && errno == EINVAL);
    close(fd); done();

    begin("network_connect.raw-differential");
    fd = udp(AF_INET6);
    errno = 0; mark("IPV6_OVERLONG_EINVAL", syscall(SYS_connect, fd, storage, 129) == -1 && errno == EINVAL);
    close(fd);
    memset(storage, 0, sizeof(storage)); v4 = (void *)storage; v4->sin_family = AF_INET;
    v4->sin_addr.s_addr = htonl(INADDR_LOOPBACK); v4->sin_port = htons(9);
    fd = udp(AF_INET);
    errno = 0; mark("IPV4_OVERLONG_EINVAL", syscall(SYS_connect, fd, storage, 129) == -1 && errno == EINVAL);
    close(fd);
    int nl = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    mark("NETLINK_SOCKET", nl >= 0);
    struct sockaddr_nl kernel = {.nl_family = AF_NETLINK};
    mark("NETLINK_KERNEL_CONNECT", connect(nl, (struct sockaddr *)&kernel, sizeof(kernel)) == 0);
    struct sockaddr_nl local = {0}; socklen_t size = sizeof(local);
    mark("NETLINK_AUTOBIND", getsockname(nl, (struct sockaddr *)&local, &size) == 0 && local.nl_pid != 0);
    struct sockaddr unspec = {.sa_family = AF_UNSPEC};
    mark("NETLINK_DISCONNECT", connect(nl, &unspec, sizeof(sa_family_t)) == 0);
    mark("NETLINK_DISCONNECTED_PEER", peer(nl, 0, 0));
    struct sockaddr bad = {.sa_family = AF_INET};
    errno = 0; mark("NETLINK_BAD_FAMILY", connect(nl, &bad, sizeof(bad)) == -1 && errno == EINVAL);
    close(nl);
    tcp_close_queued(AF_INET, 0);
    tcp_close_queued(AF_INET6, 0);
    mark("TCP_CLOSE_QUEUED", 1);
    done();

    begin("network_getpeername.raw-differential");
    nl = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    mark("NETLINK_UNCONNECTED_ZERO", nl >= 0 && peer(nl, 0, 0));
    mark("NETLINK_CONNECTED_ZERO", connect(nl, (struct sockaddr *)&kernel, sizeof(kernel)) == 0 && peer(nl, 0, 0));
    struct sockaddr_nl destination = {.nl_family = AF_NETLINK, .nl_pid = 12345, .nl_groups = 6};
    struct __user_cap_header_struct cap_header = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
    struct __user_cap_data_struct cap_data[2] = {{0}, {0}};
    if (syscall(SYS_capget, &cap_header, cap_data) != 0) {
        perror("network capget");
        return 1;
    }
    int has_net_admin = (cap_data[CAP_NET_ADMIN / 32].effective &
                         (1U << (CAP_NET_ADMIN % 32))) != 0;
    errno = 0;
    int connected = connect(nl, (struct sockaddr *)&destination, sizeof(destination));
    mark("NETLINK_PEER_POLICY", has_net_admin ? connected == 0 : (connected == -1 && errno == EPERM));
    mark("NETLINK_PEER_STATE", has_net_admin ? peer(nl, 12345, 2) : peer(nl, 0, 0));
    mark("NETLINK_PEER_RESET", connect(nl, &unspec, sizeof(sa_family_t)) == 0 && peer(nl, 0, 0));
    unsigned char short_name[2] = {0}; size = sizeof(short_name);
    mark("NETLINK_TRUNCATED_LENGTH", getpeername(nl, (struct sockaddr *)short_name, &size) == 0 && size == sizeof(kernel));
    close(nl); done();

    begin("network_sendto.raw-differential");
    fd = udp(AF_INET);
    errno = 0; mark("IPV4_OVERLONG_EINVAL", syscall(SYS_sendto, fd, "x", 1, 0, storage, 129) == -1 && errno == EINVAL);
    close(fd);
    memset(storage, 0, sizeof(storage)); v6 = (void *)storage; v6->sin6_family = AF_INET6;
    v6->sin6_addr = in6addr_loopback; v6->sin6_port = htons(9);
    fd = udp(AF_INET6);
    errno = 0; mark("IPV6_OVERLONG_EINVAL", syscall(SYS_sendto, fd, "x", 1, 0, storage, 129) == -1 && errno == EINVAL);
    close(fd);
    udp_error_queue(AF_INET);
    udp_error_queue(AF_INET6);
    done();
    puts("THEKERNEL_NETWORK_BASIC_PASS");
    return 0;
}
