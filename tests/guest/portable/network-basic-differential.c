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
int main(void) {
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
    close(nl); done();

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
    close(fd); done();
    puts("THEKERNEL_NETWORK_BASIC_PASS");
    return 0;
}
